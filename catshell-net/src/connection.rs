//! One authenticated SSH transport, carrying every channel for a host.
//!
//! This is the heart of the design. MobaXterm opens a *separate* SSH session for its
//! SFTP browser, which is why that browser drifts out of step with the shell — different
//! session, different working directory, no idea when a command finished. catshell opens
//! one connection per host and multiplexes: a channel per shell pane, and (from
//! milestone 3) an SFTP subsystem channel beside them. One authentication, one keepalive,
//! one reconnect, and a file pane that can follow the shell because it *is* the shell's
//! session.
//!
//! Bulk transfers are the one deliberate exception. SSH channels share a single TCP
//! stream, so a large upload would add latency to typing; those get their own connection,
//! opened lazily (milestone 4).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use russh::client::{self, Handle};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{Channel, ChannelMsg};
use tokio::sync::Mutex;

use crate::host::HostConfig;
use crate::known_hosts::{self, HostKeyVerdict, UnknownHostPolicy};

/// How often to send a keepalive. Enough to keep NAT mappings and idle-timeout-happy
/// firewalls from dropping a connection someone leaves open all day.
const KEEPALIVE: Duration = Duration::from_secs(30);

/// How the user is authenticated, tried in this order.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// Keys held by a running ssh-agent.
    Agent,
    /// A private key file, with an optional passphrase.
    Key {
        path: std::path::PathBuf,
        passphrase: Option<String>,
    },
    Password(String),
}

/// Why a connection attempt failed.
///
/// Structured rather than a message, because the UI has to respond differently to each:
/// an unknown host is a question for the user, a failed password is a prompt to retry,
/// and a changed host key is a refusal.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The host is not in `known_hosts`. Show the fingerprint and ask.
    #[error("{address} is not a known host (fingerprint {fingerprint})")]
    UnknownHost {
        address: String,
        fingerprint: String,
    },

    /// A different key is on record for this host. Do not offer to "just accept" this;
    /// it is what an intercepted connection looks like.
    #[error(
        "the host key for {address} has CHANGED (now {fingerprint}). The server may have \
         been rebuilt, or the connection may be intercepted. Verify the new key out of \
         band, then remove the old known_hosts entry."
    )]
    ChangedHostKey {
        address: String,
        fingerprint: String,
    },

    /// Every configured method was rejected. Usually means a password is needed.
    #[error("could not authenticate to {address}:\n  {detail}")]
    AuthFailed { address: String, detail: String },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ConnectError {
    /// Whether offering the user a password field is a sensible next step.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, ConnectError::AuthFailed { .. })
    }
}

/// Everything needed to establish a connection, gathered before any I/O.
pub struct ConnectOptions {
    pub host: HostConfig,
    /// Tried in order; the first that succeeds wins.
    pub methods: Vec<AuthMethod>,
    pub unknown_host_policy: UnknownHostPolicy,
    /// Where to look up and record host keys.
    pub known_hosts: std::path::PathBuf,
}

impl ConnectOptions {
    /// Options for `host`, with the conventional defaults: agent first, then any key
    /// files the SSH config named, and host keys checked against `~/.ssh/known_hosts`.
    pub fn new(host: HostConfig) -> Self {
        let mut methods = vec![AuthMethod::Agent];
        methods.extend(host.identity_files.iter().map(|path| AuthMethod::Key {
            path: path.clone(),
            passphrase: None,
        }));
        Self {
            host,
            methods,
            unknown_host_policy: UnknownHostPolicy::Ask,
            known_hosts: known_hosts_path(),
        }
    }
}

/// Environment variable overriding which `known_hosts` file to use.
pub const KNOWN_HOSTS_ENV: &str = "CATSHELL_KNOWN_HOSTS";

/// The `known_hosts` file to check against, `~/.ssh/known_hosts` unless
/// [`KNOWN_HOSTS_ENV`] names another.
pub fn known_hosts_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os(KNOWN_HOSTS_ENV) {
        return std::path::PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".ssh")
        .join("known_hosts")
}

/// Receives protocol callbacks and applies the host key policy.
struct ClientHandler {
    host: String,
    port: u16,
    policy: UnknownHostPolicy,
    known_hosts: std::path::PathBuf,
    /// Reported back so the caller can tell the user exactly what went wrong.
    verdict: Arc<std::sync::Mutex<Option<HostKeyVerdict>>>,
}

impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Host certificates are a different trust model — validity is decided by a CA
        // signature, not by `known_hosts`. Rather than pretend to check one, refuse and
        // say so; supporting them properly is its own piece of work.
        let PublicKeyOrCertificate::PublicKey { key, .. } = offered else {
            tracing::warn!(
                "{} offered a host certificate, which catshell cannot yet verify",
                self.host
            );
            return Ok(false);
        };

        let verdict = known_hosts::verify(&self.host, self.port, key, &self.known_hosts);
        *self.verdict.lock().unwrap() = Some(verdict.clone());

        match (&verdict, self.policy) {
            (HostKeyVerdict::Known, _) => Ok(true),
            // A changed key is refused whatever the policy says. It is indistinguishable
            // from an interception, and no convenience setting should wave it through.
            (HostKeyVerdict::Changed { .. }, _) => Ok(false),
            (HostKeyVerdict::Unknown { .. }, UnknownHostPolicy::Accept) => {
                if let Err(err) = known_hosts::learn(&self.host, self.port, key, &self.known_hosts)
                {
                    tracing::warn!("could not record host key: {err}");
                }
                Ok(true)
            }
            // `Ask` is resolved before connecting, by showing the fingerprint and
            // reconnecting with `Accept`; reaching here means the answer was no.
            (HostKeyVerdict::Unknown { .. }, _) => Ok(false),
        }
    }
}

/// An authenticated connection to one host.
///
/// Cheap to clone the `Arc` and open more channels from: `channel_open_session` takes
/// `&self`, so every pane shares this one transport.
pub struct HostConnection {
    handle: Handle<ClientHandler>,
    host: HostConfig,
    /// Serialises channel opening. russh allows concurrent opens, but serialising keeps
    /// channel numbering deterministic, which makes failures far easier to read in logs.
    open_lock: Mutex<()>,
}

impl HostConnection {
    /// Connect and authenticate.
    pub async fn connect(options: ConnectOptions) -> Result<Arc<Self>, ConnectError> {
        let ConnectOptions {
            host,
            methods,
            unknown_host_policy,
            known_hosts,
        } = options;

        let config = Arc::new(client::Config {
            keepalive_interval: Some(KEEPALIVE),
            ..Default::default()
        });

        let verdict = Arc::new(std::sync::Mutex::new(None));
        let handler = ClientHandler {
            host: host.hostname.clone(),
            port: host.port,
            policy: unknown_host_policy,
            known_hosts,
            verdict: Arc::clone(&verdict),
        };

        let mut handle = client::connect(config, (host.hostname.as_str(), host.port), handler)
            .await
            .map_err(|err| classify_connect_error(err, &verdict, &host))?;

        let user = host
            .user
            .clone()
            .or_else(whoami)
            .ok_or_else(|| anyhow!("no username for {}", host.address()))?;

        authenticate(&mut handle, &user, &methods)
            .await
            .map_err(|detail| ConnectError::AuthFailed {
                address: host.address(),
                detail: detail.to_string(),
            })?;

        Ok(Arc::new(Self {
            handle,
            host,
            open_lock: Mutex::new(()),
        }))
    }

    pub fn host(&self) -> &HostConfig {
        &self.host
    }

    /// Whether the transport is still up.
    pub fn is_connected(&self) -> bool {
        !self.handle.is_closed()
    }

    /// Open a channel running a login shell on a pseudoterminal.
    pub async fn open_shell(
        &self,
        term: &str,
        columns: u16,
        lines: u16,
    ) -> anyhow::Result<Channel<client::Msg>> {
        let _guard = self.open_lock.lock().await;

        let channel = self
            .handle
            .channel_open_session()
            .await
            .context("opening a shell channel")?;

        channel
            .request_pty(true, term, u32::from(columns), u32::from(lines), 0, 0, &[])
            .await
            .context("requesting a pseudoterminal")?;
        channel
            .request_shell(true)
            .await
            .context("starting the shell")?;

        Ok(channel)
    }

    /// Open a channel running a subsystem, such as `sftp`.
    pub async fn open_subsystem(&self, name: &str) -> anyhow::Result<Channel<client::Msg>> {
        let _guard = self.open_lock.lock().await;

        let channel = self
            .handle
            .channel_open_session()
            .await
            .with_context(|| format!("opening a channel for the {name} subsystem"))?;
        channel
            .request_subsystem(true, name)
            .await
            .with_context(|| format!("starting the {name} subsystem"))?;
        Ok(channel)
    }

    /// Close the transport and every channel on it.
    pub async fn disconnect(&self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "catshell closing", "")
            .await;
    }
}

/// Work out what actually went wrong, using the host-key verdict the handler recorded.
///
/// The transport error alone cannot tell these apart: rejecting a host key surfaces as a
/// generic disconnect, which would leave the user with "connection closed" and no way to
/// know a fingerprint was the reason.
fn classify_connect_error(
    err: anyhow::Error,
    verdict: &Arc<std::sync::Mutex<Option<HostKeyVerdict>>>,
    host: &HostConfig,
) -> ConnectError {
    match verdict.lock().unwrap().clone() {
        Some(HostKeyVerdict::Changed { fingerprint }) => ConnectError::ChangedHostKey {
            address: host.address(),
            fingerprint,
        },
        Some(HostKeyVerdict::Unknown { fingerprint }) => ConnectError::UnknownHost {
            address: host.address(),
            fingerprint,
        },
        _ => ConnectError::Other(err.context(format!("connecting to {}", host.address()))),
    }
}

/// Try each method in turn until one authenticates.
async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    methods: &[AuthMethod],
) -> anyhow::Result<()> {
    // Some servers accept `none`, and asking costs one round trip. More usefully, the
    // failure reply lists the methods the server will accept, which sharpens the error
    // if everything else fails too.
    if let Ok(result) = handle.authenticate_none(user).await {
        if result.success() {
            return Ok(());
        }
    }

    let mut failures = Vec::new();
    for method in methods {
        match try_method(handle, user, method).await {
            Ok(true) => return Ok(()),
            Ok(false) => failures.push(format!("{} was rejected", describe(method))),
            Err(err) => failures.push(format!("{}: {err}", describe(method))),
        }
    }

    if failures.is_empty() {
        Err(anyhow!("no authentication methods were configured"))
    } else {
        Err(anyhow!("every method failed:\n  {}", failures.join("\n  ")))
    }
}

fn describe(method: &AuthMethod) -> String {
    match method {
        AuthMethod::Agent => "ssh-agent".into(),
        AuthMethod::Key { path, .. } => format!("key {}", path.display()),
        AuthMethod::Password(_) => "password".into(),
    }
}

async fn try_method(
    handle: &mut Handle<ClientHandler>,
    user: &str,
    method: &AuthMethod,
) -> anyhow::Result<bool> {
    match method {
        AuthMethod::Password(password) => Ok(handle
            .authenticate_password(user, password)
            .await?
            .success()),

        AuthMethod::Key { path, passphrase } => {
            let key = russh::keys::load_secret_key(path, passphrase.as_deref())
                .with_context(|| format!("reading {}", path.display()))?;
            // Ask the server which RSA signature hash it wants; without this, an RSA key
            // fails against any server that has disabled the legacy SHA-1 signature.
            let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
            Ok(handle
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg))
                .await?
                .success())
        }

        AuthMethod::Agent => {
            let mut agent = connect_agent().await?;
            let identities = agent
                .request_identities()
                .await
                .context("listing agent keys")?;
            if identities.is_empty() {
                return Err(anyhow!("the agent is running but holds no keys"));
            }

            let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
            for identity in identities {
                let public_key = match &identity {
                    russh::keys::agent::AgentIdentity::PublicKey { key, .. } => key.clone(),
                    // Certificates need a different auth call; skip rather than fail, so
                    // one certificate in the agent cannot block the plain keys behind it.
                    russh::keys::agent::AgentIdentity::Certificate { .. } => continue,
                };

                match handle
                    .authenticate_publickey_with(user, public_key, hash_alg, &mut agent)
                    .await
                {
                    Ok(result) if result.success() => return Ok(true),
                    Ok(_) => continue,
                    Err(err) => {
                        tracing::debug!("agent key rejected: {err}");
                        continue;
                    }
                }
            }
            Ok(false)
        }
    }
}

/// Connect to whatever SSH agent this platform exposes.
///
/// There is no portable constructor, because the agent is reached completely differently
/// per platform: on Unix it is a socket named by `SSH_AUTH_SOCK`, while on Windows
/// OpenSSH publishes a *named pipe* and PuTTY's Pageant uses its own mechanism again.
/// Each result is boxed with `dynamic()` so callers see one type either way.
async fn connect_agent() -> anyhow::Result<AgentClient<Box<dyn AgentStream + Send + Unpin>>> {
    #[cfg(unix)]
    {
        AgentClient::connect_env()
            .await
            .map(AgentClient::dynamic)
            .context("no ssh-agent available (is SSH_AUTH_SOCK set?)")
    }

    #[cfg(windows)]
    {
        // OpenSSH for Windows publishes its agent as a named pipe at this fixed path.
        // `SSH_AUTH_SOCK`, when set on Windows at all, also names a pipe rather than a
        // socket, so it is preferred for anyone who has pointed it elsewhere.
        const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
        let pipe =
            std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| OPENSSH_AGENT_PIPE.to_string());

        let pipe_error = match AgentClient::connect_named_pipe(&pipe).await {
            Ok(agent) => return Ok(agent.dynamic()),
            Err(err) => err,
        };

        // Then Pageant, which PuTTY and WinSCP users are likely to have running instead.
        match AgentClient::connect_pageant().await {
            Ok(agent) => Ok(agent.dynamic()),
            Err(pageant_error) => Err(anyhow!(
                "no ssh-agent available: named pipe {pipe}: {pipe_error}; \
                 Pageant: {pageant_error}"
            )),
        }
    }
}

fn whoami() -> Option<String> {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
}

/// Wait for a channel's exit status, if the server sends one.
pub async fn exit_status(channel: &mut Channel<client::Msg>) -> Option<u32> {
    while let Some(msg) = channel.wait().await {
        if let ChannelMsg::ExitStatus { exit_status } = msg {
            return Some(exit_status);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_try_the_agent_before_key_files() {
        // Agent first is what makes an unlocked key "just work" without touching disk.
        let host = HostConfig {
            name: "web".into(),
            hostname: "web.example.com".into(),
            port: 22,
            user: Some("deploy".into()),
            identity_files: vec!["/home/me/.ssh/id_ed25519".into()],
        };
        let options = ConnectOptions::new(host);

        assert!(matches!(options.methods[0], AuthMethod::Agent));
        assert!(matches!(options.methods[1], AuthMethod::Key { .. }));
        // Unknown hosts are referred to the user, never accepted silently.
        assert_eq!(options.unknown_host_policy, UnknownHostPolicy::Ask);
    }

    #[test]
    fn the_known_hosts_path_can_be_overridden() {
        let previous = std::env::var_os(KNOWN_HOSTS_ENV);
        unsafe { std::env::set_var(KNOWN_HOSTS_ENV, "/tmp/catshell-test-known-hosts") };
        assert_eq!(
            known_hosts_path(),
            std::path::PathBuf::from("/tmp/catshell-test-known-hosts")
        );

        unsafe { std::env::remove_var(KNOWN_HOSTS_ENV) };
        assert!(known_hosts_path().ends_with(".ssh/known_hosts"));

        if let Some(previous) = previous {
            unsafe { std::env::set_var(KNOWN_HOSTS_ENV, previous) };
        }
    }

    #[test]
    fn methods_are_described_without_leaking_the_secret() {
        // These strings end up in error messages and logs.
        let rendered = describe(&AuthMethod::Password("hunter2".into()));
        assert_eq!(rendered, "password");
        assert!(!rendered.contains("hunter2"));
    }

    #[test]
    fn an_auth_failure_is_distinguishable_so_the_ui_can_offer_a_password() {
        let failure = ConnectError::AuthFailed {
            address: "me@host:22".into(),
            detail: "ssh-agent was rejected".into(),
        };
        assert!(failure.is_auth_failure());

        let unknown = ConnectError::UnknownHost {
            address: "me@host:22".into(),
            fingerprint: "SHA256:abc".into(),
        };
        assert!(
            !unknown.is_auth_failure(),
            "an unknown host is not a password problem"
        );
    }

    #[test]
    fn a_changed_host_key_produces_a_pointed_error() {
        let host = HostConfig {
            name: "web".into(),
            hostname: "web.example.com".into(),
            port: 22,
            user: None,
            identity_files: vec![],
        };
        let verdict = Arc::new(std::sync::Mutex::new(Some(HostKeyVerdict::Changed {
            fingerprint: "SHA256:abc".into(),
        })));
        let error = classify_connect_error(anyhow!("connection reset"), &verdict, &host);
        assert!(
            matches!(error, ConnectError::ChangedHostKey { .. }),
            "{error:?}"
        );

        let message = error.to_string();
        assert!(message.contains("CHANGED"), "{message}");
        assert!(message.contains("SHA256:abc"), "{message}");
        // It must say what to do about it, not just that it happened.
        assert!(message.contains("known_hosts"), "{message}");
    }

    #[test]
    fn an_unknown_host_error_shows_the_fingerprint_to_compare() {
        let host = HostConfig {
            name: "new".into(),
            hostname: "new.example.com".into(),
            port: 2222,
            user: Some("me".into()),
            identity_files: vec![],
        };
        let verdict = Arc::new(std::sync::Mutex::new(Some(HostKeyVerdict::Unknown {
            fingerprint: "SHA256:xyz".into(),
        })));
        let error = classify_connect_error(anyhow!("rejected"), &verdict, &host);
        assert!(
            matches!(error, ConnectError::UnknownHost { .. }),
            "{error:?}"
        );

        let message = error.to_string();
        assert!(message.contains("SHA256:xyz"), "{message}");
        assert!(message.contains("me@new.example.com:2222"), "{message}");
    }
}
