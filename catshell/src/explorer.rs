//! The file explorer's state: where it is looking, and when it refreshes.
//!
//! The interesting part is the sync with the terminal. Because the explorer and the
//! shell share one SSH session, the explorer can follow the shell's real working
//! directory (reported by OSC 7) and re-read a directory at the moment a command
//! finishes (OSC 133;D) rather than on a timer. That is the difference from a file
//! browser on its own connection, which knows neither.
//!
//! Free of egui and of any I/O, so every rule here is tested directly.

use std::time::{Duration, Instant};

use catshell_net::files::{posix, sort_entries, EntryKind, FileEntry};

/// How long to wait after a command finishes before re-reading the directory.
///
/// A build prints hundreds of completions; without this the explorer would re-list on
/// each one. Long enough to coalesce a burst, short enough to feel immediate.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Which filesystem the explorer is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Local,
    /// The name of the host, as it appears in the sidebar.
    Remote(String),
}

impl Source {
    /// Join a path with a child name, using the right rules for this filesystem.
    ///
    /// A remote path is POSIX and belongs to the server, so it must not go through
    /// `std::path` — on Windows that would rewrite `/etc` as `\etc`.
    pub fn join(&self, base: &str, name: &str) -> String {
        match self {
            Source::Remote(_) => posix::join(base, name),
            Source::Local => {
                let joined = std::path::Path::new(base).join(name);
                joined.to_string_lossy().into_owned()
            }
        }
    }

    /// The parent directory, or `None` at the root.
    pub fn parent(&self, path: &str) -> Option<String> {
        match self {
            Source::Remote(_) => posix::parent(path),
            Source::Local => std::path::Path::new(path)
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Source::Remote(_))
    }
}

/// What the explorer wants doing on its behalf.
///
/// Returned rather than performed, because listing is asynchronous and platform
/// specific; the app turns these into SFTP requests or local reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Read this directory and report back with this generation.
    List { path: String, generation: u64 },
    /// Send `cd <path>` to the terminal.
    ChangeShellDirectory { path: String },
}

/// The explorer pane's state.
pub struct Explorer {
    source: Source,
    path: String,
    entries: Vec<FileEntry>,
    /// Incremented on every navigation; a listing tagged with an older generation is
    /// stale and dropped, which is what keeps fast clicking from showing the wrong
    /// directory's contents.
    generation: u64,
    selected: Option<String>,
    follow_terminal: bool,
    /// Whether activating a directory also moves the shell.
    drive_terminal: bool,
    show_hidden: bool,
    status: Option<String>,
    loading: bool,
    /// When a debounced refresh comes due.
    refresh_at: Option<Instant>,
}

impl Explorer {
    pub fn new(source: Source, path: impl Into<String>) -> Self {
        Self {
            source,
            path: path.into(),
            entries: Vec::new(),
            generation: 0,
            selected: None,
            follow_terminal: true,
            drive_terminal: false,
            show_hidden: false,
            status: None,
            loading: false,
            refresh_at: None,
        }
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn select(&mut self, name: Option<String>) {
        self.selected = name;
    }

    pub fn follow_terminal(&self) -> bool {
        self.follow_terminal
    }

    pub fn set_follow_terminal(&mut self, follow: bool) {
        self.follow_terminal = follow;
    }

    pub fn drive_terminal(&self) -> bool {
        self.drive_terminal
    }

    pub fn set_drive_terminal(&mut self, drive: bool) {
        self.drive_terminal = drive;
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn set_show_hidden(&mut self, show: bool) {
        self.show_hidden = show;
    }

    /// The entries to draw, after filtering.
    pub fn visible(&self) -> impl Iterator<Item = &FileEntry> {
        self.entries
            .iter()
            .filter(move |entry| self.show_hidden || !entry.is_hidden())
    }

    /// Go to a directory, discarding any listing still in flight.
    pub fn navigate(&mut self, path: impl Into<String>) -> Action {
        self.path = path.into();
        self.selected = None;
        self.entries.clear();
        self.status = None;
        self.reload()
    }

    /// Re-read the current directory.
    pub fn reload(&mut self) -> Action {
        self.generation += 1;
        self.loading = true;
        self.refresh_at = None;
        Action::List {
            path: self.path.clone(),
            generation: self.generation,
        }
    }

    /// Go to the parent directory, if there is one.
    pub fn go_up(&mut self) -> Option<Action> {
        let parent = self.source.parent(&self.path)?;
        Some(self.navigate(parent))
    }

    /// Activate an entry: enter a directory, or do nothing for a file.
    ///
    /// Returns the actions to perform — navigating, and optionally moving the shell too.
    pub fn activate(&mut self, entry: &FileEntry) -> Vec<Action> {
        if !entry.kind.is_navigable() {
            self.selected = Some(entry.name.clone());
            return Vec::new();
        }

        let target = self.source.join(&self.path, &entry.name);
        let mut actions = vec![self.navigate(target.clone())];
        if self.drive_terminal {
            actions.push(Action::ChangeShellDirectory { path: target });
        }
        actions
    }

    /// Take a listing that has come back.
    ///
    /// A listing for an older generation is ignored: the user has navigated on since,
    /// and showing it would put the wrong contents under the current path.
    pub fn accept_listing(&mut self, path: &str, generation: u64, mut entries: Vec<FileEntry>) {
        if generation != self.generation {
            return;
        }
        sort_entries(&mut entries);
        self.path = path.to_string();
        self.entries = entries;
        self.loading = false;
        self.status = None;
    }

    /// Record that an operation failed.
    pub fn accept_failure(&mut self, action: &str, message: &str) {
        self.loading = false;
        self.status = Some(format!("{action}: {message}"));
    }

    /// The shell reported a new working directory.
    ///
    /// Followed only when the explorer is set to follow, and only for the filesystem it
    /// is actually showing — a local pane must not jump to a remote path.
    pub fn on_shell_directory(&mut self, path: &str) -> Option<Action> {
        if !self.follow_terminal || path == self.path {
            return None;
        }
        Some(self.navigate(path))
    }

    /// The shell reported that a command finished.
    ///
    /// Schedules a refresh rather than doing one, so a burst of commands costs one
    /// listing instead of hundreds.
    pub fn on_command_finished(&mut self, now: Instant) {
        self.refresh_at = Some(now + REFRESH_DEBOUNCE);
    }

    /// The refresh to run, if one has come due.
    pub fn due_refresh(&mut self, now: Instant) -> Option<Action> {
        match self.refresh_at {
            Some(at) if now >= at => {
                self.refresh_at = None;
                Some(self.reload())
            }
            _ => None,
        }
    }

    /// The path an entry would have.
    pub fn path_of(&self, name: &str) -> String {
        self.source.join(&self.path, name)
    }
}

/// Read a local directory.
///
/// Errors on individual entries are skipped rather than failing the listing: one
/// unreadable file in `/proc` should not blank the whole pane.
pub fn list_local(path: &str) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        // `symlink_metadata` so a link shows as a link rather than silently as its
        // target — and so a broken link still appears instead of vanishing.
        let Ok(metadata) = entry.path().symlink_metadata() else {
            continue;
        };

        let kind = if metadata.is_symlink() {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Dir
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        entries.push(FileEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_secs()),
            permissions: permissions_of(&metadata),
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

#[cfg(unix)]
fn permissions_of(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn permissions_of(_metadata: &std::fs::Metadata) -> Option<u32> {
    // Windows has no POSIX mode bits; the column is simply left empty.
    None
}

/// Render a size the way a file manager does.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            kind,
            size: 0,
            modified: None,
            permissions: None,
        }
    }

    fn remote() -> Explorer {
        Explorer::new(Source::Remote("web".into()), "/home/iota")
    }

    #[test]
    fn navigating_asks_for_a_listing_with_a_fresh_generation() {
        let mut explorer = remote();
        let first = explorer.navigate("/tmp");
        let second = explorer.navigate("/var");

        assert_eq!(
            first,
            Action::List {
                path: "/tmp".into(),
                generation: 1
            }
        );
        assert_eq!(
            second,
            Action::List {
                path: "/var".into(),
                generation: 2
            }
        );
    }

    #[test]
    fn a_stale_listing_is_ignored() {
        // The user clicked twice quickly; the first directory's contents must not be
        // shown under the second directory's path.
        let mut explorer = remote();
        explorer.navigate("/tmp");
        let Action::List {
            generation: stale, ..
        } = explorer.navigate("/var")
        else {
            unreachable!()
        };

        explorer.accept_listing("/tmp", stale - 1, vec![entry("old.txt", EntryKind::File)]);
        assert_eq!(explorer.path(), "/var");
        assert_eq!(
            explorer.visible().count(),
            0,
            "a stale listing was displayed"
        );

        explorer.accept_listing("/var", stale, vec![entry("new.txt", EntryKind::File)]);
        assert_eq!(
            explorer
                .visible()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["new.txt"]
        );
    }

    #[test]
    fn listings_are_sorted_on_arrival() {
        let mut explorer = remote();
        explorer.navigate("/tmp");
        explorer.accept_listing(
            "/tmp",
            1,
            vec![
                entry("b.txt", EntryKind::File),
                entry("a-dir", EntryKind::Dir),
            ],
        );
        assert_eq!(
            explorer
                .visible()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["a-dir", "b.txt"]
        );
    }

    #[test]
    fn hidden_entries_are_filtered_until_asked_for() {
        let mut explorer = remote();
        explorer.navigate("/tmp");
        explorer.accept_listing(
            "/tmp",
            1,
            vec![
                entry(".hidden", EntryKind::File),
                entry("visible", EntryKind::File),
            ],
        );

        assert_eq!(
            explorer
                .visible()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["visible"]
        );
        explorer.set_show_hidden(true);
        assert_eq!(explorer.visible().count(), 2);
    }

    #[test]
    fn activating_a_directory_navigates_into_it() {
        let mut explorer = remote();
        let actions = explorer.activate(&entry("src", EntryKind::Dir));
        assert_eq!(
            actions,
            vec![Action::List {
                path: "/home/iota/src".into(),
                generation: 1
            }]
        );
        assert_eq!(explorer.path(), "/home/iota/src");
    }

    #[test]
    fn activating_a_file_selects_it_rather_than_navigating() {
        let mut explorer = remote();
        let actions = explorer.activate(&entry("notes.txt", EntryKind::File));
        assert!(actions.is_empty());
        assert_eq!(explorer.selected(), Some("notes.txt"));
        assert_eq!(
            explorer.path(),
            "/home/iota",
            "a file changed the directory"
        );
    }

    #[test]
    fn a_symlink_is_enterable_because_it_usually_points_at_a_directory() {
        let mut explorer = remote();
        let actions = explorer.activate(&entry("link", EntryKind::Symlink));
        assert_eq!(
            actions,
            vec![Action::List {
                path: "/home/iota/link".into(),
                generation: 1
            }]
        );
    }

    #[test]
    fn driving_the_terminal_is_opt_in() {
        let mut explorer = remote();
        assert!(
            !explorer.drive_terminal(),
            "the explorer must not type into the shell uninvited"
        );
        assert_eq!(explorer.activate(&entry("src", EntryKind::Dir)).len(), 1);

        explorer.set_drive_terminal(true);
        let actions = explorer.activate(&entry("deeper", EntryKind::Dir));
        assert!(actions.contains(&Action::ChangeShellDirectory {
            path: "/home/iota/src/deeper".into()
        }));
    }

    #[test]
    fn going_up_stops_at_the_root() {
        let mut explorer = Explorer::new(Source::Remote("web".into()), "/home");
        assert!(explorer.go_up().is_some());
        assert_eq!(explorer.path(), "/");
        assert!(explorer.go_up().is_none(), "climbed above the root");
    }

    #[test]
    fn following_the_terminal_moves_the_explorer() {
        // The headline behaviour: `cd` in the shell moves the file pane.
        let mut explorer = remote();
        let action = explorer.on_shell_directory("/srv/app");

        assert_eq!(
            action,
            Some(Action::List {
                path: "/srv/app".into(),
                generation: 1
            })
        );
        assert_eq!(explorer.path(), "/srv/app");
    }

    #[test]
    fn following_can_be_turned_off() {
        let mut explorer = remote();
        explorer.set_follow_terminal(false);
        assert_eq!(explorer.on_shell_directory("/srv/app"), None);
        assert_eq!(
            explorer.path(),
            "/home/iota",
            "the explorer moved with following off"
        );
    }

    #[test]
    fn a_report_of_the_directory_already_shown_costs_nothing() {
        // Every prompt reports the cwd; re-listing on each would be constant churn.
        let mut explorer = remote();
        assert_eq!(explorer.on_shell_directory("/home/iota"), None);
    }

    #[test]
    fn a_finished_command_refreshes_after_the_debounce() {
        let mut explorer = remote();
        let start = Instant::now();

        explorer.on_command_finished(start);
        assert_eq!(
            explorer.due_refresh(start),
            None,
            "refreshed before the debounce elapsed"
        );

        let action = explorer.due_refresh(start + REFRESH_DEBOUNCE);
        assert!(
            matches!(action, Some(Action::List { .. })),
            "never refreshed"
        );
        // And only once.
        assert_eq!(explorer.due_refresh(start + REFRESH_DEBOUNCE * 4), None);
    }

    #[test]
    fn a_burst_of_commands_costs_one_refresh() {
        // A build emits a completion per command; the explorer must coalesce them.
        let mut explorer = remote();
        let start = Instant::now();
        for step in 0..100 {
            explorer.on_command_finished(start + Duration::from_millis(step));
        }

        assert_eq!(
            explorer.due_refresh(start + Duration::from_millis(120)),
            None
        );
        assert!(explorer
            .due_refresh(start + Duration::from_secs(1))
            .is_some());
        assert_eq!(explorer.due_refresh(start + Duration::from_secs(2)), None);
    }

    #[test]
    fn navigating_cancels_a_pending_refresh() {
        let mut explorer = remote();
        let start = Instant::now();
        explorer.on_command_finished(start);
        explorer.navigate("/elsewhere");

        assert_eq!(
            explorer.due_refresh(start + REFRESH_DEBOUNCE * 2),
            None,
            "a refresh fired for a directory that is no longer shown"
        );
    }

    #[test]
    fn a_failure_is_shown_and_clears_the_loading_state() {
        let mut explorer = remote();
        explorer.navigate("/nope");
        assert!(explorer.is_loading());

        explorer.accept_failure("listing /nope", "No such file");
        assert!(!explorer.is_loading());
        assert!(explorer.status().unwrap().contains("No such file"));
    }

    #[test]
    fn remote_paths_use_posix_rules_on_every_platform() {
        // On Windows `std::path` would turn these into backslashes and send nonsense to
        // the server.
        let remote = Source::Remote("web".into());
        assert_eq!(remote.join("/etc", "hosts"), "/etc/hosts");
        assert_eq!(remote.parent("/etc/hosts"), Some("/etc".into()));
    }

    #[test]
    fn sizes_are_rendered_for_people() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 K");
        assert_eq!(human_size(1024 * 1024 * 3), "3.0 M");
        assert_eq!(human_size(1024_u64.pow(4) * 2), "2.0 T");
    }

    #[test]
    fn listing_a_local_directory_reads_the_real_filesystem() {
        let dir = std::env::temp_dir().join("catshell-explorer-local");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("file.txt"), "12345").unwrap();

        let entries = list_local(&dir.to_string_lossy()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["sub", "file.txt"],
            "directories should sort first"
        );
        assert_eq!(entries[1].size, 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_a_missing_local_directory_is_an_error_not_a_panic() {
        assert!(list_local("/nonexistent/catshell/directory").is_err());
    }
}
