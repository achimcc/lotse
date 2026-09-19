//! lotse: queue, admit and retry the heavy runs of parallel sessions on one
//! workstation. No daemon: the state is a directory, liveness is an `flock`.

pub mod admit;
pub mod config;
pub mod proc;
pub mod run;
pub mod snapshot;
pub mod state;
pub mod status;
pub mod units;
pub mod wait;

/// Wrong usage, no or broken configuration, no state directory.
pub const EXIT_USAGE: i32 = 2;
/// `--max-wait` passed without a slot (`run`) or with the class still busy (`wait`).
pub const EXIT_WAIT_LIMIT: i32 = 200;
/// Every attempt failed with a network pattern in its output.
pub const EXIT_NETWORK: i32 = 201;
