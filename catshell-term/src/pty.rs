//! Driver for a local shell running on a pseudoterminal.
//!
//! One thread per session owns the PTY and pumps bytes both ways. It is built on
//! `polling` rather than blocking reads because a blocking read cannot be interrupted to
//! deliver keyboard input or a resize, and because `alacritty_terminal`'s `Pty` exposes
//! its handles only through `EventedReadWrite` — the one interface implemented on both
//! Unix and Windows (ConPTY), which is what keeps this file free of platform `cfg`s.
//!
//! `alacritty_terminal` ships its own `EventLoop` that does much of this, but it parses
//! the bytes internally, leaving no way to see the OSC 7 and OSC 133 sequences shell
//! integration depends on — and it requires an `EventedPty`, which an SSH channel will
//! never be. Driving the pump ourselves serves both.

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use crate::palette::Palette;
use crate::session::{ExitReason, GridSize, Msg, Notifier, Session, TermFeed, Wakeup};

/// Read buffer size, matching alacritty's: large enough that a flood of output is
/// consumed in few syscalls, small enough not to hold the terminal lock too long.
const READ_BUFFER_SIZE: usize = 0x10_0000;

/// How much output to process before releasing the terminal lock, so that a program
/// producing output without pause cannot starve the renderer.
const MAX_LOCKED_READ: usize = READ_BUFFER_SIZE;

/// What shell to run and how.
#[derive(Debug, Clone, Default)]
pub struct LocalShellOptions {
    /// Program and arguments. `None` runs the user's login shell.
    pub shell: Option<(String, Vec<String>)>,
    pub working_directory: Option<PathBuf>,
    /// Extra environment for the child, applied over the defaults below.
    pub env: HashMap<String, String>,
}

impl LocalShellOptions {
    fn into_tty_options(self) -> tty::Options {
        let mut env = self.env;
        // Set the terminal type per-child rather than calling `tty::setup_env`, which
        // mutates the whole process's environment. We describe ourselves as
        // `xterm-256color` because no `catshell` terminfo entry exists on any host.
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into())
            .or_insert_with(|| "truecolor".into());

        tty::Options {
            shell: self
                .shell
                .map(|(program, args)| tty::Shell::new(program, args)),
            working_directory: self.working_directory,
            drain_on_exit: true,
            env,
            #[cfg(target_os = "windows")]
            escape_args: true,
        }
    }
}

/// Wakes the PTY thread out of `Poller::wait`.
struct PtyNotifier {
    tx: Sender<Msg>,
    poller: Arc<Poller>,
}

impl Notifier for PtyNotifier {
    fn notify(&self, msg: Msg) {
        if self.tx.send(msg).is_ok() {
            // Without this the thread stays blocked until the child happens to write.
            let _ = self.poller.notify();
        }
    }
}

/// Start a local shell and return the UI's handle to it.
pub fn spawn(
    options: LocalShellOptions,
    config: TermConfig,
    size: GridSize,
    window_size: WindowSize,
    palette: Palette,
    wakeup: Option<Wakeup>,
) -> anyhow::Result<Session> {
    let pty = tty::new(&options.into_tty_options(), window_size, 0)?;

    let (event_tx, event_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let poller = Arc::new(Poller::new()?);

    let feed = TermFeed::new(config, size, window_size, palette, event_tx, wakeup);
    let term = feed.term();

    let notifier = PtyNotifier {
        tx: msg_tx,
        poller: Arc::clone(&poller),
    };

    std::thread::Builder::new()
        .name("catshell-pty".into())
        .spawn({
            let poller = Arc::clone(&poller);
            move || {
                let mut pump = Pump {
                    pty,
                    feed,
                    poller,
                    msgs: msg_rx,
                    pending: VecDeque::new(),
                };
                let reason = pump.run();
                pump.feed.shutdown(reason);
            }
        })?;

    Ok(Session::new(term, Box::new(notifier), event_rx))
}

/// Bytes waiting to go to the PTY, tracking how much of the front item has gone.
#[derive(Default)]
struct PendingWrite {
    bytes: Vec<u8>,
    written: usize,
}

struct Pump {
    pty: tty::Pty,
    feed: TermFeed,
    poller: Arc<Poller>,
    msgs: Receiver<Msg>,
    pending: VecDeque<PendingWrite>,
}

impl Pump {
    fn run(&mut self) -> ExitReason {
        let poll_mode = PollMode::Level;
        let mut interest = PollEvent::readable(0);

        // SAFETY: the `Pty` is owned by this function and deregistered before it returns,
        // so the registered sources outlive their registration.
        if let Err(err) = unsafe { self.pty.register(&self.poller, interest, poll_mode) } {
            return ExitReason::Error(format!("registering PTY: {err}"));
        }

        let mut events = Events::with_capacity(NonZeroUsize::new(64).unwrap());
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        let reason = loop {
            // A program in a synchronized update must not be repainted, but must not
            // freeze the view either; wake up when its deadline passes.
            let timeout = self
                .feed
                .sync_timeout()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));

            events.clear();
            if let Err(err) = self.poller.wait(&mut events, timeout) {
                match err.kind() {
                    ErrorKind::Interrupted => continue,
                    _ => break ExitReason::Error(format!("polling PTY: {err}")),
                }
            }

            if events.is_empty() {
                self.feed.stop_sync();
            }

            match self.drain_messages() {
                Ok(()) => {}
                Err(reason) => break reason,
            }

            // The event keys distinguishing the PTY from the child-exit notifier are
            // private to `alacritty_terminal`, so rather than depend on their values we
            // take the union and let each operation decide it has nothing to do. Reads
            // and writes are non-blocking, so a spurious attempt just returns `WouldBlock`.
            let readable = events.iter().any(|event| event.readable);
            let writable = events.iter().any(|event| event.writable);

            if let Some(ChildEvent::Exited(status)) = self.pty.next_child_event() {
                // Drain whatever the child wrote before exiting, or its final output is
                // lost — this is what makes `ls && exit` show its listing.
                let _ = self.read(&mut buf);
                break ExitReason::Status(status.and_then(|status| status.code()));
            }

            if readable {
                if let Err(err) = self.read(&mut buf) {
                    // On Linux a read can fail with EIO once the child hangs up; that is
                    // not an error, just the exit event arriving on the next iteration.
                    #[cfg(target_os = "linux")]
                    if err.raw_os_error() == Some(libc::EIO) {
                        continue;
                    }
                    break ExitReason::Error(format!("reading from PTY: {err}"));
                }
            }

            if writable {
                if let Err(err) = self.write() {
                    break ExitReason::Error(format!("writing to PTY: {err}"));
                }
            }

            // Ask to be told about writability only while there is something to write,
            // otherwise a level-triggered poller spins on an always-writable PTY.
            let wants_write = !self.pending.is_empty();
            if wants_write != interest.writable {
                interest.writable = wants_write;
                if let Err(err) = self.pty.reregister(&self.poller, interest, poll_mode) {
                    break ExitReason::Error(format!("re-registering PTY: {err}"));
                }
            }
        };

        let _ = self.pty.deregister(&self.poller);
        reason
    }

    /// Apply everything the UI has asked for. `Err` means the session should end.
    fn drain_messages(&mut self) -> Result<(), ExitReason> {
        loop {
            match self.msgs.try_recv() {
                Ok(Msg::Input(bytes)) => self.enqueue(bytes),
                Ok(Msg::InputHidden(bytes)) => {
                    // Armed before the bytes are queued, so the echo cannot beat it back.
                    self.feed.suppress_echo(&bytes);
                    self.enqueue(bytes);
                }
                Ok(Msg::Resize { size, window_size }) => {
                    self.feed.resize(size, window_size);
                    self.pty.on_resize(window_size);
                }
                Ok(Msg::Shutdown) => return Err(ExitReason::Status(None)),
                Err(TryRecvError::Empty) => return Ok(()),
                // The UI dropped the session.
                Err(TryRecvError::Disconnected) => return Err(ExitReason::Status(None)),
            }
        }
    }

    fn enqueue(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.pending.push_back(PendingWrite { bytes, written: 0 });
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<()> {
        let mut processed = 0;
        loop {
            let got = match self.pty.reader().read(buf) {
                Ok(0) => break,
                Ok(got) => got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => break,
                    _ => return Err(err),
                },
            };

            // The feed hands back anything the program is owed (colour and size queries),
            // which goes into the write queue like any other output.
            let replies = self.feed.advance(&buf[..got]);
            self.enqueue(replies);

            processed += got;
            if processed >= MAX_LOCKED_READ {
                break;
            }
        }
        Ok(())
    }

    fn write(&mut self) -> std::io::Result<()> {
        while let Some(front) = self.pending.front_mut() {
            match self.pty.writer().write(&front.bytes[front.written..]) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    front.written += n;
                    if front.written >= front.bytes.len() {
                        self.pending.pop_front();
                    }
                }
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => return Ok(()),
                    _ => return Err(err),
                },
            }
        }
        Ok(())
    }
}
