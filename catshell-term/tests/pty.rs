//! End-to-end tests for the local PTY driver: a real shell, a real pseudoterminal.
//!
//! These exercise the whole pump — spawn, read, parse, shell-integration sniffing,
//! write, resize and exit — which no unit test of the pieces can cover.

#![cfg(unix)]

use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config as TermConfig;
use catshell_term::palette::Palette;
use catshell_term::pty::{spawn, LocalShellOptions};
use catshell_term::session::{ExitReason, GridSize, Session, SessionEvent};
use catshell_term::ShellEvent;

const TIMEOUT: Duration = Duration::from_secs(10);

fn window_size(size: GridSize) -> WindowSize {
    WindowSize {
        num_lines: size.screen_lines as u16,
        num_cols: size.columns as u16,
        cell_width: 8,
        cell_height: 16,
    }
}

fn run(script: &str, size: GridSize) -> Session {
    let options = LocalShellOptions {
        shell: Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
        ..Default::default()
    };
    spawn(
        options,
        TermConfig::default(),
        size,
        window_size(size),
        Palette::default(),
        None,
    )
    .expect("spawn shell")
}

/// Collect events until `done` is satisfied or the timeout expires.
fn collect_until(
    session: &Session,
    mut done: impl FnMut(&[SessionEvent]) -> bool,
) -> Vec<SessionEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        seen.extend(session.drain_events());
        if done(&seen) {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out; events so far: {seen:?}");
}

fn wait_for_exit(session: &Session) -> ExitReason {
    let events = collect_until(session, |seen| {
        seen.iter().any(|e| matches!(e, SessionEvent::Exited(_)))
    });
    events
        .into_iter()
        .find_map(|e| match e {
            SessionEvent::Exited(reason) => Some(reason),
            _ => None,
        })
        .unwrap()
}

/// The visible grid as text, one line per row, trailing blanks trimmed.
fn screen(session: &Session) -> String {
    let term = session.term().lock();
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|line| {
            let row: String = (0..grid.columns())
                .map(|col| grid[Line(line as i32)][Column(col)].c)
                .collect();
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

#[test]
fn shell_output_reaches_the_grid() {
    let session = run("printf 'hello from catshell'", GridSize::new(80, 24));
    wait_for_exit(&session);
    assert!(
        screen(&session).contains("hello from catshell"),
        "got: {:?}",
        screen(&session)
    );
}

#[test]
fn exit_status_is_reported() {
    let session = run("exit 42", GridSize::new(80, 24));
    assert_eq!(wait_for_exit(&session), ExitReason::Status(Some(42)));
}

#[test]
fn output_written_just_before_exit_is_not_lost() {
    // The child can exit before the pump has read its last write; `drain_on_exit`
    // plus the final read in the pump is what keeps that output.
    let session = run("printf 'last words'; exit 0", GridSize::new(80, 24));
    wait_for_exit(&session);
    assert!(
        screen(&session).contains("last words"),
        "got: {:?}",
        screen(&session)
    );
}

#[test]
fn osc7_from_the_shell_becomes_a_shell_event() {
    // Exactly what a shell-integration prompt hook emits.
    let session = run(
        r"printf '\033]7;file://myhost/tmp/somewhere\007'",
        GridSize::new(80, 24),
    );
    let events = collect_until(&session, |seen| {
        seen.iter()
            .any(|e| matches!(e, SessionEvent::Shell(ShellEvent::CwdChanged { .. })))
    });
    let cwd = events
        .iter()
        .find_map(|e| match e {
            SessionEvent::Shell(ShellEvent::CwdChanged { path, .. }) => Some(path.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(cwd, "/tmp/somewhere");
}

#[test]
fn osc133_command_marks_become_shell_events() {
    let session = run(
        r"printf '\033]133;C\007done\033]133;D;3\007'",
        GridSize::new(80, 24),
    );
    let events = collect_until(&session, |seen| {
        seen.iter()
            .any(|e| matches!(e, SessionEvent::Shell(ShellEvent::CommandEnd { .. })))
    });
    let shell: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Shell(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        shell,
        vec![
            ShellEvent::CommandStart,
            ShellEvent::CommandEnd {
                exit_status: Some(3)
            }
        ]
    );
}

#[test]
fn title_changes_are_reported() {
    let session = run(r"printf '\033]0;my title\007'", GridSize::new(80, 24));
    collect_until(&session, |seen| {
        seen.iter()
            .any(|e| matches!(e, SessionEvent::Title(Some(t)) if t == "my title"))
    });
}

#[test]
fn input_written_by_the_ui_reaches_the_shell() {
    // `read` blocks until the pump delivers what we write, so this only passes if the
    // write path and the poller wakeup both work.
    let session = run(
        "read line; printf 'got:%s' \"$line\"",
        GridSize::new(80, 24),
    );
    session.write("ping\n".as_bytes().to_vec());
    wait_for_exit(&session);
    assert!(
        screen(&session).contains("got:ping"),
        "got: {:?}",
        screen(&session)
    );
}

#[test]
fn resize_is_visible_to_the_child() {
    // The shell reports the window size the kernel knows about, which only matches if
    // the resize reached the PTY via TIOCSWINSZ and not merely the grid.
    let size = GridSize::new(80, 24);
    let session = run("sleep 0.4; printf 'cols=%s' \"$(tput cols)\"", size);

    let resized = GridSize::new(120, 40);
    session.resize(resized, window_size(resized));

    wait_for_exit(&session);
    assert!(
        screen(&session).contains("cols=120"),
        "got: {:?}",
        screen(&session)
    );
    assert_eq!(session.term().lock().grid().columns(), 120);
}

#[test]
fn shutdown_ends_the_session() {
    let session = run("sleep 60", GridSize::new(80, 24));
    session.shutdown();
    // Must return promptly rather than waiting out the sleep.
    let started = Instant::now();
    wait_for_exit(&session);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown took {:?}",
        started.elapsed()
    );
}

#[test]
fn large_output_is_fully_consumed() {
    // Enough to span many read buffers and exercise the MAX_LOCKED_READ break.
    let size = GridSize::new(80, 24);
    let session = run(
        "i=0; while [ $i -lt 5000 ]; do echo \"line $i\"; i=$((i+1)); done",
        size,
    );
    wait_for_exit(&session);
    assert!(
        screen(&session).contains("line 4999"),
        "got tail: {:?}",
        screen(&session)
    );
}

// --- Shell integration -----------------------------------------------------------
//
// The snippets in `catshell_term::integration` are shell code, so the only way to know
// they are correct is to run them in the shell they target. These tests drive a real
// bash on a real pseudoterminal and assert the events come back out.

use catshell_term::integration::{install_command, Shell};

/// Start an interactive bash with a predictable environment.
///
/// `--norc` keeps the user's own configuration out of it, so the test measures the
/// snippet rather than whatever is in someone's `.bashrc`.
fn interactive_bash(size: GridSize) -> Session {
    let options = LocalShellOptions {
        shell: Some((
            "/bin/bash".into(),
            vec!["--norc".into(), "--noprofile".into(), "-i".into()],
        )),
        ..Default::default()
    };
    spawn(
        options,
        TermConfig::default(),
        size,
        window_size(size),
        Palette::default(),
        None,
    )
    .expect("spawn bash")
}

/// Collect shell-integration events until `done` is satisfied.
fn wait_for_shell_events(
    session: &Session,
    what: &str,
    done: impl Fn(&[ShellEvent]) -> bool,
) -> Vec<ShellEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        for event in session.drain_events() {
            if let SessionEvent::Shell(shell) = event {
                seen.push(shell);
            }
        }
        if done(&seen) {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}; shell events so far: {seen:?}");
}

fn latest_cwd(events: &[ShellEvent]) -> Option<&str> {
    events.iter().rev().find_map(|event| match event {
        ShellEvent::CwdChanged { path, .. } => Some(path.as_str()),
        _ => None,
    })
}

#[test]
fn the_bash_snippet_reports_the_working_directory() {
    // The whole file-explorer sync rests on this actually running in bash.
    let session = interactive_bash(GridSize::new(80, 24));
    session.write(install_command(Shell::Bash).into_bytes());
    session.write(b"cd /tmp\n".to_vec());

    let events = wait_for_shell_events(&session, "a cwd report from /tmp", |seen| {
        latest_cwd(seen) == Some("/tmp")
    });
    assert_eq!(latest_cwd(&events), Some("/tmp"));
}

#[test]
fn the_bash_snippet_follows_the_shell_as_it_moves() {
    let session = interactive_bash(GridSize::new(80, 24));
    session.write(install_command(Shell::Bash).into_bytes());

    session.write(b"cd /usr\n".to_vec());
    wait_for_shell_events(&session, "/usr", |seen| latest_cwd(seen) == Some("/usr"));

    session.write(b"cd /usr/share\n".to_vec());
    wait_for_shell_events(&session, "/usr/share", |seen| {
        latest_cwd(seen) == Some("/usr/share")
    });

    // And back up again, which is the `..` case the explorer has to follow too.
    session.write(b"cd ..\n".to_vec());
    wait_for_shell_events(&session, "/usr again", |seen| {
        latest_cwd(seen) == Some("/usr")
    });
}

#[test]
fn the_bash_snippet_reports_when_a_command_finishes() {
    // This is what triggers the explorer to refresh, so it must carry the real status.
    let session = interactive_bash(GridSize::new(80, 24));
    session.write(install_command(Shell::Bash).into_bytes());
    session.write(b"(exit 42)\n".to_vec());

    let events = wait_for_shell_events(&session, "a command completion with status 42", |seen| {
        seen.iter().any(|e| {
            matches!(
                e,
                ShellEvent::CommandEnd {
                    exit_status: Some(42)
                }
            )
        })
    });
    assert!(events.iter().any(|e| matches!(
        e,
        ShellEvent::CommandEnd {
            exit_status: Some(42)
        }
    )));
}

#[test]
fn the_bash_snippet_survives_a_directory_containing_a_space() {
    let session = interactive_bash(GridSize::new(80, 24));
    session.write(install_command(Shell::Bash).into_bytes());
    session.write(b"mkdir -p '/tmp/catshell test dir' && cd '/tmp/catshell test dir'\n".to_vec());

    let events = wait_for_shell_events(&session, "a cwd with a space in it", |seen| {
        latest_cwd(seen) == Some("/tmp/catshell test dir")
    });
    assert_eq!(latest_cwd(&events), Some("/tmp/catshell test dir"));
    let _ = std::fs::remove_dir_all("/tmp/catshell test dir");
}

#[test]
fn the_bash_snippet_survives_a_directory_containing_a_percent() {
    // `%` is the one character that must be escaped, because OSC 7 paths are
    // percent-encoded and a literal one would decode as garbage.
    let session = interactive_bash(GridSize::new(80, 24));
    session.write(install_command(Shell::Bash).into_bytes());
    session.write(b"mkdir -p '/tmp/catshell-100%done' && cd '/tmp/catshell-100%done'\n".to_vec());

    let events = wait_for_shell_events(&session, "a cwd with a percent in it", |seen| {
        latest_cwd(seen) == Some("/tmp/catshell-100%done")
    });
    assert_eq!(latest_cwd(&events), Some("/tmp/catshell-100%done"));
    let _ = std::fs::remove_dir_all("/tmp/catshell-100%done");
}

#[test]
fn installing_the_snippet_keeps_any_existing_prompt_command() {
    // Users have their own PROMPT_COMMAND; replacing it outright would break their
    // prompt, so the snippet has to chain onto whatever is already there.
    let session = interactive_bash(GridSize::new(80, 24));
    session.write(b"PROMPT_COMMAND='printf \"[pre-existing]\"'\n".to_vec());
    session.write(install_command(Shell::Bash).into_bytes());
    session.write(b"cd /tmp\n".to_vec());

    wait_for_shell_events(&session, "a cwd report", |seen| {
        latest_cwd(seen) == Some("/tmp")
    });
    assert!(
        screen(&session).contains("[pre-existing]"),
        "the original PROMPT_COMMAND stopped running:\n{}",
        screen(&session)
    );
}

#[test]
fn bash_reports_its_directory_from_the_environment_with_nothing_typed() {
    // The local path: the hook arrives in bash's environment, so nothing is echoed into
    // the user's terminal and the first prompt already reports where it is.
    let (name, value) =
        catshell_term::integration::install_env(Shell::Bash).expect("an env route for bash");
    let mut env = std::collections::HashMap::new();
    env.insert(name.to_string(), value);

    let size = GridSize::new(80, 24);
    let session = spawn(
        LocalShellOptions {
            shell: Some((
                "/bin/bash".into(),
                vec!["--norc".into(), "--noprofile".into(), "-i".into()],
            )),
            env,
            ..Default::default()
        },
        TermConfig::default(),
        size,
        window_size(size),
        Palette::default(),
        None,
    )
    .expect("spawn bash");

    // No install command is written — the environment already carried it.
    session.write(b"cd /tmp\n".to_vec());
    let events = wait_for_shell_events(&session, "a cwd report from /tmp", |seen| {
        latest_cwd(seen) == Some("/tmp")
    });
    assert_eq!(latest_cwd(&events), Some("/tmp"));

    // And nothing resembling the snippet was ever echoed to the screen.
    assert!(
        !screen(&session).contains("__catshell"),
        "the hook was echoed into the terminal:\n{}",
        screen(&session)
    );
}
