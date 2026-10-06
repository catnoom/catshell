//! End-to-end tests for the multiplexed connection, against a real SSH server.
//!
//! The server is `russh`'s own, run in-process on an ephemeral port. That keeps these
//! tests self-contained — no Docker, no `sshd`, no fixtures on disk — while still
//! exercising the actual protocol: key exchange, authentication, channel open, PTY
//! request, and data both ways.
//!
//! The claim under test is the one the whole design rests on: several panes on one host
//! share **one** authenticated transport. The server counts TCP connections and channels
//! separately so that claim is checked rather than assumed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config as TermConfig;
use catshell_net::connection::{AuthMethod, ConnectOptions, HostConnection};
use catshell_net::host::HostConfig;
use catshell_net::known_hosts::UnknownHostPolicy;
use catshell_net::shell;
use catshell_term::palette::Palette;
use catshell_term::session::{GridSize, Session, SessionEvent};
use russh::server::{Auth, Handler, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;

const PASSWORD: &str = "correct horse battery staple";
const TIMEOUT: Duration = Duration::from_secs(20);

/// Counters shared by every connection the test server accepts.
#[derive(Default)]
struct Counts {
    connections: AtomicUsize,
    channels: AtomicUsize,
    shells: AtomicUsize,
}

#[derive(Clone)]
struct TestServer {
    counts: Arc<Counts>,
}

impl russh::server::Server for TestServer {
    type Handler = TestHandler;

    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Self::Handler {
        self.counts.connections.fetch_add(1, Ordering::SeqCst);
        TestHandler {
            counts: Arc::clone(&self.counts),
        }
    }
}

struct TestHandler {
    counts: Arc<Counts>,
}

impl Handler for TestHandler {
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
        _channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.counts.channels.fetch_add(1, Ordering::SeqCst);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _cols: u32,
        _rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        // Number each shell, so a test can tell the panes apart and prove they are
        // genuinely separate channels rather than one shared stream.
        let index = self.counts.shells.fetch_add(1, Ordering::SeqCst) + 1;
        session.channel_success(channel)?;
        session.data(channel, format!("shell {index} ready\r\n").into_bytes())?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        // Echoing the new size back is what lets a test prove the resize reached the
        // server rather than only the local grid.
        session.data(channel, format!("resized {cols}x{rows}\r\n").into_bytes())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        // A line marked this way is echoed back verbatim twice, the way a real
        // pseudoterminal and readline both echo what is typed at a shell.
        if data.windows(6).any(|window| window == b"HIDEME") {
            let text = String::from_utf8_lossy(data).replace('\r', "\r\n");
            session.data(channel, text.clone().into_bytes())?;
            session.data(channel, text.into_bytes())?;
            return Ok(());
        }

        // A crude shell: echo what was typed, and treat "exit" as a request to close.
        if data.starts_with(b"exit") {
            session.exit_status_request(channel, 7)?;
            session.close(channel)?;
            return Ok(());
        }
        let echoed = String::from_utf8_lossy(data).replace('\r', "\r\n");
        session.data(channel, format!("echo:{echoed}").into_bytes())?;
        Ok(())
    }
}

/// A running test server and the details needed to reach it.
struct Fixture {
    host: HostConfig,
    counts: Arc<Counts>,
    known_hosts: std::path::PathBuf,
    _dir: TempDir,
}

/// A temporary directory removed on drop, so tests leave nothing behind.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let counts = Arc::<Counts>::default();

        // A throwaway host key. Seeded per test name so a run is reproducible, and
        // never reused outside these tests.
        use rand::SeedableRng as _;
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(name.len() as u64);
        let key =
            russh::keys::PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519).unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            inactivity_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        });

        // Port 0 lets the OS pick a free port, so tests can run in parallel.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // The server and its listener both have to outlive the borrow `run_on_socket`
        // takes, so they are moved into the task rather than left on the stack here.
        let server_counts = Arc::clone(&counts);
        tokio::spawn(async move {
            let mut server = TestServer {
                counts: server_counts,
            };
            let _ = server.run_on_socket(config, &listener).await;
        });

        let dir = std::env::temp_dir().join(format!("catshell-mux-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        Self {
            host: HostConfig {
                name: "test".into(),
                hostname: "127.0.0.1".into(),
                port,
                user: Some("tester".into()),
                identity_files: vec![],
            },
            counts,
            known_hosts: dir.join("known_hosts"),
            _dir: TempDir(dir),
        }
    }

    fn options(&self, password: &str) -> ConnectOptions {
        ConnectOptions {
            host: self.host.clone(),
            methods: vec![AuthMethod::Password(password.into())],
            // The test server's key is generated fresh each run, so there is nothing to
            // have recorded; accepting into a throwaway file is the point here.
            unknown_host_policy: UnknownHostPolicy::Accept,
            known_hosts: self.known_hosts.clone(),
        }
    }

    async fn connect(&self) -> Arc<HostConnection> {
        HostConnection::connect(self.options(PASSWORD))
            .await
            .expect("connect")
    }
}

/// Unwrap the error from a connection that was supposed to fail.
///
/// `unwrap_err` would need `HostConnection: Debug`, which it deliberately is not — a
/// debug dump of a live connection is not something worth printing.
fn expect_failure(
    result: Result<Arc<HostConnection>, catshell_net::ConnectError>,
) -> catshell_net::ConnectError {
    match result {
        Ok(_) => panic!("the connection succeeded when it should have failed"),
        Err(err) => err,
    }
}

fn window_size(size: GridSize) -> WindowSize {
    WindowSize {
        num_lines: size.screen_lines as u16,
        num_cols: size.columns as u16,
        cell_width: 8,
        cell_height: 16,
    }
}

async fn open_shell(connection: &Arc<HostConnection>, size: GridSize) -> Session {
    shell::open(
        Arc::clone(connection),
        TermConfig::default(),
        size,
        window_size(size),
        Palette::default(),
        None,
    )
    .await
    .expect("open shell")
}

/// The visible grid as text.
fn screen(session: &Session) -> String {
    let term = session.term().lock();
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|line| {
            let row: String = (0..grid.columns())
                .map(|col| grid[Line(line as i32)][Column(col)].c)
                .collect();
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// Wait until the pane's visible text satisfies `done`.
async fn wait_for(session: &Session, what: &str, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        session.drain_events();
        let text = screen(session);
        if done(&text) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen was:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_shell_connects_and_receives_output() {
    let fixture = Fixture::start("basic").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(80, 24)).await;

    wait_for(&session, "the shell banner", |text| {
        text.contains("shell 1 ready")
    })
    .await;
}

#[tokio::test]
async fn several_panes_share_one_authenticated_transport() {
    // The claim the whole design rests on, and the milestone's acceptance criterion.
    let fixture = Fixture::start("multiplex").await;
    let connection = fixture.connect().await;

    let mut sessions = Vec::new();
    for _ in 0..3 {
        sessions.push(open_shell(&connection, GridSize::new(80, 24)).await);
    }

    // Each pane is a distinct channel with its own output.
    for (index, session) in sessions.iter().enumerate() {
        let expected = format!("shell {} ready", index + 1);
        wait_for(session, &expected, |text| text.contains(&expected)).await;
    }

    assert_eq!(
        fixture.counts.connections.load(Ordering::SeqCst),
        1,
        "three panes should share one TCP connection"
    );
    assert_eq!(
        fixture.counts.channels.load(Ordering::SeqCst),
        3,
        "each pane should have its own channel"
    );
}

#[tokio::test]
async fn a_subsystem_channel_joins_the_same_connection() {
    // What milestone 3 needs: the SFTP browser rides the shells' connection rather than
    // opening a second one, which is what lets it stay in step with them.
    let fixture = Fixture::start("subsystem").await;
    let connection = fixture.connect().await;

    let _shell = open_shell(&connection, GridSize::new(80, 24)).await;
    connection
        .open_subsystem("sftp")
        .await
        .expect("open sftp channel");

    assert_eq!(fixture.counts.connections.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.counts.channels.load(Ordering::SeqCst),
        2,
        "the shell and the subsystem should be two channels on one connection"
    );
}

#[tokio::test]
async fn input_reaches_the_remote_shell() {
    let fixture = Fixture::start("input").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(80, 24)).await;
    wait_for(&session, "the banner", |text| text.contains("ready")).await;

    session.write(b"hello\r".to_vec());
    wait_for(&session, "the echo", |text| text.contains("echo:hello")).await;
}

#[tokio::test]
async fn typing_goes_only_to_the_pane_it_was_typed_in() {
    // Panes share a transport but must not share a stream.
    let fixture = Fixture::start("isolation").await;
    let connection = fixture.connect().await;
    let first = open_shell(&connection, GridSize::new(80, 24)).await;
    let second = open_shell(&connection, GridSize::new(80, 24)).await;

    wait_for(&first, "banner", |text| text.contains("ready")).await;
    wait_for(&second, "banner", |text| text.contains("ready")).await;

    first.write(b"only-in-first\r".to_vec());
    wait_for(&first, "the echo", |text| {
        text.contains("echo:only-in-first")
    })
    .await;

    assert!(
        !screen(&second).contains("only-in-first"),
        "input leaked between panes:\n{}",
        screen(&second)
    );
}

#[tokio::test]
async fn resizing_a_pane_reaches_the_server() {
    let fixture = Fixture::start("resize").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(80, 24)).await;
    wait_for(&session, "the banner", |text| text.contains("ready")).await;

    let resized = GridSize::new(120, 40);
    session.resize(resized, window_size(resized));

    wait_for(&session, "the resize", |text| {
        text.contains("resized 120x40")
    })
    .await;
    assert_eq!(
        session.term().lock().grid().columns(),
        120,
        "the local grid did not resize"
    );
}

#[tokio::test]
async fn a_remote_exit_ends_the_session_with_its_status() {
    let fixture = Fixture::start("exit").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(80, 24)).await;
    wait_for(&session, "the banner", |text| text.contains("ready")).await;

    session.write(b"exit\r".to_vec());

    let deadline = Instant::now() + TIMEOUT;
    let mut exit = None;
    while Instant::now() < deadline && exit.is_none() {
        for event in session.drain_events() {
            if let SessionEvent::Exited(reason) = event {
                exit = Some(reason);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        exit,
        Some(catshell_term::session::ExitReason::Status(Some(7))),
        "the remote exit status was not reported"
    );
}

#[tokio::test]
async fn shell_integration_events_survive_the_ssh_transport() {
    // OSC 7 has to reach the sniffer through the channel exactly as it does through a
    // PTY; this is what the milestone 3 file pane will follow.
    let fixture = Fixture::start("osc").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(80, 24)).await;
    wait_for(&session, "the banner", |text| text.contains("ready")).await;

    session.write(b"\x1b]7;file://myhost/srv/app\x07".to_vec());

    let deadline = Instant::now() + TIMEOUT;
    let mut cwd = None;
    while Instant::now() < deadline && cwd.is_none() {
        for event in session.drain_events() {
            if let SessionEvent::Shell(catshell_term::ShellEvent::CwdChanged { path, .. }) = event {
                cwd = Some(path);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(cwd.as_deref(), Some("/srv/app"));
}

#[tokio::test]
async fn a_wrong_password_fails_without_a_connection() {
    let fixture = Fixture::start("badpass").await;
    let err = expect_failure(HostConnection::connect(fixture.options("wrong")).await);

    // The UI relies on this to know it should offer a password field.
    assert!(
        err.is_auth_failure(),
        "not reported as an auth failure: {err:?}"
    );
    // The password itself must never appear in an error that will be logged.
    assert!(
        !format!("{err:?}").contains("wrong"),
        "the password leaked into the error"
    );
}

#[tokio::test]
async fn an_unknown_host_is_refused_when_the_policy_says_so() {
    let fixture = Fixture::start("unknown").await;
    let mut options = fixture.options(PASSWORD);
    options.unknown_host_policy = UnknownHostPolicy::Reject;

    let err = expect_failure(HostConnection::connect(options).await);

    // Reported as its own case, so the UI can show the fingerprint and ask rather than
    // just saying the connection failed.
    let catshell_net::ConnectError::UnknownHost { fingerprint, .. } = &err else {
        panic!("expected an unknown-host error, got {err:?}");
    };
    assert!(
        fingerprint.starts_with("SHA256:"),
        "no fingerprint to compare: {fingerprint}"
    );
}

#[tokio::test]
async fn accepting_an_unknown_host_records_its_key() {
    let fixture = Fixture::start("learn").await;
    let _connection = fixture.connect().await;

    let recorded = std::fs::read_to_string(&fixture.known_hosts).expect("known_hosts written");
    assert!(
        recorded.contains("127.0.0.1"),
        "host not recorded: {recorded}"
    );

    // And a second connection is then a known host rather than an unknown one.
    let mut options = fixture.options(PASSWORD);
    options.unknown_host_policy = UnknownHostPolicy::Reject;
    HostConnection::connect(options)
        .await
        .expect("the recorded key should be trusted");
}

/// Run the test server until killed, for driving the real application by hand.
///
/// Not part of the suite — it never returns. It exists so the GUI's SSH path can be
/// exercised on a machine with no `sshd`:
///
/// ```text
/// cargo test -p catshell-net --test multiplexing -- --ignored --nocapture serve_forever &
/// CATSHELL_SSH_CONFIG=/tmp/config CATSHELL_KNOWN_HOSTS=/tmp/known_hosts cargo run -p catshell
/// ```
///
/// The port and password are read from `CATSHELL_TEST_PORT` and `CATSHELL_TEST_PASSWORD`.
#[tokio::test]
#[ignore = "development aid: runs an SSH server until killed"]
async fn serve_forever() {
    let port: u16 = std::env::var("CATSHELL_TEST_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2222);

    let counts = Arc::<Counts>::default();
    use rand::SeedableRng as _;
    let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(1);
    let key = russh::keys::PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519).unwrap();
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        ..Default::default()
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("test SSH server listening on 127.0.0.1:{port}");

    let mut server = TestServer { counts };
    let _ = server.run_on_socket(config, &listener).await;
}

#[tokio::test]
async fn a_hidden_command_is_not_shown_over_ssh() {
    // The SSH driver has to arm the echo filter before the bytes go out, exactly as the
    // local one does; this is the path that shows catshell's shell-integration snippet
    // to the user if it gets it wrong.
    let fixture = Fixture::start("hidden").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(100, 24)).await;
    wait_for(&session, "the banner", |text| text.contains("ready")).await;

    session.write_hidden(b"HIDEME-secret-plumbing\r".to_vec());
    // Then something visible, to know the hidden write has been through.
    session.write(b"visible\r".to_vec());
    wait_for(&session, "the visible echo", |text| {
        text.contains("echo:visible")
    })
    .await;

    let text = screen(&session);
    assert!(
        !text.contains("HIDEME"),
        "the hidden command was shown:\n{text}"
    );
}

#[tokio::test]
async fn an_ordinary_command_is_still_shown_over_ssh() {
    // The converse, so the filter cannot be hiding everything.
    let fixture = Fixture::start("not-hidden").await;
    let connection = fixture.connect().await;
    let session = open_shell(&connection, GridSize::new(100, 24)).await;
    wait_for(&session, "the banner", |text| text.contains("ready")).await;

    session.write(b"HIDEME-but-typed-by-the-user\r".to_vec());
    let text = wait_for(&session, "the echo", |text| text.contains("HIDEME")).await;
    assert!(
        text.contains("HIDEME"),
        "the user's own command was hidden:\n{text}"
    );
}
