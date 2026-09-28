//! SSH transport for catshell.
//!
//! Empty until milestone 2. This is where the multiplexed connection will live: one
//! authenticated `russh` session carrying a channel per shell pane plus an SFTP
//! subsystem channel, so the file explorer and the shells share one connection and stay
//! in sync, with a second connection opened lazily for bulk transfers.

pub mod connection;
pub mod files;
pub mod host;
pub mod known_hosts;
pub mod secrets;
pub mod sftp;
pub mod shell;

pub use connection::{AuthMethod, ConnectError, ConnectOptions, HostConnection};
pub use files::{EntryKind, FileEntry};
pub use host::HostConfig;
