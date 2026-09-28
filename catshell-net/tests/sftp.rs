//! End-to-end tests for the remote file explorer.
//!
//! The SFTP server here speaks the real protocol (via `russh-sftp`'s server side) over a
//! real SSH channel, and is backed by a real temporary directory — so a test can create
//! a file on disk and assert the browser sees it, rather than asserting against a mock
//! that agrees with itself.
//!
//! The point being tested is the one the whole design turns on: the file explorer rides
//! the *same* connection as the shells.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use catshell_net::connection::{AuthMethod, ConnectOptions, HostConnection};
use catshell_net::files::EntryKind;
use catshell_net::host::HostConfig;
use catshell_net::known_hosts::UnknownHostPolicy;
use catshell_net::sftp::{Event, Request, SftpBrowser};
use russh::server::{Auth, Handler as SshHandler, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{File, FileAttributes, Handle, Name, Status, StatusCode, Version};
use tokio::net::TcpListener;

const PASSWORD: &str = "sftp-test";
const TIMEOUT: Duration = Duration::from_secs(20);

// --- A temporary directory that cleans up after itself -----------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("catshell-sftp-{name}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // Resolve symlinks now (on macOS /tmp is one), so paths the server reports match
        // what the test expects.
        Self(path.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn as_str(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- An SFTP server backed by the local filesystem ---------------------------------

struct SftpHandler {
    /// Open directory handles and the entries still to be delivered for each.
    dirs: HashMap<String, Vec<File>>,
    next_handle: usize,
    root: PathBuf,
}

#[derive(Debug)]
struct SftpError(StatusCode, String);

impl From<SftpError> for russh_sftp::server::StatusReply {
    fn from(error: SftpError) -> Self {
        russh_sftp::server::StatusReply {
            status_code: error.0,
            error_message: Some(error.1),
            language_tag: Some("en-US".into()),
        }
    }
}

fn io_error(err: std::io::Error) -> SftpError {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    };
    SftpError(code, err.to_string())
}

impl russh_sftp::server::Handler for SftpHandler {
    type Error = SftpError;

    fn unimplemented(&self) -> Self::Error {
        SftpError(StatusCode::OpUnsupported, "unimplemented".into())
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        // "." means the login directory, which is what the browser asks for first.
        let resolved = if path == "." || path.is_empty() {
            self.root.clone()
        } else {
            PathBuf::from(&path)
        };
        Ok(Name {
            id,
            files: vec![File::dummy(resolved.to_string_lossy().into_owned())],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            // `symlink_metadata` so a link is reported as a link rather than its target.
            let metadata = entry.path().symlink_metadata().map_err(io_error)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            files.push(File {
                filename: name.clone(),
                longname: name,
                attrs: FileAttributes::from(&metadata),
            });
        }

        let handle = format!("dir-{}", self.next_handle);
        self.next_handle += 1;
        self.dirs.insert(handle.clone(), files);
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        // The protocol expects the listing in batches, ending with EOF; the client loops
        // until it sees that.
        let files = self
            .dirs
            .get_mut(&handle)
            .map(std::mem::take)
            .unwrap_or_default();
        if files.is_empty() {
            return Err(SftpError(StatusCode::Eof, "end of directory".into()));
        }
        Ok(Name { id, files })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.dirs.remove(&handle);
        Ok(ok_status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(&path).map_err(io_error)?;
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(&path).map_err(io_error)?;
        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(&filename).map_err(io_error)?;
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        std::fs::rename(&oldpath, &newpath).map_err(io_error)?;
        Ok(ok_status(id))
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "ok".into(),
        language_tag: "en-US".into(),
    }
}

// --- The SSH server that carries it ------------------------------------------------

#[derive(Clone)]
struct TestServer {
    root: PathBuf,
    connections: Arc<AtomicUsize>,
    channels: Arc<AtomicUsize>,
}

impl russh::server::Server for TestServer {
    type Handler = SshHandlerImpl;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        self.connections.fetch_add(1, Ordering::SeqCst);
        SshHandlerImpl {
            root: self.root.clone(),
            channels: Arc::clone(&self.channels),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct SshHandlerImpl {
    root: PathBuf,
    channels: Arc<AtomicUsize>,
    /// Channels awaiting a subsystem request. `subsystem_request` is handed only a
    /// channel *id*, so the channel itself has to be kept from when it was opened.
    pending: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl SshHandler for SshHandlerImpl {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        let expected =
            std::env::var("CATSHELL_TEST_PASSWORD").unwrap_or_else(|_| PASSWORD.to_string());
        if password == expected {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.channels.fetch_add(1, Ordering::SeqCst);
        self.pending.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, "shell ready\r\n".to_string().into_bytes())?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel)?;
            return Ok(());
        }

        let Some(channel) = self.pending.lock().unwrap().remove(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };

        session.channel_success(channel.id())?;
        russh_sftp::server::run(
            channel.into_stream(),
            SftpHandler {
                dirs: HashMap::new(),
                next_handle: 0,
                root: self.root.clone(),
            },
        )
        .await;
        Ok(())
    }
}

// --- Fixture ------------------------------------------------------------------------

struct Fixture {
    host: HostConfig,
    connections: Arc<AtomicUsize>,
    channels: Arc<AtomicUsize>,
    dir: TempDir,
    known_hosts: PathBuf,
    _known_hosts_dir: TempDir,
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let dir = TempDir::new(name);
        let known_hosts_dir = TempDir::new(&format!("{name}-kh"));
        let connections = Arc::new(AtomicUsize::new(0));
        let channels = Arc::new(AtomicUsize::new(0));

        use rand::SeedableRng as _;
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(name.len() as u64);
        let key =
            russh::keys::PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519).unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let root = dir.path().to_path_buf();
        let (server_connections, server_channels) =
            (Arc::clone(&connections), Arc::clone(&channels));
        tokio::spawn(async move {
            let mut server = TestServer {
                root,
                connections: server_connections,
                channels: server_channels,
            };
            let _ = server.run_on_socket(config, &listener).await;
        });

        Self {
            host: HostConfig {
                name: "sftp-test".into(),
                hostname: "127.0.0.1".into(),
                port,
                user: Some("tester".into()),
                identity_files: vec![],
            },
            connections,
            channels,
            known_hosts: known_hosts_dir.path().join("known_hosts"),
            _known_hosts_dir: known_hosts_dir,
            dir,
        }
    }

    async fn connect(&self) -> Arc<HostConnection> {
        HostConnection::connect(ConnectOptions {
            host: self.host.clone(),
            methods: vec![AuthMethod::Password(PASSWORD.into())],
            unknown_host_policy: UnknownHostPolicy::Accept,
            known_hosts: self.known_hosts.clone(),
        })
        .await
        .expect("connect")
    }
}

/// Wait for a browser event satisfying `done`.
async fn wait_for(browser: &SftpBrowser, what: &str, done: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        for event in browser.drain() {
            if done(&event) {
                return event;
            }
            seen.push(event);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}; events so far: {seen:?}");
}

fn names(event: &Event) -> Vec<String> {
    match event {
        Event::Listed { entries, .. } => entries.iter().map(|e| e.name.clone()).collect(),
        other => panic!("expected a listing, got {other:?}"),
    }
}

#[tokio::test]
async fn the_explorer_lists_a_remote_directory() {
    let fixture = Fixture::start("list").await;
    std::fs::write(fixture.dir.path().join("notes.txt"), "hello").unwrap();
    std::fs::create_dir(fixture.dir.path().join("projects")).unwrap();

    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");
    browser.send(Request::List {
        path: fixture.dir.as_str(),
        generation: 1,
    });

    let event = wait_for(&browser, "a listing", |e| matches!(e, Event::Listed { .. })).await;
    // Directories sort before files.
    assert_eq!(names(&event), vec!["projects", "notes.txt"]);

    let Event::Listed {
        entries,
        generation,
        ..
    } = &event
    else {
        unreachable!()
    };
    assert_eq!(
        *generation, 1,
        "the generation must come back for staleness checks"
    );
    assert_eq!(entries[0].kind, EntryKind::Dir);
    assert_eq!(entries[1].kind, EntryKind::File);
    assert_eq!(entries[1].size, 5, "file size was not reported");
}

#[tokio::test]
async fn the_explorer_rides_the_same_connection_as_the_shells() {
    // The reason the explorer can follow the terminal at all.
    let fixture = Fixture::start("shared").await;
    let connection = fixture.connect().await;

    let _shell = connection
        .open_shell("xterm-256color", 80, 24)
        .await
        .expect("shell");
    let browser = SftpBrowser::open(Arc::clone(&connection), None)
        .await
        .expect("sftp");
    browser.send(Request::List {
        path: fixture.dir.as_str(),
        generation: 1,
    });
    wait_for(&browser, "a listing", |e| matches!(e, Event::Listed { .. })).await;

    assert_eq!(
        fixture.connections.load(Ordering::SeqCst),
        1,
        "the explorer opened its own connection instead of sharing the shell's"
    );
    assert_eq!(
        fixture.channels.load(Ordering::SeqCst),
        2,
        "expected one channel for the shell and one for SFTP"
    );
}

#[tokio::test]
async fn the_home_directory_is_resolved_by_asking_the_server() {
    // Guessing `/home/<user>` is wrong on plenty of systems, so the browser asks.
    let fixture = Fixture::start("home").await;
    std::fs::write(fixture.dir.path().join("marker"), "x").unwrap();

    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");
    browser.send(Request::Home { generation: 7 });

    let event = wait_for(&browser, "the home listing", |e| {
        matches!(e, Event::Listed { .. })
    })
    .await;
    let Event::Listed {
        path, generation, ..
    } = &event
    else {
        unreachable!()
    };
    assert_eq!(path, &fixture.dir.as_str());
    assert_eq!(*generation, 7);
    assert_eq!(names(&event), vec!["marker"]);
}

#[tokio::test]
async fn a_file_created_by_a_command_shows_up_on_the_next_listing() {
    // The staleness MobaXterm has and catshell does not: something changes the
    // filesystem, and the very next listing reflects it over the same session.
    let fixture = Fixture::start("refresh").await;
    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");

    browser.send(Request::List {
        path: fixture.dir.as_str(),
        generation: 1,
    });
    let first = wait_for(&browser, "the first listing", |e| {
        matches!(e, Event::Listed { .. })
    })
    .await;
    assert!(names(&first).is_empty());

    // Stand in for a shell command having written a file.
    std::fs::write(fixture.dir.path().join("built.log"), "output").unwrap();

    browser.send(Request::List {
        path: fixture.dir.as_str(),
        generation: 2,
    });
    let second = wait_for(&browser, "the refreshed listing", |e| {
        matches!(e, Event::Listed { generation: 2, .. })
    })
    .await;
    assert_eq!(names(&second), vec!["built.log"]);
}

#[tokio::test]
async fn directories_can_be_created_and_removed() {
    let fixture = Fixture::start("mkdir").await;
    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");

    let created = format!("{}/new-folder", fixture.dir.as_str());
    browser.send(Request::MakeDir {
        path: created.clone(),
    });
    wait_for(&browser, "the directory to be created", |e| {
        matches!(e, Event::Changed)
    })
    .await;
    assert!(fixture.dir.path().join("new-folder").is_dir());

    browser.send(Request::Remove {
        path: created,
        kind: EntryKind::Dir,
    });
    wait_for(&browser, "the directory to be removed", |e| {
        matches!(e, Event::Changed)
    })
    .await;
    assert!(!fixture.dir.path().join("new-folder").exists());
}

#[tokio::test]
async fn files_can_be_renamed_and_removed() {
    let fixture = Fixture::start("rename").await;
    std::fs::write(fixture.dir.path().join("before.txt"), "x").unwrap();

    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");

    browser.send(Request::Rename {
        from: format!("{}/before.txt", fixture.dir.as_str()),
        to: format!("{}/after.txt", fixture.dir.as_str()),
    });
    wait_for(&browser, "the rename", |e| matches!(e, Event::Changed)).await;
    assert!(fixture.dir.path().join("after.txt").exists());
    assert!(!fixture.dir.path().join("before.txt").exists());

    browser.send(Request::Remove {
        path: format!("{}/after.txt", fixture.dir.as_str()),
        kind: EntryKind::File,
    });
    wait_for(&browser, "the removal", |e| matches!(e, Event::Changed)).await;
    assert!(!fixture.dir.path().join("after.txt").exists());
}

#[tokio::test]
async fn a_failed_operation_is_reported_rather_than_silently_dropped() {
    let fixture = Fixture::start("failure").await;
    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");

    browser.send(Request::List {
        path: format!("{}/does-not-exist", fixture.dir.as_str()),
        generation: 1,
    });

    let event = wait_for(&browser, "a failure", |e| matches!(e, Event::Failed { .. })).await;
    let Event::Failed { action, message } = &event else {
        unreachable!()
    };
    // The message has to name what was being done, or it is unactionable in the UI.
    assert!(action.contains("listing"), "unhelpful action: {action}");
    assert!(!message.is_empty());
}

#[tokio::test]
async fn symlinks_are_reported_as_links_not_as_their_targets() {
    let fixture = Fixture::start("symlink").await;
    std::fs::create_dir(fixture.dir.path().join("target")).unwrap();
    std::os::unix::fs::symlink("target", fixture.dir.path().join("link")).unwrap();

    let connection = fixture.connect().await;
    let browser = SftpBrowser::open(connection, None)
        .await
        .expect("open sftp");
    browser.send(Request::List {
        path: fixture.dir.as_str(),
        generation: 1,
    });

    let event = wait_for(&browser, "a listing", |e| matches!(e, Event::Listed { .. })).await;
    let Event::Listed { entries, .. } = &event else {
        unreachable!()
    };
    let link = entries
        .iter()
        .find(|e| e.name == "link")
        .expect("no link entry");
    assert_eq!(link.kind, EntryKind::Symlink);
}

/// Run the SSH + SFTP test server until killed, for driving the real application.
///
/// Not part of the suite. It exists so the GUI's remote file explorer can be exercised
/// on a machine with no `sshd`:
///
/// ```text
/// CATSHELL_TEST_ROOT=/tmp/remote-root \
///   cargo test -p catshell-net --test sftp -- --ignored --nocapture serve_forever &
/// ```
///
/// Port, password and served directory come from `CATSHELL_TEST_PORT`,
/// `CATSHELL_TEST_PASSWORD` and `CATSHELL_TEST_ROOT`.
#[tokio::test]
#[ignore = "development aid: runs an SSH/SFTP server until killed"]
async fn serve_forever() {
    let port: u16 = std::env::var("CATSHELL_TEST_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2223);
    let root = std::env::var("CATSHELL_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());

    use rand::SeedableRng as _;
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(99);
    let key = russh::keys::PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519).unwrap();
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        ..Default::default()
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!(
        "test SSH/SFTP server on 127.0.0.1:{port} serving {}",
        root.display()
    );

    let mut server = TestServer {
        root,
        connections: Arc::new(AtomicUsize::new(0)),
        channels: Arc::new(AtomicUsize::new(0)),
    };
    let _ = server.run_on_socket(config, &listener).await;
}
