//! Where catshell gets its list of hosts.
//!
//! Two sources, deliberately. Hosts you already have in `~/.ssh/config` should not need
//! re-entering, so those are imported; hosts you define in catshell's own file are kept
//! separately so importing again never clobbers them.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ssh2_config::{ParseRule, SshConfig};

/// The default SSH port, used when neither source names one.
pub const DEFAULT_PORT: u16 = 22;

/// Everything needed to reach one host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfig {
    /// Label shown in the UI. For an imported host this is the `Host` pattern.
    pub name: String,
    /// The address to connect to.
    pub hostname: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Login user. `None` falls back to the local username at connection time.
    #[serde(default)]
    pub user: Option<String>,
    /// Private keys to try, in order, before falling back to the agent.
    #[serde(default)]
    pub identity_files: Vec<PathBuf>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl HostConfig {
    /// `user@host:port`, the form used as the keyring account and in log messages.
    pub fn address(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}:{}", self.hostname, self.port),
            None => format!("{}:{}", self.hostname, self.port),
        }
    }
}

/// Environment variable overriding which SSH config to read.
pub const SSH_CONFIG_ENV: &str = "CATSHELL_SSH_CONFIG";

/// The SSH config to import from.
///
/// `~/.ssh/config` unless [`SSH_CONFIG_ENV`] names another, which is what lets a
/// per-project config be used — and what keeps tests off the developer's real one.
pub fn ssh_config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(SSH_CONFIG_ENV) {
        return Some(PathBuf::from(path));
    }
    Some(dirs::home_dir()?.join(".ssh").join("config"))
}

/// Read the SSH config and return the hosts it names.
///
/// A missing or unreadable file yields an empty list rather than an error: not having an
/// SSH config is entirely normal, and it must not stop catshell from starting.
pub fn import_ssh_config() -> Vec<HostConfig> {
    let Some(path) = ssh_config_path() else {
        return Vec::new();
    };
    import_from(&path)
}

/// Read one SSH config file.
pub fn import_from(path: &std::path::Path) -> Vec<HostConfig> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            tracing::debug!("no usable {}: {err}", path.display());
            return Vec::new();
        }
    };

    // Unknown and unsupported keywords are tolerated. A real config is full of options
    // catshell has no opinion about, and refusing to parse the file because of one of
    // them would lose every host in it.
    let mut reader = std::io::BufReader::new(file);
    match SshConfig::default().parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS) {
        Ok(config) => hosts_from(&config),
        Err(err) => {
            tracing::warn!("could not parse {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// Extract the concrete hosts from a parsed SSH config.
pub fn hosts_from(config: &SshConfig) -> Vec<HostConfig> {
    let mut hosts = Vec::new();
    for host in config.get_hosts() {
        for clause in &host.pattern {
            // A negation subtracts from another pattern; on its own it names no host.
            if clause.negated {
                continue;
            }
            // Wildcards configure other entries rather than naming something connectable.
            if clause.pattern.contains(['*', '?']) {
                continue;
            }

            // Re-query so that settings inherited from wildcard blocks are applied,
            // which is how `Host *` defaults reach an individual host.
            let params = config.query(&clause.pattern);
            hosts.push(HostConfig {
                name: clause.pattern.clone(),
                hostname: params
                    .host_name
                    .clone()
                    .unwrap_or_else(|| clause.pattern.clone()),
                port: params.port.unwrap_or(DEFAULT_PORT),
                user: params.user.clone(),
                identity_files: params.identity_file.clone().unwrap_or_default(),
            });
        }
    }

    // The same host can be matched by several blocks; keep the first of each.
    hosts.sort_by(|a, b| a.name.cmp(&b.name));
    hosts.dedup_by(|a, b| a.name == b.name);
    hosts
}

// --- catshell's own hosts -----------------------------------------------------------

/// Environment variable overriding where catshell's own host list is kept.
pub const HOSTS_FILE_ENV: &str = "CATSHELL_HOSTS";

/// Where catshell stores the hosts you define in it.
pub fn hosts_file_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(HOSTS_FILE_ENV) {
        return Some(PathBuf::from(path));
    }
    Some(dirs::config_dir()?.join("catshell").join("hosts.toml"))
}

/// Where a host in the list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Defined in catshell, and therefore editable here.
    Own,
    /// Read from `~/.ssh/config`. Shown but not edited: that file is the user's, often
    /// hand-written and full of options catshell does not model, so rewriting it would
    /// risk losing things it does not understand.
    Imported,
}

/// A host as presented in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedHost {
    pub config: HostConfig,
    pub origin: Origin,
}

/// Why a host definition is not usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    EmptyName,
    EmptyHostname,
    /// Port 0 is not connectable.
    PortIsZero,
    /// Another host already uses this name; the name identifies it everywhere.
    DuplicateName,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Invalid::EmptyName => "give the host a name",
            Invalid::EmptyHostname => "give the host an address to connect to",
            Invalid::PortIsZero => "the port must be between 1 and 65535",
            Invalid::DuplicateName => "another host already has that name",
        };
        f.write_str(message)
    }
}

/// Check a host definition before it is saved.
///
/// `existing` is every other name already in use — the name is the identity used for
/// connection state, keyring lookups and the explorer's target, so duplicates would make
/// two hosts indistinguishable.
pub fn validate<'a>(
    host: &HostConfig,
    existing: impl IntoIterator<Item = &'a str>,
) -> Result<(), Invalid> {
    if host.name.trim().is_empty() {
        return Err(Invalid::EmptyName);
    }
    if host.hostname.trim().is_empty() {
        return Err(Invalid::EmptyHostname);
    }
    if host.port == 0 {
        return Err(Invalid::PortIsZero);
    }
    if existing.into_iter().any(|name| name == host.name) {
        return Err(Invalid::DuplicateName);
    }
    Ok(())
}

/// The hosts defined in catshell, loaded from and saved to a TOML file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostStore {
    #[serde(default, rename = "host")]
    hosts: Vec<HostConfig>,
}

impl HostStore {
    /// Load the store, falling back to an empty one.
    ///
    /// A missing file is the normal first-run case. A malformed one is reported and
    /// treated as empty rather than fatal — but is never overwritten silently, since
    /// that would discard hosts the user typed.
    pub fn load() -> Self {
        let Some(path) = hosts_file_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&text) {
            Ok(mut store) => {
                // Sorted on load as well as on edit, so a hand-written file and an
                // edited one present the list the same way.
                store.hosts.sort_by(|a, b| a.name.cmp(&b.name));
                store
            }
            Err(err) => {
                tracing::warn!("ignoring {}: {err}", path.display());
                Self::default()
            }
        }
    }

    /// Write the store back out.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = hosts_file_path()
            .ok_or_else(|| anyhow::anyhow!("no configuration directory to save hosts in"))?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    pub fn hosts(&self) -> &[HostConfig] {
        &self.hosts
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// Add a host, or replace the one previously called `replacing`.
    ///
    /// Passing the old name is what makes renaming work: the entry is found by its
    /// previous identity, not its new one.
    pub fn upsert(&mut self, host: HostConfig, replacing: Option<&str>) {
        match replacing.and_then(|name| self.hosts.iter().position(|h| h.name == name)) {
            Some(index) => self.hosts[index] = host,
            None => self.hosts.push(host),
        }
        self.hosts.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Remove a host. Returns whether there was one to remove.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.hosts.len();
        self.hosts.retain(|host| host.name != name);
        self.hosts.len() != before
    }

    pub fn get(&self, name: &str) -> Option<&HostConfig> {
        self.hosts.iter().find(|host| host.name == name)
    }

    /// Every name in use except `excluding`, for validating an edit in progress.
    pub fn names_except(&self, excluding: Option<&str>) -> Vec<&str> {
        self.hosts
            .iter()
            .map(|host| host.name.as_str())
            .filter(|name| Some(*name) != excluding)
            .collect()
    }
}

/// The sidebar's list: catshell's own hosts, then imported ones.
///
/// A name defined in both belongs to the user's own definition — they set it here
/// deliberately, and it should not be shadowed by whatever `~/.ssh/config` says.
pub fn merge(own: &[HostConfig], imported: &[HostConfig]) -> Vec<ListedHost> {
    let mut listed: Vec<ListedHost> = own
        .iter()
        .map(|config| ListedHost {
            config: config.clone(),
            origin: Origin::Own,
        })
        .collect();

    for config in imported {
        if own.iter().any(|host| host.name == config.name) {
            continue;
        }
        listed.push(ListedHost {
            config: config.clone(),
            origin: Origin::Imported,
        });
    }
    listed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<HostConfig> {
        let mut reader = std::io::BufReader::new(text.as_bytes());
        let config = SshConfig::default()
            .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
            .expect("parse");
        hosts_from(&config)
    }

    #[test]
    fn a_simple_host_is_imported() {
        let hosts = parse("Host web\n  HostName web.example.com\n  User deploy\n  Port 2222\n");
        assert_eq!(
            hosts,
            vec![HostConfig {
                name: "web".into(),
                hostname: "web.example.com".into(),
                port: 2222,
                user: Some("deploy".into()),
                identity_files: vec![],
            }]
        );
    }

    #[test]
    fn a_host_without_a_hostname_uses_its_own_name() {
        let hosts = parse("Host router\n  User admin\n");
        assert_eq!(hosts[0].hostname, "router");
        assert_eq!(hosts[0].port, DEFAULT_PORT);
    }

    #[test]
    fn wildcard_blocks_are_not_hosts_but_still_apply() {
        // `Host *` configures other entries; it is not something you can connect to.
        let hosts = parse("Host box\n  HostName box.local\n\nHost *\n  User default\n");
        assert_eq!(hosts.len(), 1, "wildcard was imported as a host: {hosts:?}");
        assert_eq!(hosts[0].name, "box");
        // But its settings are inherited.
        assert_eq!(hosts[0].user.as_deref(), Some("default"));
    }

    #[test]
    fn the_first_matching_block_wins() {
        // OpenSSH uses the first value it obtains for each parameter, which is why
        // `Host *` defaults belong at the end of a config. Both orderings are exercised
        // so the import cannot silently disagree with what `ssh` itself would do.
        let specific_first = parse("Host box\n  User specific\n\nHost *\n  User default\n");
        assert_eq!(specific_first[0].user.as_deref(), Some("specific"));

        let wildcard_first = parse("Host *\n  User default\n\nHost box\n  User specific\n");
        assert_eq!(wildcard_first[0].user.as_deref(), Some("default"));
    }

    #[test]
    fn several_patterns_on_one_line_become_several_hosts() {
        let hosts = parse("Host alpha beta\n  User shared\n");
        let names: Vec<&str> = hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(hosts.iter().all(|h| h.user.as_deref() == Some("shared")));
    }

    #[test]
    fn negated_patterns_are_not_hosts() {
        let hosts = parse("Host * !secret\n  User default\n");
        assert!(
            hosts.is_empty(),
            "negated or wildcard pattern imported: {hosts:?}"
        );
    }

    #[test]
    fn identity_files_are_carried_over() {
        let hosts = parse("Host key\n  HostName k.example.com\n  IdentityFile ~/.ssh/id_ed25519\n");
        assert_eq!(hosts[0].identity_files.len(), 1);
    }

    #[test]
    fn an_empty_config_yields_no_hosts() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn unknown_keywords_do_not_lose_the_host() {
        // Real configs contain plenty catshell has no opinion about; one of them must
        // not cost us the whole file.
        let hosts =
            parse("Host box\n  HostName box.local\n  SomeFutureOption yes\n  ControlMaster auto\n");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].hostname, "box.local");
    }

    #[test]
    fn the_config_path_can_be_overridden() {
        // Used by tests to stay off the developer's real config, and by anyone keeping
        // a per-project one.
        let previous = std::env::var_os(SSH_CONFIG_ENV);
        unsafe { std::env::set_var(SSH_CONFIG_ENV, "/tmp/catshell-test-config") };
        assert_eq!(
            ssh_config_path(),
            Some(PathBuf::from("/tmp/catshell-test-config"))
        );

        unsafe { std::env::remove_var(SSH_CONFIG_ENV) };
        assert!(
            ssh_config_path().is_none_or(|path| path.ends_with(".ssh/config")),
            "the default is not ~/.ssh/config"
        );

        if let Some(previous) = previous {
            unsafe { std::env::set_var(SSH_CONFIG_ENV, previous) };
        }
    }

    #[test]
    fn a_missing_config_file_is_not_an_error() {
        assert!(import_from(std::path::Path::new("/nonexistent/catshell/config")).is_empty());
    }

    #[test]
    fn importing_reads_hosts_from_a_real_file() {
        let path = std::env::temp_dir().join("catshell-import-test-config");
        std::fs::write(
            &path,
            "Host imported\n  HostName imported.example.com\n  Port 2200\n",
        )
        .unwrap();

        let hosts = import_from(&path);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].hostname, "imported.example.com");
        assert_eq!(hosts[0].port, 2200);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn addresses_are_formatted_for_display_and_lookup() {
        let host = HostConfig {
            name: "web".into(),
            hostname: "web.example.com".into(),
            port: 2222,
            user: Some("deploy".into()),
            identity_files: vec![],
        };
        assert_eq!(host.address(), "deploy@web.example.com:2222");

        let anonymous = HostConfig { user: None, ..host };
        assert_eq!(anonymous.address(), "web.example.com:2222");
    }

    // --- catshell's own hosts ------------------------------------------------------

    fn host(name: &str) -> HostConfig {
        HostConfig {
            name: name.into(),
            hostname: format!("{name}.example.com"),
            port: 22,
            user: Some("me".into()),
            identity_files: vec![],
        }
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("catshell-hosts-{name}.toml"));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_store_round_trips_through_its_file() {
        // This is user-typed data; losing it on a save/load cycle would be the worst
        // possible bug in a host editor.
        let file = TempFile::new("roundtrip");
        let mut store = HostStore::default();
        store.upsert(
            HostConfig {
                name: "web".into(),
                hostname: "web.example.com".into(),
                port: 2222,
                user: Some("deploy".into()),
                identity_files: vec!["/home/me/.ssh/id_ed25519".into()],
            },
            None,
        );
        store.save_to(&file.0).unwrap();

        let loaded = HostStore::load_from(&file.0);
        assert_eq!(loaded.hosts(), store.hosts());
    }

    #[test]
    fn a_missing_store_is_empty_rather_than_an_error() {
        let store = HostStore::load_from(std::path::Path::new("/nonexistent/catshell/hosts.toml"));
        assert!(store.is_empty());
    }

    #[test]
    fn a_malformed_store_does_not_take_the_app_down() {
        let file = TempFile::new("malformed");
        std::fs::write(&file.0, "this is not toml {{{").unwrap();
        assert!(HostStore::load_from(&file.0).is_empty());
    }

    #[test]
    fn a_hand_written_file_is_sorted_on_load() {
        let file = TempFile::new("sorted");
        std::fs::write(
            &file.0,
            "[[host]]\nname = \"zulu\"\nhostname = \"z\"\n\n[[host]]\nname = \"alpha\"\nhostname = \"a\"\n",
        )
        .unwrap();

        let store = HostStore::load_from(&file.0);
        let names: Vec<&str> = store.hosts().iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zulu"]);
    }

    #[test]
    fn adding_keeps_the_list_sorted() {
        let mut store = HostStore::default();
        store.upsert(host("zulu"), None);
        store.upsert(host("alpha"), None);
        store.upsert(host("mike"), None);

        let names: Vec<&str> = store.hosts().iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn editing_replaces_rather_than_duplicates() {
        let mut store = HostStore::default();
        store.upsert(host("web"), None);

        let mut edited = host("web");
        edited.port = 2222;
        store.upsert(edited, Some("web"));

        assert_eq!(store.hosts().len(), 1, "editing created a second entry");
        assert_eq!(store.get("web").unwrap().port, 2222);
    }

    #[test]
    fn renaming_finds_the_entry_by_its_old_name() {
        // The edit dialog changes the name, so the entry has to be located by what it
        // was called before, not what it is called now.
        let mut store = HostStore::default();
        store.upsert(host("old-name"), None);

        let mut renamed = host("new-name");
        renamed.port = 2022;
        store.upsert(renamed, Some("old-name"));

        assert_eq!(store.hosts().len(), 1, "renaming left the old entry behind");
        assert!(store.get("old-name").is_none());
        assert_eq!(store.get("new-name").unwrap().port, 2022);
    }

    #[test]
    fn removing_reports_whether_anything_went() {
        let mut store = HostStore::default();
        store.upsert(host("web"), None);

        assert!(store.remove("web"));
        assert!(store.is_empty());
        assert!(
            !store.remove("web"),
            "removing a missing host claimed success"
        );
    }

    #[test]
    fn validation_rejects_definitions_that_cannot_connect() {
        let mut empty_name = host("web");
        empty_name.name = "   ".into();
        assert_eq!(validate(&empty_name, []), Err(Invalid::EmptyName));

        let mut empty_hostname = host("web");
        empty_hostname.hostname = String::new();
        assert_eq!(validate(&empty_hostname, []), Err(Invalid::EmptyHostname));

        let mut zero_port = host("web");
        zero_port.port = 0;
        assert_eq!(validate(&zero_port, []), Err(Invalid::PortIsZero));
    }

    #[test]
    fn validation_rejects_a_duplicate_name() {
        // The name is the identity used for connection state, keyring lookups and the
        // explorer's target; two hosts sharing one would be indistinguishable.
        assert_eq!(validate(&host("web"), ["web"]), Err(Invalid::DuplicateName));
        assert_eq!(validate(&host("web"), ["other"]), Ok(()));
    }

    #[test]
    fn editing_a_host_does_not_collide_with_itself() {
        let mut store = HostStore::default();
        store.upsert(host("web"), None);
        store.upsert(host("db"), None);

        // Saving "web" unchanged must not be rejected for clashing with "web".
        assert_eq!(
            validate(&host("web"), store.names_except(Some("web"))),
            Ok(())
        );
        // But renaming it to an existing name must be.
        let mut renamed = host("db");
        renamed.name = "db".into();
        assert_eq!(
            validate(&renamed, store.names_except(Some("web"))),
            Err(Invalid::DuplicateName)
        );
    }

    #[test]
    fn every_validation_failure_says_what_to_do() {
        for problem in [
            Invalid::EmptyName,
            Invalid::EmptyHostname,
            Invalid::PortIsZero,
            Invalid::DuplicateName,
        ] {
            let message = problem.to_string();
            assert!(!message.is_empty(), "{problem:?} has no message");
            assert!(
                message.chars().next().unwrap().is_lowercase(),
                "{problem:?} reads as a sentence fragment, not advice: {message}"
            );
        }
    }

    #[test]
    fn the_merged_list_shows_both_sources() {
        let own = vec![host("mine")];
        let imported = vec![host("theirs")];
        let listed = merge(&own, &imported);

        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].origin, Origin::Own);
        assert_eq!(listed[1].origin, Origin::Imported);
    }

    #[test]
    fn a_host_defined_in_catshell_shadows_the_imported_one() {
        // The user set it here on purpose; ~/.ssh/config must not override that.
        let mut mine = host("web");
        mine.port = 2222;
        let mut theirs = host("web");
        theirs.port = 22;

        let listed = merge(&[mine], &[theirs]);
        assert_eq!(listed.len(), 1, "the same host appeared twice");
        assert_eq!(listed[0].origin, Origin::Own);
        assert_eq!(listed[0].config.port, 2222);
    }

    #[test]
    fn the_store_path_can_be_overridden() {
        let previous = std::env::var_os(HOSTS_FILE_ENV);
        unsafe { std::env::set_var(HOSTS_FILE_ENV, "/tmp/catshell-test-hosts.toml") };
        assert_eq!(
            hosts_file_path(),
            Some(PathBuf::from("/tmp/catshell-test-hosts.toml"))
        );

        unsafe { std::env::remove_var(HOSTS_FILE_ENV) };
        assert!(hosts_file_path().is_none_or(|path| path.ends_with("catshell/hosts.toml")));

        if let Some(previous) = previous {
            unsafe { std::env::set_var(HOSTS_FILE_ENV, previous) };
        }
    }
}
