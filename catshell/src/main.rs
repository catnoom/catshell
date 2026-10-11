//! catshell: a terminal and file explorer that share one connection.

mod app;
mod config;
mod debug;
mod explorer;
mod font;
mod input;
mod pane;
mod render;
mod ssh;

use config::Config;

/// What `--version` prints, taken from the manifest at compile time.
const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
catshell: a terminal and file explorer that share one connection.

Usage: catshell [OPTIONS]

Options:
  -V, --version  Print the version and exit
  -h, --help     Print this message and exit

Hosts live in catshell/hosts.toml and settings in catshell/config.toml,
under the user config directory. Set RUST_LOG=catshell=debug for verbose logging.
";

/// Answer `--version` and `--help` before opening a window.
///
/// A downloaded binary has to be able to say which version it is, and it has to be able
/// to say so on a machine with no display — which is also what lets the release
/// pipeline smoke-test the artifact it just built. Returning `true` means the argument
/// has been answered and the GUI should not start.
fn answered_on_the_command_line(args: impl Iterator<Item = String>) -> bool {
    for arg in args {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("catshell {VERSION}");
                return true;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return true;
            }
            // Anything else is left alone: eframe and winit read arguments of their
            // own, and rejecting unknown ones here would break them.
            _ => {}
        }
    }
    false
}

fn main() -> anyhow::Result<()> {
    if answered_on_the_command_line(std::env::args().skip(1)) {
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "catshell=info".into()),
        )
        .init();

    let config = Config::load();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("catshell")
            .with_inner_size([1024.0, 640.0])
            .with_min_inner_size([320.0, 200.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "catshell",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, config)?))),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(args: &[&str]) -> bool {
        answered_on_the_command_line(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn version_and_help_are_answered_without_a_window() {
        // The release pipeline runs `catshell --version` on a machine with no display
        // to prove the binary it built actually starts.
        assert!(answered(&["--version"]));
        assert!(answered(&["-V"]));
        assert!(answered(&["--help"]));
        assert!(answered(&["-h"]));
    }

    #[test]
    fn no_arguments_starts_the_interface() {
        assert!(!answered(&[]));
    }

    #[test]
    fn unknown_arguments_are_left_for_winit() {
        // eframe and winit read arguments of their own, so an unrecognised one is not
        // an error here.
        assert!(!answered(&["--some-winit-flag"]));
    }
}
