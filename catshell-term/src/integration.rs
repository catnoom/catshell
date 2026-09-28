//! Shell-integration snippets.
//!
//! A terminal cannot see a shell's working directory: it only receives the bytes the
//! shell prints. The shell has to volunteer it, which is what OSC 7 is for — and what
//! makes the file explorer able to follow the terminal rather than guess.
//!
//! These snippets install a prompt hook that reports, before each prompt:
//!
//! * OSC 133;D — the previous command finished, with its exit status. The explorer uses
//!   this to refresh exactly when something might have changed, instead of polling.
//! * OSC 7 — the current directory.
//! * OSC 133;A — a new prompt is starting.
//!
//! The counterpart is [`crate::osc::OscSniffer`], which recovers these from the stream.

/// Shells catshell knows how to instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Guess the shell from the path or name of its executable.
    ///
    /// `None` when it is not one we can instrument, in which case nothing is sent —
    /// injecting bash syntax into an unknown shell would print errors at the user.
    pub fn detect(program: &str) -> Option<Self> {
        // Trailing version digits are common (`bash5`, `zsh-5.9`), and a login shell is
        // conventionally spelled with a leading dash.
        let name = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .trim_start_matches('-')
            .to_ascii_lowercase();

        if name.starts_with("bash") {
            Some(Shell::Bash)
        } else if name.starts_with("zsh") {
            Some(Shell::Zsh)
        } else if name.starts_with("fish") {
            Some(Shell::Fish)
        } else {
            None
        }
    }

    /// The shell's usual executable name.
    pub fn program(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }
}

/// Environment that installs the hook with nothing typed at all.
///
/// bash reads `PROMPT_COMMAND` from its environment, so a *local* shell — whose
/// environment catshell controls — can be instrumented invisibly: no echoed snippet, no
/// history entry, and the working directory is reported before the first prompt rather
/// than after the first command.
///
/// `None` for shells with no equivalent. zsh's hook is an array (`precmd_functions`)
/// that cannot be set from the environment, and fish's is an event handler; both fall
/// back to [`install_command`]. Remote sessions always fall back too, since servers
/// generally refuse to pass environment variables through (`AcceptEnv`).
pub fn install_env(shell: Shell) -> Option<(&'static str, String)> {
    match shell {
        Shell::Bash => Some((
            "PROMPT_COMMAND",
            concat!(
                // The status has to be captured first: every command below overwrites it.
                "__catshell_s=$?;",
                " printf '\\033]133;D;%s\\007' \"$__catshell_s\";",
                " printf '\\033]7;file://%s%s\\007' \"${HOSTNAME:-}\" \"${PWD//%/%25}\";",
                " printf '\\033]133;A\\007'",
            )
            .to_string(),
        )),
        Shell::Zsh | Shell::Fish => None,
    }
}

/// A one-line command that installs the hook, ready to write to the shell's input.
///
/// One line, because it is typed into a live shell rather than sourced from a file. It
/// begins with a space so that shells configured with `HISTCONTROL=ignorespace` (or
/// zsh's `HIST_IGNORE_SPACE`) keep it out of history, and ends with a newline to run it.
///
/// The shell still echoes the line, so the user sees it go past once per session. That
/// is the price of not needing anything installed on the remote host, which is the whole
/// point — MobaXterm's file browser needs no setup either, and drifts out of sync as a
/// result.
pub fn install_command(shell: Shell) -> String {
    // `%` is doubled before sending because OSC 7 paths are percent-encoded; a literal
    // `%` in a directory name would otherwise be read back as the start of an escape.
    // Nothing else needs encoding: the sequence is terminated by BEL, not by whitespace.
    match shell {
        Shell::Bash => concat!(
            " __catshell_report() { local s=$?;",
            " printf '\\033]133;D;%s\\007' \"$s\";",
            " printf '\\033]7;file://%s%s\\007' \"${HOSTNAME:-}\" \"${PWD//%/%25}\";",
            " printf '\\033]133;A\\007'; };",
            " PROMPT_COMMAND=\"__catshell_report${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"\n",
        )
        .to_string(),

        Shell::Zsh => concat!(
            " __catshell_report() { local s=$?;",
            " printf '\\033]133;D;%s\\007' \"$s\";",
            " printf '\\033]7;file://%s%s\\007' \"${HOST:-}\" \"${PWD//\\%/%25}\";",
            " printf '\\033]133;A\\007' };",
            " precmd_functions+=(__catshell_report)\n",
        )
        .to_string(),

        // fish has no `$?`; the previous status is `$status`, and it must be read first
        // because any command in the function overwrites it.
        Shell::Fish => concat!(
            " function __catshell_report --on-event fish_prompt;",
            " set -l s $status;",
            " printf '\\033]133;D;%s\\007' $s;",
            " printf '\\033]7;file://%s%s\\007' (hostname) (string replace -a '%' '%25' $PWD);",
            " printf '\\033]133;A\\007'; end\n",
        )
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::{OscSniffer, ShellEvent};

    #[test]
    fn shells_are_detected_from_their_path() {
        assert_eq!(Shell::detect("/bin/bash"), Some(Shell::Bash));
        assert_eq!(Shell::detect("/usr/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::detect("/usr/local/bin/fish"), Some(Shell::Fish));
        assert_eq!(Shell::detect("bash"), Some(Shell::Bash));
    }

    #[test]
    fn login_shells_and_versioned_names_are_detected() {
        // A login shell is spelled `-bash`, and versioned names are common.
        assert_eq!(Shell::detect("-bash"), Some(Shell::Bash));
        assert_eq!(Shell::detect("/bin/bash5"), Some(Shell::Bash));
        assert_eq!(Shell::detect("zsh-5.9"), Some(Shell::Zsh));
        // Windows-style separators, for a local shell path.
        assert_eq!(
            Shell::detect(r"C:\msys64\usr\bin\bash.exe"),
            Some(Shell::Bash)
        );
    }

    #[test]
    fn unknown_shells_are_not_guessed_at() {
        // Sending bash syntax to something else just prints errors at the user.
        assert_eq!(Shell::detect("/bin/sh"), None);
        assert_eq!(Shell::detect("/usr/bin/pwsh"), None);
        assert_eq!(Shell::detect("cmd.exe"), None);
        assert_eq!(Shell::detect(""), None);
    }

    #[test]
    fn bash_can_be_instrumented_through_the_environment() {
        // Which is what spares a local bash the echoed snippet entirely.
        let (name, value) = install_env(Shell::Bash).expect("bash should have an env route");
        assert_eq!(name, "PROMPT_COMMAND");
        assert!(value.contains("133;D"));
        assert!(value.contains("]7;file://"));
        assert!(value.contains("133;A"));
        // It is a command list, not a function definition, so it must not span lines.
        assert!(!value.contains('\n'));
    }

    #[test]
    fn shells_without_an_environment_hook_fall_back_to_typing() {
        // zsh's hook is an array and fish's is an event handler; neither can be set
        // from the environment, so they must report that rather than pretend.
        assert!(install_env(Shell::Zsh).is_none());
        assert!(install_env(Shell::Fish).is_none());
    }

    #[test]
    fn every_snippet_is_one_runnable_line() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let command = install_command(shell);
            assert!(
                command.ends_with('\n'),
                "{shell:?} snippet would sit unrun on the prompt"
            );
            assert_eq!(
                command.matches('\n').count(),
                1,
                "{shell:?} snippet spans lines; it is typed into a live shell"
            );
        }
    }

    #[test]
    fn every_snippet_starts_with_a_space_to_stay_out_of_history() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            assert!(
                install_command(shell).starts_with(' '),
                "{shell:?} snippet would be recorded in shell history"
            );
        }
    }

    #[test]
    fn every_snippet_reports_all_three_sequences() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let command = install_command(shell);
            assert!(
                command.contains("133;D"),
                "{shell:?} never reports command completion"
            );
            assert!(
                command.contains("]7;file://"),
                "{shell:?} never reports the directory"
            );
            assert!(
                command.contains("133;A"),
                "{shell:?} never marks the prompt"
            );
        }
    }

    /// What the bash snippet prints, with the shell's expansions already applied.
    ///
    /// The snippet itself cannot be run here without a shell, so this stands in for its
    /// output and proves the sniffer understands the shape it produces.
    fn simulated_report(host: &str, cwd: &str, status: i32) -> Vec<u8> {
        format!("\x1b]133;D;{status}\x07\x1b]7;file://{host}{cwd}\x07\x1b]133;A\x07").into_bytes()
    }

    #[test]
    fn the_sniffer_understands_what_the_snippet_prints() {
        // The two halves of shell integration have to agree, so this checks them
        // against each other rather than each in isolation.
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(&simulated_report("myhost", "/srv/app", 0), |event| {
            events.push(event)
        });

        assert_eq!(
            events,
            vec![
                ShellEvent::CommandEnd {
                    exit_status: Some(0)
                },
                ShellEvent::CwdChanged {
                    host: Some("myhost".into()),
                    path: "/srv/app".into()
                },
                ShellEvent::PromptStart,
            ]
        );
    }

    #[test]
    fn a_failing_command_reports_its_status() {
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(&simulated_report("h", "/tmp", 127), |event| {
            events.push(event)
        });
        assert!(events.contains(&ShellEvent::CommandEnd {
            exit_status: Some(127)
        }));
    }

    #[test]
    fn a_directory_containing_a_percent_survives_the_round_trip() {
        // The snippet doubles `%` so the decoder cannot mistake a literal one for the
        // start of an escape; without it, `/tmp/100%done` would decode as garbage.
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(&simulated_report("h", "/tmp/100%25done", 0), |event| {
            events.push(event)
        });

        assert!(events.contains(&ShellEvent::CwdChanged {
            host: Some("h".into()),
            path: "/tmp/100%done".into()
        }));
    }

    #[test]
    fn a_directory_containing_a_space_survives_the_round_trip() {
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(&simulated_report("h", "/tmp/my dir", 0), |event| {
            events.push(event)
        });

        assert!(events.contains(&ShellEvent::CwdChanged {
            host: Some("h".into()),
            path: "/tmp/my dir".into()
        }));
    }
}
