//! User configuration, read from a TOML file at startup.

use std::path::PathBuf;

use catshell_term::palette::Palette;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub terminal: TerminalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Font family name. `monospace` resolves to whatever the system considers its
    /// default fixed-width face.
    pub family: String,
    pub size: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Lines of scrollback kept per pane.
    pub scrollback: usize,
    /// Render bold text in the bright colour variant, as older terminals did.
    pub bold_is_bright: bool,
    /// Shell to run. `None` uses the user's login shell.
    pub shell: Option<String>,
    /// Which shell's integration snippet to install, so the file explorer can follow
    /// the terminal.
    ///
    /// `auto` detects a local shell from its program name. A *remote* shell cannot be
    /// detected — nothing has run yet to ask — so `auto` assumes bash there; set this
    /// explicitly if your remote login shell is zsh or fish. `off` disables it.
    pub shell_integration: ShellIntegration,
}

/// Which shell integration to install.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellIntegration {
    #[default]
    Auto,
    Off,
    Bash,
    Zsh,
    Fish,
}

impl ShellIntegration {
    /// The shell to instrument, given the program being run when that is known.
    ///
    /// `program` is `None` for a remote session, where the login shell is whatever the
    /// server decides and there is nothing to inspect.
    pub fn resolve(self, program: Option<&str>) -> Option<catshell_term::integration::Shell> {
        use catshell_term::integration::Shell;
        match self {
            ShellIntegration::Off => None,
            ShellIntegration::Bash => Some(Shell::Bash),
            ShellIntegration::Zsh => Some(Shell::Zsh),
            ShellIntegration::Fish => Some(Shell::Fish),
            ShellIntegration::Auto => match program {
                Some(program) => Shell::detect(program),
                // Remote: bash is much the most common login shell, and installing the
                // wrong one is harmless — the snippet defines a function the shell never
                // calls rather than breaking anything.
                None => Some(Shell::Bash),
            },
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            size: 13.0,
        }
    }
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            bold_is_bright: false,
            shell: None,
            shell_integration: ShellIntegration::default(),
        }
    }
}

impl Config {
    /// Where the config file lives, following the platform's convention.
    pub fn path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("catshell").join("config.toml"))
    }

    /// Load the config, falling back to defaults.
    ///
    /// A missing file is normal and silent. A malformed one is reported but not fatal:
    /// a typo in a colour should not stop the terminal from opening.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };

        match toml::from_str(&text) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("ignoring {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub fn palette(&self) -> Palette {
        Palette::default()
    }

    pub fn term_config(&self) -> alacritty_terminal::term::Config {
        alacritty_terminal::term::Config {
            scrolling_history: self.terminal.scrollback,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = toml::to_string(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.font.family, Config::default().font.family);
        assert_eq!(
            parsed.terminal.scrollback,
            Config::default().terminal.scrollback
        );
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        // Users write two lines, not the whole schema.
        let parsed: Config = toml::from_str("[font]\nsize = 16.0\n").unwrap();
        assert_eq!(parsed.font.size, 16.0);
        assert_eq!(parsed.font.family, "monospace");
        assert_eq!(parsed.terminal.scrollback, 10_000);
    }

    #[test]
    fn scrollback_reaches_the_terminal_config() {
        let parsed: Config = toml::from_str("[terminal]\nscrollback = 50\n").unwrap();
        assert_eq!(parsed.term_config().scrolling_history, 50);
    }

    #[test]
    fn shell_integration_is_detected_for_local_shells() {
        use catshell_term::integration::Shell;
        let auto = ShellIntegration::Auto;
        assert_eq!(auto.resolve(Some("/bin/bash")), Some(Shell::Bash));
        assert_eq!(auto.resolve(Some("/usr/bin/zsh")), Some(Shell::Zsh));
        // Nothing is sent to a shell we cannot instrument.
        assert_eq!(auto.resolve(Some("/bin/sh")), None);
    }

    #[test]
    fn shell_integration_assumes_bash_for_remote_sessions() {
        use catshell_term::integration::Shell;
        // Nothing has run yet to ask what the login shell is.
        assert_eq!(ShellIntegration::Auto.resolve(None), Some(Shell::Bash));
    }

    #[test]
    fn shell_integration_can_be_pinned_or_turned_off() {
        use catshell_term::integration::Shell;
        assert_eq!(
            ShellIntegration::Zsh.resolve(Some("/bin/bash")),
            Some(Shell::Zsh)
        );
        assert_eq!(ShellIntegration::Off.resolve(Some("/bin/bash")), None);
        assert_eq!(ShellIntegration::Off.resolve(None), None);
    }

    #[test]
    fn shell_integration_parses_from_config() {
        let parsed: Config = toml::from_str("[terminal]\nshell_integration = \"zsh\"\n").unwrap();
        assert_eq!(parsed.terminal.shell_integration, ShellIntegration::Zsh);
    }

    #[test]
    fn an_unknown_key_is_an_error_rather_than_silently_ignored() {
        // A typo that silently did nothing would be worse than one that is reported.
        assert!(toml::from_str::<Config>("[font]\nsizee = 16.0\n").is_err());
    }
}
