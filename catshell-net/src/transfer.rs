//! Copying files between this machine and a host.
//!
//! Transfers deliberately do *not* run on the connection the shells and the file
//! explorer share. SSH multiplexes channels over one TCP stream, so a large upload would
//! sit in front of every keystroke and every directory listing — the connection would
//! stay responsive on paper and feel broken in practice. Bulk work gets its own
//! connection, opened when the first transfer needs it.
//!
//! The queue is an actor: jobs go in, progress comes back, and the UI polls between
//! frames without ever blocking on the network.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Context as _;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::connection::HostConnection;
use crate::files::posix;

/// Copy buffer size. Large enough that the per-packet overhead of SFTP does not
/// dominate, small enough that cancelling is felt immediately.
const CHUNK: usize = 64 * 1024;

/// Report progress at most this often, so a fast transfer does not flood the UI with
/// more updates than it can draw.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Which way a file is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

impl Direction {
    pub fn verb(self) -> &'static str {
        match self {
            Direction::Upload => "Uploading",
            Direction::Download => "Downloading",
        }
    }
}

/// Identifies a queued transfer.
pub type JobId = u64;

/// One file to copy.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: JobId,
    pub direction: Direction,
    pub local: PathBuf,
    pub remote: String,
    /// Size of the source, known when the job is queued so progress has a denominator.
    pub size: u64,
    /// Continue a partial destination rather than starting again.
    ///
    /// Only ever set for a transfer this session already started and did not finish, so
    /// the partial file is known to be a prefix of the source. Resuming onto an
    /// unrelated file of the same name would silently corrupt it.
    pub resume: bool,
}

impl Job {
    /// The name to show in the queue.
    pub fn name(&self) -> String {
        match self.direction {
            Direction::Upload => self
                .local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.local.to_string_lossy().into_owned()),
            Direction::Download => posix::file_name(&self.remote).to_string(),
        }
    }
}

/// What the queue reports back.
#[derive(Debug, Clone)]
pub enum Event {
    Queued {
        job: Job,
    },
    Started {
        id: JobId,
    },
    /// Bytes copied so far, including anything skipped by resuming.
    Progress {
        id: JobId,
        transferred: u64,
    },
    Finished {
        id: JobId,
    },
    Failed {
        id: JobId,
        message: String,
    },
    Cancelled {
        id: JobId,
    },
}

/// Instructions to the queue.
enum Request {
    /// The cancellation flag travels with the job: the worker has to check the very
    /// same one the UI sets, not a copy made at either end.
    Enqueue(Job, Arc<Cancel>),
    Shutdown,
}

/// Shared cancellation state for one job.
#[derive(Debug, Default)]
struct Cancel {
    flag: AtomicBool,
}

impl Cancel {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// Called when the queue has events waiting, so the UI can repaint without polling.
pub type Wakeup = Arc<dyn Fn() + Send + Sync>;

/// Makes the connection transfers run on.
///
/// A factory rather than a connection, because the queue closes the connection when it
/// has been idle and builds a new one for the next transfer. Keeping an authenticated
/// connection open for a session's worth of not transferring anything is a socket, a
/// keepalive and an idle login on the server, all for nothing.
pub type Connector = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<Arc<HostConnection>>> + Send>,
        > + Send
        + Sync,
>;

/// How long the bulk connection stays up with nothing to do.
///
/// Long enough that a run of transfers reuses one connection — queueing files one at a
/// time from the explorer should not reconnect between each — and short enough that it
/// does not outstay the work.
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The UI's handle to a host's transfer queue.
pub struct TransferQueue {
    requests: UnboundedSender<Request>,
    events: std::sync::mpsc::Receiver<Event>,
    /// Cancellation flags, kept here so a job can be stopped while it is running.
    cancels: parking_lot::Mutex<std::collections::HashMap<JobId, Arc<Cancel>>>,
    next_id: AtomicU64,
}

impl TransferQueue {
    /// Start a queue that connects with `connect` whenever it has work.
    ///
    /// Nothing is connected until the first transfer, and the connection is dropped
    /// again once `idle_timeout` passes with nothing to do.
    pub fn new(
        connect: Connector,
        idle_timeout: std::time::Duration,
        wakeup: Option<Wakeup>,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();

        tokio::spawn(serve(connect, idle_timeout, request_rx, event_tx, wakeup));

        Self {
            requests: request_tx,
            events: event_rx,
            cancels: parking_lot::Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Queue a transfer, returning the id it was given.
    ///
    /// An existing destination is replaced.
    pub fn enqueue(
        &self,
        direction: Direction,
        local: PathBuf,
        remote: String,
        size: u64,
    ) -> JobId {
        self.enqueue_job(direction, local, remote, size, false)
    }

    /// Queue a transfer that continues a partial destination.
    ///
    /// Only for a transfer this session already started and stopped, where the partial
    /// file is known to be a prefix of the source. Resuming onto an unrelated file of
    /// the same name would splice two different files together.
    pub fn resume(&self, direction: Direction, local: PathBuf, remote: String, size: u64) -> JobId {
        self.enqueue_job(direction, local, remote, size, true)
    }

    fn enqueue_job(
        &self,
        direction: Direction,
        local: PathBuf,
        remote: String,
        size: u64,
        resume: bool,
    ) -> JobId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let job = Job {
            id,
            direction,
            local,
            remote,
            size,
            resume,
        };
        let cancel = Arc::new(Cancel::default());
        self.cancels.lock().insert(id, Arc::clone(&cancel));
        let _ = self.requests.send(Request::Enqueue(job, cancel));
        id
    }

    /// Ask for a job to stop.
    ///
    /// A job still queued never starts; a running one stops at its next chunk, leaving
    /// the partial destination in place so it can be resumed.
    pub fn cancel(&self, id: JobId) {
        if let Some(cancel) = self.cancels.lock().get(&id) {
            cancel.flag.store(true, Ordering::Relaxed);
        }
    }

    /// Take whatever has happened since the last call.
    pub fn drain(&self) -> Vec<Event> {
        let events: Vec<Event> = self.events.try_iter().collect();
        for event in &events {
            // A finished job can no longer be cancelled; drop its flag rather than
            // accumulate one per file for the life of the session.
            let done = match event {
                Event::Finished { id } | Event::Failed { id, .. } | Event::Cancelled { id } => {
                    Some(*id)
                }
                _ => None,
            };
            if let Some(id) = done {
                self.cancels.lock().remove(&id);
            }
        }
        events
    }
}

impl Drop for TransferQueue {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
    }
}

/// Run queued jobs one after another.
///
/// Sequential on purpose: several concurrent transfers over one connection finish no
/// sooner in total, and a single progress bar that advances steadily is easier to read
/// than five that crawl.
async fn serve(
    connect: Connector,
    idle_timeout: std::time::Duration,
    mut requests: UnboundedReceiver<Request>,
    events: std::sync::mpsc::Sender<Event>,
    wakeup: Option<Wakeup>,
) {
    let notify = |event: Event| -> bool {
        let sent = events.send(event).is_ok();
        if sent {
            if let Some(wakeup) = &wakeup {
                wakeup();
            }
        }
        sent
    };

    let mut pending: std::collections::VecDeque<(Job, Arc<Cancel>)> =
        std::collections::VecDeque::new();
    // The connection and its SFTP session, made on demand and dropped when idle.
    let mut live: Option<(Arc<HostConnection>, SftpSession)> = None;

    loop {
        // Take everything waiting before starting the next job, so a cancel issued while
        // a file is still queued is seen before it begins.
        while let Ok(request) = requests.try_recv() {
            match request {
                Request::Enqueue(job, cancel) => {
                    if !notify(Event::Queued { job: job.clone() }) {
                        return;
                    }
                    pending.push_back((job, cancel));
                }
                Request::Shutdown => return,
            }
        }

        let Some((job, cancel)) = pending.pop_front() else {
            // Nothing to do. Wait for the next request rather than spinning — but only
            // for as long as the connection is worth keeping. Timing out here rather
            // than tearing down from outside is what makes this race-free: the worker
            // alone decides, and a request that arrives first is simply taken instead.
            let next = if live.is_some() {
                match tokio::time::timeout(idle_timeout, requests.recv()).await {
                    Ok(next) => next,
                    Err(_) => {
                        close(live.take()).await;
                        // Now there is nothing to keep alive; wait indefinitely.
                        requests.recv().await
                    }
                }
            } else {
                requests.recv().await
            };

            match next {
                Some(Request::Enqueue(job, cancel)) => {
                    if !notify(Event::Queued { job: job.clone() }) {
                        return;
                    }
                    pending.push_back((job, cancel));
                    continue;
                }
                Some(Request::Shutdown) | None => {
                    close(live.take()).await;
                    return;
                }
            }
        };

        // Cancelled while it waited its turn: it never starts.
        if cancel.is_cancelled() {
            if !notify(Event::Cancelled { id: job.id }) {
                return;
            }
            continue;
        }

        if !notify(Event::Started { id: job.id }) {
            return;
        }

        let id = job.id;

        // Connect if this is the first transfer, or the first since going idle.
        if live.is_none() {
            match open_session(&connect).await {
                Ok(opened) => live = Some(opened),
                Err(err) => {
                    if !notify(Event::Failed {
                        id,
                        message: format!("{err:#}"),
                    }) {
                        return;
                    }
                    continue;
                }
            }
        }
        let Some((_, session)) = &live else { continue };

        let result = run(session, job, &cancel, &notify).await;
        let event = match result {
            Ok(Outcome::Finished) => Event::Finished { id },
            Ok(Outcome::Cancelled) => Event::Cancelled { id },
            Err(err) => Event::Failed {
                id,
                message: format!("{err:#}"),
            },
        };
        if !notify(event) {
            return;
        }
    }
}

enum Outcome {
    Finished,
    Cancelled,
}

/// Make a connection and start an SFTP session on it.
async fn open_session(connect: &Connector) -> anyhow::Result<(Arc<HostConnection>, SftpSession)> {
    let connection = connect()
        .await
        .context("opening a connection for transfers")?;
    let channel = connection.open_subsystem("sftp").await?;
    let session = SftpSession::new(channel.into_stream())
        .await
        .context("starting the SFTP session for transfers")?;
    Ok((connection, session))
}

/// Close a connection and its session, if there is one.
async fn close(live: Option<(Arc<HostConnection>, SftpSession)>) {
    let Some((connection, session)) = live else {
        return;
    };
    let _ = session.close().await;
    connection.disconnect().await;
}

async fn run(
    session: &SftpSession,
    job: Job,
    cancel: &Cancel,
    notify: &impl Fn(Event) -> bool,
) -> anyhow::Result<Outcome> {
    match job.direction {
        Direction::Upload => upload(session, &job, cancel, notify).await,
        Direction::Download => download(session, &job, cancel, notify).await,
    }
}

/// Copy a local file to the host.
async fn upload(
    session: &SftpSession,
    job: &Job,
    cancel: &Cancel,
    notify: &impl Fn(Event) -> bool,
) -> anyhow::Result<Outcome> {
    let mut source = tokio::fs::File::open(&job.local)
        .await
        .with_context(|| format!("opening {}", job.local.display()))?;

    // Make sure the directory exists before the file does, or the open fails for a
    // reason that reads like a permissions problem.
    if let Some(parent) = posix::parent(&job.remote) {
        let _ = session.create_dir(&parent).await;
    }

    let already = if job.resume {
        session
            .metadata(&job.remote)
            .await
            .ok()
            .and_then(|m| m.size)
            .unwrap_or(0)
    } else {
        0
    };
    let already = already.min(job.size);

    let flags = if already > 0 {
        OpenFlags::WRITE | OpenFlags::CREATE
    } else {
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
    };
    let mut destination = session
        .open_with_flags(&job.remote, flags)
        .await
        .with_context(|| format!("opening {} for writing", job.remote))?;

    if already > 0 {
        source.seek(std::io::SeekFrom::Start(already)).await?;
        destination.seek(std::io::SeekFrom::Start(already)).await?;
    }

    let outcome = copy(&mut source, &mut destination, job, already, cancel, notify).await?;
    destination
        .shutdown()
        .await
        .context("finishing the upload")?;
    Ok(outcome)
}

/// Copy a file from the host to this machine.
async fn download(
    session: &SftpSession,
    job: &Job,
    cancel: &Cancel,
    notify: &impl Fn(Event) -> bool,
) -> anyhow::Result<Outcome> {
    let mut source = session
        .open(&job.remote)
        .await
        .with_context(|| format!("opening {}", job.remote))?;

    if let Some(parent) = job.local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let already = if job.resume {
        tokio::fs::metadata(&job.local)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };
    let already = already.min(job.size);

    let mut destination = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(already == 0)
        .open(&job.local)
        .await
        .with_context(|| format!("opening {} for writing", job.local.display()))?;

    if already > 0 {
        source.seek(std::io::SeekFrom::Start(already)).await?;
        destination.seek(std::io::SeekFrom::Start(already)).await?;
    }

    let outcome = copy(&mut source, &mut destination, job, already, cancel, notify).await?;
    destination
        .flush()
        .await
        .context("finishing the download")?;
    Ok(outcome)
}

/// Move bytes until the source runs out or the job is cancelled.
async fn copy<R, W>(
    source: &mut R,
    destination: &mut W,
    job: &Job,
    already: u64,
    cancel: &Cancel,
    notify: &impl Fn(Event) -> bool,
) -> anyhow::Result<Outcome>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buffer = vec![0u8; CHUNK];
    let mut transferred = already;
    let mut last_report = std::time::Instant::now();

    // Report the starting point, so a resumed transfer does not appear to begin at zero.
    if already > 0 {
        notify(Event::Progress {
            id: job.id,
            transferred,
        });
    }

    loop {
        if cancel.is_cancelled() {
            // The partial destination is left in place deliberately: it is what makes
            // resuming possible.
            return Ok(Outcome::Cancelled);
        }

        let read = source.read(&mut buffer).await.context("reading")?;
        if read == 0 {
            break;
        }
        destination
            .write_all(&buffer[..read])
            .await
            .context("writing")?;
        transferred += read as u64;

        if last_report.elapsed() >= PROGRESS_INTERVAL {
            last_report = std::time::Instant::now();
            notify(Event::Progress {
                id: job.id,
                transferred,
            });
        }
    }

    notify(Event::Progress {
        id: job.id,
        transferred,
    });
    Ok(Outcome::Finished)
}

/// Every file under a local directory, paired with where it goes on the host.
///
/// Returned as a flat list of file jobs: a tree is just files, and a flat queue gives
/// honest total progress instead of a directory that sits at nothing until it finishes.
pub fn expand_local(
    root: &Path,
    remote_root: &str,
) -> std::io::Result<Vec<(PathBuf, String, u64)>> {
    let mut out = Vec::new();
    let metadata = root.symlink_metadata()?;

    if !metadata.is_dir() {
        out.push((root.to_path_buf(), remote_root.to_string(), metadata.len()));
        return Ok(out);
    }

    let mut stack = vec![(root.to_path_buf(), remote_root.to_string())];
    while let Some((directory, remote)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let target = posix::join(&remote, &name);

            if metadata.is_dir() {
                stack.push((entry.path(), target));
            } else if metadata.is_file() {
                // Symlinks are skipped rather than followed: following one can copy a
                // vast tree, or loop forever.
                out.push((entry.path(), target, metadata.len()));
            }
        }
    }

    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_is_named_after_the_file_being_moved() {
        let upload = Job {
            id: 1,
            direction: Direction::Upload,
            local: PathBuf::from("/home/me/report.pdf"),
            remote: "/srv/report.pdf".into(),
            size: 10,
            resume: false,
        };
        assert_eq!(upload.name(), "report.pdf");

        let download = Job {
            direction: Direction::Download,
            ..upload
        };
        assert_eq!(download.name(), "report.pdf");
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("catshell-transfer-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_single_file_expands_to_itself() {
        let dir = TempDir::new("single");
        let file = dir.0.join("one.txt");
        std::fs::write(&file, "12345").unwrap();

        let expanded = expand_local(&file, "/srv/one.txt").unwrap();
        assert_eq!(expanded, vec![(file, "/srv/one.txt".to_string(), 5)]);
    }

    #[test]
    fn a_directory_expands_to_the_files_under_it() {
        let dir = TempDir::new("tree");
        std::fs::create_dir_all(dir.0.join("sub/deeper")).unwrap();
        std::fs::write(dir.0.join("top.txt"), "a").unwrap();
        std::fs::write(dir.0.join("sub/middle.txt"), "bb").unwrap();
        std::fs::write(dir.0.join("sub/deeper/bottom.txt"), "ccc").unwrap();

        let expanded = expand_local(&dir.0, "/srv/app").unwrap();
        let remotes: Vec<&str> = expanded.iter().map(|(_, r, _)| r.as_str()).collect();
        assert_eq!(
            remotes,
            vec![
                "/srv/app/sub/deeper/bottom.txt",
                "/srv/app/sub/middle.txt",
                "/srv/app/top.txt",
            ]
        );
        // Sizes come along, so the queue knows its totals before it starts.
        assert_eq!(expanded.iter().map(|(_, _, size)| size).sum::<u64>(), 6);
    }

    #[test]
    fn an_empty_directory_expands_to_nothing() {
        let dir = TempDir::new("empty");
        assert!(expand_local(&dir.0, "/srv/empty").unwrap().is_empty());
    }

    #[test]
    fn symlinks_are_not_followed() {
        // Following one can copy a whole filesystem, or loop forever.
        let dir = TempDir::new("symlink");
        std::fs::create_dir(dir.0.join("real")).unwrap();
        std::fs::write(dir.0.join("real/file.txt"), "x").unwrap();
        std::os::unix::fs::symlink(dir.0.join("real"), dir.0.join("link")).unwrap();

        let expanded = expand_local(&dir.0, "/srv").unwrap();
        let remotes: Vec<&str> = expanded.iter().map(|(_, r, _)| r.as_str()).collect();
        assert_eq!(
            remotes,
            vec!["/srv/real/file.txt"],
            "a symlink was followed"
        );
    }

    #[test]
    fn a_missing_source_is_an_error_not_an_empty_list() {
        // Silently transferring nothing would look like success.
        assert!(expand_local(Path::new("/nonexistent/catshell"), "/srv").is_err());
    }

    #[test]
    fn directions_read_as_what_they_are_doing() {
        assert_eq!(Direction::Upload.verb(), "Uploading");
        assert_eq!(Direction::Download.verb(), "Downloading");
    }
}
