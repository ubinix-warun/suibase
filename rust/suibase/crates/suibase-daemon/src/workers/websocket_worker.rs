// Child thread of events_writer_worker
//
// Responsible to:
//   - websocket auto-reconnect for a single server.
//   - keep alive the connection with Ping
//   - subscribe/unsubscribe to Sui events, filter and forward the
//     validated data to its parent thread.
//
// The thread is auto-restart in case of panic.

use std::{collections::HashMap, sync::Arc};

use crate::shared_types::Globals;

use common::shared_types::{
    WORKDIRS_KEYS, WORKDIR_IDX_DEVNET, WORKDIR_IDX_LOCALNET, WORKDIR_IDX_MAINNET,
    WORKDIR_IDX_TESTNET,
};

use anyhow::Result;
use axum::async_trait;

use common::{
    basic_types::{
        self, AutoThread, GenericChannelMsg, GenericRx, GenericTx, Runnable, WorkdirIdx,
    },
    log_safe, mpsc_q_check,
};

use futures::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_graceful_shutdown::{FutureExt, SubsystemHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

use common::basic_types::remove_generic_event_dups;
use common::workers::{SubscriptionTracking, SubscriptionTrackingState};

#[derive(Clone)]
pub struct WebSocketWorkerParams {
    globals: Globals,
    event_rx: Arc<Mutex<GenericRx>>,
    event_tx: GenericTx,         // To send message to self.
    events_writer_tx: GenericTx, // To send message to parent EventsWriterWorker.
    workdir_idx: WorkdirIdx,
    workdir_name: String,
}

impl WebSocketWorkerParams {
    pub fn new(
        globals: Globals,
        event_rx: GenericRx,
        event_tx: GenericTx,
        events_writer_tx: GenericTx,
        workdir_idx: WorkdirIdx,
    ) -> Self {
        Self {
            globals,
            event_rx: Arc::new(Mutex::new(event_rx)),
            event_tx,
            events_writer_tx,
            workdir_idx,
            workdir_name: WORKDIRS_KEYS[workdir_idx as usize].to_string(),
        }
    }
}

pub struct WebSocketWorker {
    auto_thread: AutoThread<WebSocketWorkerThread, WebSocketWorkerParams>,
}

impl WebSocketWorker {
    pub fn new(params: WebSocketWorkerParams) -> Self {
        Self {
            auto_thread: AutoThread::new("WebSocketWorker".to_string(), params),
        }
    }

    pub async fn run(self, subsys: SubsystemHandle) -> Result<()> {
        self.auto_thread.run(subsys).await
    }
}

#[derive(Debug, Default)]
struct WebSocketManagement {
    // Active websocket connection.
    write: Option<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    read: Option<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>>,

    // Sequence number to use as "id" for JSON-RPC.
    // Must be incremented prior to use it in a new request.
    seq_number: u64,
}

impl WebSocketManagement {
    pub fn new() -> Self {
        // TODO Initialize sequence number with a UTC in milliseconds.
        Self {
            write: None,
            read: None,
            seq_number: 0,
        }
    }
}

struct WebSocketWorkerThread {
    task_name: String,
    params: WebSocketWorkerParams,

    // Key is the package_id.
    package_subs: HashMap<String, SubscriptionTracking>,

    websocket: WebSocketManagement,
}

#[async_trait]
impl Runnable<WebSocketWorkerParams> for WebSocketWorkerThread {
    fn new(task_name: String, params: WebSocketWorkerParams) -> Self {
        Self {
            task_name,
            params,
            package_subs: HashMap::new(),
            websocket: WebSocketManagement::new(),
        }
    }

    async fn run(mut self, subsys: SubsystemHandle) -> Result<()> {
        // let output = format!("started {}", self.params.workdir_name);
        // log::info!("{}", output);

        match self.event_loop(&subsys).cancel_on_shutdown(&subsys).await {
            Ok(()) => {
                // log::info!("normal thread exit (2)");
                Ok(())
            }
            Err(_cancelled_by_shutdown) => {
                log::info!("{} normal thread exit (1)", self.task_name);
                Ok(())
            }
        }
    }
}

impl WebSocketWorkerThread {
    fn subscribe_request_format(id: u64, package_id: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"suix_subscribeEvent","id":{},"params":[{{"Package":"{}"}}]}}"#,
            id, package_id
        )
    }

    fn unsubscribe_request_format(id: u64, unsubscribe_id: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"suix_unsubscribeEvent","id":{},"params":[{}]}}"#,
            id, unsubscribe_id
        )
    }

    async fn process_ws_msg(&mut self, msg: Message) {
        //log::info!("Received a websocket message: {:?}", msg);

        // **********************************************************************
        // **********************************************************************
        // **********************************************************************
        // **********************************************************************
        // TODO Important !!!!!!! Replace all [] with get()... like done in DB worker
        // **********************************************************************
        // **********************************************************************
        // **********************************************************************
        // **********************************************************************
        let (json_msg, msg_seq_number) = match msg {
            Message::Text(text) => {
                let json = serde_json::from_str(&text);
                if json.is_err() {
                    log::error!("Failed to parse JSON: {:?}", text);
                    return;
                }
                let json_msg: serde_json::Value = json.unwrap();
                let id = json_msg["id"].as_u64().unwrap_or(0);
                (json_msg, id)
            }
            _ => {
                log::error!("Unexpected websocket message: {:?}", msg);
                return;
            }
        };

        // Check for expected response (correlate using the JSON-RPC id).
        let mut trig_audit_event = false;
        let mut correlated_msg = false;
        if msg_seq_number != 0 {
            for package in self.package_subs.values_mut() {
                let state = package.state();
                if state == &SubscriptionTrackingState::Subscribing {
                    if package.did_sent_subscribe_request(msg_seq_number) {
                        correlated_msg = true;
                        /*log::info!(
                            "Received subscribe resp. workdir={} resp={:?}",
                            self.params.workdir_name,
                            json_msg,
                        );*/
                        // Got an expected subscribe response.
                        // Extract the result string from the JSON message.
                        let result = json_msg["result"].as_u64();
                        if result.is_none() {
                            log::error!(
                                "Missing result field in subscribe JSON resp. workdir={} resp={:?}",
                                self.params.workdir_name,
                                json_msg
                            );
                            return;
                        }
                        let unsubscribe_id = result.unwrap();
                        package.report_subscribing_response(unsubscribe_id.to_string());
                        trig_audit_event = true;
                        break;
                    }
                } else if state == &SubscriptionTrackingState::Unsubscribing
                    && package.did_sent_unsubscribe_request(msg_seq_number)
                {
                    // Got an expected unsubscribe response.
                    correlated_msg = true;
                    log::info!(
                        "Received unsubscribe resp. workdir={} resp={:?}",
                        self.params.workdir_name,
                        json_msg,
                    );

                    package.report_unsubscribing_response();
                    trig_audit_event = true;
                    break;
                }
            }
        }

        if !correlated_msg {
            // Check if a valid Sui event message.
            let method = json_msg["method"].as_str();
            if method.is_none() {
                log::error!(
                    "Missing method in Sui Event message. workdir={} message={:?}",
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }
            let method = method.unwrap();
            if method != "suix_subscribeEvent" {
                log::error!(
                    "Unexpected method in Sui Event message. workdir={} message={:?}",
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }

            let params = json_msg["params"].as_object();
            if params.is_none() {
                log::error!(
                    "Missing params in Sui Event message. workdir={} message={:?}",
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }
            let params = params.unwrap();
            let subscription = params["subscription"].as_u64();
            if subscription.is_none() {
                log::error!(
                    "Missing subscription in Sui Event message. workdir={} message={:?}",
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }
            let subscription_number = subscription.unwrap();
            let result = params["result"].as_object();
            if result.is_none() {
                log::error!(
                    "Missing result in Sui Event message. workdir={} message={:?}",
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }
            let result = result.unwrap();

            // Find the related package uuid (Suibase ID) and name using the
            // subscription number.
            let mut package_uuid: Option<String> = None;
            let mut package_name: Option<String> = None;
            for tracker in self.package_subs.values_mut() {
                let state = tracker.state();
                if state == &SubscriptionTrackingState::Subscribed
                    && tracker.subscription_number() == subscription_number
                {
                    package_uuid = Some(tracker.uuid().clone());
                    package_name = Some(tracker.name().clone());
                    // While we are here... do a sanity check that packageId field
                    // match what is in PackageTrackingState.
                    let package_id = result["packageId"].as_str();
                    if package_id.is_none() {
                        log::error!(
                            "Missing packageId in Sui Event message. workdir={} message={:?}",
                            self.params.workdir_name,
                            json_msg
                        );
                        return;
                    }
                    let package_id = package_id.unwrap();
                    // Verify package_id starts with "0x", and then create a slice that
                    // remove the "0x".
                    if !package_id.starts_with("0x") {
                        log::error!(
                            "Invalid packageId in Sui Event message. workdir={} message={:?}",
                            self.params.workdir_name,
                            json_msg
                        );
                        return;
                    }
                    let package_id = &package_id[2..];
                    let expected_package_id = tracker.package_filter().cloned().unwrap_or_default();
                    if package_id != expected_package_id {
                        log::error!(
                                "packageId {} not matching {} in Sui Event message. workdir={} message={:?}",
                                package_id,
                                expected_package_id,
                                self.params.workdir_name,
                                json_msg
                            );
                        return;
                    }
                    break;
                }
            }

            if package_uuid.is_none() {
                log::warn!(
                    "Unsubscribed state for subscription number {} for Sui Event message. workdir={} message={:?}",
                    subscription_number,
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }
            let package_uuid = package_uuid.unwrap();

            if package_name.is_none() {
                log::warn!(
                    "Missing package name for subscription number {} for Sui Event message. workdir={} message={:?}",
                    subscription_number,
                    self.params.workdir_name,
                    json_msg
                );
                return;
            }
            let package_name = package_name.unwrap();

            // {"jsonrpc": String("2.0"),
            //  "method": String("suix_subscribeEvent"),
            //  "params": Object { "subscription": Number(6351273490251832),
            //                     "result": Object {
            //                        "id": Object {"txDigest": String("3Vua...ChrL"), "eventSeq": String("1")},
            //                        "packageId": String("0xe065...3b08"),
            //                        "transactionModule": String("Counter"),
            //                        "sender": String("0xf7ae...1462"),
            //                        "type": String("0xe065...3b08::Counter::CounterChanged"),
            //                        "parsedJson": Object {"by_address": String("0xf7ae...1462"), "count": String("1")},
            //                        "bcs": String("3t9dC...ELZ"),
            //                        "timestampMs": String("1703895010111")
            //                      }
            //                    }
            // }
            // TODO Validate here if from an expected subscribed package.
            // Forward to the parent thread for deduplication.
            let msg = GenericChannelMsg {
                event_id: basic_types::EVENT_EXEC,
                command: Some("add_sui_event".to_string()),
                params: vec![package_uuid, package_name],
                data_json: Some(json_msg),
                workdir_idx: Some(self.params.workdir_idx),
                resp_channel: None,
            };
            if self.params.events_writer_tx.send(msg).await.is_err() {
                log::error!(
                    "Failed to add_sui_event for workdir_idx={}",
                    self.params.workdir_idx
                );
            }
        }

        if trig_audit_event {
            let msg = GenericChannelMsg {
                event_id: basic_types::EVENT_AUDIT,
                command: None,
                params: Vec::new(),
                data_json: None,
                workdir_idx: Some(self.params.workdir_idx),
                resp_channel: None,
            };
            if self.params.event_tx.send(msg).await.is_err() {
                log::error!(
                    "Failed to send audit message for workdir_idx={}",
                    self.params.workdir_idx
                );
            }
        }
    }

    async fn process_audit_msg(&mut self, msg: GenericChannelMsg) {
        // This function takes care of operation that need to sync
        // between self.packages and the packages_config information.
        //
        // Changes to packages_config are NOT allowed here. See process_update_msg()
        // for operations that requires touching the packages_config globals.

        if msg.event_id != basic_types::EVENT_AUDIT {
            log::error!("Unexpected event_id {:?}", msg);
            return;
        }

        // Verify that the workdir_idx is as expected.
        if let Some(workdir_idx) = msg.workdir_idx {
            if workdir_idx != self.params.workdir_idx {
                log::error!(
                    "Unexpected workdir_idx {:?} (expected {:?})",
                    workdir_idx,
                    self.params.workdir_idx
                );
                return;
            }
        } else {
            log::error!("Missing workdir_idx {:?}", msg);
            return;
        }
        let workdir_idx = self.params.workdir_idx;

        // log::info!("Received an audit message: {:?}", msg);
        let mut state_change = false;
        {
            // Get a reader lock on the globals packages_config.
            let globals_read_guard = self.params.globals.get_packages(workdir_idx).read().await;
            let globals = &globals_read_guard;

            // Get the move_configs in ui for workdir_idx.
            if let Some(ui) = &globals.ui {
                let wp_resp = ui.get_data();

                // Check for adding PackagesTracking.
                // Add a PackagesTracking in the packages HashMap for every latests in packages_config.
                // Once created, the PackagesTracking remains until removed from ui.
                // The package_id is used as the key in the packages HashMap.
                for latest in wp_resp.iter_most_recent_package_instance() {
                    // Check if the package is already in the packages HashMap.
                    if !self.package_subs.contains_key(latest.get_package_id()) {
                        // Create a new PackagesTracking.
                        let package_tracking = SubscriptionTracking::new_for_managed_package(
                            latest.get_package_name().to_string(),
                            latest.get_package_uuid().to_string(),
                            latest.get_package_timestamp().to_string(),
                            latest.get_package_id().to_string(),
                        );
                        // Add the PackagesTracking to the packages HashMap.
                        self.package_subs
                            .insert(latest.get_package_id().to_string(), package_tracking);
                    }
                }

                // Transition package to Unsubscribing state when no longer in the config.
                // Remove the package tracking once unsubscription confirmed (or timeout).
                self.package_subs.retain(|package_id, package_tracking| {
                    let is_most_recent = wp_resp
                        .is_most_recent(package_tracking.uuid(), package_tracking.timestamp());
                    if !is_most_recent {
                        if package_tracking.can_be_deleted() {
                            log::info!("Deleting tracking for package_id={}", package_id);
                            return false; // Delete the element in the HashMap.
                        }
                        // Transition toward eventual deletion after Unsubscribing completes (or timeout).
                        if !package_tracking.is_remove_requested() {
                            package_tracking.report_remove_request();
                        }
                    }
                    true // Keep the element in the HashMap.
                });
            }
        } // End of reader lock.

        let websocket = &mut self.websocket;
        let packages = &mut self.package_subs;

        // TODO Transition here to Disconnected or ReadyToDelete on connection lost?

        // Check to update every PackagesTracking state machine.
        for package in packages.values_mut() {
            if package.is_remove_requested() {
                //log::info!("Initiating processing removed from package");
                if Self::try_to_unsubscribe(package, websocket).await {
                    state_change = true;
                }
            } else {
                match package.state() {
                    SubscriptionTrackingState::Disconnected => {
                        // Initial state.
                        if Self::try_to_subscribe(package, websocket).await {
                            state_change = true;
                        }
                    }
                    SubscriptionTrackingState::Subscribing => {
                        if Self::try_to_subscribe(package, websocket).await {
                            state_change = true;
                        }
                    }
                    SubscriptionTrackingState::Subscribed => {
                        // Nothing to do.
                        // Valid next states are Unsubscribing (removed from config) or Disconnected (on connection loss).
                    }
                    SubscriptionTrackingState::Unsubscribing => {
                        // Valid next state is Unsubscribed (on unsubscribed confirmation, timeout) and ReadyToDelete (on connection loss).
                        if Self::try_to_unsubscribe(package, websocket).await {
                            state_change = true;
                        }
                    }
                    SubscriptionTrackingState::ReadyToDelete => {
                        // End state. Nothing to do. The package will eventually be deleted on next audit.
                    }
                }
            }
        }

        if state_change {
            // Update the packages_config globals.
            let msg = GenericChannelMsg {
                event_id: basic_types::EVENT_UPDATE,
                command: None,
                params: Vec::new(),
                data_json: None,
                workdir_idx: Some(self.params.workdir_idx),
                resp_channel: None,
            };
            if self.params.event_tx.send(msg).await.is_err() {
                log::error!(
                    "Failed to send update message for workdir_idx={}",
                    self.params.workdir_idx
                );
            }
        }
    }

    async fn process_update_msg(&mut self, msg: GenericChannelMsg) {
        // This function takes care of synching from self.packages to the global ui.
        //
        // Unlike an audit, changes to ui are allowed here.
        //
        //log::info!("Received an update message: {:?}", msg);

        // Make sure the event_id is EVENT_UPDATE.
        if msg.event_id != basic_types::EVENT_UPDATE {
            log::error!("Unexpected event_id {:?}", msg);
            return;
        }

        // Verify that the workdir_idx is as expected.
        if let Some(workdir_idx) = msg.workdir_idx {
            if workdir_idx != self.params.workdir_idx {
                log::error!(
                    "Unexpected workdir_idx {:?} (expected {:?})",
                    workdir_idx,
                    self.params.workdir_idx
                );
                return;
            }
        } else {
            log::error!("Unexpected workdir_idx {:?}", msg);
            return;
        }
        let workdir_idx = self.params.workdir_idx;

        let mut trig_audit = false;
        {
            // Get a writer lock on the globals ui.
            let mut globals_write_guard =
                self.params.globals.get_packages(workdir_idx).write().await;
            let globals = &mut *globals_write_guard;

            if let Some(ui) = &mut globals.ui {
                let resp = ui.get_mut_data();

                // Check for adding PackagesTracking.
                // Add a PackagesTracking in the packages HashMap for every latests in packages_config.
                // Once created, the PackagesTracking remains until removed from packages_config.
                // The package_id is used as the key in the packages HashMap.
                for latest in resp.iter_mut_most_recent_package_instance() {
                    // Check if the package is already in the packages HashMap.
                    if !self.package_subs.contains_key(latest.get_package_id()) {
                        // Create a new PackagesTracking.
                        let package_tracking = SubscriptionTracking::new_for_managed_package(
                            latest.get_package_name().to_string(),
                            latest.get_package_uuid().to_string(),
                            latest.get_package_timestamp().to_string(),
                            latest.get_package_id().to_string(),
                        );
                        // Add the PackagesTracking to the packages HashMap.
                        self.package_subs
                            .insert(latest.get_package_id().to_string(), package_tracking);
                        trig_audit = true;
                    }
                    /*else {
                        let package_tracking = &self.package_subs[&latest.package_id];
                        let package_tracking_state: u32 = package_tracking.state().clone().into();
                        if move_config.tracking_state != package_tracking_state {
                            move_config.tracking_state = package_tracking_state;
                        }
                    }*/
                }
            }
        }

        if trig_audit {
            let msg = GenericChannelMsg {
                event_id: basic_types::EVENT_AUDIT,
                command: None,
                params: Vec::new(),
                data_json: None,
                workdir_idx: Some(self.params.workdir_idx),
                resp_channel: None,
            };
            if self.params.event_tx.send(msg).await.is_err() {
                log::error!(
                    "Failed to send audit message for workdir_idx={}",
                    self.params.workdir_idx
                );
            }
        }
    }

    async fn try_to_subscribe(
        tracker: &mut SubscriptionTracking,
        websocket: &mut WebSocketManagement,
    ) -> bool {
        // Send a subscribe message, unless there is one already recently pending.
        // On failure, keep retrying as long that package is configured.
        // (retry will be on subsequent call).
        //
        // Return true if there is a state change.
        let mut state_change = false;
        match tracker.state() {
            SubscriptionTrackingState::Disconnected => {
                // Valid state when calling this function.
                if tracker.change_state_to(SubscriptionTrackingState::Subscribing) {
                    state_change = true;
                }
            }
            SubscriptionTrackingState::Subscribing => {
                if tracker.unsubscribed_id().is_some() {
                    if tracker.change_state_to(SubscriptionTrackingState::Subscribed) {
                        state_change = true;
                    }
                    return state_change;
                }
            }
            _ => {
                // All set. Nothing to do.
                return false;
            }
        };

        let mut send_subscribe_message = true;

        // Don't do it if one was already sent in last 2 seconds.
        if tracker.secs_since_last_request() < 2 {
            send_subscribe_message = false;
        }

        if send_subscribe_message {
            if tracker.package_filter().is_none() {
                log::error!("Missing package_filter in SubscriptionTracking");
                return false;
            }
            let package_id = tracker.package_filter().cloned().unwrap_or_default();

            // Check if retrying and log error only on first retry and once in a while after.
            if tracker.request_retry() % 3 == 1 {
                log::error!("Failed to subscribe package_id={}", package_id);
            }
            websocket.seq_number += 1;
            tracker.report_subscribing_request(websocket.seq_number);
            let msg = Message::Text(Self::subscribe_request_format(
                websocket.seq_number,
                &package_id, // Must not have leading 0x
            ));

            if let Some(ref mut write) = websocket.write {
                log::info!("Sending subscribe message: {:?}", msg);
                if let Err(e) = write.send(msg).await {
                    log::error!("subscribe write.send error: {:?}", e);
                } else {
                    log::info!("subscribe write.send success");
                }
            }
        }

        state_change
    }

    async fn try_to_unsubscribe(
        tracker: &mut SubscriptionTracking,
        websocket: &mut WebSocketManagement,
    ) -> bool {
        // If subscribed, then send a unsubscribe message, unless there is one
        // already recently pending.
        //
        // On failure, keep retrying until timeout (retry will be on subsequent call).
        // After being confirmed unsubscribe (or timeout) the PackageTracking state
        // becomes ReadyToDelete.
        let mut state_change = false;
        match tracker.state() {
            SubscriptionTrackingState::Disconnected => {
                // No subscription on-going...
                if tracker.change_state_to(SubscriptionTrackingState::ReadyToDelete) {
                    state_change = true;
                }
                return state_change;
            }
            SubscriptionTrackingState::Subscribing => {
                // If trying to unsubscribe while a subscription request was already sent (and
                // no response receive yet), then let the subscription a chance to complete.
                // This will allow for a clean unsubscribe later.
                // Check for a subscription timeout transition to avoid being block forever.
                if tracker.is_subscribe_request_pending_response()
                    && tracker.secs_since_last_request() >= 2
                {
                    // Do nothing... to give a chance for the subscription to succeed.
                    state_change = false;
                    return state_change;
                }

                if tracker.change_state_to(SubscriptionTrackingState::Unsubscribing) {
                    state_change = true;
                }
                return state_change;
            }
            SubscriptionTrackingState::Subscribed => {
                if tracker.change_state_to(SubscriptionTrackingState::Unsubscribing) {
                    state_change = true;
                }
                return state_change;
            }

            SubscriptionTrackingState::Unsubscribing => {
                // Ready to delete if unsubscribed_id is clear or timeout.
                // The unsubscribed_id is clear when receiving a unsubscribe response.
                if tracker.unsubscribed_id().is_none() || tracker.request_retry() > 10 {
                    if tracker.change_state_to(SubscriptionTrackingState::ReadyToDelete) {
                        state_change = true;
                    }
                    return state_change;
                }
            }

            SubscriptionTrackingState::ReadyToDelete => {
                // Nothing to do.
                state_change = false;
                return state_change;
            }
        };

        // If there is no known unsubscribed_id, then no point to try to unsubscribe.
        if tracker.unsubscribed_id().is_none() {
            if tracker.change_state_to(SubscriptionTrackingState::ReadyToDelete) {
                state_change = true;
            }
            return state_change;
        }

        let mut send_unsubscribe_message = true;
        // Don't do it if one was already sent in last 2 seconds.
        if tracker.secs_since_last_request() < 2 {
            send_unsubscribe_message = false;
        }

        if send_unsubscribe_message {
            // Periodically report an error on too many retry.
            if tracker.request_retry() % 3 == 1 {
                log::error!("Failed to unsubscribe");
            }
            websocket.seq_number += 1;
            tracker.report_unsubscribing_request(websocket.seq_number);
            let msg = Message::Text(Self::unsubscribe_request_format(
                websocket.seq_number,
                tracker.unsubscribed_id().unwrap(), // Must not have leading 0x
            ));

            if let Some(ref mut write) = websocket.write {
                log::info!("Sending unsubscribe message: {:?}", msg);
                if let Err(e) = write.send(msg).await {
                    log::error!("unsubscribe write.send error: {:?}", e);
                } else {
                    log::info!("unsubscribe write.send success");
                }
            }
        }

        state_change
    }

    async fn open_websocket(&mut self) -> bool {
        // Open a websocket connection to the server for this workdir.

        // TODO Change this to the actual server URL from the config.
        // For now, use hard coded Mysten Labs servers...
        let socket_url = match self.params.workdir_idx {
            WORKDIR_IDX_LOCALNET => "ws://localhost:9000",
            WORKDIR_IDX_DEVNET => "wss://fullnode.devnet.sui.io:443",
            WORKDIR_IDX_TESTNET => "wss://fullnode.testnet.sui.io:443",
            WORKDIR_IDX_MAINNET => "wss://fullnode.mainnet.sui.io:443",
            _ => {
                log::error!("Unexpected workdir_idx {:?}", self.params.workdir_idx);
                return false;
            }
        };

        match connect_async(socket_url).await {
            Ok((ws_stream, _response)) => {
                let (write, read) = ws_stream.split();
                self.websocket.write = Some(write);
                self.websocket.read = Some(read);
            }
            Err(e) => {
                if !e.to_string().contains("Connection refused") {
                    // "Connection refused" is annoying when localnet is not running, so ignore it.
                    // TODO Make this more "aware" about if localnet should be running or not.
                    log_safe!(format!("connect_async error: {:?}", e));
                }
                self.websocket.write = None;
                self.websocket.read = None;
            }
        }

        self.websocket.write.is_some()
    }

    async fn event_loop(&mut self, subsys: &SubsystemHandle) {
        // Take mutable ownership of the event_rx channel as long this thread is running.
        let event_rx = Arc::clone(&self.params.event_rx);
        let mut event_rx = event_rx.lock().await;

        // Remove duplicate of EVENT_AUDIT and EVENT_UPDATE in the event_rx queue.
        remove_generic_event_dups(&mut event_rx, &self.params.event_tx);
        mpsc_q_check!(event_rx); // Just to help verify if the Q unexpectedly "accumulate".

        if self.websocket.write.is_none() && !self.open_websocket().await {
            // Delay to avoid fast restart loop.
            tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;
            return; // This will restart the thread.
        }

        while !subsys.is_shutdown_requested() {
            let ws_stream_future =
                futures::FutureExt::fuse(self.websocket.read.as_mut().unwrap().next());
            let event_rx_future = futures::FutureExt::fuse(event_rx.recv());

            // TODO Add testnet support at least...
            // For now, only localnet works reliably (subscription service not yet reliable, may be graphql is the solution?)
            if self.params.workdir_idx != WORKDIR_IDX_LOCALNET {
                continue;
            }

            tokio::select! {
                msg = ws_stream_future => {
                    if let Some(msg) = msg {
                        if let Ok(msg) = msg {
                            // Process the message.
                            self.process_ws_msg(msg).await;
                        } else {
                            // Connection lost.
                            //log::info!("Connection lost for {}", self.params.workdir_name);
                            self.websocket.write = None;
                            self.websocket.read = None;
                            return;
                        }
                    } else {
                        // Shutdown requested.
                        log::info!("Received {} None websocket message", self.params.workdir_name);
                        return;
                    }
                }
                msg = event_rx_future => {
                    if let Some(msg) = msg {
                        // Process the message.
                        match msg.event_id {
                            basic_types::EVENT_AUDIT => {
                                self.process_audit_msg(msg).await;
                            },
                            basic_types::EVENT_UPDATE => {
                                self.process_update_msg(msg).await;
                            },
                            _ => {
                                // Consume unexpected messages.
                                log::error!("Unexpected event_id {:?}", msg );
                            }
                        }
                    } else {
                        // Channel closed or shutdown requested.
                        log::info!("Received {} None internal message", self.params.workdir_name );
                        return;
                    }
                }
            }
        }
    }
}
