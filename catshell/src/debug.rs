//! A self-contained way to see what the app actually draws.
//!
//! The terminal renders through a custom wgpu pass, so a unit test of the quad list
//! proves the geometry is right but not that anything reaches the screen. This drives a
//! real window, feeds a script to the shell, captures the framebuffer and exits — which
//! is what makes "it renders" checkable rather than assumed, on a machine with no
//! screenshot tool and in CI.
//!
//! Off unless `CATSHELL_SCREENSHOT` names an output path.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long the shell gets to start up and run the script before the capture.
const SETTLE: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone)]
pub struct ScreenshotRequest {
    pub path: PathBuf,
    /// Typed into every pane once they are running.
    pub script: Option<String>,
    /// Splits to make before capturing, as a comma-separated list of `h` and `v`.
    pub splits: Vec<Split>,
    /// Hosts to connect to before capturing, comma-separated. Naming the same host
    /// twice exercises the path where a second pane reuses the existing transport.
    pub connect: Vec<String>,
    /// Password to answer a password prompt with.
    pub password: Option<String>,
    /// Answer "yes" to an unknown-host prompt. Off by default, so the prompt itself can
    /// be captured.
    pub trust_host: bool,
    /// Open the host editor before capturing.
    pub open_host_editor: bool,
}

/// Which way to split, kept separate from the app's own `Direction` so this debug-only
/// module does not widen that type's public surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Horizontal,
    Vertical,
}

impl ScreenshotRequest {
    /// Read the request from the environment, if there is one.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("CATSHELL_SCREENSHOT")?;
        Some(Self {
            path: PathBuf::from(path),
            script: std::env::var("CATSHELL_SCRIPT").ok(),
            splits: std::env::var("CATSHELL_SPLITS")
                .map(|value| parse_splits(&value))
                .unwrap_or_default(),
            connect: std::env::var("CATSHELL_CONNECT")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            password: std::env::var("CATSHELL_PASSWORD").ok(),
            trust_host: std::env::var("CATSHELL_TRUST").is_ok_and(|value| value != "0"),
            open_host_editor: std::env::var("CATSHELL_HOST_EDITOR").is_ok(),
        })
    }
}

fn parse_splits(value: &str) -> Vec<Split> {
    value
        .split(',')
        .filter_map(|part| match part.trim() {
            "h" => Some(Split::Horizontal),
            "v" => Some(Split::Vertical),
            _ => None,
        })
        .collect()
}

/// Tracks the capture across frames.
pub struct Screenshotter {
    request: ScreenshotRequest,
    deadline: Instant,
    splits_done: bool,
    connect_done: bool,
    script_sent: bool,
    requested: bool,
}

impl Screenshotter {
    pub fn new(request: ScreenshotRequest) -> Self {
        Self {
            deadline: Instant::now() + SETTLE,
            request,
            splits_done: false,
            connect_done: false,
            script_sent: false,
            requested: false,
        }
    }

    /// The hosts to connect to, the first time this is asked.
    pub fn take_connect(&mut self) -> Vec<String> {
        if self.connect_done {
            return Vec::new();
        }
        self.connect_done = true;
        self.request.connect.clone()
    }

    /// The password to answer a prompt with.
    pub fn password(&self) -> Option<&str> {
        self.request.password.as_deref()
    }

    /// Whether to answer an unknown-host prompt affirmatively.
    pub fn trusts_host(&self) -> bool {
        self.request.trust_host
    }

    /// Whether the host editor should be opened, the first time this is asked.
    pub fn take_open_host_editor(&mut self) -> bool {
        let wanted = self.request.open_host_editor;
        self.request.open_host_editor = false;
        wanted
    }

    /// Give the connection longer to complete before capturing.
    pub fn extend(&mut self, by: Duration) {
        self.deadline = self.deadline.max(Instant::now() + by);
    }

    /// The splits to make, the first time this is asked.
    pub fn take_splits(&mut self) -> Vec<Split> {
        if self.splits_done {
            return Vec::new();
        }
        self.splits_done = true;
        self.request.splits.clone()
    }

    /// The script to type this frame, if it is time to type it.
    pub fn take_script(&mut self) -> Option<String> {
        if self.script_sent {
            return None;
        }
        self.script_sent = true;
        self.request
            .script
            .clone()
            .map(|script| format!("{script}\n"))
    }

    /// Ask for the capture once the shell has had time to draw.
    ///
    /// Repaints are requested while waiting; otherwise the app would go idle by design
    /// and the deadline would never be reached.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if self.requested {
            return;
        }
        if Instant::now() < self.deadline {
            ctx.request_repaint();
            return;
        }
        self.requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
    }

    /// Write out the captured image and report whether the app should now exit.
    pub fn handle_events(&self, ctx: &egui::Context) -> bool {
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });

        let Some(image) = image else { return false };
        match write_png(&self.request.path, &image) {
            Ok(()) => tracing::info!("wrote {}", self.request.path.display()),
            Err(err) => tracing::error!("could not write screenshot: {err}"),
        }
        true
    }
}

/// Write an RGBA image as a PNG.
///
/// Hand-rolled rather than pulling in an image crate: this is debug-only code, and a
/// dependency that ships in every build to serve it would be a poor trade.
fn write_png(path: &std::path::Path, image: &egui::ColorImage) -> std::io::Result<()> {
    use std::io::Write as _;

    let (width, height) = (image.width() as u32, image.height() as u32);

    // Raw scanlines, each prefixed with a filter-type byte (0 = none).
    let mut raw = Vec::with_capacity((height * (1 + width * 4)) as usize);
    for y in 0..height as usize {
        raw.push(0);
        for x in 0..width as usize {
            let px = image[(x, y)];
            raw.extend_from_slice(&[px.r(), px.g(), px.b(), px.a()]);
        }
    }

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // 8-bit RGBA, no interlacing.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut png, b"IEND", &[]);

    let mut file = std::fs::File::create(path)?;
    file.write_all(&png)
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// Wrap bytes in a zlib stream using only uncompressed ("stored") deflate blocks.
///
/// Larger on disk than real compression, but it needs no compressor and every PNG
/// reader accepts it.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // zlib header: deflate, 32K window, no dictionary.
                                    // A stored block's length field is 16 bits, so long data spans several blocks.
    let mut chunks = data.chunks(0xffff).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        let final_block = chunks.peek().is_none();
        out.push(u8::from(final_block));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            // The reversed CRC-32 polynomial, as PNG specifies.
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_are_parsed_from_the_environment_value() {
        assert_eq!(
            parse_splits("h,v"),
            vec![Split::Horizontal, Split::Vertical]
        );
        assert_eq!(
            parse_splits(" h , h "),
            vec![Split::Horizontal, Split::Horizontal]
        );
        assert!(parse_splits("").is_empty());
        assert!(parse_splits("nonsense").is_empty());
    }

    #[test]
    fn crc32_matches_the_known_check_value() {
        // The standard CRC-32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn adler32_matches_the_known_check_value() {
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn stored_blocks_span_long_data() {
        // Longer than one 16-bit block, so it must be split and only the last marked final.
        let data = vec![0u8; 0x20000];
        let stream = zlib_stored(&data);
        assert_eq!(&stream[..2], &[0x78, 0x01]);
        // Three blocks: two full, one remainder, only the last with the final bit set.
        assert_eq!(stream[2], 0);
        assert_eq!(stream[2 + 5 + 0xffff], 0);
        assert_eq!(stream[2 + 2 * (5 + 0xffff)], 1);
    }

    #[test]
    fn a_written_png_has_the_expected_structure() {
        let image = egui::ColorImage::new([2, 2], vec![egui::Color32::RED; 4]);
        let path = std::env::temp_dir().join("catshell-png-test.png");
        write_png(&path, &image).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        assert_eq!(&bytes[12..16], b"IHDR");
        assert_eq!(&bytes[16..20], &2u32.to_be_bytes());
        assert!(
            bytes.ends_with(&[0xae, 0x42, 0x60, 0x82]),
            "missing IEND checksum"
        );
        let _ = std::fs::remove_file(&path);
    }
}
