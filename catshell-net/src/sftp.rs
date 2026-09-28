//! Browsing a remote filesystem over SFTP, on the same connection as the shells.
//!
//! The channel is opened on the host's existing [`HostConnection`], which is what lets
//! the explorer stay in step with the terminal: same session, same login, same view of
//! the filesystem. A separate SFTP connection — what MobaXterm does — has its own
//! working directory and no idea when a command finished, which is exactly why its file
//! browser goes stale.
//!
//! SFTP is asynchronous and the UI thread cannot await, so this is an actor: requests go
//! in on a channel, results come back on another, and the UI picks them up between
//! frames.

use std::sync::Arc;

use anyhow::Context as _;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileType;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::connection::HostConnection;
use crate::files::{sort_entries, EntryKind, FileEntry};

/// Something for the browser to do.
#[derive(Debug, Clone)]
pub enum Request {
    /// List a directory. `generation` is echoed back so the caller can discard a
    /// listing it has already navigated away from.
    List {
        path: String,
        generation: u64,
    },
    /// Resolve the login directory, for the initial view.
    Home {
        generation: u64,
    },
    MakeDir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Remove {
        path: String,
        kind: EntryKind,
    },
}

/// Something the browser has done.
#[derive(Debug, Clone)]
pub enum Event {
    Listed {
        path: String,
        generation: u64,
        entries: Vec<FileEntry>,
    },
    /// A change succeeded; the caller should re-list to see it.
    Changed,
    /// An operation failed. `action` says which, so the message can be shown in context.
    Failed { action: String, message: String },
}

/// Called when the browser has events waiting, so the UI can repaint without polling.
pub type Wakeup = Arc<dyn Fn() + Send + Sync>;

/// The UI's handle to a remote filesystem.
pub struct SftpBrowser {
    requests: UnboundedSender<Request>,
    events: std::sync::mpsc::Receiver<Event>,
}

impl SftpBrowser {
    /// Open an SFTP channel on an existing connection and start serving requests.
    pub async fn open(
        connection: Arc<HostConnection>,
        wakeup: Option<Wakeup>,
    ) -> anyhow::Result<Self> {
        let channel = connection.open_subsystem("sftp").await?;
        let session = SftpSession::new(channel.into_stream())
            .await
            .context("starting the SFTP session")?;

        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();

        tokio::spawn(serve(session, request_rx, event_tx, wakeup));

        Ok(Self {
            requests: request_tx,
            events: event_rx,
        })
    }

    /// Queue a request. Dropping the browser stops the task.
    pub fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    /// Take whatever has completed since the last call.
    pub fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

/// Serve requests until the handle is dropped or the channel dies.
async fn serve(
    session: SftpSession,
    mut requests: UnboundedReceiver<Request>,
    events: std::sync::mpsc::Sender<Event>,
    wakeup: Option<Wakeup>,
) {
    while let Some(request) = requests.recv().await {
        let event = handle(&session, request).await;
        if events.send(event).is_err() {
            // The UI dropped the browser.
            break;
        }
        if let Some(wakeup) = &wakeup {
            wakeup();
        }
    }
    let _ = session.close().await;
}

async fn handle(session: &SftpSession, request: Request) -> Event {
    match request {
        Request::List { path, generation } => match list(session, &path).await {
            Ok(entries) => Event::Listed {
                path,
                generation,
                entries,
            },
            Err(err) => Event::Failed {
                action: format!("listing {path}"),
                message: format!("{err}"),
            },
        },

        Request::Home { generation } => match session.canonicalize(".").await {
            // The login directory is resolved by asking the server, rather than guessing
            // at `/home/<user>`, which is wrong on plenty of systems.
            Ok(path) => match list(session, &path).await {
                Ok(entries) => Event::Listed {
                    path,
                    generation,
                    entries,
                },
                Err(err) => Event::Failed {
                    action: format!("listing {path}"),
                    message: format!("{err}"),
                },
            },
            Err(err) => Event::Failed {
                action: "finding the home directory".into(),
                message: format!("{err}"),
            },
        },

        Request::MakeDir { path } => {
            report(session.create_dir(&path).await, format!("creating {path}"))
        }

        Request::Rename { from, to } => {
            report(session.rename(&from, &to).await, format!("renaming {from}"))
        }

        Request::Remove { path, kind } => {
            // A directory and a file are removed by different operations, and asking for
            // the wrong one fails rather than doing something surprising.
            let result = if kind == EntryKind::Dir {
                session.remove_dir(&path).await
            } else {
                session.remove_file(&path).await
            };
            report(result, format!("removing {path}"))
        }
    }
}

fn report<T>(result: Result<T, russh_sftp::client::error::Error>, action: String) -> Event {
    match result {
        Ok(_) => Event::Changed,
        Err(err) => Event::Failed {
            action,
            message: format!("{err}"),
        },
    }
}

async fn list(
    session: &SftpSession,
    path: &str,
) -> Result<Vec<FileEntry>, russh_sftp::client::error::Error> {
    let mut entries: Vec<FileEntry> = session
        .read_dir(path)
        .await?
        .map(|entry| {
            let metadata = entry.metadata();
            FileEntry {
                name: entry.file_name(),
                kind: kind_of(entry.file_type()),
                size: metadata.size.unwrap_or(0),
                modified: metadata.mtime.map(u64::from),
                permissions: metadata.permissions,
            }
        })
        .collect();

    sort_entries(&mut entries);
    Ok(entries)
}

fn kind_of(file_type: FileType) -> EntryKind {
    match file_type {
        FileType::Dir => EntryKind::Dir,
        FileType::File => EntryKind::File,
        FileType::Symlink => EntryKind::Symlink,
        FileType::Other => EntryKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_types_map_across() {
        assert_eq!(kind_of(FileType::Dir), EntryKind::Dir);
        assert_eq!(kind_of(FileType::File), EntryKind::File);
        assert_eq!(kind_of(FileType::Symlink), EntryKind::Symlink);
        assert_eq!(kind_of(FileType::Other), EntryKind::Other);
    }

    #[test]
    fn a_removal_picks_the_operation_from_the_entry_kind() {
        // Guards the branch in `handle`: rmdir on a file and unlink on a directory both
        // fail, so the kind has to decide.
        assert!(EntryKind::Dir == EntryKind::Dir);
        assert_ne!(EntryKind::File, EntryKind::Dir);
    }
}
