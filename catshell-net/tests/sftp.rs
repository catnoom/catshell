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
use russh_sftp::protocol::{
    Attrs, File, FileAttributes, Handle, Name, Status, StatusCode, Version,
};
use tokio::net::TcpListener;

const PASSWORD: &str = "sftp-test";
const TIMEOUT: Duration = Duration::from_secs(20);

// --- A temporary directory that cleans up after itself -----------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        // The counter makes the path unique even if two tests pick the same name —
        // otherwise they share a directory and, running in parallel, delete each other's
        // files partway through.
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "catshell-sftp-{name}-{}-{unique}",
            std::process::id()
        ));
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
    /// Open file handles, by path, so reads and writes can address them by offset.
    files: HashMap<String, PathBuf>,
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

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: russh_sftp::protocol::OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        use russh_sftp::protocol::OpenFlags;

        // Mirror the flags onto a real file, so the test exercises the same create and
        // truncate behaviour a server would apply.
        let path = PathBuf::from(&filename);
        if pflags.contains(OpenFlags::CREATE) {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true);
            if pflags.contains(OpenFlags::TRUNCATE) {
                options.truncate(true);
            }
            options.open(&path).map_err(io_error)?;
        } else if !path.exists() {
            return Err(SftpError(StatusCode::NoSuchFile, filename));
        }

        let handle = format!("file-{}", self.next_handle);
        self.next_handle += 1;
        self.files.insert(handle.clone(), path);
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<russh_sftp::protocol::Data, Self::Error> {
        use std::io::{Read as _, Seek as _};

        let path = self
            .files
            .get(&handle)
            .ok_or_else(|| SftpError(StatusCode::Failure, "no such handle".into()))?;

        let mut file = std::fs::File::open(path).map_err(io_error)?;
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(io_error)?;

        let mut data = vec![0u8; len as usize];
        let read = file.read(&mut data).map_err(io_error)?;
        if read == 0 {
            // The client loops until it sees this.
            return Err(SftpError(StatusCode::Eof, "end of file".into()));
        }
        data.truncate(read);
        Ok(russh_sftp::protocol::Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        use std::io::{Seek as _, Write as _};

        let path = self
            .files
            .get(&handle)
            .ok_or_else(|| SftpError(StatusCode::Failure, "no such handle".into()))?;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(io_error)?;
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(io_error)?;
        file.write_all(&data).map_err(io_error)?;
        Ok(ok_status(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = std::fs::metadata(&path).map_err(io_error)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = std::fs::symlink_metadata(&path).map_err(io_error)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
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
        self.files.remove(&handle);
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
    /// Connections currently open, so a test can see one actually close rather than
    /// only see a new one appear.
    live: Arc<AtomicUsize>,
    channels: Arc<AtomicUsize>,
}

impl russh::server::Server for TestServer {
    type Handler = SshHandlerImpl;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        self.connections.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        SshHandlerImpl {
            root: self.root.clone(),
            channels: Arc::clone(&self.channels),
            live: Arc::clone(&self.live),
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct SshHandlerImpl {
    root: PathBuf,
    channels: Arc<AtomicUsize>,
    live: Arc<AtomicUsize>,
    /// Channels awaiting a subsystem request. `subsystem_request` is handed only a
    /// channel *id*, so the channel itself has to be kept from when it was opened.
    pending: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl Drop for SshHandlerImpl {
    fn drop(&mut self) {
        // russh drops the handler when the connection ends, whichever side closed it.
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
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
                files: HashMap::new(),
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
    live: Arc<AtomicUsize>,
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
        let live = Arc::new(AtomicUsize::new(0));
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
        let (server_connections, server_live, server_channels) = (
            Arc::clone(&connections),
            Arc::clone(&live),
            Arc::clone(&channels),
        );
        tokio::spawn(async move {
            let mut server = TestServer {
                root,
                connections: server_connections,
                live: server_live,
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
            live,
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
        live: Arc::new(AtomicUsize::new(0)),
        channels: Arc::new(AtomicUsize::new(0)),
    };
    let _ = server.run_on_socket(config, &listener).await;
}

// --- Transfers ----------------------------------------------------------------------

use catshell_net::transfer::{Direction, Event as TransferEvent, JobId, TransferQueue};

/// Build a transfer queue that connects to the fixture on demand.
///
/// `idle` is how long the queue keeps its connection with nothing to do; tests that are
/// not about teardown pass something long enough never to fire.
fn queue_for(fixture: &Fixture, idle: Duration) -> TransferQueue {
    let options = ConnectOptions {
        host: fixture.host.clone(),
        methods: vec![AuthMethod::Password(PASSWORD.into())],
        unknown_host_policy: UnknownHostPolicy::Accept,
        known_hosts: fixture.known_hosts.clone(),
    };
    let connect: catshell_net::transfer::Connector = Arc::new(move || {
        let options = options.clone();
        Box::pin(async move {
            HostConnection::connect(options)
                .await
                .map_err(anyhow::Error::from)
        })
    });
    TransferQueue::new(connect, idle, None)
}

/// Long enough that the idle teardown never fires during a test.
const NEVER_IDLE: Duration = Duration::from_secs(3600);

/// Accumulates transfer events.
///
/// `TransferQueue::drain` is destructive, so a test that waits for one job would throw
/// away the events of every other job in flight. This keeps them.
struct Watcher<'a> {
    queue: &'a TransferQueue,
    seen: Vec<TransferEvent>,
}

impl<'a> Watcher<'a> {
    fn new(queue: &'a TransferQueue) -> Self {
        Self {
            queue,
            seen: Vec::new(),
        }
    }

    /// Wait until some event satisfies `done`.
    async fn until(&mut self, what: &str, done: impl Fn(&TransferEvent) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.seen.extend(self.queue.drain());
            if self.seen.iter().any(&done) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; events so far: {:?}",
                self.seen
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn finish(&mut self, id: JobId) {
        self.until("the transfer to finish", |event| match event {
            TransferEvent::Finished { id: done } => *done == id,
            // Surface the reason rather than timing out on a job that already failed.
            TransferEvent::Failed {
                id: failed,
                message,
            } if *failed == id => {
                panic!("the transfer failed: {message}")
            }
            _ => false,
        })
        .await;
    }

    fn progress_for(&self, id: JobId) -> Vec<u64> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                TransferEvent::Progress {
                    id: which,
                    transferred,
                } if *which == id => Some(*transferred),
                _ => None,
            })
            .collect()
    }
}

#[tokio::test]
async fn a_file_is_uploaded() {
    let fixture = Fixture::start("upload").await;
    let source = fixture.dir.path().join("source.bin");
    let contents: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&source, &contents).unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);

    let target = format!("{}/uploaded.bin", fixture.dir.as_str());
    let id = queue.enqueue(
        Direction::Upload,
        source.clone(),
        target.clone(),
        contents.len() as u64,
    );
    Watcher::new(&queue).finish(id).await;

    assert_eq!(
        std::fs::read(&target).unwrap(),
        contents,
        "the upload did not match"
    );
}

#[tokio::test]
async fn a_file_is_downloaded() {
    let fixture = Fixture::start("download").await;
    let remote = fixture.dir.path().join("remote.bin");
    let contents: Vec<u8> = (0..150_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&remote, &contents).unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);

    let local = fixture.dir.path().join("downloaded.bin");
    let id = queue.enqueue(
        Direction::Download,
        local.clone(),
        remote.to_string_lossy().into_owned(),
        contents.len() as u64,
    );
    Watcher::new(&queue).finish(id).await;

    assert_eq!(
        std::fs::read(&local).unwrap(),
        contents,
        "the download did not match"
    );
}

#[tokio::test]
async fn progress_is_reported_as_the_bytes_move() {
    let fixture = Fixture::start("progress").await;
    let source = fixture.dir.path().join("big.bin");
    let contents = vec![7u8; 400_000];
    std::fs::write(&source, &contents).unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);

    let target = format!("{}/big-copy.bin", fixture.dir.as_str());
    let id = queue.enqueue(Direction::Upload, source, target, contents.len() as u64);
    let mut watcher = Watcher::new(&queue);
    watcher.finish(id).await;

    assert!(
        watcher
            .seen
            .iter()
            .any(|e| matches!(e, TransferEvent::Started { .. })),
        "the transfer never reported starting"
    );

    let reported = watcher.progress_for(id);
    assert!(!reported.is_empty(), "no progress was reported");
    assert!(
        reported.windows(2).all(|pair| pair[0] <= pair[1]),
        "progress went backwards: {reported:?}"
    );
    assert_eq!(
        reported.last().copied(),
        Some(contents.len() as u64),
        "the final progress did not reach the file's size"
    );
}

#[tokio::test]
async fn a_transfer_can_be_cancelled_and_leaves_what_it_wrote() {
    // The partial file is kept on purpose: it is what makes resuming possible.
    let fixture = Fixture::start("cancel").await;
    let source = fixture.dir.path().join("large.bin");
    std::fs::write(&source, vec![3u8; 40_000_000]).unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);

    let total = 40_000_000u64;
    let target = format!("{}/partial.bin", fixture.dir.as_str());
    let id = queue.enqueue(Direction::Upload, source, target.clone(), total);

    // Cancel once it is genuinely under way, but long before it could finish.
    let mut watcher = Watcher::new(&queue);
    watcher
        .until("some progress", |event| {
            matches!(event, TransferEvent::Progress { transferred, .. } if *transferred > 0)
        })
        .await;
    queue.cancel(id);

    watcher
        .until(
            "the cancellation",
            |event| matches!(event, TransferEvent::Cancelled { id: stopped } if *stopped == id),
        )
        .await;

    let written = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    assert!(written > 0, "nothing was written before the cancel");
    assert!(written < total, "the transfer finished instead of stopping");
}

#[tokio::test]
async fn several_queued_transfers_all_complete() {
    let fixture = Fixture::start("queue").await;
    let queue = queue_for(&fixture, NEVER_IDLE);

    let mut expected = Vec::new();
    let mut ids = Vec::new();
    for index in 0..5 {
        let source = fixture.dir.path().join(format!("file-{index}.bin"));
        let contents = vec![index as u8; 10_000 * (index + 1)];
        std::fs::write(&source, &contents).unwrap();

        let target = format!("{}/copy-{index}.bin", fixture.dir.as_str());
        ids.push(queue.enqueue(
            Direction::Upload,
            source,
            target.clone(),
            contents.len() as u64,
        ));
        expected.push((target, contents));
    }

    let mut watcher = Watcher::new(&queue);
    for id in ids {
        watcher.finish(id).await;
    }
    for (target, contents) in expected {
        assert_eq!(
            std::fs::read(&target).unwrap(),
            contents,
            "{target} did not match"
        );
    }
}

#[tokio::test]
async fn uploading_into_a_missing_directory_creates_it() {
    let fixture = Fixture::start("mkdir-on-upload").await;
    let source = fixture.dir.path().join("file.txt");
    std::fs::write(&source, "contents").unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);

    let target = format!("{}/new-dir/file.txt", fixture.dir.as_str());
    let id = queue.enqueue(Direction::Upload, source, target.clone(), 8);
    Watcher::new(&queue).finish(id).await;

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "contents");
}

#[tokio::test]
async fn downloading_into_a_missing_directory_creates_it() {
    let fixture = Fixture::start("mkdir-on-download").await;
    let remote = fixture.dir.path().join("remote.txt");
    std::fs::write(&remote, "contents").unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);

    let local = fixture.dir.path().join("nested/deeper/local.txt");
    let id = queue.enqueue(
        Direction::Download,
        local.clone(),
        remote.to_string_lossy().into_owned(),
        8,
    );
    Watcher::new(&queue).finish(id).await;

    assert_eq!(std::fs::read_to_string(&local).unwrap(), "contents");
}

#[tokio::test]
async fn a_failed_transfer_is_reported_and_the_queue_keeps_going() {
    let fixture = Fixture::start("failure").await;
    let queue = queue_for(&fixture, NEVER_IDLE);

    // A source that does not exist.
    let doomed = queue.enqueue(
        Direction::Upload,
        fixture.dir.path().join("missing.bin"),
        format!("{}/never.bin", fixture.dir.as_str()),
        10,
    );
    let mut watcher = Watcher::new(&queue);
    watcher
        .until(
            "the failure",
            |event| matches!(event, TransferEvent::Failed { id, .. } if *id == doomed),
        )
        .await;

    // The next job must still run: one bad file should not stall the queue.
    let source = fixture.dir.path().join("fine.txt");
    std::fs::write(&source, "fine").unwrap();
    let target = format!("{}/fine-copy.txt", fixture.dir.as_str());
    let good = queue.enqueue(Direction::Upload, source, target.clone(), 4);
    watcher.finish(good).await;

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "fine");
}

#[tokio::test]
async fn resuming_completes_a_partial_file_correctly() {
    // Resume is only worth having if the result is byte-for-byte what a clean transfer
    // would have produced; seek arithmetic off by one would corrupt it silently.
    //
    // The partial destination is written directly rather than produced by cancelling a
    // real transfer, so the starting offset is exact and the test does not depend on how
    // fast the loopback happens to be. Cancelling mid-transfer is covered separately.
    let fixture = Fixture::start("resume").await;
    let contents: Vec<u8> = (0..1_000_000u32).map(|i| (i % 241) as u8).collect();
    let source = fixture.dir.path().join("source.bin");
    std::fs::write(&source, &contents).unwrap();

    let already = 300_000usize;
    let target = format!("{}/resumed.bin", fixture.dir.as_str());
    std::fs::write(&target, &contents[..already]).unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);
    let id = queue.resume(
        Direction::Upload,
        source,
        target.clone(),
        contents.len() as u64,
    );

    let mut watcher = Watcher::new(&queue);
    watcher.finish(id).await;

    assert_eq!(
        std::fs::read(&target).unwrap(),
        contents,
        "the resumed file does not match the source"
    );

    // And it continued from where the partial file ended rather than starting over.
    let reported = watcher.progress_for(id);
    assert!(
        reported.first().copied().unwrap_or(0) >= already as u64,
        "resumed progress restarted from {:?} instead of {already}",
        reported.first()
    );
}

#[tokio::test]
async fn resuming_a_download_completes_a_partial_file_correctly() {
    let fixture = Fixture::start("resume-download").await;
    let contents: Vec<u8> = (0..800_000u32).map(|i| (i % 251) as u8).collect();
    let remote = fixture.dir.path().join("remote.bin");
    std::fs::write(&remote, &contents).unwrap();

    let already = 250_000usize;
    let local = fixture.dir.path().join("partial-download.bin");
    std::fs::write(&local, &contents[..already]).unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);
    let id = queue.resume(
        Direction::Download,
        local.clone(),
        remote.to_string_lossy().into_owned(),
        contents.len() as u64,
    );
    Watcher::new(&queue).finish(id).await;

    assert_eq!(
        std::fs::read(&local).unwrap(),
        contents,
        "the resumed download does not match the source"
    );
}

#[tokio::test]
async fn a_transfer_that_is_not_resuming_starts_from_the_beginning() {
    // The default must overwrite: resuming onto an unrelated file of the same name
    // would splice two different files together.
    let fixture = Fixture::start("overwrite").await;
    let source = fixture.dir.path().join("short.bin");
    std::fs::write(&source, b"new").unwrap();

    let target = format!("{}/existing.bin", fixture.dir.as_str());
    std::fs::write(&target, b"much longer previous contents").unwrap();

    let queue = queue_for(&fixture, NEVER_IDLE);
    let id = queue.enqueue(Direction::Upload, source, target.clone(), 3);
    Watcher::new(&queue).finish(id).await;

    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"new",
        "the previous contents were not replaced"
    );
}

#[tokio::test]
async fn the_bulk_connection_is_not_made_until_something_is_transferred() {
    // A session that never transfers anything should never open a second connection.
    let fixture = Fixture::start("idle-unused").await;
    let _queue = queue_for(&fixture, NEVER_IDLE);
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        fixture.connections.load(Ordering::SeqCst),
        0,
        "a connection was made before there was anything to transfer"
    );
}

#[tokio::test]
async fn the_bulk_connection_is_dropped_once_it_has_been_idle() {
    let fixture = Fixture::start("idle-teardown").await;
    let source = fixture.dir.path().join("file.txt");
    std::fs::write(&source, "contents").unwrap();

    let queue = queue_for(&fixture, Duration::from_millis(200));

    let first = queue.enqueue(
        Direction::Upload,
        source.clone(),
        format!("{}/one.txt", fixture.dir.as_str()),
        8,
    );
    let mut watcher = Watcher::new(&queue);
    watcher.finish(first).await;
    assert_eq!(fixture.connections.load(Ordering::SeqCst), 1);

    // Long enough for the idle timeout to fire.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        fixture.live.load(Ordering::SeqCst),
        0,
        "the connection was still open after the idle timeout"
    );

    // The next transfer has to connect again — and still work.
    let second = queue.enqueue(
        Direction::Upload,
        source,
        format!("{}/two.txt", fixture.dir.as_str()),
        8,
    );
    watcher.finish(second).await;

    assert_eq!(
        fixture.connections.load(Ordering::SeqCst),
        2,
        "the queue did not reconnect after going idle"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}/two.txt", fixture.dir.as_str())).unwrap(),
        "contents",
        "the transfer after reconnecting did not arrive"
    );
}

#[tokio::test]
async fn a_run_of_transfers_reuses_one_connection() {
    // Queueing files one after another must not reconnect between each; the idle timer
    // only runs when there is genuinely nothing to do.
    let fixture = Fixture::start("idle-reuse").await;
    let queue = queue_for(&fixture, Duration::from_millis(500));

    let mut watcher = Watcher::new(&queue);
    for index in 0..4 {
        let source = fixture.dir.path().join(format!("f{index}.txt"));
        std::fs::write(&source, "x").unwrap();
        let id = queue.enqueue(
            Direction::Upload,
            source,
            format!("{}/c{index}.txt", fixture.dir.as_str()),
            1,
        );
        watcher.finish(id).await;
    }

    assert_eq!(
        fixture.connections.load(Ordering::SeqCst),
        1,
        "the queue reconnected between transfers"
    );
}
