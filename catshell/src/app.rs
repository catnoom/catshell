//! The application: tabs, panes, input routing, and the frame loop.

use std::collections::HashMap;
use std::sync::Arc;

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use catshell_term::palette::Palette;
use catshell_term::pty::{self, LocalShellOptions};
use catshell_term::session::{GridSize, Session, SessionEvent};
use egui::{Color32, Key, Modifiers, Rect, Vec2};

use crate::config::Config;
use crate::debug::Screenshotter;
use crate::font::GlyphAtlas;
use crate::pane::{Direction, Layout, PaneId};
use crate::render::{self, RenderOptions, TerminalCallback, TerminalRenderer};
use catshell_net::FileEntry;

use crate::explorer::{self, Action, Explorer, Source};
use crate::ssh::{Attempt, ConnectionState, Prompt, ShellRequest, SshManager};

/// Gap between panes, in points. Wide enough to grab with a mouse.
const DIVIDER: f32 = 4.0;

/// The host add/edit form.
///
/// Fields are held as text because that is what the user is mid-way through typing; an
/// unparsable port is a state the form has to be able to display, not a value to reject
/// keystroke by keystroke.
struct HostEditor {
    /// The name the host had when the form opened, so a rename replaces the right entry
    /// instead of leaving the old one behind.
    original_name: Option<String>,
    name: String,
    hostname: String,
    port: String,
    user: String,
    identity_file: String,
    error: Option<String>,
    /// Where the result of an open file dialog will arrive.
    ///
    /// The dialog runs on its own thread: `rfd`'s picker blocks until the user chooses,
    /// and blocking here would freeze the frame loop — the window would stop repainting
    /// and the system would mark it unresponsive while the dialog is up.
    picker: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
}

impl HostEditor {
    fn blank() -> catshell_net::HostConfig {
        catshell_net::HostConfig {
            name: String::new(),
            hostname: String::new(),
            port: catshell_net::host::DEFAULT_PORT,
            user: None,
            identity_files: Vec::new(),
        }
    }

    fn new(host: catshell_net::HostConfig, editing_existing: bool) -> Self {
        Self {
            original_name: editing_existing.then(|| host.name.clone()),
            name: host.name,
            hostname: host.hostname,
            port: host.port.to_string(),
            user: host.user.unwrap_or_default(),
            identity_file: host
                .identity_files
                .first()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            error: None,
            picker: None,
        }
    }

    /// Whether a file dialog is currently open.
    fn picking(&self) -> bool {
        self.picker.is_some()
    }

    /// Open a dialog to choose a private key file.
    fn pick_identity_file(&mut self) {
        if self.picking() {
            return;
        }

        // Start where keys actually live, so the common case needs no navigation.
        let start = dirs::home_dir().map(|home| home.join(".ssh"));
        let start = start.filter(|path| path.is_dir());

        let (sender, receiver) = std::sync::mpsc::channel();
        self.picker = Some(receiver);

        std::thread::Builder::new()
            .name("catshell-file-dialog".into())
            .spawn(move || {
                let mut dialog = rfd::FileDialog::new().set_title("Choose a private key");
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                // No extension filter: SSH private keys usually have no extension at all
                // (`id_ed25519`, `id_rsa`), so filtering would hide exactly the files
                // being looked for.
                let _ = sender.send(dialog.pick_file().map(|file| file.to_path_buf()));
            })
            .ok();
    }

    /// Take the chosen file, if the dialog has finished.
    ///
    /// Returns whether a dialog is still open, so the caller knows to keep repainting —
    /// otherwise the UI would go idle and never notice the answer.
    fn poll_picker(&mut self) -> bool {
        let Some(receiver) = &self.picker else {
            return false;
        };
        match receiver.try_recv() {
            Ok(chosen) => {
                if let Some(path) = chosen {
                    self.identity_file = path.to_string_lossy().into_owned();
                    // A previous complaint about this field is no longer current.
                    self.error = None;
                }
                self.picker = None;
                false
            }
            // The dialog thread died without answering; stop waiting on it.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.picker = None;
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
        }
    }

    /// Build a host from the form, or say what is wrong with it.
    fn to_host(&self) -> Result<catshell_net::HostConfig, String> {
        let port: u16 = match self.port.trim().parse() {
            Ok(port) => port,
            Err(_) => return Err("the port must be a number between 1 and 65535".into()),
        };
        let user = self.user.trim();
        let identity = self.identity_file.trim();

        Ok(catshell_net::HostConfig {
            name: self.name.trim().to_string(),
            hostname: self.hostname.trim().to_string(),
            port,
            user: (!user.is_empty()).then(|| user.to_string()),
            identity_files: if identity.is_empty() {
                Vec::new()
            } else {
                vec![std::path::PathBuf::from(identity)]
            },
        })
    }
}

/// A name not already taken, for copying an imported host into catshell's own list.
fn unique_name(base: &str, existing: &[catshell_net::host::ListedHost]) -> String {
    let taken = |candidate: &str| existing.iter().any(|h| h.config.name == candidate);
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| base.to_string())
}

/// A file operation the user has started but not yet confirmed.
///
/// Held as state rather than done immediately, because each needs either a name or an
/// explicit confirmation — deleting on a single click would be a bad idea over SSH.
#[derive(Debug, Clone)]
enum FileOp {
    NewFolder {
        name: String,
    },
    Rename {
        from: String,
        name: String,
    },
    ConfirmDelete {
        name: String,
        kind: catshell_net::EntryKind,
    },
}

/// Where a pane's shell is running.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Local,
    /// The name of the host, as it appears in the sidebar.
    Remote(String),
}

/// One terminal pane.
struct Pane {
    session: Session,
    origin: Origin,
    /// Title reported by the program via OSC 0/2, shown on the tab.
    title: Option<String>,
    /// Working directory reported via OSC 7. Unused until the file explorer lands, but
    /// tracked from the start so shell integration can be verified now.
    cwd: Option<String>,
    /// Set once the program exits; the pane stays open so its last output is readable.
    exited: bool,
    /// Grid size last sent to the program, to avoid resizing on every frame.
    size: GridSize,
}

struct Tab {
    layout: Layout,
    focused: PaneId,
    /// Send keystrokes to every pane in this tab at once.
    broadcast: bool,
}

impl Tab {
    fn title(&self, panes: &HashMap<PaneId, Pane>) -> String {
        let Some(pane) = panes.get(&self.focused) else {
            return "shell".into();
        };
        match (&pane.origin, &pane.title) {
            // The host matters more than the program's title when several tabs are open
            // on different machines, so it is never dropped.
            (Origin::Remote(host), Some(title)) => format!("{host}: {title}"),
            (Origin::Remote(host), None) => host.clone(),
            (Origin::Local, Some(title)) => title.clone(),
            (Origin::Local, None) => "shell".into(),
        }
    }
}

pub struct App {
    config: Config,
    palette: Palette,
    atlas: GlyphAtlas,
    /// Display scale the atlas was built for; glyphs must be re-rasterised if it changes,
    /// or the terminal turns blurry when moved to a different monitor.
    atlas_scale: f32,

    tabs: Vec<Tab>,
    active_tab: usize,
    panes: HashMap<PaneId, Pane>,
    next_pane_id: PaneId,

    /// Kept so that a closed pane's GPU buffers can be released; they live in egui's
    /// callback resources, which are only reachable through the render state.
    render_state: egui_wgpu::RenderState,
    /// Wakes the UI thread when a session produces output.
    wakeup: Arc<dyn Fn() + Send + Sync>,
    ssh: SshManager,
    /// The file pane. Retargets to follow whichever terminal has focus, so it is always
    /// showing the filesystem the user is actually working on.
    explorer: Option<Explorer>,
    show_explorer: bool,
    /// A file operation awaiting a name or a confirmation.
    file_op: Option<FileOp>,
    /// The host being added or edited, if any.
    host_editor: Option<HostEditor>,
    /// A question the user has to answer before a connection can continue.
    prompt: Option<Prompt>,
    /// What the user has typed into a password prompt.
    password: String,
    remember_password: bool,
    show_hosts: bool,

    /// Set while the mouse is dragging out a selection.
    selecting: Option<PaneId>,
    status: Option<String>,
    /// Present only when the app was asked to capture a frame and exit.
    screenshotter: Option<Screenshotter>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config) -> anyhow::Result<Self> {
        let render_state = cc
            .wgpu_render_state
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("catshell requires the wgpu renderer"))?;

        let scale = cc.egui_ctx.pixels_per_point();
        let atlas = GlyphAtlas::new(&config.font.family, config.font.size, scale);

        // The GPU resources outlive any single frame, so they live in egui's callback
        // resources rather than in a paint callback.
        let renderer = TerminalRenderer::new(
            &render_state.device,
            render_state.target_format,
            atlas.atlas_size(),
        );
        render_state
            .renderer
            .write()
            .callback_resources
            .insert(renderer);

        let ctx = cc.egui_ctx.clone();
        let wakeup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());

        let mut app = Self {
            palette: config.palette(),
            config,
            render_state: render_state.clone(),
            atlas,
            atlas_scale: scale,
            tabs: Vec::new(),
            active_tab: 0,
            panes: HashMap::new(),
            next_pane_id: 1,
            ssh: SshManager::new()?,
            explorer: None,
            // Shown by default: the synced file pane is the point of catshell, and a
            // hidden one is an undiscoverable one. It costs a directory read locally,
            // and an SFTP channel only once a remote pane has focus.
            show_explorer: true,
            file_op: None,
            host_editor: None,
            prompt: None,
            password: String::new(),
            remember_password: false,
            show_hosts: true,
            wakeup,
            selecting: None,
            status: None,
            screenshotter: crate::debug::ScreenshotRequest::from_env().map(Screenshotter::new),
        };
        app.new_tab()?;
        Ok(app)
    }

    /// Register a ready session as a pane.
    fn add_pane(&mut self, session: Session, origin: Origin, size: GridSize) -> PaneId {
        // Type the integration snippet only when it could not be installed invisibly.
        // A local bash gets it through its environment at spawn time (see
        // `local_integration`); a remote shell has to be told at its prompt, because no
        // server passes an environment through and an exported hook would be overwritten
        // by the host's own rc files anyway.
        //
        // Sent hidden: the remote echoes a typed line back, and that echo is catshell's
        // plumbing rather than anything the user asked to see.
        if let Some(shell) = self.typed_integration(&origin) {
            session.write_hidden(catshell_term::integration::install_command(shell).into_bytes());
        }

        let id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.insert(
            id,
            Pane {
                session,
                origin,
                title: None,
                cwd: None,
                exited: false,
                size,
            },
        );
        id
    }

    /// Which shell integration a local shell needs.
    fn local_integration(&self) -> Option<catshell_term::integration::Shell> {
        let program = self.config.terminal.shell.clone().or_else(default_shell);
        self.config
            .terminal
            .shell_integration
            .resolve(program.as_deref())
    }

    /// The integration that has to be typed, because it could not be set up invisibly.
    fn typed_integration(&self, origin: &Origin) -> Option<catshell_term::integration::Shell> {
        match origin {
            // A server generally refuses to pass environment variables through, so a
            // remote shell always has to be told at the prompt.
            Origin::Remote(_) => self.config.terminal.shell_integration.resolve(None),
            Origin::Local => {
                let shell = self.local_integration()?;
                // Already delivered through the environment; typing it as well would
                // install the hook twice and echo it at the user for nothing.
                if catshell_term::integration::install_env(shell).is_some() {
                    None
                } else {
                    Some(shell)
                }
            }
        }
    }

    /// Start a local shell and register it as a pane.
    fn spawn_pane(&mut self) -> anyhow::Result<PaneId> {
        // A placeholder size; the first frame resizes it to the pane's real dimensions
        // before the shell has a chance to draw anything.
        let size = GridSize::new(80, 24);
        let window_size = self.window_size(size);

        // Carrying the hook in the child's environment means nothing is echoed into the
        // terminal, and the first prompt already reports its directory.
        let mut env = std::collections::HashMap::new();
        if let Some(shell) = self.local_integration() {
            if let Some((name, value)) = catshell_term::integration::install_env(shell) {
                env.insert(name.to_string(), value);
            }
        }

        let options = LocalShellOptions {
            shell: self
                .config
                .terminal
                .shell
                .as_ref()
                .map(|program| (program.clone(), Vec::new())),
            env,
            ..Default::default()
        };

        let session = pty::spawn(
            options,
            self.config.term_config(),
            size,
            window_size,
            self.palette,
            Some(Arc::clone(&self.wakeup)),
        )?;

        Ok(self.add_pane(session, Origin::Local, size))
    }

    /// What a new remote shell should be sized and configured as.
    fn shell_request(&self) -> ShellRequest {
        let size = GridSize::new(80, 24);
        ShellRequest {
            config: self.config.term_config(),
            size,
            window_size: self.window_size(size),
            palette: self.palette,
            wakeup: Some(Arc::clone(&self.wakeup)),
        }
    }

    /// Begin connecting to a host, carrying any answers already given.
    fn connect(&mut self, host: catshell_net::HostConfig, trust: bool, password: Option<String>) {
        let request = self.shell_request();
        self.ssh.connect(host, trust, password, request);
    }

    /// Apply finished connection attempts.
    fn drain_ssh(&mut self) {
        for attempt in self.ssh.drain() {
            match attempt {
                Attempt::Connected { host, session, .. } => {
                    let pane = self.add_pane(
                        session,
                        Origin::Remote(host.name.clone()),
                        GridSize::new(80, 24),
                    );
                    self.tabs.push(Tab {
                        layout: Layout::new(pane),
                        focused: pane,
                        broadcast: false,
                    });
                    self.active_tab = self.tabs.len() - 1;
                    self.prompt = None;
                    self.password.clear();
                }
                // Consumed inside the manager; it never reaches here.
                Attempt::BrowserReady { .. } => {}
                Attempt::ShellFailed { host, message } => {
                    self.status = Some(format!("{}: {message}", host.name));
                }
                Attempt::NeedsInput(prompt) => {
                    self.password.clear();
                    self.prompt = Some(prompt);
                }
            }
        }
    }

    /// Draw the sidebar of hosts.
    fn show_host_list(&mut self, ui: &mut egui::Ui) {
        ui.heading("Hosts");
        ui.add_space(4.0);

        if ui.button("New host…").clicked() {
            self.host_editor = Some(HostEditor::new(HostEditor::blank(), false));
        }
        if ui.button("Local shell").clicked() {
            if let Err(err) = self.new_tab() {
                self.status = Some(format!("{err}"));
            }
        }

        ui.separator();

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("~/.ssh/config").small());
            if ui
                .small_button("Refresh")
                .on_hover_text("Re-read ~/.ssh/config")
                .clicked()
            {
                self.ssh.reload_hosts();
            }
        });

        if self.ssh.hosts().is_empty() {
            ui.label("No hosts in ~/.ssh/config.");
        }

        let hosts = self.ssh.hosts();
        let mut connect_to = None;
        let mut edit = None;
        let mut delete = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            for listed in &hosts {
                let host = &listed.config;
                let connecting = self.ssh.is_connecting(&host.name);
                let connected = matches!(
                    self.ssh.state(&host.name),
                    Some(ConnectionState::Connected(_))
                );

                // Why a host failed is worth keeping: it is the difference between
                // "wrong password" and "no route to host", and it would otherwise be
                // lost as soon as the prompt closed.
                let failure = match self.ssh.state(&host.name) {
                    Some(ConnectionState::Failed(message)) => Some(message.clone()),
                    _ => None,
                };

                ui.horizontal(|ui| {
                    // The dot is painted rather than written: a glyph like U+25CF is not
                    // in every bundled font, and a missing one renders as a blank box —
                    // which is exactly the state indicator failing to indicate anything.
                    let (colour, filled) = if connected {
                        (Color32::from_rgb(0xb5, 0xbd, 0x68), true)
                    } else if connecting {
                        (Color32::from_rgb(0xf0, 0xc6, 0x74), false)
                    } else if failure.is_some() {
                        (Color32::from_rgb(0xd5, 0x4e, 0x53), true)
                    } else {
                        (Color32::GRAY, false)
                    };
                    status_dot(ui, colour, filled);

                    let button = ui.button(&host.name);
                    let button = match &failure {
                        Some(message) => button.on_hover_text(message),
                        None => button.on_hover_text(host.address()),
                    };
                    if button.clicked() {
                        connect_to = Some(host.clone());
                    }

                    match listed.origin {
                        catshell_net::host::Origin::Own => {
                            if ui.small_button("Edit").clicked() {
                                edit = Some(host.clone());
                            }
                            if ui
                                .small_button("x")
                                .on_hover_text("Remove this host")
                                .clicked()
                            {
                                delete = Some(host.name.clone());
                            }
                        }
                        // Imported hosts are shown but not edited: `~/.ssh/config` is the
                        // user's file, and rewriting it could lose options catshell does
                        // not model. Copying one into catshell is offered instead.
                        catshell_net::host::Origin::Imported => {
                            if ui
                                .small_button("Copy")
                                .on_hover_text(
                                    "Copy into catshell's own hosts, where it can be edited",
                                )
                                .clicked()
                            {
                                let mut copy = host.clone();
                                copy.name = unique_name(&copy.name, &hosts);
                                edit = Some(copy);
                            }
                        }
                    }
                });
            }
        });

        if let Some(host) = edit {
            let editing = self.ssh.store().get(&host.name).is_some();
            self.host_editor = Some(HostEditor::new(host, editing));
        }
        if let Some(name) = delete {
            if let Err(err) = self.ssh.delete_host(&name) {
                self.status = Some(format!("{err}"));
            }
        }

        if let Some(host) = connect_to {
            self.connect(host, false, None);
        }
    }

    /// Draw the file explorer.
    fn show_explorer(&mut self, ui: &mut egui::Ui) {
        let Some(explorer) = &self.explorer else {
            ui.label("No pane in focus.");
            return;
        };

        let source_label = match explorer.source() {
            Source::Local => "This machine".to_string(),
            Source::Remote(host) => host.clone(),
        };
        let path = explorer.path().to_string();
        let loading = explorer.is_loading();
        let status = explorer.status().map(str::to_owned);
        let (mut follow, mut drive, mut hidden) = (
            explorer.follow_terminal(),
            explorer.drive_terminal(),
            explorer.show_hidden(),
        );

        // Collected while drawing and applied after, since drawing borrows the explorer.
        let mut pending: Vec<Action> = Vec::new();
        let mut go_up = false;
        let mut reload = false;
        let mut navigate_to: Option<String> = None;
        let mut activate: Option<FileEntry> = None;
        let mut select: Option<String> = None;
        let (mut new_folder, mut rename, mut delete) = (false, false, false);
        let selection: Option<FileEntry> = explorer
            .selected()
            .and_then(|name| explorer.visible().find(|entry| entry.name == name).cloned());

        ui.horizontal(|ui| {
            ui.heading("Files");
            if loading {
                ui.spinner();
            }
        });
        ui.label(egui::RichText::new(&source_label).small());

        ui.horizontal(|ui| {
            if ui.small_button("Up").clicked() {
                go_up = true;
            }
            if ui.small_button("Refresh").clicked() {
                reload = true;
            }
            if ui.small_button("New folder").clicked() {
                new_folder = true;
            }
            if ui
                .add_enabled(
                    selection.is_some(),
                    egui::Button::small(egui::Button::new("Rename")),
                )
                .clicked()
            {
                rename = true;
            }
            if ui
                .add_enabled(
                    selection.is_some(),
                    egui::Button::small(egui::Button::new("Delete")),
                )
                .clicked()
            {
                delete = true;
            }
        });

        // A breadcrumb for remote paths, where the separator is known; a local path is
        // shown as-is because its separator is the platform's.
        if explorer.source().is_remote() {
            ui.horizontal_wrapped(|ui| {
                for (target, label) in catshell_net::files::posix::breadcrumbs(&path) {
                    if ui.small_button(&label).clicked() {
                        navigate_to = Some(target);
                    }
                }
            });
        } else {
            ui.label(egui::RichText::new(&path).monospace().small());
        }

        ui.horizontal(|ui| {
            ui.checkbox(&mut follow, "Follow")
                .on_hover_text("Move with the terminal's working directory");
            ui.checkbox(&mut drive, "cd")
                .on_hover_text("Also change the terminal's directory when entering one here");
            ui.checkbox(&mut hidden, "Hidden");
        });

        if let Some(status) = &status {
            ui.colored_label(Color32::from_rgb(0xd5, 0x4e, 0x53), status);
        }

        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            let selected = explorer.selected().map(str::to_owned);
            for entry in explorer.visible() {
                let icon = match entry.kind {
                    catshell_net::EntryKind::Dir => "[dir] ",
                    catshell_net::EntryKind::Symlink => "[link]",
                    _ => "      ",
                };
                let label = format!("{icon} {}", entry.name);
                let is_selected = selected.as_deref() == Some(entry.name.as_str());

                let response = ui.selectable_label(is_selected, label);
                if !entry.kind.is_navigable() {
                    response
                        .clone()
                        .on_hover_text(explorer::human_size(entry.size));
                }
                // Double-click enters a directory; a single click only selects, which is
                // what every file manager does and what makes rename and delete safe to
                // offer.
                if response.double_clicked() {
                    activate = Some(entry.clone());
                } else if response.clicked() {
                    select = Some(entry.name.clone());
                }
            }
        });

        // Apply everything the frame asked for.
        if let Some(explorer) = &mut self.explorer {
            explorer.set_follow_terminal(follow);
            explorer.set_drive_terminal(drive);
            explorer.set_show_hidden(hidden);

            if let Some(name) = select {
                explorer.select(Some(name));
            }
            if go_up {
                if let Some(action) = explorer.go_up() {
                    pending.push(action);
                }
            }
            if reload {
                pending.push(explorer.reload());
            }
            if let Some(target) = navigate_to {
                pending.push(explorer.navigate(target));
            }
            if let Some(entry) = activate {
                pending.extend(explorer.activate(&entry));
            }
        }
        if new_folder {
            self.file_op = Some(FileOp::NewFolder {
                name: String::new(),
            });
        }
        if let Some(entry) = &selection {
            if rename {
                self.file_op = Some(FileOp::Rename {
                    from: entry.name.clone(),
                    name: entry.name.clone(),
                });
            }
            if delete {
                self.file_op = Some(FileOp::ConfirmDelete {
                    name: entry.name.clone(),
                    kind: entry.kind,
                });
            }
        }

        for action in pending {
            self.run_explorer_action(action);
        }
    }

    /// Draw the host add/edit form.
    fn show_host_editor(&mut self, ctx: &egui::Context) {
        if self.host_editor.is_none() {
            return;
        }

        let mut save = false;
        let mut cancel = false;
        let mut browse = false;
        let mut clear_key = false;

        // A dialog is open on another thread; keep painting so its answer is noticed.
        let picking = match &mut self.host_editor {
            Some(editor) => editor.poll_picker(),
            None => false,
        };
        if picking {
            ctx.request_repaint();
        }

        egui::Modal::new(egui::Id::new("catshell host editor")).show(ctx, |ui| {
            ui.set_width(440.0);
            let Some(editor) = &mut self.host_editor else {
                return;
            };

            ui.heading(if editor.original_name.is_some() {
                "Edit host"
            } else {
                "New host"
            });
            ui.add_space(6.0);

            egui::Grid::new("catshell host fields")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    let name = ui.add(
                        egui::TextEdit::singleline(&mut editor.name)
                            .hint_text("how it appears in the sidebar")
                            .desired_width(f32::INFINITY),
                    );
                    focus_when_opened(ui, &name);
                    ui.end_row();

                    ui.label("Address");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.hostname)
                            .hint_text("host name or IP address")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Port");
                    ui.add(egui::TextEdit::singleline(&mut editor.port).desired_width(80.0));
                    ui.end_row();

                    ui.label("User");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.user)
                            .hint_text("blank uses your local username")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Key file");
                    ui.horizontal(|ui| {
                        // The dialog is a convenience, not the only way in: the path
                        // stays editable so it can be pasted or typed, which is what a
                        // remote or not-yet-created key needs.
                        if ui
                            .add_enabled(!picking, egui::Button::new("Browse…"))
                            .on_hover_text("Choose a private key file")
                            .clicked()
                        {
                            browse = true;
                        }
                        if !editor.identity_file.is_empty() && ui.button("Clear").clicked() {
                            clear_key = true;
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.identity_file)
                                .hint_text("blank tries the ssh-agent")
                                .desired_width(f32::INFINITY),
                        );
                    });
                    ui.end_row();
                });

            if let Some(error) = &editor.error {
                ui.add_space(4.0);
                ui.colored_label(Color32::from_rgb(0xd5, 0x4e, 0x53), error);
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                // Enter saves, Escape cancels — a form that needs the mouse to close is
                // a form people avoid.
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
        });

        if browse {
            if let Some(editor) = &mut self.host_editor {
                editor.pick_identity_file();
            }
        }
        if clear_key {
            if let Some(editor) = &mut self.host_editor {
                editor.identity_file.clear();
            }
        }

        if cancel {
            self.host_editor = None;
            return;
        }
        // Enter is bound to Save, and a file dialog closing can deliver one; ignore it
        // while a dialog is up so choosing a key does not also submit the form.
        if !save || picking {
            return;
        }

        let Some(editor) = &self.host_editor else {
            return;
        };
        let original = editor.original_name.clone();
        let host = match editor.to_host() {
            Ok(host) => host,
            Err(message) => {
                if let Some(editor) = &mut self.host_editor {
                    editor.error = Some(message);
                }
                return;
            }
        };

        // Validated against every *other* name, so saving an unchanged host is not
        // rejected for clashing with itself.
        let taken: Vec<String> = self
            .ssh
            .store()
            .names_except(original.as_deref())
            .into_iter()
            .map(str::to_owned)
            .collect();
        if let Err(problem) = catshell_net::host::validate(&host, taken.iter().map(String::as_str))
        {
            if let Some(editor) = &mut self.host_editor {
                editor.error = Some(problem.to_string());
            }
            return;
        }

        match self.ssh.save_host(host, original.as_deref()) {
            Ok(()) => self.host_editor = None,
            Err(err) => {
                if let Some(editor) = &mut self.host_editor {
                    editor.error = Some(format!("could not save: {err}"));
                }
            }
        }
    }

    /// Draw whichever question is outstanding, if any.
    fn show_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.prompt.clone() else {
            return;
        };

        egui::Modal::new(egui::Id::new("catshell prompt")).show(ctx, |ui| {
            ui.set_width(460.0);
            match prompt {
                Prompt::HostKey { host, fingerprint } => {
                    ui.heading("Unknown host");
                    ui.label(format!(
                        "{} is not in your known_hosts file.",
                        host.address()
                    ));
                    ui.add_space(4.0);
                    ui.monospace(&fingerprint);
                    ui.add_space(4.0);
                    ui.label(
                        "Compare this fingerprint with the server before trusting it. \
                         Accepting a key you cannot verify defeats the protection it provides.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Trust and connect").clicked() {
                            self.prompt = None;
                            self.connect(host.clone(), true, None);
                        }
                        if ui.button("Cancel").clicked() {
                            self.prompt = None;
                        }
                    });
                }

                Prompt::Password { host, detail } => {
                    ui.heading("Password");
                    ui.label(format!("Log in as {}", host.address()));
                    ui.add_space(4.0);
                    ui.small(&detail);
                    ui.add_space(8.0);

                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    focus_when_opened(ui, &field);
                    ui.checkbox(
                        &mut self.remember_password,
                        "Remember in the system keyring",
                    );

                    let submitted =
                        field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Connect").clicked() || submitted {
                            let password = std::mem::take(&mut self.password);
                            if self.remember_password
                                && !catshell_net::secrets::set(&host.address(), &password)
                            {
                                // Say so rather than silently not remembering it.
                                self.status =
                                    Some("no credential store available; not remembered".into());
                            }
                            self.prompt = None;
                            self.connect(host.clone(), false, Some(password));
                        }
                        if ui.button("Cancel").clicked() {
                            self.password.clear();
                            self.prompt = None;
                        }
                    });
                }

                Prompt::Failed { host, message } => {
                    ui.heading("Cannot connect");
                    ui.label(&host.name);
                    ui.add_space(4.0);
                    ui.label(&message);
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.prompt = None;
                    }
                }
            }
        });
    }

    fn window_size(&self, size: GridSize) -> WindowSize {
        let metrics = self.atlas.metrics();
        WindowSize {
            num_lines: size.screen_lines as u16,
            num_cols: size.columns as u16,
            cell_width: metrics.width as u16,
            cell_height: metrics.height as u16,
        }
    }

    fn new_tab(&mut self) -> anyhow::Result<()> {
        let pane = self.spawn_pane()?;
        self.tabs.push(Tab {
            layout: Layout::new(pane),
            focused: pane,
            broadcast: false,
        });
        self.active_tab = self.tabs.len() - 1;
        Ok(())
    }

    fn split(&mut self, direction: Direction) -> anyhow::Result<()> {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return Ok(());
        };
        let target = tab.focused;
        let pane = self.spawn_pane()?;
        let tab = &mut self.tabs[self.active_tab];
        tab.layout.split(target, direction, pane);
        tab.focused = pane;
        Ok(())
    }

    /// Close a pane, and the tab if it was the last one.
    fn close_pane(&mut self, pane: PaneId) {
        if let Some(state) = self.panes.remove(&pane) {
            state.session.shutdown();
        }
        self.release_pane_buffers();

        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if !tab.layout.close(pane) {
            self.tabs.remove(self.active_tab);
            self.active_tab = self.active_tab.saturating_sub(1);
            return;
        }
        if tab.focused == pane {
            tab.focused = tab.layout.panes().first().copied().unwrap_or(pane);
        }
    }

    /// Drop the vertex buffers of panes that no longer exist.
    ///
    /// Without this a long session of opening and closing splits would grow GPU memory
    /// forever, since the renderer keeps a buffer per pane id and ids are never reused.
    fn release_pane_buffers(&mut self) {
        let live: std::collections::HashSet<PaneId> = self.panes.keys().copied().collect();
        let mut renderer = self.render_state.renderer.write();
        if let Some(terminal) = renderer.callback_resources.get_mut::<TerminalRenderer>() {
            terminal.retain_panes(&|pane| live.contains(&pane));
        }
    }

    /// Apply everything the sessions reported since the last frame.
    fn drain_sessions(&mut self) {
        let mut finished = Vec::new();
        let mut directory_reports = Vec::new();
        let mut completions = Vec::new();
        for (id, pane) in &mut self.panes {
            for event in pane.session.drain_events() {
                match event {
                    SessionEvent::Title(title) => pane.title = title,
                    SessionEvent::Shell(catshell_term::ShellEvent::CwdChanged { path, .. }) => {
                        pane.cwd = Some(path.clone());
                        directory_reports.push((*id, path));
                    }
                    SessionEvent::Shell(catshell_term::ShellEvent::CommandEnd { .. }) => {
                        completions.push(*id);
                    }
                    SessionEvent::Exited(_) => {
                        pane.exited = true;
                        finished.push(*id);
                    }
                    // Redraw is implicit: the wakeup already scheduled a repaint.
                    SessionEvent::Redraw | SessionEvent::Bell | SessionEvent::Shell(_) => {}
                    SessionEvent::ClipboardStore(text) => set_clipboard(&text),
                }
            }
        }

        // Shell integration only steers the explorer for the pane the user is looking
        // at; a background build must not drag the file pane somewhere else.
        let focused = self.focused_pane();
        for (pane, path) in directory_reports {
            if Some(pane) == focused {
                self.explorer_shell_directory(&path);
            }
        }
        for pane in completions {
            if Some(pane) == focused {
                if let Some(explorer) = &mut self.explorer {
                    explorer.on_command_finished(std::time::Instant::now());
                }
            }
        }

        // A shell that exits closes its pane, which is what makes `exit` and Ctrl-D work.
        for pane in finished {
            self.close_pane(pane);
        }
    }

    fn focused_pane(&self) -> Option<PaneId> {
        self.tabs.get(self.active_tab).map(|tab| tab.focused)
    }

    /// Point the explorer at the filesystem the focused pane is using.
    fn retarget_explorer(&mut self) {
        let Some(pane) = self.focused_pane().and_then(|id| self.panes.get(&id)) else {
            return;
        };

        let wanted = match &pane.origin {
            Origin::Local => Source::Local,
            Origin::Remote(host) => Source::Remote(host.clone()),
        };
        if self.explorer.as_ref().map(Explorer::source) == Some(&wanted) {
            return;
        }

        match &wanted {
            Source::Local => {
                // The shell's own directory when it has reported one, else ours.
                let start = pane.cwd.clone().unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| ".".into())
                });
                let mut explorer = Explorer::new(Source::Local, start);
                let action = explorer.reload();
                self.explorer = Some(explorer);
                self.run_explorer_action(action);
            }
            Source::Remote(host) => {
                let host = host.clone();
                // The login directory is unknown until the server says; start empty and
                // let the Home request fill it in.
                let explorer = Explorer::new(Source::Remote(host.clone()), String::new());
                self.explorer = Some(explorer);
                self.ssh.open_browser(&host, Some(Arc::clone(&self.wakeup)));
                self.request_remote_home(&host);
            }
        }
    }

    fn request_remote_home(&mut self, host: &str) {
        let Some(explorer) = &mut self.explorer else {
            return;
        };
        let generation = match explorer.reload() {
            Action::List { generation, .. } => generation,
            _ => return,
        };
        if let Some(browser) = self.ssh.browser(host) {
            browser.send(catshell_net::sftp::Request::Home { generation });
        }
    }

    /// Tell the explorer the shell moved, and act on what it decides.
    fn explorer_shell_directory(&mut self, path: &str) {
        let Some(explorer) = &mut self.explorer else {
            return;
        };
        // A local pane must not follow a path from a remote shell, or vice versa.
        let Some(action) = explorer.on_shell_directory(path) else {
            return;
        };
        self.run_explorer_action(action);
    }

    /// Carry out what the explorer asked for.
    fn run_explorer_action(&mut self, action: Action) {
        let Some(explorer) = &self.explorer else {
            return;
        };
        match action {
            Action::List { path, generation } => match explorer.source().clone() {
                Source::Local => {
                    // Local listing is a direct read; it is fast enough to do inline and
                    // needs no round trip.
                    match explorer::list_local(&path) {
                        Ok(entries) => {
                            if let Some(explorer) = &mut self.explorer {
                                explorer.accept_listing(&path, generation, entries);
                            }
                        }
                        Err(err) => {
                            if let Some(explorer) = &mut self.explorer {
                                explorer
                                    .accept_failure(&format!("listing {path}"), &format!("{err}"));
                            }
                        }
                    }
                }
                Source::Remote(host) => {
                    if let Some(browser) = self.ssh.browser(&host) {
                        browser.send(catshell_net::sftp::Request::List { path, generation });
                    }
                    // No browser yet: the listing is re-requested once it opens.
                }
            },

            Action::ChangeShellDirectory { path } => {
                if let Some(pane) = self.focused_pane().and_then(|id| self.panes.get(&id)) {
                    // A trailing newline runs it; the leading space keeps it out of
                    // history for shells configured to do that.
                    pane.session
                        .write(format!(" cd {}\n", shell_quote(&path)).into_bytes());
                }
            }
        }
    }

    /// Carry out a confirmed file operation.
    ///
    /// Remote work goes through the browser on the shared connection; local work is a
    /// direct call. Either way the explorer reloads afterwards so the result is visible.
    fn run_file_op(&mut self, op: FileOp) {
        use catshell_net::sftp::Request;

        let Some(explorer) = &self.explorer else {
            return;
        };
        let source = explorer.source().clone();

        let (request, local): (Option<Request>, Option<std::io::Result<()>>) = match &op {
            FileOp::NewFolder { name } => {
                let path = explorer.path_of(name);
                match &source {
                    Source::Remote(_) => (Some(Request::MakeDir { path }), None),
                    Source::Local => (None, Some(std::fs::create_dir(&path))),
                }
            }
            FileOp::Rename { from, name } => {
                let (from, to) = (explorer.path_of(from), explorer.path_of(name));
                match &source {
                    Source::Remote(_) => (Some(Request::Rename { from, to }), None),
                    Source::Local => (None, Some(std::fs::rename(&from, &to))),
                }
            }
            FileOp::ConfirmDelete { name, kind } => {
                let path = explorer.path_of(name);
                match &source {
                    Source::Remote(_) => (Some(Request::Remove { path, kind: *kind }), None),
                    Source::Local => {
                        let result =
                            if kind.is_navigable() && *kind != catshell_net::EntryKind::Symlink {
                                std::fs::remove_dir(&path)
                            } else {
                                std::fs::remove_file(&path)
                            };
                        (None, Some(result))
                    }
                }
            }
        };

        if let (Some(request), Source::Remote(host)) = (request, &source) {
            if let Some(browser) = self.ssh.browser(host) {
                browser.send(request);
            }
            // The reload happens when the browser reports `Changed`.
            return;
        }

        match local {
            Some(Ok(())) => {
                if let Some(explorer) = &mut self.explorer {
                    let action = explorer.reload();
                    self.run_explorer_action(action);
                }
            }
            Some(Err(err)) => {
                if let Some(explorer) = &mut self.explorer {
                    explorer.accept_failure("file operation", &format!("{err}"));
                }
            }
            None => {}
        }
    }

    /// Draw the dialog for a pending file operation.
    fn show_file_op(&mut self, ctx: &egui::Context) {
        let Some(op) = self.file_op.clone() else {
            return;
        };

        egui::Modal::new(egui::Id::new("catshell file op")).show(ctx, |ui| {
            ui.set_width(380.0);
            let mut confirm = false;
            let mut cancel = false;

            match &op {
                FileOp::NewFolder { .. } | FileOp::Rename { .. } => {
                    let (heading, name) = match &mut self.file_op {
                        Some(FileOp::NewFolder { name }) => ("New folder", name),
                        Some(FileOp::Rename { name, .. }) => ("Rename", name),
                        _ => return,
                    };
                    ui.heading(heading);
                    let field =
                        ui.add(egui::TextEdit::singleline(name).desired_width(f32::INFINITY));
                    focus_when_opened(ui, &field);
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        confirm = true;
                    }
                }
                FileOp::ConfirmDelete { name, .. } => {
                    ui.heading("Delete");
                    // Named explicitly: this is not undoable, and on a remote host there
                    // is no recycle bin to fall back on.
                    ui.label(format!("Permanently delete {name}?"));
                }
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = if matches!(op, FileOp::ConfirmDelete { .. }) {
                    "Delete"
                } else {
                    "OK"
                };
                if ui.button(label).clicked() {
                    confirm = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });

            if confirm {
                if let Some(op) = self.file_op.take() {
                    // An empty name would create or rename to nothing.
                    let named = match &op {
                        FileOp::NewFolder { name } | FileOp::Rename { name, .. } => {
                            !name.is_empty()
                        }
                        FileOp::ConfirmDelete { .. } => true,
                    };
                    if named {
                        self.run_file_op(op);
                    }
                }
            } else if cancel {
                self.file_op = None;
            }
        });
    }

    /// Apply whatever the file browsers have reported.
    fn drain_browsers(&mut self) {
        let Some(explorer) = &self.explorer else {
            return;
        };
        let Source::Remote(host) = explorer.source().clone() else {
            return;
        };
        let Some(browser) = self.ssh.browser(&host) else {
            return;
        };

        let events = browser.drain();
        let mut changed = false;
        for event in events {
            let Some(explorer) = &mut self.explorer else {
                return;
            };
            match event {
                catshell_net::sftp::Event::Listed {
                    path,
                    generation,
                    entries,
                } => {
                    explorer.accept_listing(&path, generation, entries);
                }
                catshell_net::sftp::Event::Failed { action, message } => {
                    explorer.accept_failure(&action, &message);
                }
                catshell_net::sftp::Event::Changed => changed = true,
            }
        }

        if changed {
            // A rename or delete succeeded; show the result rather than making the user
            // press refresh.
            if let Some(explorer) = &mut self.explorer {
                let action = explorer.reload();
                self.run_explorer_action(action);
            }
        }
    }

    /// Handle a key press that belongs to the application rather than the program.
    fn shortcut(&mut self, key: Key, modifiers: Modifiers) -> bool {
        // Ctrl+Tab moves between panes. Programs almost never bind it, and reserving it
        // avoids making pane navigation a three-key chord.
        if modifiers.ctrl && key == Key::Tab {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                if let Some(next) = tab.layout.next_pane(tab.focused) {
                    tab.focused = next;
                }
            }
            return true;
        }

        // Ctrl+Shift is the terminal convention: it leaves every plain Ctrl combination
        // free for the program, which needs Ctrl+C far more than we need a shortcut.
        if !(modifiers.ctrl && modifiers.shift) {
            return false;
        }

        let result = match key {
            Key::T => self.new_tab(),
            Key::D => self.split(Direction::Horizontal),
            Key::E => self.split(Direction::Vertical),
            Key::W => {
                if let Some(tab) = self.tabs.get(self.active_tab) {
                    self.close_pane(tab.focused);
                }
                Ok(())
            }
            Key::B => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.broadcast = !tab.broadcast;
                }
                Ok(())
            }
            Key::C => {
                self.copy_selection();
                Ok(())
            }
            _ => return false,
        };

        if let Err(err) = result {
            self.status = Some(format!("{err}"));
        }
        true
    }

    fn copy_selection(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let Some(pane) = self.panes.get(&tab.focused) else {
            return;
        };
        if let Some(text) = pane.session.term().lock().selection_to_string() {
            if !text.is_empty() {
                set_clipboard(&text);
            }
        }
    }

    /// Whether a dialog is up and owns the keyboard.
    ///
    /// Covers dialogs with no text field of their own — the delete confirmation, for
    /// instance — where `wants_keyboard_input` is false but typing still must not reach
    /// the shell.
    fn modal_open(&self) -> bool {
        self.prompt.is_some() || self.file_op.is_some() || self.host_editor.is_some()
    }

    /// Route this frame's keyboard input to the focused pane, or to every pane in the
    /// tab when broadcast is on.
    fn handle_input(&mut self, ctx: &egui::Context) {
        // A text field or a dialog takes the keyboard exclusively. The terminal reads the
        // *raw* event list, which egui's widgets read too, so without this every
        // keystroke goes to both — a password typed into the prompt would also be sent
        // to the shell behind it, and run there as a command.
        //
        // Specifically `text_edit_focused`, not `egui_wants_keyboard_input`: the latter
        // is true for any focused widget, so a sidebar button left focused by a Tab
        // would silently stop the terminal receiving input at all.
        if self.modal_open() || ctx.text_edit_focused() {
            return;
        }

        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let (focused, broadcast) = (tab.focused, tab.broadcast);
        let Some(pane) = self.panes.get(&focused) else {
            return;
        };
        let mode = *pane.session.term().lock().mode();

        // Shortcuts are collected first and applied after, because handling them needs
        // `&mut self` while the events are still borrowed from the context.
        let mut shortcuts = Vec::new();
        let actions = ctx.input(|input| {
            crate::input::translate(&input.events, mode, |key, modifiers| {
                if is_shortcut(key, modifiers) {
                    shortcuts.push((key, modifiers));
                    true
                } else {
                    false
                }
            })
        });

        for (key, modifiers) in shortcuts {
            self.shortcut(key, modifiers);
        }

        if actions.copy {
            self.copy_selection();
        }

        if !actions.bytes.is_empty() {
            let targets: Vec<PaneId> = if broadcast {
                self.tabs
                    .get(self.active_tab)
                    .map(|tab| tab.layout.panes())
                    .unwrap_or_default()
            } else {
                vec![focused]
            };
            for target in targets {
                if let Some(pane) = self.panes.get(&target) {
                    // Typing scrolls back to the prompt, as every terminal does.
                    pane.session
                        .term()
                        .lock()
                        .scroll_display(alacritty_terminal::grid::Scroll::Bottom);
                    pane.session.write(actions.bytes.clone());
                }
            }
        }
    }

    /// Drive the screenshot harness, when one was requested.
    fn run_screenshotter(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.screenshotter else {
            return;
        };

        if shot.handle_events(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if shot.take_open_host_editor() {
            self.host_editor = Some(HostEditor::new(HostEditor::blank(), false));
        }
        let pick = self
            .screenshotter
            .as_mut()
            .is_some_and(|shot| shot.take_open_file_picker());
        if pick {
            if let Some(editor) = &mut self.host_editor {
                editor.pick_identity_file();
            }
        }
        let Some(shot) = &mut self.screenshotter else {
            return;
        };

        let wanted = shot.take_connect();
        if !wanted.is_empty() {
            shot.extend(std::time::Duration::from_millis(3500));
        }
        let initial_password = self
            .screenshotter
            .as_ref()
            .and_then(|s| s.password().map(str::to_owned));
        for name in wanted {
            // Deliberately not pre-trusting: this follows the same path a user does,
            // so the host-key prompt is exercised rather than bypassed. The password,
            // when given, goes in from the start so that several requests for the same
            // host queue behind one attempt instead of each failing and re-prompting.
            if let Some(host) = self.ssh.host(&name) {
                self.connect(host, false, initial_password.clone());
            } else {
                self.status = Some(format!("no such host: {name}"));
            }
        }

        // Answer the prompts the way the environment says to, so a connected state can
        // be captured without a person clicking through.
        let answers = self
            .screenshotter
            .as_ref()
            .map(|shot| (shot.trusts_host(), shot.password().map(str::to_owned)));
        if let Some((trust_host, password)) = answers {
            match self.prompt.clone() {
                Some(Prompt::HostKey { host, .. }) if trust_host => {
                    self.prompt = None;
                    self.connect(host, true, None);
                    if let Some(shot) = &mut self.screenshotter {
                        shot.extend(std::time::Duration::from_millis(2000));
                    }
                }
                Some(Prompt::Password { host, .. }) if password.is_some() => {
                    self.prompt = None;
                    self.connect(host, true, password);
                    if let Some(shot) = &mut self.screenshotter {
                        shot.extend(std::time::Duration::from_millis(2000));
                    }
                }
                _ => {}
            }
        }

        let Some(shot) = &mut self.screenshotter else {
            return;
        };
        for split in shot.take_splits() {
            let direction = match split {
                crate::debug::Split::Horizontal => Direction::Horizontal,
                crate::debug::Split::Vertical => Direction::Vertical,
            };
            let _ = self.split(direction);
        }

        let script = self
            .screenshotter
            .as_mut()
            .and_then(|shot| shot.take_script());
        if let Some(script) = script {
            // Every pane, so a split screenshot shows content in all of them.
            let panes = self
                .tabs
                .get(self.active_tab)
                .map(|tab| tab.layout.panes())
                .unwrap_or_default();
            for id in panes {
                if let Some(pane) = self.panes.get(&id) {
                    pane.session.write(script.clone().into_bytes());
                }
            }
        }
        if let Some(shot) = &mut self.screenshotter {
            shot.poll(ctx);
        }
    }

    /// Draw one pane and handle the mouse inside it.
    fn show_pane(&mut self, ui: &mut egui::Ui, id: PaneId, rect: Rect, focused: bool) {
        let scale = ui.ctx().pixels_per_point();
        let metrics = self.atlas.metrics();
        // Metrics are in physical pixels; egui lays out in points.
        let cell = Vec2::new(metrics.width / scale, metrics.height / scale);

        let response = ui.interact(
            rect,
            egui::Id::new(("catshell pane", id)),
            egui::Sense::click_and_drag(),
        );

        // Not while a dialog is up: it owns the keyboard, and its fields need Tab.
        if focused && !self.modal_open() {
            claim_keyboard(ui, &response);
        }

        let columns = (rect.width() / cell.x).floor().max(1.0) as usize;
        let lines = (rect.height() / cell.y).floor().max(1.0) as usize;
        let size = GridSize::new(columns, lines);

        let Some(pane) = self.panes.get_mut(&id) else {
            return;
        };
        if pane.size != size {
            pane.size = size;
            let metrics = self.atlas.metrics();
            pane.session.resize(
                size,
                WindowSize {
                    num_lines: lines as u16,
                    num_cols: columns as u16,
                    cell_width: metrics.width as u16,
                    cell_height: metrics.height as u16,
                },
            );
        }

        self.handle_pane_mouse(id, &response, rect, cell);

        // Scrollback.
        if response.hovered() {
            let scroll = ui.ctx().input(|input| input.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let lines = (scroll / cell.y).round() as i32;
                if lines != 0 {
                    if let Some(pane) = self.panes.get(&id) {
                        pane.session
                            .term()
                            .lock()
                            .scroll_display(alacritty_terminal::grid::Scroll::Delta(lines));
                    }
                }
            }
        }

        let Some(pane) = self.panes.get(&id) else {
            return;
        };
        let instances = {
            let term = pane.session.term().lock();
            render::build_instances(
                term.renderable_content(),
                &self.palette,
                &mut self.atlas,
                RenderOptions {
                    focused,
                    bold_is_bright: self.config.terminal.bold_is_bright,
                },
            )
        };

        // Glyphs rasterised while building this frame must reach the GPU before it draws.
        let atlas_upload = self
            .atlas
            .take_dirty()
            .map(|(first, rows, data)| (first, rows, data.to_vec()));

        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            rect,
            TerminalCallback {
                pane: id,
                rect,
                instances,
                atlas_upload,
            },
        ));
    }

    fn handle_pane_mouse(&mut self, id: PaneId, response: &egui::Response, rect: Rect, cell: Vec2) {
        if response.clicked() || response.drag_started() {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.focused = id;
            }
        }

        let point_at = |pos: egui::Pos2| -> (Point, Side) {
            let column = ((pos.x - rect.min.x) / cell.x).floor().max(0.0) as usize;
            let line = ((pos.y - rect.min.y) / cell.y).floor().max(0.0) as i32;
            let fraction = ((pos.x - rect.min.x) / cell.x).fract();
            // Which half of the cell was hit decides whether the character is included,
            // which is what makes a drag select what it looks like it selects.
            let side = if fraction < 0.5 {
                Side::Left
            } else {
                Side::Right
            };
            (Point::new(Line(line), Column(column)), side)
        };

        let Some(pane) = self.panes.get(&id) else {
            return;
        };

        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                let (point, side) = point_at(pos);
                let point = viewport_to_grid(&pane.session, point);
                pane.session.term().lock().selection =
                    Some(Selection::new(SelectionType::Simple, point, side));
                self.selecting = Some(id);
            }
        } else if response.dragged() && self.selecting == Some(id) {
            if let Some(pos) = response.interact_pointer_pos() {
                let (point, side) = point_at(pos);
                let point = viewport_to_grid(&pane.session, point);
                if let Some(selection) = &mut pane.session.term().lock().selection {
                    selection.update(point, side);
                }
            }
        } else if response.drag_stopped() {
            self.selecting = None;
        } else if response.clicked() {
            // A plain click clears the selection, as it does everywhere else.
            pane.session.term().lock().selection = None;
        }

        if response.middle_clicked() {
            // Primary-selection paste, the Unix convention.
            if let Some(text) = get_clipboard() {
                let mode = *pane.session.term().lock().mode();
                pane.session
                    .write(catshell_term::keys::encode_paste(&text, mode));
            }
        }
    }

    fn show_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut activate = None;
            let mut close = None;
            for (index, tab) in self.tabs.iter().enumerate() {
                let selected = index == self.active_tab;
                let mut title = tab.title(&self.panes);
                if tab.broadcast {
                    // Broadcast sends your typing to several shells at once; it must be
                    // impossible to have it on without noticing.
                    title = format!("⇉ {title}");
                }
                if ui.selectable_label(selected, title).clicked() {
                    activate = Some(index);
                }
                if selected && self.tabs.len() > 1 && ui.small_button("×").clicked() {
                    close = Some(index);
                }
            }
            if let Some(index) = activate {
                self.active_tab = index;
            }
            if let Some(index) = close {
                let panes = self.tabs[index].layout.panes();
                self.active_tab = index;
                for pane in panes {
                    self.close_pane(pane);
                }
            }

            ui.separator();
            // A word, not a glyph: the box-drawing characters used here rendered as
            // blank tofu in the bundled UI font.
            if ui
                .selectable_label(self.show_hosts, "Hosts")
                .on_hover_text("Show or hide the host list")
                .clicked()
            {
                self.show_hosts = !self.show_hosts;
            }
            if ui
                .selectable_label(self.show_explorer, "Files")
                .on_hover_text("Show or hide the file explorer")
                .clicked()
            {
                self.show_explorer = !self.show_explorer;
            }
            if ui.small_button("+").clicked() {
                if let Err(err) = self.new_tab() {
                    self.status = Some(format!("{err}"));
                }
            }

            if let Some(status) = &self.status {
                ui.separator();
                ui.colored_label(Color32::from_rgb(0xd5, 0x4e, 0x53), status);
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_sessions();
        self.drain_ssh();
        self.retarget_explorer();

        // A browser that has just opened may have missed the request that triggered it,
        // since opening is asynchronous; ask again now that it can answer.
        for host in self.ssh.take_ready_browsers() {
            if self.explorer.as_ref().map(Explorer::source) == Some(&Source::Remote(host.clone())) {
                self.request_remote_home(&host);
            }
        }

        self.drain_browsers();

        // A refresh scheduled when a command finished may now be due.
        if let Some(explorer) = &mut self.explorer {
            if let Some(action) = explorer.due_refresh(std::time::Instant::now()) {
                self.run_explorer_action(action);
            }
        }
        self.run_screenshotter(&ctx);

        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Following the display scale keeps text crisp when the window moves between
        // monitors, and it must happen before any glyph is looked up this frame.
        let scale = ctx.pixels_per_point();
        if (scale - self.atlas_scale).abs() > f32::EPSILON {
            self.atlas_scale = scale;
            self.atlas
                .reconfigure(&self.config.font.family, self.config.font.size, scale);
        }

        self.handle_input(&ctx);

        egui::Panel::top("catshell tabs").show(ui, |ui| self.show_tab_bar(ui));

        if self.show_hosts {
            egui::Panel::left("catshell hosts")
                .resizable(true)
                .show(ui, |ui| self.show_host_list(ui));
        }
        if self.show_explorer {
            egui::Panel::right("catshell files")
                .resizable(true)
                .show(ui, |ui| self.show_explorer(ui));
        }
        self.show_host_editor(&ctx);
        self.show_file_op(&ctx);
        self.show_prompt(&ctx);

        let background = self.palette.background;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(
                background.r,
                background.g,
                background.b,
            )))
            .show(ui, |ui| {
                let Some(tab) = self.tabs.get(self.active_tab) else {
                    return;
                };
                let area = ui.max_rect();
                let placed = tab.layout.layout(area, DIVIDER);
                let dividers = tab.layout.dividers(area, DIVIDER);
                let focused = tab.focused;

                for (id, rect) in placed {
                    self.show_pane(ui, id, rect, id == focused);
                }

                for (path, handle, direction) in dividers {
                    // Without a line the panes are invisible against each other, since
                    // they share a background colour.
                    ui.painter()
                        .rect_filled(handle, 0.0, divider_color(background));

                    let response = ui.interact(
                        handle,
                        egui::Id::new(("catshell divider", &path)),
                        egui::Sense::drag(),
                    );
                    let cursor = match direction {
                        Direction::Horizontal => egui::CursorIcon::ResizeHorizontal,
                        Direction::Vertical => egui::CursorIcon::ResizeVertical,
                    };
                    if response.hovered() || response.dragged() {
                        ui.ctx().set_cursor_icon(cursor);
                    }
                    if response.dragged() {
                        if let Some(pos) = response.interact_pointer_pos() {
                            let ratio = match direction {
                                Direction::Horizontal => {
                                    (pos.x - area.min.x) / area.width().max(1.0)
                                }
                                Direction::Vertical => {
                                    (pos.y - area.min.y) / area.height().max(1.0)
                                }
                            };
                            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                                tab.layout.set_ratio(&path, ratio);
                            }
                        }
                    }
                }
            });

        // No repaint is requested here on purpose: an idle terminal should cost nothing.
        // Frames happen when a session wakes us or the user interacts.
    }

    fn on_exit(&mut self) {
        // Send a proper disconnect rather than letting the sockets drop, so servers log
        // a clean close instead of a reset.
        self.ssh.disconnect_all();
    }
}

/// The login shell, for deciding which integration snippet a local pane needs.
fn default_shell() -> Option<String> {
    std::env::var("SHELL").ok()
}

/// Hold egui's keyboard focus for a terminal pane, and claim the keys egui would
/// otherwise use to move between widgets.
///
/// Without this, Tab does two things at once: the shell completes a filename *and* egui
/// moves focus to the next button — so the Return that follows presses that button
/// instead of running the command. Arrow keys would walk the interface while scrolling
/// shell history, and Escape would surrender focus mid-session, which matters to
/// everything from vim to a pager.
fn claim_keyboard(ui: &mut egui::Ui, pane: &egui::Response) {
    // Taken back from anything that is not a text field. In a terminal the keyboard
    // belongs to the shell by default, so clicking a button in the interface should do
    // that button's job and hand the keyboard straight back — otherwise the next Tab
    // walks the interface instead of completing a filename.
    let elsewhere = ui.ctx().text_edit_focused();
    if !elsewhere && !pane.has_focus() {
        pane.request_focus();
    }
    if pane.has_focus() {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                pane.id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
    }
}

/// Give a dialog's first field focus when the dialog opens, and only then.
///
/// `Response::request_focus` has to be called every frame to *hold* focus, so calling it
/// unconditionally pins the cursor to that one field forever — Tab and clicks into the
/// next field are undone the moment the frame redraws. Claiming focus only when nothing
/// else has it focuses the field on the first frame and then leaves the user alone.
fn focus_when_opened(ui: &egui::Ui, field: &egui::Response) {
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
}

/// Quote a path for a POSIX shell.
///
/// The explorer types `cd <path>` into a live shell, and a path is not trustworthy
/// input: it comes from a remote directory listing, and a directory can be named
/// anything at all — including `; rm -rf ~`. Single quotes disable every expansion the
/// shell performs, and the only character that needs handling inside them is the single
/// quote itself, which is closed, escaped, and reopened.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Draw a small connection-status dot.
fn status_dot(ui: &mut egui::Ui, colour: Color32, filled: bool) {
    let diameter = ui.spacing().interact_size.y * 0.4;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(diameter), egui::Sense::hover());
    let centre = rect.center();
    let radius = diameter * 0.5;
    if filled {
        ui.painter().circle_filled(centre, radius, colour);
    } else {
        ui.painter()
            .circle_stroke(centre, radius, egui::Stroke::new(1.5, colour));
    }
}

/// A divider just light enough to read against the terminal background, in either a
/// dark or a light theme.
fn divider_color(background: alacritty_terminal::vte::ansi::Rgb) -> Color32 {
    let luminance = 0.2126 * f32::from(background.r)
        + 0.7152 * f32::from(background.g)
        + 0.0722 * f32::from(background.b);
    let shift = if luminance < 128.0 { 40 } else { -40i16 };
    let nudge = |channel: u8| (i16::from(channel) + shift).clamp(0, 255) as u8;
    Color32::from_rgb(
        nudge(background.r),
        nudge(background.g),
        nudge(background.b),
    )
}

/// True for key combinations the application claims for itself.
fn is_shortcut(key: Key, modifiers: Modifiers) -> bool {
    if modifiers.ctrl && key == Key::Tab {
        return true;
    }
    modifiers.ctrl
        && modifiers.shift
        && matches!(key, Key::T | Key::W | Key::D | Key::E | Key::B | Key::C)
}

/// Convert a point in viewport coordinates to the grid coordinates the terminal uses,
/// which differ by however far the view is scrolled back.
fn viewport_to_grid(session: &Session, point: Point) -> Point {
    let term = session.term().lock();
    let offset = term.grid().display_offset() as i32;
    Point::new(Line(point.line.0 - offset), point.column)
}

fn set_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
        Ok(()) => {}
        Err(err) => tracing::warn!("clipboard unavailable: {err}"),
    }
}

fn get_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;

    #[test]
    fn only_ctrl_shift_combinations_are_shortcuts() {
        let ctrl_shift = Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        assert!(is_shortcut(Key::T, ctrl_shift));

        // Plain Ctrl belongs to the program: Ctrl+C must interrupt, not copy.
        let ctrl = Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert!(!is_shortcut(Key::C, ctrl));
        assert!(!is_shortcut(Key::T, ctrl));

        // And an unclaimed key passes through even with the modifiers held.
        assert!(!is_shortcut(Key::Q, ctrl_shift));

        // Ctrl+Tab cycles panes without needing shift.
        assert!(is_shortcut(Key::Tab, ctrl));
        assert!(!is_shortcut(Key::Tab, Modifiers::default()));
    }

    // --- Dialog focus and input routing ---------------------------------------------
    //
    // Run against a headless `egui::Context`, which is the only way to exercise focus:
    // egui resolves Tab and click-to-focus inside its own frame processing, so events
    // injected from application code arrive too late to move focus.

    /// Draw two text fields, focusing the first through `focus_when_opened`.
    fn two_fields(
        ctx: &egui::Context,
        input: egui::RawInput,
        first: &mut String,
        second: &mut String,
    ) -> (egui::Id, egui::Id) {
        let mut ids = (egui::Id::NULL, egui::Id::NULL);
        let mut output = ctx.run_ui(input, |ui| {
            let a = ui.add(egui::TextEdit::singleline(first));
            focus_when_opened(ui, &a);
            let b = ui.add(egui::TextEdit::singleline(second));
            ids = (a.id, b.id);
        });
        // A headless run still produces font textures, and epaint refuses to have them
        // dropped unhandled.
        output.textures_delta.clear();
        ids
    }

    #[test]
    fn the_first_field_takes_focus_when_a_dialog_opens() {
        let ctx = egui::Context::default();
        let (mut first, mut second) = (String::new(), String::new());

        let (a, _) = two_fields(&ctx, Default::default(), &mut first, &mut second);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(a),
            "the first field was not focused"
        );
    }

    #[test]
    fn focus_moves_to_the_next_field_and_stays_there() {
        // The reported bug: `request_focus` called every frame pinned the cursor to the
        // first field, so Tab and clicks into the next one were undone on the redraw.
        let ctx = egui::Context::default();
        let (mut first, mut second) = (String::new(), String::new());

        let (_, b) = two_fields(&ctx, Default::default(), &mut first, &mut second);
        ctx.memory_mut(|m| m.request_focus(b));

        // Several more frames, because the old bug reasserted itself on every one.
        for frame in 0..5 {
            two_fields(&ctx, Default::default(), &mut first, &mut second);
            assert_eq!(
                ctx.memory(|m| m.focused()),
                Some(b),
                "focus was yanked back to the first field on frame {frame}"
            );
        }
    }

    #[test]
    fn typing_reaches_only_the_field_that_has_focus() {
        let ctx = egui::Context::default();
        let (mut first, mut second) = (String::new(), String::new());
        let (_, b) = two_fields(&ctx, Default::default(), &mut first, &mut second);
        ctx.memory_mut(|m| m.request_focus(b));
        two_fields(&ctx, Default::default(), &mut first, &mut second);

        let typed = egui::RawInput {
            events: vec![egui::Event::Text("abc".into())],
            ..Default::default()
        };
        two_fields(&ctx, typed, &mut first, &mut second);

        assert_eq!(second, "abc", "typing did not reach the focused field");
        assert!(first.is_empty(), "typing also landed in the first field");
    }

    #[test]
    fn a_focused_text_field_claims_the_keyboard() {
        // This is the signal `handle_input` uses to keep keystrokes out of the shell.
        let ctx = egui::Context::default();
        let (mut first, mut second) = (String::new(), String::new());
        two_fields(&ctx, Default::default(), &mut first, &mut second);

        assert!(
            ctx.text_edit_focused(),
            "a focused text field did not claim the keyboard, so typing would also reach \
             the terminal behind the dialog"
        );
    }

    #[test]
    fn nothing_claims_the_keyboard_when_no_field_is_focused() {
        // The converse: with no dialog up, the terminal must still receive input.
        let ctx = egui::Context::default();
        ctx.run_ui(Default::default(), |ui| {
            ui.label("no fields here");
        })
        .textures_delta
        .clear();
        assert!(!ctx.text_edit_focused());
    }

    #[test]
    fn a_focused_button_does_not_claim_the_keyboard() {
        // Why `text_edit_focused` rather than `egui_wants_keyboard_input`: the latter is
        // true for any focused widget, so a button left focused by a Tab would silently
        // stop the terminal receiving input at all.
        let ctx = egui::Context::default();
        let mut id = egui::Id::NULL;
        ctx.run_ui(Default::default(), |ui| {
            id = ui.button("press me").id;
        })
        .textures_delta
        .clear();
        ctx.memory_mut(|m| m.request_focus(id));
        ctx.run_ui(Default::default(), |ui| {
            let _ = ui.button("press me");
        })
        .textures_delta
        .clear();

        assert!(
            ctx.egui_wants_keyboard_input(),
            "the button should be focused"
        );
        assert!(
            !ctx.text_edit_focused(),
            "a focused button must not block the terminal"
        );
    }

    /// Draw a terminal-like pane followed by a button, and return their ids.
    fn pane_and_button(
        ctx: &egui::Context,
        input: egui::RawInput,
        claim: bool,
    ) -> (egui::Id, egui::Id) {
        let mut ids = (egui::Id::NULL, egui::Id::NULL);
        ctx.run_ui(input, |ui| {
            let pane = ui.interact(
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 50.0)),
                egui::Id::new("test pane"),
                egui::Sense::click_and_drag(),
            );
            if claim {
                claim_keyboard(ui, &pane);
            }
            let button = ui.button("somewhere else");
            ids = (pane.id, button.id);
        })
        .textures_delta
        .clear();
        ids
    }

    fn press(key: egui::Key) -> egui::RawInput {
        egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_focused_pane_takes_the_keyboard() {
        let ctx = egui::Context::default();
        let (pane, _) = pane_and_button(&ctx, Default::default(), true);
        assert_eq!(ctx.memory(|m| m.focused()), Some(pane));
    }

    #[test]
    fn tab_stays_in_the_terminal_instead_of_moving_the_interface() {
        // The reported bug: Tab completed a filename in the shell *and* moved egui's
        // focus to the next widget, so the following Return pressed that widget.
        let ctx = egui::Context::default();
        let (pane, button) = pane_and_button(&ctx, Default::default(), true);
        // A second frame, because the lock only applies once the pane has held focus
        // across a frame boundary.
        pane_and_button(&ctx, Default::default(), true);

        pane_and_button(&ctx, press(egui::Key::Tab), true);
        let focused = ctx.memory(|m| m.focused());
        assert_ne!(
            focused,
            Some(button),
            "Tab moved focus onto a button in the interface"
        );
        assert_eq!(focused, Some(pane), "the terminal lost focus on Tab");
    }

    #[test]
    fn without_the_claim_tab_does_move_focus() {
        // Confirms the test above is testing the claim, not a quirk of the harness: the
        // same pane, focused the same way but without claiming Tab, loses focus to the
        // next widget — which is the bug that was reported.
        let ctx = egui::Context::default();
        let (pane, button) = pane_and_button(&ctx, Default::default(), false);
        ctx.memory_mut(|m| m.request_focus(pane));
        pane_and_button(&ctx, Default::default(), false);

        pane_and_button(&ctx, press(egui::Key::Tab), false);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(button),
            "expected egui's default Tab navigation to move focus off the pane"
        );
    }

    #[test]
    fn arrow_keys_stay_in_the_terminal() {
        // Shell history and vim both live on the arrow keys; they must not walk the
        // interface.
        let ctx = egui::Context::default();
        let (pane, _) = pane_and_button(&ctx, Default::default(), true);
        pane_and_button(&ctx, Default::default(), true);

        for key in [
            egui::Key::ArrowDown,
            egui::Key::ArrowUp,
            egui::Key::ArrowLeft,
            egui::Key::ArrowRight,
        ] {
            pane_and_button(&ctx, press(key), true);
            assert_eq!(
                ctx.memory(|m| m.focused()),
                Some(pane),
                "{key:?} moved focus away"
            );
        }
    }

    #[test]
    fn escape_stays_in_the_terminal() {
        // Escape is how you leave insert mode, not how you leave the terminal.
        let ctx = egui::Context::default();
        let (pane, _) = pane_and_button(&ctx, Default::default(), true);
        pane_and_button(&ctx, Default::default(), true);

        pane_and_button(&ctx, press(egui::Key::Escape), true);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(pane),
            "Escape surrendered focus"
        );
    }

    #[test]
    fn the_pane_takes_the_keyboard_back_from_a_button() {
        // Click "Refresh" in the explorer and the next Tab should still complete a
        // filename in the shell, not move to the next button.
        let ctx = egui::Context::default();
        let (pane, button) = pane_and_button(&ctx, Default::default(), true);
        ctx.memory_mut(|m| m.request_focus(button));

        pane_and_button(&ctx, Default::default(), true);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(pane),
            "the keyboard stayed on the button instead of returning to the terminal"
        );
    }

    #[test]
    fn a_pane_holding_the_keyboard_does_not_block_terminal_input() {
        // `handle_input` stops at `text_edit_focused`; a focused pane is not a text
        // field, so keystrokes must still reach the shell.
        let ctx = egui::Context::default();
        pane_and_button(&ctx, Default::default(), true);

        assert!(
            ctx.egui_wants_keyboard_input(),
            "the pane should hold focus"
        );
        assert!(
            !ctx.text_edit_focused(),
            "a focused pane must not look like a text field"
        );
    }

    #[test]
    fn a_chosen_file_lands_in_the_key_field() {
        // The dialog answers on a channel; this exercises the receiving half without
        // needing a real dialog, which cannot be opened in a test.
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        let (sender, receiver) = std::sync::mpsc::channel();
        editor.picker = Some(receiver);
        assert!(editor.picking());

        sender
            .send(Some(std::path::PathBuf::from("/home/me/.ssh/id_ed25519")))
            .unwrap();
        assert!(
            !editor.poll_picker(),
            "still waiting after an answer arrived"
        );
        assert_eq!(editor.identity_file, "/home/me/.ssh/id_ed25519");
        assert!(!editor.picking());
    }

    #[test]
    fn cancelling_the_dialog_leaves_the_key_field_alone() {
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        editor.identity_file = "/existing/key".into();

        let (sender, receiver) = std::sync::mpsc::channel();
        editor.picker = Some(receiver);
        sender.send(None).unwrap();

        assert!(!editor.poll_picker());
        assert_eq!(
            editor.identity_file, "/existing/key",
            "cancelling cleared the field"
        );
    }

    #[test]
    fn a_dialog_thread_that_dies_does_not_hang_the_form() {
        // Dropping the sender without answering must not leave the form permanently
        // believing a dialog is open, which would disable the button forever.
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        let (sender, receiver) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        editor.picker = Some(receiver);
        drop(sender);

        assert!(!editor.poll_picker());
        assert!(!editor.picking());
    }

    #[test]
    fn waiting_on_the_dialog_keeps_the_ui_repainting() {
        // If this returned false while waiting, the frame loop would go idle and never
        // notice the answer.
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        let (_sender, receiver) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        editor.picker = Some(receiver);
        assert!(editor.poll_picker());
    }

    #[test]
    fn a_chosen_file_clears_a_stale_complaint() {
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        editor.error = Some("something about the key".into());

        let (sender, receiver) = std::sync::mpsc::channel();
        editor.picker = Some(receiver);
        sender.send(Some(std::path::PathBuf::from("/k"))).unwrap();
        editor.poll_picker();

        assert!(editor.error.is_none(), "a stale error outlived the fix");
    }

    #[test]
    fn opening_a_second_dialog_is_refused_while_one_is_up() {
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        let (_sender, receiver) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        editor.picker = Some(receiver);

        // Must not replace the pending receiver, or the first answer is lost.
        editor.pick_identity_file();
        assert!(editor.picking());
    }

    #[test]
    fn the_editor_round_trips_a_host() {
        let host = catshell_net::HostConfig {
            name: "web".into(),
            hostname: "web.example.com".into(),
            port: 2222,
            user: Some("deploy".into()),
            identity_files: vec!["/home/me/.ssh/id_ed25519".into()],
        };
        let editor = HostEditor::new(host.clone(), true);
        assert_eq!(editor.to_host().unwrap(), host);
        assert_eq!(editor.original_name.as_deref(), Some("web"));
    }

    #[test]
    fn blank_optional_fields_become_none_rather_than_empty_strings() {
        // An empty user must mean "use my local username", not a user literally called "".
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        editor.name = "web".into();
        editor.hostname = "web.example.com".into();

        let host = editor.to_host().unwrap();
        assert_eq!(host.user, None);
        assert!(host.identity_files.is_empty());
    }

    #[test]
    fn fields_are_trimmed_so_a_stray_space_does_not_break_a_connection() {
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        editor.name = "  web  ".into();
        editor.hostname = " web.example.com ".into();
        editor.user = " deploy ".into();
        editor.port = " 2222 ".into();

        let host = editor.to_host().unwrap();
        assert_eq!(host.name, "web");
        assert_eq!(host.hostname, "web.example.com");
        assert_eq!(host.user.as_deref(), Some("deploy"));
        assert_eq!(host.port, 2222);
    }

    #[test]
    fn an_unparsable_port_is_reported_rather_than_silently_defaulted() {
        // Silently falling back to 22 would connect somewhere the user did not ask for.
        let mut editor = HostEditor::new(HostEditor::blank(), false);
        editor.name = "web".into();
        editor.hostname = "web.example.com".into();
        editor.port = "not-a-number".into();

        let message = editor.to_host().unwrap_err();
        assert!(message.contains("port"), "unhelpful message: {message}");

        editor.port = "70000".into();
        assert!(editor.to_host().is_err(), "a port above 65535 was accepted");
    }

    #[test]
    fn copying_an_imported_host_picks_a_free_name() {
        use catshell_net::host::{ListedHost, Origin};
        let listed = |name: &str| ListedHost {
            config: catshell_net::HostConfig {
                name: name.into(),
                hostname: "h".into(),
                port: 22,
                user: None,
                identity_files: vec![],
            },
            origin: Origin::Imported,
        };

        assert_eq!(unique_name("web", &[]), "web");
        assert_eq!(unique_name("web", &[listed("web")]), "web-2");
        assert_eq!(
            unique_name("web", &[listed("web"), listed("web-2")]),
            "web-3"
        );
    }

    #[test]
    fn paths_are_quoted_before_being_typed_into_a_shell() {
        assert_eq!(shell_quote("/home/iota"), "'/home/iota'");
        assert_eq!(shell_quote("/tmp/my dir"), "'/tmp/my dir'");
    }

    #[test]
    fn a_hostile_directory_name_cannot_run_a_command() {
        // Directory names come from a remote listing and can be anything. Every one of
        // these must end up as a single inert argument to `cd`.
        for hostile in [
            "/tmp/; rm -rf ~",
            "/tmp/$(whoami)",
            "/tmp/`id`",
            "/tmp/a&&b",
            "/tmp/new\nline",
            "/tmp/$HOME",
        ] {
            let quoted = shell_quote(hostile);
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "{quoted}"
            );
            // Nothing between the outer quotes may close them.
            let inner = &quoted[1..quoted.len() - 1];
            assert!(
                !inner.contains('\'') || inner.contains(r"'\''"),
                "{hostile:?} escaped its quoting as {quoted}"
            );
        }
    }

    #[test]
    fn a_name_containing_a_quote_is_closed_escaped_and_reopened() {
        // The one case single quoting cannot handle directly.
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }

    #[test]
    fn the_divider_contrasts_with_the_background() {
        use alacritty_terminal::vte::ansi::Rgb;
        // It must be visible on a dark background and on a light one, so a light theme
        // does not end up with invisible splits.
        for background in [
            Rgb {
                r: 29,
                g: 31,
                b: 33,
            },
            Rgb {
                r: 250,
                g: 250,
                b: 250,
            },
        ] {
            let divider = divider_color(background);
            let difference = i16::from(divider.r()) - i16::from(background.r);
            assert!(
                difference.abs() >= 30,
                "divider is invisible on {background:?}"
            );
        }
    }

    #[test]
    fn term_mode_flag_is_reachable() {
        // Guards the import used for bracketed paste detection.
        assert!(!TermMode::empty().contains(TermMode::BRACKETED_PASTE));
    }
}
