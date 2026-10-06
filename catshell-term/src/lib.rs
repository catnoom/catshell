pub mod echo;
pub mod integration;
pub mod keys;
pub mod osc;
pub mod palette;
pub mod pty;
pub mod session;

pub use integration::Shell;
pub use keys::{Key, Modifiers};
pub use osc::{OscSniffer, ShellEvent};
pub use palette::Palette;
pub use session::{ExitReason, GridSize, Msg, Session, SessionEvent, Wakeup};

/// Re-exported so dependents can name terminal types without also depending on
/// `alacritty_terminal` directly.
pub use alacritty_terminal::term;
