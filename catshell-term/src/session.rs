//! Pieces shared by every kind of terminal session, whatever the bytes arrive over.
//!
//! A session is three things: a [`Term`] holding the grid, a byte source, and a channel
//! of notifications for the UI. Only the byte source differs between a local PTY and an
//! SSH channel, so everything else lives here and both drivers reuse it.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi::{Processor, Rgb};
use parking_lot::Mutex;

use crate::osc::{OscSniffer, ShellEvent};
use crate::palette::Palette;

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// The child process or remote command exited with this status, when one was reported.
    Status(Option<i32>),
    /// The byte source failed.
    Error(String),
}

/// A notification from a session to the UI.
///
/// The UI polls these between frames; none of them require an immediate response.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// The grid changed and the view should be repainted.
    Redraw,
    /// The program set the window title, or reset it with `None`.
    Title(Option<String>),
    /// The program rang the bell.
    Bell,
    /// The program asked for text to be put on the system clipboard (OSC 52).
    ClipboardStore(String),
    /// Shell integration reported something (see [`ShellEvent`]).
    Shell(ShellEvent),
    /// The session ended; no further events will arrive.
    Exited(ExitReason),
}

/// Something the session owes the program, to be answered once the terminal lock is free.
///
/// Replies cannot be produced inside [`EventListener::send_event`]: that runs while the
/// parser holds the terminal lock, so reading terminal state there would deadlock.
enum Reply {
    /// Bytes to write to the byte source verbatim.
    Bytes(Vec<u8>),
    /// A colour query (OSC 4/10/11), answered once the colour table can be read.
    Color(usize, Arc<dyn Fn(Rgb) -> String + Sync + Send + 'static>),
}

/// Called when a session has events waiting.
///
/// Sessions run on their own threads, so without this the UI would have to poll — and a
/// terminal that polls is a terminal that burns CPU while sitting idle. The UI supplies a
/// hook that wakes it, and otherwise sleeps until something actually happens.
pub type Wakeup = Arc<dyn Fn() + Send + Sync>;

/// Receives [`Event`]s from the parser and turns them into [`SessionEvent`]s.
///
/// Cloneable and cheap; the terminal holds one.
#[derive(Clone)]
pub struct EventProxy {
    events: Sender<SessionEvent>,
    replies: Arc<Mutex<Vec<Reply>>>,
    window_size: Arc<Mutex<WindowSize>>,
    wakeup: Option<Wakeup>,
}

impl EventProxy {
    fn new(events: Sender<SessionEvent>, window_size: WindowSize, wakeup: Option<Wakeup>) -> Self {
        Self {
            events,
            replies: Arc::new(Mutex::new(Vec::new())),
            window_size: Arc::new(Mutex::new(window_size)),
            wakeup,
        }
    }

    fn send(&self, event: SessionEvent) {
        // A closed receiver means the UI dropped the session; the driver will notice
        // when it next tries to talk to us, so there is nothing useful to do here.
        if self.events.send(event).is_ok() {
            if let Some(wakeup) = &self.wakeup {
                wakeup();
            }
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.send(SessionEvent::Redraw)
            }
            Event::Title(title) => self.send(SessionEvent::Title(Some(title))),
            Event::ResetTitle => self.send(SessionEvent::Title(None)),
            Event::Bell => self.send(SessionEvent::Bell),
            Event::ClipboardStore(_, text) => self.send(SessionEvent::ClipboardStore(text)),
            Event::PtyWrite(text) => self.replies.lock().push(Reply::Bytes(text.into_bytes())),
            Event::TextAreaSizeRequest(format) => {
                // Answerable straight away: the size is ours, not the terminal's.
                let size = *self.window_size.lock();
                self.replies
                    .lock()
                    .push(Reply::Bytes(format(size).into_bytes()));
            }
            Event::ColorRequest(index, format) => {
                self.replies.lock().push(Reply::Color(index, format))
            }
            Event::ClipboardLoad(_, format) => {
                // Paste-out of OSC 52 is disabled by default (`Osc52::OnlyCopy`), so this
                // normally never fires. Answer with nothing rather than leave a program
                // that asked anyway waiting forever.
                self.replies
                    .lock()
                    .push(Reply::Bytes(format("").into_bytes()));
            }
            // Exit is not reported from here. `Term::exit` fires it in response to the
            // driver shutting the session down, so translating it would emit a reasonless
            // `Exited` *before* the driver sends the real one, and the UI would believe
            // the first. The driver is the only thing that knows why a session ended.
            Event::Exit | Event::ChildExit(_) => {}
        }
    }
}

/// Terminal dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl GridSize {
    /// Clamps to at least 1×1: a zero-sized grid would divide by zero deep inside the
    /// grid code, and transient zero sizes do happen while a window is being laid out.
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns: columns.max(1),
            screen_lines: screen_lines.max(1),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Drives bytes from a source into the terminal.
///
/// This is the shared half of every session driver: it owns the parser and the OSC
/// sniffer, and it is what makes shell integration work — the sniffer sees each slice
/// before the parser does, recovering the OSC 7 and OSC 133 sequences that `vte` drops.
pub struct TermFeed {
    term: Arc<FairMutex<Term<EventProxy>>>,
    parser: Processor,
    sniffer: OscSniffer,
    proxy: EventProxy,
    palette: Palette,
}

impl TermFeed {
    /// Build a terminal of `size` and the feed that drives it.
    pub fn new(
        config: alacritty_terminal::term::Config,
        size: GridSize,
        window_size: WindowSize,
        palette: Palette,
        events: Sender<SessionEvent>,
        wakeup: Option<Wakeup>,
    ) -> Self {
        let proxy = EventProxy::new(events, window_size, wakeup);
        let term = Term::new(config, &size, proxy.clone());
        Self {
            term: Arc::new(FairMutex::new(term)),
            parser: Processor::new(),
            sniffer: OscSniffer::new(),
            proxy,
            palette,
        }
    }

    /// The terminal, shared with the UI thread for rendering.
    pub fn term(&self) -> Arc<FairMutex<Term<EventProxy>>> {
        Arc::clone(&self.term)
    }

    /// Feed output from the byte source into the terminal.
    ///
    /// Returns any bytes the program is owed in response (colour and size queries, and
    /// replies the parser generated), which the caller must write back to the source.
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        // The sniffer only observes; the parser still sees the slice in full.
        let proxy = &self.proxy;
        self.sniffer
            .feed(bytes, |event| proxy.send(SessionEvent::Shell(event)));

        {
            let mut term = self.term.lock();
            self.parser.advance(&mut *term, bytes);
        }

        // Only worth a repaint if the bytes were not swallowed by a synchronized update.
        if self.parser.sync_bytes_count() < bytes.len() {
            self.proxy.send(SessionEvent::Redraw);
        }

        self.take_replies()
    }

    /// Deadline for an in-progress synchronized update (DEC 2026), if any.
    ///
    /// While a program holds a synchronized update open the terminal must not repaint,
    /// but it must not stay frozen forever either, so the driver waits at most this long.
    pub fn sync_timeout(&mut self) -> Option<std::time::Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Abandon a synchronized update whose deadline passed and repaint.
    pub fn stop_sync(&mut self) {
        self.parser.stop_sync(&mut *self.term.lock());
        self.proxy.send(SessionEvent::Redraw);
    }

    /// Resize the grid. The caller is responsible for resizing the byte source too.
    pub fn resize(&mut self, size: GridSize, window_size: WindowSize) {
        *self.proxy.window_size.lock() = window_size;
        self.term.lock().resize(size);
    }

    /// Mark the terminal as finished, so the UI stops treating it as live.
    pub fn shutdown(&mut self, reason: ExitReason) {
        self.term.lock().exit();
        self.proxy.send(SessionEvent::Exited(reason));
    }

    /// Discard any half-parsed escape sequence, for when the byte stream is replaced.
    pub fn reset_parser(&mut self) {
        self.sniffer.reset();
    }

    /// Drain pending replies, resolving colour queries now that the lock is free.
    fn take_replies(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut *self.proxy.replies.lock());
        if pending.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        for reply in pending {
            match reply {
                Reply::Bytes(bytes) => out.extend_from_slice(&bytes),
                Reply::Color(index, format) => {
                    let rgb = {
                        let term = self.term.lock();
                        self.palette.resolve(term.colors(), index)
                    };
                    out.extend_from_slice(format(rgb).as_bytes());
                }
            }
        }
        out
    }
}

/// Instructions from the UI to a session driver.
#[derive(Debug)]
pub enum Msg {
    /// Bytes to write to the byte source (keyboard input, pastes).
    Input(Vec<u8>),
    /// The view was resized.
    Resize {
        size: GridSize,
        window_size: WindowSize,
    },
    /// Shut the session down.
    Shutdown,
}

/// The UI's end of a running session.
pub struct Session {
    term: Arc<FairMutex<Term<EventProxy>>>,
    notifier: Box<dyn Notifier>,
    events: Receiver<SessionEvent>,
}

/// Wakes a session driver with a new [`Msg`].
///
/// Implemented per driver because how you interrupt a blocked driver differs: a PTY loop
/// sits in `Poller::wait`, while an SSH session will be parked on its runtime.
pub trait Notifier: Send {
    fn notify(&self, msg: Msg);
}

impl Session {
    pub fn new(
        term: Arc<FairMutex<Term<EventProxy>>>,
        notifier: Box<dyn Notifier>,
        events: Receiver<SessionEvent>,
    ) -> Self {
        Self {
            term,
            notifier,
            events,
        }
    }

    /// The terminal, for rendering. Hold the lock only as long as a frame needs it.
    pub fn term(&self) -> &Arc<FairMutex<Term<EventProxy>>> {
        &self.term
    }

    /// Send input to the program.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if !bytes.is_empty() {
            self.notifier.notify(Msg::Input(bytes));
        }
    }

    /// Tell the program the view changed size.
    pub fn resize(&self, size: GridSize, window_size: WindowSize) {
        self.notifier.notify(Msg::Resize { size, window_size });
    }

    /// Ask the driver to stop. The session ends asynchronously.
    pub fn shutdown(&self) {
        self.notifier.notify(Msg::Shutdown);
    }

    /// Take whatever notifications have arrived since the last call.
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        self.events.try_iter().collect()
    }
}
