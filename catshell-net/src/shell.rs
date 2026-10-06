//! Driving a terminal from an SSH channel.
//!
//! The counterpart to `catshell_term::pty` for remote shells. Everything above the byte
//! source is shared: the same [`TermFeed`] parses the output, the same OSC sniffer
//! recovers shell-integration events, the same [`Session`] handle serves the UI. Only
//! the transport differs — which is exactly why the PTY driver was written around
//! `TermFeed` rather than using `alacritty_terminal`'s own event loop.

use std::sync::Arc;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::term::Config as TermConfig;
use catshell_term::palette::Palette;
use catshell_term::session::{ExitReason, GridSize, Msg, Notifier, Session, TermFeed, Wakeup};
use russh::client;
use russh::{Channel, ChannelMsg};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::connection::HostConnection;

/// Terminal type announced to the remote host.
///
/// `xterm-256color` because no `catshell` terminfo entry exists on any server, and
/// claiming one that is not installed leaves programs unable to draw at all.
pub const TERM: &str = "xterm-256color";

/// Sends instructions to a shell task.
struct ChannelNotifier {
    sender: UnboundedSender<Msg>,
}

impl Notifier for ChannelNotifier {
    fn notify(&self, msg: Msg) {
        // A closed receiver means the shell already ended; the UI finds out from the
        // `Exited` event it was already sent.
        let _ = self.sender.send(msg);
    }
}

/// Open a shell on `connection` and return the UI's handle to it.
///
/// The channel joins whatever else is already running on that transport, so several
/// panes on one host cost one connection and one authentication between them.
pub async fn open(
    connection: Arc<HostConnection>,
    config: TermConfig,
    size: GridSize,
    window_size: WindowSize,
    palette: Palette,
    wakeup: Option<Wakeup>,
) -> anyhow::Result<Session> {
    let channel = connection
        .open_shell(TERM, size.columns as u16, size.screen_lines as u16)
        .await?;

    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();

    let feed = TermFeed::new(config, size, window_size, palette, event_tx, wakeup);
    let term = feed.term();

    tokio::spawn(pump(channel, feed, msg_rx));

    Ok(Session::new(
        term,
        Box::new(ChannelNotifier { sender: msg_tx }),
        event_rx,
    ))
}

/// Move bytes between the channel and the terminal until one end stops.
async fn pump(
    mut channel: Channel<client::Msg>,
    mut feed: TermFeed,
    mut msgs: UnboundedReceiver<Msg>,
) -> ExitReason {
    let mut exit_status = None;

    let reason = loop {
        tokio::select! {
            // Biased so pending input is delivered before more output is processed;
            // otherwise a program producing output without pause could starve typing.
            biased;

            msg = msgs.recv() => match msg {
                Some(Msg::Input(bytes)) => {
                    if channel.data(&bytes[..]).await.is_err() {
                        break ExitReason::Error("the connection dropped while sending".into());
                    }
                }
                Some(Msg::InputHidden(bytes)) => {
                    // Armed before the bytes go out, so the echo cannot beat it back.
                    feed.suppress_echo(&bytes);
                    if channel.data(&bytes[..]).await.is_err() {
                        break ExitReason::Error("the connection dropped while sending".into());
                    }
                }
                Some(Msg::Resize { size, window_size }) => {
                    feed.resize(size, window_size);
                    let _ = channel
                        .window_change(
                            u32::from(window_size.num_cols),
                            u32::from(window_size.num_lines),
                            0,
                            0,
                        )
                        .await;
                }
                Some(Msg::Shutdown) | None => {
                    let _ = channel.eof().await;
                    let _ = channel.close().await;
                    break ExitReason::Status(exit_status);
                }
            },

            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => {
                    let replies = feed.advance(&data);
                    if !replies.is_empty() && channel.data(&replies[..]).await.is_err() {
                        break ExitReason::Error("the connection dropped while replying".into());
                    }
                }
                // The remote's stderr. A shell on a pty merges it into the output
                // stream, but a program can still write here, and dropping it would
                // lose error messages.
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let replies = feed.advance(&data);
                    if !replies.is_empty() && channel.data(&replies[..]).await.is_err() {
                        break ExitReason::Error("the connection dropped while replying".into());
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                    // Recorded, not acted on: output can still follow, and the channel
                    // closing is what actually ends the session.
                    exit_status = Some(status as i32);
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    break ExitReason::Error(format!("the remote command was killed by {signal_name:?}"));
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    break ExitReason::Status(exit_status);
                }
                Some(_) => {}
            },
        }
    };

    feed.shutdown(reason.clone());
    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertised_terminal_is_one_servers_actually_have() {
        // Announcing a terminfo name the server does not have leaves full-screen
        // programs unable to draw.
        assert_eq!(TERM, "xterm-256color");
    }
}
