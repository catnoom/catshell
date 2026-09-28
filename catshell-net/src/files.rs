//! The file model the explorer shows, and the path arithmetic behind it.
//!
//! Shared by the local and remote sides so the explorer has one kind of entry to render.
//! It lives here, beside the SFTP client, because the remote side is the constraining
//! one: a remote path is a *POSIX* path belonging to the server, and it must not be run
//! through `std::path`, which would rewrite separators on Windows and quietly turn
//! `/etc/hosts` into `\etc\hosts`.

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl EntryKind {
    /// Whether activating this entry navigates into it.
    ///
    /// Symlinks count: the explorer resolves them when entered, and a link to a
    /// directory is the common case.
    pub fn is_navigable(self) -> bool {
        matches!(self, EntryKind::Dir | EntryKind::Symlink)
    }
}

/// One entry in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Modification time in seconds since the Unix epoch, when the source reports one.
    pub modified: Option<u64>,
    /// POSIX mode bits, when the source reports them.
    pub permissions: Option<u32>,
}

impl FileEntry {
    /// Whether the entry is hidden by convention (a leading dot).
    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }
}

/// Sort a listing the way a file manager should: directories first, then by name,
/// case-insensitively.
pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        let a_dir = a.kind.is_navigable();
        let b_dir = b.kind.is_navigable();
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// POSIX path arithmetic, for paths that belong to a remote host.
pub mod posix {
    /// Join a path with a child name.
    pub fn join(base: &str, name: &str) -> String {
        if name.starts_with('/') {
            return normalize(name);
        }
        if base.is_empty() || base == "/" {
            normalize(&format!("/{name}"))
        } else {
            normalize(&format!("{}/{name}", base.trim_end_matches('/')))
        }
    }

    /// The parent of a path, or `None` at the root.
    pub fn parent(path: &str) -> Option<String> {
        let path = normalize(path);
        if path == "/" {
            return None;
        }
        match path.rfind('/') {
            Some(0) => Some("/".to_string()),
            Some(index) => Some(path[..index].to_string()),
            None => None,
        }
    }

    /// The final component of a path.
    pub fn file_name(path: &str) -> &str {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(index) => &trimmed[index + 1..],
            None => trimmed,
        }
    }

    /// Collapse `.`, `..` and repeated separators.
    ///
    /// Done textually and without touching the filesystem, which is the only option for
    /// a path on another machine.
    pub fn normalize(path: &str) -> String {
        let absolute = path.starts_with('/');
        let mut parts: Vec<&str> = Vec::new();

        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    // Climbing above the root just stays at the root, as it does in a
                    // real filesystem.
                    if matches!(parts.last(), Some(&"..")) || (!absolute && parts.is_empty()) {
                        parts.push("..");
                    } else {
                        parts.pop();
                    }
                }
                part => parts.push(part),
            }
        }

        let joined = parts.join("/");
        if absolute {
            format!("/{joined}")
        } else if joined.is_empty() {
            ".".to_string()
        } else {
            joined
        }
    }

    /// Each ancestor of a path with its display name, root first.
    ///
    /// Used to render a breadcrumb the user can click through.
    pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
        let path = normalize(path);
        let mut crumbs = vec![("/".to_string(), "/".to_string())];
        if path == "/" {
            return crumbs;
        }

        let mut current = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            current.push('/');
            current.push_str(part);
            crumbs.push((current.clone(), part.to_string()));
        }
        crumbs
    }
}

#[cfg(test)]
mod tests {
    use super::posix::*;
    use super::*;

    #[test]
    fn joining_builds_child_paths() {
        assert_eq!(join("/home/iota", "src"), "/home/iota/src");
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("", "etc"), "/etc");
        // A trailing separator on the base must not double up.
        assert_eq!(join("/home/", "iota"), "/home/iota");
        // An absolute name replaces the base entirely, as `cd /tmp` does.
        assert_eq!(join("/home/iota", "/tmp"), "/tmp");
    }

    #[test]
    fn joining_resolves_dot_dot() {
        assert_eq!(join("/home/iota", ".."), "/home");
        assert_eq!(join("/home/iota", "../other"), "/home/other");
    }

    #[test]
    fn parents_walk_up_to_the_root_and_stop() {
        assert_eq!(parent("/home/iota/src"), Some("/home/iota".to_string()));
        assert_eq!(parent("/home"), Some("/".to_string()));
        assert_eq!(parent("/"), None, "the root has no parent to climb to");
    }

    #[test]
    fn file_names_are_the_last_component() {
        assert_eq!(file_name("/home/iota/notes.txt"), "notes.txt");
        assert_eq!(file_name("/home/iota/"), "iota");
        assert_eq!(file_name("/"), "");
    }

    #[test]
    fn normalizing_collapses_noise() {
        assert_eq!(normalize("/home//iota/./src"), "/home/iota/src");
        assert_eq!(normalize("/home/iota/../other"), "/home/other");
        assert_eq!(normalize("/home/iota/"), "/home/iota");
        assert_eq!(normalize("/"), "/");
    }

    #[test]
    fn climbing_above_the_root_stays_at_the_root() {
        // A path from a remote host is untrusted input; it must not escape upwards into
        // something meaningless.
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(normalize("/home/../../.."), "/");
    }

    #[test]
    fn relative_paths_keep_their_leading_dot_dot() {
        assert_eq!(normalize("../sibling"), "../sibling");
        assert_eq!(normalize("./here"), "here");
        assert_eq!(normalize(""), ".");
    }

    #[test]
    fn windows_separators_are_left_alone() {
        // A remote POSIX path may legitimately contain a backslash in a file name, and
        // it must never be treated as a separator.
        assert_eq!(join("/tmp", "back\\slash"), "/tmp/back\\slash");
        assert_eq!(file_name("/tmp/back\\slash"), "back\\slash");
    }

    #[test]
    fn breadcrumbs_lead_back_to_the_root() {
        assert_eq!(
            breadcrumbs("/home/iota/src"),
            vec![
                ("/".to_string(), "/".to_string()),
                ("/home".to_string(), "home".to_string()),
                ("/home/iota".to_string(), "iota".to_string()),
                ("/home/iota/src".to_string(), "src".to_string()),
            ]
        );
        assert_eq!(breadcrumbs("/"), vec![("/".to_string(), "/".to_string())]);
    }

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            kind,
            size: 0,
            modified: None,
            permissions: None,
        }
    }

    #[test]
    fn listings_put_directories_first_then_sort_by_name() {
        let mut entries = vec![
            entry("zebra.txt", EntryKind::File),
            entry("Apple", EntryKind::Dir),
            entry("beta.txt", EntryKind::File),
            entry("alpha", EntryKind::Dir),
        ];
        sort_entries(&mut entries);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Apple", "beta.txt", "zebra.txt"]);
    }

    #[test]
    fn symlinks_sort_with_directories_because_they_are_enterable() {
        let mut entries = vec![
            entry("file", EntryKind::File),
            entry("link", EntryKind::Symlink),
        ];
        sort_entries(&mut entries);
        assert_eq!(entries[0].name, "link");
    }

    #[test]
    fn dotfiles_are_recognised() {
        assert!(entry(".bashrc", EntryKind::File).is_hidden());
        assert!(!entry("bashrc", EntryKind::File).is_hidden());
    }
}
