//! Passwords and passphrases, kept in the platform's own credential store.
//!
//! Nothing secret goes in catshell's config file. Keys come from an agent or from disk;
//! only what has to be typed — a password, a key passphrase — is stored here, and only
//! when the user asks for it to be remembered.
//!
//! Every operation degrades to `None` rather than failing. A Linux box with no Secret
//! Service running is ordinary, and it must mean "you will be asked each time", not
//! "you cannot connect".

const SERVICE: &str = "catshell";

/// Look up a stored secret for `account`, typically `user@host:port`.
pub fn get(account: &str) -> Option<String> {
    match keyring::Entry::new(SERVICE, account) {
        Ok(entry) => match entry.get_password() {
            Ok(password) => Some(password),
            Err(keyring::Error::NoEntry) => None,
            Err(err) => {
                tracing::debug!("no stored secret for {account}: {err}");
                None
            }
        },
        Err(err) => {
            tracing::debug!("credential store unavailable: {err}");
            None
        }
    }
}

/// Store a secret, replacing any previous one.
///
/// Returns whether it was actually saved, so the UI can tell the user their "remember
/// this" did not take effect rather than silently losing it.
pub fn set(account: &str, secret: &str) -> bool {
    match keyring::Entry::new(SERVICE, account).and_then(|entry| entry.set_password(secret)) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("could not store secret for {account}: {err}");
            false
        }
    }
}

/// Forget a stored secret. Succeeds if there was nothing to forget.
pub fn delete(account: &str) -> bool {
    match keyring::Entry::new(SERVICE, account).and_then(|entry| entry.delete_credential()) {
        Ok(()) => true,
        Err(keyring::Error::NoEntry) => true,
        Err(err) => {
            tracing::warn!("could not delete secret for {account}: {err}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_secret_is_none_not_a_failure() {
        // Also covers the machine having no credential store at all, which is the
        // normal case in a container or a bare WSL install.
        assert_eq!(get("catshell-test-definitely-not-present"), None);
    }

    #[test]
    fn deleting_something_absent_is_not_an_error() {
        assert!(delete("catshell-test-definitely-not-present"));
    }
}
