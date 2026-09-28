//! The app's side of SSH: connection state, and the prompts a connection may need.
//!
//! Connecting is asynchronous and can stop to ask a question — is this host key
//! trustworthy, what is the password — so it cannot be a blocking call from the frame
//! loop. Attempts run on a tokio runtime and report back through a channel; this module
//! holds the state machine that drives them, free of egui so it can be tested.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::term::Config as TermConfig;
use catshell_net::connection::{AuthMethod, ConnectError, ConnectOptions, HostConnection};
use catshell_net::host::{HostStore, ListedHost};
use catshell_net::known_hosts::UnknownHostPolicy;
use catshell_net::sftp::SftpBrowser;
use catshell_net::{host, secrets, shell, HostConfig};
use catshell_term::palette::Palette;
use catshell_term::session::{GridSize, Session, Wakeup};

/// What the UI must ask the user before an attempt can continue.
#[derive(Debug, Clone, PartialEq)]
pub enum Prompt {
    /// The host is not in `known_hosts`. Show the fingerprint and ask whether to trust it.
    HostKey {
        host: HostConfig,
        fingerprint: String,
    },
    /// Every key method was rejected; a password is the remaining option.
    Password { host: HostConfig, detail: String },
    /// A dead end: the key changed, or something else went wrong.
    Failed { host: HostConfig, message: String },
}

/// The outcome of one connection attempt, sent back to the UI thread.
pub enum Attempt {
    /// An SFTP channel is ready on an existing connection.
    BrowserReady {
        host: String,
        browser: SftpBrowser,
    },
    Connected {
        host: HostConfig,
        connection: Arc<HostConnection>,
        session: Session,
    },
    /// The connection stands but the shell could not be opened.
    ShellFailed {
        host: HostConfig,
        message: String,
    },
    NeedsInput(Prompt),
}

/// Whether a host is reachable right now.
pub enum ConnectionState {
    Connecting,
    Connected(Arc<HostConnection>),
    Failed(String),
}

/// Owns the SSH runtime and everything connected through it.
pub struct SshManager {
    runtime: tokio::runtime::Runtime,
    /// Hosts read from `~/.ssh/config`, shown but not edited.
    imported: Vec<HostConfig>,
    /// Hosts defined in catshell, which the editor owns.
    store: HostStore,
    connections: HashMap<String, ConnectionState>,
    /// One file browser per host, opened on demand.
    ///
    /// Lazily, because a user who never opens the explorer should not be paying for an
    /// extra channel — but on the *existing* connection when they do, which is what lets
    /// it see the same filesystem as the shells.
    browsers: HashMap<String, SftpBrowser>,
    /// Hosts whose browser is being opened, so it is not opened twice.
    opening_browsers: std::collections::HashSet<String>,
    /// Hosts whose browser became available since the last drain.
    ///
    /// Opening is asynchronous, so anything requested in the meantime was dropped on the
    /// floor — the caller has to know when to ask again.
    ready_browsers: Vec<String>,
    /// Shells asked for while a host was still connecting.
    ///
    /// Without this queue, opening several panes on a host at once — or simply clicking
    /// it twice — starts a fresh transport for each, because none of them has finished
    /// connecting yet and so none can be reused. That is precisely the duplicated
    /// authentication the one-connection design exists to avoid.
    pending: HashMap<String, Vec<ShellRequest>>,
    results: (Sender<Attempt>, Receiver<Attempt>),
}

impl SshManager {
    pub fn new() -> anyhow::Result<Self> {
        // A small pool: this runtime carries SSH transports, not compute.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("catshell-ssh")
            .enable_all()
            .build()?;

        Ok(Self {
            runtime,
            imported: host::import_ssh_config(),
            store: HostStore::load(),
            connections: HashMap::new(),
            browsers: HashMap::new(),
            opening_browsers: std::collections::HashSet::new(),
            ready_browsers: Vec::new(),
            pending: HashMap::new(),
            results: std::sync::mpsc::channel(),
        })
    }

    /// Every host to show, catshell's own first.
    pub fn hosts(&self) -> Vec<ListedHost> {
        host::merge(self.store.hosts(), &self.imported)
    }

    /// The host with this name, from either source.
    pub fn host(&self, name: &str) -> Option<HostConfig> {
        self.hosts()
            .into_iter()
            .find(|listed| listed.config.name == name)
            .map(|listed| listed.config)
    }

    /// catshell's own host list, for the editor.
    pub fn store(&self) -> &HostStore {
        &self.store
    }

    /// Add or replace one of catshell's own hosts and write the list out.
    pub fn save_host(&mut self, host: HostConfig, replacing: Option<&str>) -> anyhow::Result<()> {
        self.store.upsert(host, replacing);
        self.store.save()
    }

    /// Remove one of catshell's own hosts and write the list out.
    pub fn delete_host(&mut self, name: &str) -> anyhow::Result<()> {
        if self.store.remove(name) {
            self.store.save()?;
        }
        Ok(())
    }

    /// Re-read `~/.ssh/config` and catshell's own list.
    pub fn reload_hosts(&mut self) {
        self.imported = host::import_ssh_config();
        self.store = HostStore::load();
    }

    pub fn state(&self, host: &str) -> Option<&ConnectionState> {
        self.connections.get(host)
    }

    pub fn is_connecting(&self, host: &str) -> bool {
        matches!(
            self.connections.get(host),
            Some(ConnectionState::Connecting)
        )
    }

    /// Take whatever attempts have finished since the last frame.
    pub fn drain(&mut self) -> Vec<Attempt> {
        // A ready browser is kept here rather than handed on: the UI only needs to know
        // one has become available, which it discovers by asking for it.
        let mut finished = Vec::new();
        for attempt in self.results.1.try_iter() {
            match attempt {
                Attempt::BrowserReady { host, browser } => {
                    self.opening_browsers.remove(&host);
                    self.browsers.insert(host.clone(), browser);
                    self.ready_browsers.push(host);
                }
                other => finished.push(other),
            }
        }

        for attempt in &finished {
            match attempt {
                // Already taken out above.
                Attempt::BrowserReady { .. } => {}
                Attempt::Connected {
                    host, connection, ..
                } => {
                    self.connections.insert(
                        host.name.clone(),
                        ConnectionState::Connected(Arc::clone(connection)),
                    );
                    // Now that the transport exists, everything that queued behind it
                    // gets a channel on that same connection.
                    for request in self.pending.remove(&host.name).unwrap_or_default() {
                        self.spawn_shell(Arc::clone(connection), host.clone(), request);
                    }
                }
                Attempt::ShellFailed { host, message } => {
                    self.connections
                        .insert(host.name.clone(), ConnectionState::Failed(message.clone()));
                    self.pending.remove(&host.name);
                }
                Attempt::NeedsInput(prompt) => {
                    let (host, message) = match prompt {
                        Prompt::HostKey { host, .. } => (host, "awaiting host key approval"),
                        Prompt::Password { host, .. } => (host, "awaiting password"),
                        Prompt::Failed { host, message } => (host, message.as_str()),
                    };
                    self.connections
                        .insert(host.name.clone(), ConnectionState::Failed(message.into()));
                    // The attempt stopped to ask a question; anything queued behind it
                    // would otherwise wait forever.
                    self.pending.remove(&host.name);
                }
            }
        }
        finished
    }

    /// Start connecting to `host` and opening a shell on it.
    ///
    /// `trust_unknown_host` and `password` carry the answers to earlier prompts; a first
    /// attempt passes `false` and `None`.
    pub fn connect(
        &mut self,
        host: HostConfig,
        trust_unknown_host: bool,
        password: Option<String>,
        shell: ShellRequest,
    ) {
        // Reuse a transport that is already up rather than authenticating again — this
        // is what makes a second pane on the same host free. Checked *before* marking
        // the host as connecting, or the state just written would mask the live one.
        if let Some(ConnectionState::Connected(connection)) = self.connections.get(&host.name) {
            if connection.is_connected() {
                let connection = Arc::clone(connection);
                self.spawn_shell(connection, host, shell);
                return;
            }
            // The transport died. Its SFTP channel died with it, so the browser has to
            // go too — keeping it would leave the explorer talking to a dead channel
            // after the reconnect, silently showing nothing.
            self.drop_browser(&host.name);
        }

        // An attempt is already in flight. Wait for it rather than racing it: several
        // panes opened at once must still share one transport.
        if matches!(
            self.connections.get(&host.name),
            Some(ConnectionState::Connecting)
        ) {
            self.pending
                .entry(host.name.clone())
                .or_default()
                .push(shell);
            return;
        }

        self.connections
            .insert(host.name.clone(), ConnectionState::Connecting);

        let mut options = ConnectOptions::new(host.clone());
        if trust_unknown_host {
            options.unknown_host_policy = UnknownHostPolicy::Accept;
        }
        if let Some(password) = password {
            options.methods.push(AuthMethod::Password(password));
        } else if let Some(stored) = secrets::get(&host.address()) {
            // A password the user asked us to remember is just another method to try,
            // after the keys.
            options.methods.push(AuthMethod::Password(stored));
        }

        let results = self.results.0.clone();
        self.runtime.spawn(async move {
            let attempt = match HostConnection::connect(options).await {
                Ok(connection) => match open_shell(&connection, &shell).await {
                    Ok(session) => Attempt::Connected {
                        host,
                        connection,
                        session,
                    },
                    Err(err) => Attempt::ShellFailed {
                        host,
                        message: format!("{err:#}"),
                    },
                },
                Err(err) => Attempt::NeedsInput(prompt_for(host, err)),
            };
            let _ = results.send(attempt);
        });
    }

    /// Open another shell on a connection that is already up.
    pub fn spawn_shell(
        &self,
        connection: Arc<HostConnection>,
        host: HostConfig,
        shell: ShellRequest,
    ) {
        let results = self.results.0.clone();
        self.runtime.spawn(async move {
            let attempt = match open_shell(&connection, &shell).await {
                Ok(session) => Attempt::Connected {
                    host,
                    connection,
                    session,
                },
                Err(err) => Attempt::ShellFailed {
                    host,
                    message: format!("{err:#}"),
                },
            };
            let _ = results.send(attempt);
        });
    }

    /// Hosts whose browser has just become usable, taken once.
    pub fn take_ready_browsers(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ready_browsers)
    }

    /// The file browser for a host, if one is open.
    pub fn browser(&self, host: &str) -> Option<&SftpBrowser> {
        self.browsers.get(host)
    }

    /// Open a file browser for a host, if the connection is up and there is not one
    /// already.
    pub fn open_browser(&mut self, host: &str, wakeup: Option<Wakeup>) {
        if self.browsers.contains_key(host) || self.opening_browsers.contains(host) {
            return;
        }
        let Some(ConnectionState::Connected(connection)) = self.connections.get(host) else {
            return;
        };

        let connection = Arc::clone(connection);
        let host = host.to_string();
        let results = self.results.0.clone();
        self.opening_browsers.insert(host.clone());

        self.runtime.spawn(async move {
            let wakeup = wakeup.map(|w| w as catshell_net::sftp::Wakeup);
            let attempt = match SftpBrowser::open(connection, wakeup).await {
                Ok(browser) => Attempt::BrowserReady { host, browser },
                Err(err) => Attempt::ShellFailed {
                    host: HostConfig {
                        name: host,
                        hostname: String::new(),
                        port: 0,
                        user: None,
                        identity_files: vec![],
                    },
                    message: format!("opening the file browser: {err:#}"),
                },
            };
            let _ = results.send(attempt);
        });
    }

    /// Forget a host's browser, so the next use opens a fresh one.
    pub fn drop_browser(&mut self, host: &str) {
        self.browsers.remove(host);
        self.opening_browsers.remove(host);
    }

    /// Close every connection. Called when the app shuts down.
    pub fn disconnect_all(&mut self) {
        let connections: Vec<Arc<HostConnection>> = self
            .connections
            .values()
            .filter_map(|state| match state {
                ConnectionState::Connected(connection) => Some(Arc::clone(connection)),
                _ => None,
            })
            .collect();

        self.browsers.clear();
        self.runtime.block_on(async {
            for connection in connections {
                connection.disconnect().await;
            }
        });
        self.connections.clear();
    }
}

/// How to size the shell being opened.
#[derive(Clone)]
pub struct ShellRequest {
    pub config: TermConfig,
    pub size: GridSize,
    pub window_size: WindowSize,
    pub palette: Palette,
    pub wakeup: Option<Wakeup>,
}

async fn open_shell(
    connection: &Arc<HostConnection>,
    request: &ShellRequest,
) -> anyhow::Result<Session> {
    shell::open(
        Arc::clone(connection),
        request.config.clone(),
        request.size,
        request.window_size,
        request.palette,
        request.wakeup.clone(),
    )
    .await
}

/// Decide what to ask the user about a failed attempt.
fn prompt_for(host: HostConfig, err: ConnectError) -> Prompt {
    match err {
        ConnectError::UnknownHost { fingerprint, .. } => Prompt::HostKey { host, fingerprint },
        ConnectError::AuthFailed { detail, .. } => Prompt::Password { host, detail },
        // A changed host key is never offered as a yes/no question: accepting it is
        // exactly what an interception needs the user to do.
        //
        // `{:#}` so the whole context chain shows: the outermost message is only ever
        // "connecting to host", and the cause underneath is the part worth reading.
        other => Prompt::Failed {
            host,
            message: format!("{other:#}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostConfig {
        HostConfig {
            name: "web".into(),
            hostname: "web.example.com".into(),
            port: 22,
            user: Some("deploy".into()),
            identity_files: vec![],
        }
    }

    #[test]
    fn requests_made_while_connecting_queue_instead_of_racing() {
        let mut manager = SshManager::new().unwrap();
        let host = host();

        // Simulate an attempt already in flight.
        manager
            .connections
            .insert(host.name.clone(), ConnectionState::Connecting);

        let request = ShellRequest {
            config: Default::default(),
            size: GridSize::new(80, 24),
            window_size: alacritty_terminal::event::WindowSize {
                num_lines: 24,
                num_cols: 80,
                cell_width: 8,
                cell_height: 16,
            },
            palette: Palette::default(),
            wakeup: None,
        };

        manager.connect(host.clone(), false, None, request.clone());
        manager.connect(host.clone(), false, None, request);

        assert_eq!(
            manager.pending.get(&host.name).map(Vec::len),
            Some(2),
            "requests raced the in-flight connection instead of queueing"
        );
        // And the host is still just connecting — no second attempt was started.
        assert!(matches!(
            manager.connections.get(&host.name),
            Some(ConnectionState::Connecting)
        ));
    }

    #[test]
    fn a_failed_attempt_does_not_leave_requests_queued_forever() {
        let mut manager = SshManager::new().unwrap();
        let host = host();
        manager
            .connections
            .insert(host.name.clone(), ConnectionState::Connecting);
        manager.pending.insert(host.name.clone(), vec![]);

        manager
            .results
            .0
            .send(Attempt::NeedsInput(Prompt::Password {
                host: host.clone(),
                detail: "rejected".into(),
            }))
            .unwrap();
        manager.drain();

        assert!(
            !manager.pending.contains_key(&host.name),
            "queued shells were left waiting on an attempt that stopped to ask a question"
        );
    }

    #[test]
    fn an_unknown_host_asks_about_the_key() {
        let prompt = prompt_for(
            host(),
            ConnectError::UnknownHost {
                address: "deploy@web.example.com:22".into(),
                fingerprint: "SHA256:abc".into(),
            },
        );
        assert!(
            matches!(&prompt, Prompt::HostKey { fingerprint, .. } if fingerprint == "SHA256:abc"),
            "{prompt:?}"
        );
    }

    #[test]
    fn a_rejected_key_asks_for_a_password() {
        let prompt = prompt_for(
            host(),
            ConnectError::AuthFailed {
                address: "deploy@web.example.com:22".into(),
                detail: "ssh-agent was rejected".into(),
            },
        );
        assert!(matches!(prompt, Prompt::Password { .. }), "{prompt:?}");
    }

    #[test]
    fn a_changed_host_key_is_never_offered_as_a_question() {
        // Accepting a changed key is precisely what an interception requires, so it must
        // be a dead end in the UI, not a prompt with a "Trust" button.
        let prompt = prompt_for(
            host(),
            ConnectError::ChangedHostKey {
                address: "deploy@web.example.com:22".into(),
                fingerprint: "SHA256:evil".into(),
            },
        );
        let Prompt::Failed { message, .. } = &prompt else {
            panic!("a changed host key became an answerable prompt: {prompt:?}");
        };
        assert!(message.contains("CHANGED"), "{message}");
    }

    #[test]
    fn other_failures_are_reported_rather_than_prompted() {
        let prompt = prompt_for(
            host(),
            ConnectError::Other(anyhow::anyhow!("no route to host")),
        );
        assert!(matches!(&prompt, Prompt::Failed { message, .. } if message.contains("no route")));
    }
}
