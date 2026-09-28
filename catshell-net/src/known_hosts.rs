//! Deciding whether to trust a server's key.
//!
//! This is the step that makes SSH resistant to a machine-in-the-middle, so it is kept
//! apart from the connection code and given its own tests. The rule catshell follows is
//! OpenSSH's: a key that matches `~/.ssh/known_hosts` is accepted, an unknown host is
//! referred to the user, and a key that *changed* is refused outright.

use russh::keys::known_hosts::check_known_hosts_path;
use russh::keys::PublicKey;

/// What to do about a server key catshell has not seen before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownHostPolicy {
    /// Refuse. The safe default for anything unattended.
    Reject,
    /// Accept and remember, as `StrictHostKeyChecking no` does.
    Accept,
    /// Ask the user.
    Ask,
}

/// The verdict on a server key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyVerdict {
    /// The key matches the one on record.
    Known,
    /// The host is new. Carries the fingerprint to show the user.
    Unknown { fingerprint: String },
    /// A key is on record and it is *not* this one.
    ///
    /// Never resolve this by overwriting the record without the user saying so: it is
    /// what a machine-in-the-middle looks like.
    Changed { fingerprint: String },
}

/// Check a server key against a `known_hosts` file.
pub fn verify(
    host: &str,
    port: u16,
    key: &PublicKey,
    known_hosts: &std::path::Path,
) -> HostKeyVerdict {
    let fingerprint = key.fingerprint(Default::default()).to_string();

    match check_known_hosts_path(host, port, key, known_hosts) {
        Ok(true) => HostKeyVerdict::Known,
        Ok(false) => HostKeyVerdict::Unknown { fingerprint },
        // A missing file simply means nothing is known yet.
        Err(russh::keys::Error::IO(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            HostKeyVerdict::Unknown { fingerprint }
        }
        Err(russh::keys::Error::KeyChanged { .. }) => HostKeyVerdict::Changed { fingerprint },
        Err(err) => {
            // Anything else — an unreadable or malformed file — is treated as "not
            // known". Failing open here would defeat the point of checking at all.
            tracing::warn!("could not read {}: {err}", known_hosts.display());
            HostKeyVerdict::Unknown { fingerprint }
        }
    }
}

/// Record a host key as trusted.
pub fn learn(
    host: &str,
    port: u16,
    key: &PublicKey,
    known_hosts: &std::path::Path,
) -> Result<(), russh::keys::Error> {
    if let Some(parent) = known_hosts.parent() {
        std::fs::create_dir_all(parent)?;
    }
    russh::keys::known_hosts::learn_known_hosts_path(host, port, key, known_hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::PrivateKey;

    struct Fixture {
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("catshell-known-hosts-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn path(&self) -> std::path::PathBuf {
            self.dir.join("known_hosts")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A key derived deterministically from , so a test can produce the same key
    /// twice or two reliably different ones.
    fn key(seed: u8) -> PublicKey {
        use rand::SeedableRng as _;
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(u64::from(seed));
        PrivateKey::random(&mut rng, russh::keys::Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .clone()
    }

    #[test]
    fn an_unseen_host_is_unknown() {
        let fixture = Fixture::new("unseen");
        let verdict = verify("example.com", 22, &key(1), &fixture.path());
        assert!(
            matches!(verdict, HostKeyVerdict::Unknown { .. }),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_learned_host_is_known_afterwards() {
        let fixture = Fixture::new("learn");
        let key = key(2);
        learn("example.com", 22, &key, &fixture.path()).unwrap();
        assert_eq!(
            verify("example.com", 22, &key, &fixture.path()),
            HostKeyVerdict::Known
        );
    }

    #[test]
    fn a_changed_key_is_reported_as_changed_not_unknown() {
        // The distinction that matters: an unknown host is a prompt, a changed key is a
        // refusal, because it is what an interception looks like.
        let fixture = Fixture::new("changed");
        learn("example.com", 22, &key(3), &fixture.path()).unwrap();

        let verdict = verify("example.com", 22, &key(4), &fixture.path());
        assert!(
            matches!(verdict, HostKeyVerdict::Changed { .. }),
            "{verdict:?}"
        );
    }

    #[test]
    fn the_port_is_part_of_the_identity() {
        let fixture = Fixture::new("port");
        let key = key(5);
        learn("example.com", 2222, &key, &fixture.path()).unwrap();

        assert_eq!(
            verify("example.com", 2222, &key, &fixture.path()),
            HostKeyVerdict::Known
        );
        // The same host on the default port is a different entry.
        let other = verify("example.com", 22, &key, &fixture.path());
        assert!(matches!(other, HostKeyVerdict::Unknown { .. }), "{other:?}");
    }

    #[test]
    fn learning_creates_the_directory() {
        let fixture = Fixture::new("mkdir");
        let nested = fixture.dir.join("nested").join("known_hosts");
        learn("example.com", 22, &key(6), &nested).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn a_verdict_carries_a_fingerprint_to_show_the_user() {
        let fixture = Fixture::new("fingerprint");
        let HostKeyVerdict::Unknown { fingerprint } =
            verify("example.com", 22, &key(7), &fixture.path())
        else {
            panic!("expected an unknown host");
        };
        // OpenSSH's format, so it can be compared with what `ssh-keygen -lf` prints.
        assert!(
            fingerprint.starts_with("SHA256:"),
            "unexpected fingerprint {fingerprint}"
        );
    }
}
