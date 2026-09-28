//! Throughput of the whole read path: PTY read, OSC sniffing, VT parsing and grid update.
//!
//! Ignored by default — it is a measurement, not a pass/fail assertion, and timings vary
//! too much between machines to be worth failing a build over. Run it with:
//!
//! ```text
//! cargo test --release -p catshell-term --test throughput -- --ignored --nocapture
//! ```
//!
//! The number to care about is MB/s: a terminal that cannot outpace `cat` on a large
//! file is one that feels slow, which is the complaint catshell exists to answer.

#![cfg(unix)]

use std::io::Write as _;
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::term::Config;
use catshell_term::palette::Palette;
use catshell_term::pty::{spawn, LocalShellOptions};
use catshell_term::session::{GridSize, SessionEvent};

/// Enough to run for a measurable time without the setup dominating.
const MEGABYTES: usize = 50;

#[test]
#[ignore = "measurement, not an assertion; run with --ignored --nocapture"]
fn measure_throughput() {
    let path = std::env::temp_dir().join("catshell-throughput.txt");
    let bytes = write_sample(&path);

    let size = GridSize::new(200, 50);
    let window_size = WindowSize {
        num_lines: 50,
        num_cols: 200,
        cell_width: 8,
        cell_height: 16,
    };
    let options = LocalShellOptions {
        shell: Some((
            "/bin/sh".into(),
            vec!["-c".into(), format!("cat {}", path.display())],
        )),
        ..Default::default()
    };
    let session = spawn(
        options,
        Config::default(),
        size,
        window_size,
        Palette::default(),
        None,
    )
    .unwrap();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(120);
    loop {
        if session
            .drain_events()
            .iter()
            .any(|event| matches!(event, SessionEvent::Exited(_)))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the terminal did not finish reading in 120s"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    let elapsed = start.elapsed();
    println!(
        "THROUGHPUT: {:.1} MB in {:.2}s = {:.0} MB/s",
        bytes as f64 / 1e6,
        elapsed.as_secs_f64(),
        bytes as f64 / 1e6 / elapsed.as_secs_f64()
    );
    let _ = std::fs::remove_file(&path);
}

/// Write a file of printable text in terminal-width lines, and return its size.
fn write_sample(path: &std::path::Path) -> usize {
    let mut line: String = ('!'..='~').cycle().take(199).collect();
    line.push('\n');

    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let target = MEGABYTES * 1_000_000;
    let mut written = 0;
    while written < target {
        file.write_all(line.as_bytes()).unwrap();
        written += line.len();
    }
    file.flush().unwrap();
    written
}
