// Process with a variable number of running instances.
//
// Used for delegation of background operations. Examples:
//   - Run a shell command.
//   - Attempt to contact a server that might timeout.
//
// Interaction with workers is with messaging (channels).
//
// Workers can also serve the purpose of serializing operations. Example, there is one
// instance of shell_worker per workdir running. This allows to :
//   - No more than one shell command executed at the time per workdir.
//   - Shell command on different workdir can be executed concurrently.
//
// flatten everything under "workers" module.
pub(crate) use self::cli_poller::*;
//pub(crate) use self::db_worker::*;
//pub(crate) use self::events_writer_worker::*;
pub(crate) use self::packages_poller::*;
pub(crate) use self::request_worker::*;
pub(crate) use self::webserver::*;
//pub(crate) use self::websocket_worker::*;

mod cli_poller;
//mod db_worker;
//mod events_writer_worker;
mod log_worker;
mod packages_poller;
mod request_worker;
mod webserver;
mod websocket_worker;
