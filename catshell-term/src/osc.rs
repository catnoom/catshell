//! Out-of-band OSC scanning for shell integration.
//!
//! `vte`'s ANSI processor drops the sequences we need: its `osc_dispatch` only routes
//! OSC 0/2, 4, 8, 10/11/12, 22, 50, 52, 104 and 110/111/112, and `vte::ansi::Handler`
//! has no hook for a working directory. OSC 7 (cwd) and OSC 133 (prompt marks) never
//! reach the terminal at all, so we scan for them ourselves.
//!
//! [`OscSniffer::feed`] observes a byte slice *without consuming or altering it* — the
//! caller passes the very same slice on to the ANSI processor afterwards. Cost on
//! ordinary text is one SIMD scan for `ESC`, which is why this can sit in the hot path.
//!
//! The sniffer is resumable: PTY and SSH reads split wherever they like, including in
//! the middle of an escape sequence, so all state lives in the struct.

use std::mem;

/// Longest OSC payload we retain. Anything past this is still consumed (so we stay in
/// sync with the stream) but not buffered; the sequence is then dropped. The sequences
/// we care about are a few hundred bytes at most, while OSC 52 clipboard payloads can
/// be enormous — this keeps a hostile or noisy stream from growing the buffer.
const MAX_OSC_LEN: usize = 4096;

const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;

/// A shell-integration event recovered from the output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// OSC 7: the shell reported its working directory.
    ///
    /// The path is kept as a `String`, not a `PathBuf`: it may name a remote host's
    /// filesystem, whose separators and root have nothing to do with the local platform.
    CwdChanged { host: Option<String>, path: String },
    /// OSC 133;A — the prompt is about to be drawn.
    PromptStart,
    /// OSC 133;B — the prompt has been drawn; user input begins here.
    PromptEnd,
    /// OSC 133;C — the shell is handing off to a command.
    CommandStart,
    /// OSC 133;D — the command finished, with its exit status when the shell reports one.
    CommandEnd { exit_status: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Ordinary output; scanning for `ESC`.
    Ground,
    /// Saw `ESC`, waiting to see whether `]` follows.
    Escape,
    /// Inside an OSC payload, accumulating until a terminator.
    Osc,
    /// Inside an OSC payload and saw `ESC`; `\` terminates, anything else aborts.
    OscEscape,
}

/// Incremental scanner for OSC 7 and OSC 133 sequences.
#[derive(Debug)]
pub struct OscSniffer {
    state: State,
    buf: Vec<u8>,
    /// Set when the payload outgrew [`MAX_OSC_LEN`], so it is discarded at the terminator.
    overflowed: bool,
}

impl Default for OscSniffer {
    fn default() -> Self {
        Self::new()
    }
}

impl OscSniffer {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Drop any partially accumulated sequence and return to ground.
    ///
    /// Call this when the underlying stream is replaced (reconnect, shell restart), so a
    /// truncated sequence from the old stream cannot merge with the new one.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.overflowed = false;
    }

    /// Scan `bytes`, invoking `on_event` for each recognised sequence.
    ///
    /// `bytes` is only read; the caller still owns it and must forward it unchanged to
    /// the ANSI processor.
    pub fn feed<F: FnMut(ShellEvent)>(&mut self, bytes: &[u8], mut on_event: F) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => {
                    // The fast path: skip to the next ESC in one SIMD-accelerated scan.
                    match memchr::memchr(ESC, &bytes[i..]) {
                        Some(off) => {
                            i += off + 1;
                            self.state = State::Escape;
                        }
                        None => return,
                    }
                }
                State::Escape => {
                    // Only OSC interests us; every other introducer goes back to ground
                    // and is left entirely to the ANSI processor. A second ESC restarts
                    // the escape, matching how the terminal itself resolves it.
                    match bytes[i] {
                        b']' => {
                            self.buf.clear();
                            self.overflowed = false;
                            self.state = State::Osc;
                            i += 1;
                        }
                        ESC => i += 1,
                        _ => {
                            self.state = State::Ground;
                            i += 1;
                        }
                    }
                }
                State::Osc => {
                    // Consume up to whichever terminator appears first.
                    let rest = &bytes[i..];
                    let stop = memchr::memchr2(BEL, ESC, rest).unwrap_or(rest.len());
                    self.push(&rest[..stop]);
                    i += stop;
                    if stop < rest.len() {
                        let terminator = rest[stop];
                        i += 1;
                        if terminator == BEL {
                            self.finish(&mut on_event);
                        } else {
                            self.state = State::OscEscape;
                        }
                    }
                }
                State::OscEscape => {
                    // `ESC \` (ST) ends the sequence. Any other byte means the OSC was
                    // interrupted by a new escape sequence, so abandon it.
                    if bytes[i] == b'\\' {
                        i += 1;
                        self.finish(&mut on_event);
                    } else {
                        self.buf.clear();
                        self.overflowed = false;
                        self.state = State::Escape;
                    }
                }
            }
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.buf.len() + chunk.len() > MAX_OSC_LEN {
            self.overflowed = true;
            self.buf.clear();
            return;
        }
        self.buf.extend_from_slice(chunk);
    }

    fn finish<F: FnMut(ShellEvent)>(&mut self, on_event: &mut F) {
        self.state = State::Ground;
        let overflowed = mem::replace(&mut self.overflowed, false);
        let buf = mem::take(&mut self.buf);
        if !overflowed {
            if let Some(event) = parse_osc(&buf) {
                on_event(event);
            }
        }
        // Reuse the allocation across sequences.
        self.buf = buf;
        self.buf.clear();
    }
}

/// Interpret an OSC payload (the bytes between `ESC ]` and the terminator).
fn parse_osc(payload: &[u8]) -> Option<ShellEvent> {
    let (code, rest) = split_once(payload, b';')?;
    match code {
        b"7" => parse_cwd(rest),
        b"133" => parse_prompt_mark(rest),
        _ => None,
    }
}

/// OSC 7 carries `file://<host>/<path>`, percent-encoded.
fn parse_cwd(rest: &[u8]) -> Option<ShellEvent> {
    let after_scheme = strip_prefix_ignore_ascii_case(rest, b"file://")?;
    // The authority runs to the first `/`, which also begins the (absolute) path.
    let slash = memchr::memchr(b'/', after_scheme)?;
    let host = &after_scheme[..slash];
    let path = percent_decode(&after_scheme[slash..]);
    let path = String::from_utf8(path).ok()?;
    if path.is_empty() {
        return None;
    }
    let host = if host.is_empty() {
        None
    } else {
        Some(percent_decode(host)).and_then(|h| String::from_utf8(h).ok())
    };
    Some(ShellEvent::CwdChanged { host, path })
}

/// OSC 133 marks: `A` prompt start, `B` prompt end, `C` command start, `D[;status]` end.
fn parse_prompt_mark(rest: &[u8]) -> Option<ShellEvent> {
    let (kind, params) = match split_once(rest, b';') {
        Some((kind, params)) => (kind, Some(params)),
        None => (rest, None),
    };
    match kind {
        b"A" => Some(ShellEvent::PromptStart),
        b"B" => Some(ShellEvent::PromptEnd),
        b"C" => Some(ShellEvent::CommandStart),
        b"D" => {
            // The status is optional, and shells append their own `key=value` extras
            // after it which we ignore.
            let exit_status = params
                .map(|p| match split_once(p, b';') {
                    Some((first, _)) => first,
                    None => p,
                })
                .filter(|s| !s.is_empty())
                .and_then(|s| std::str::from_utf8(s).ok()?.trim().parse::<i32>().ok());
            Some(ShellEvent::CommandEnd { exit_status })
        }
        _ => None,
    }
}

fn split_once(haystack: &[u8], needle: u8) -> Option<(&[u8], &[u8])> {
    let idx = memchr::memchr(needle, haystack)?;
    Some((&haystack[..idx], &haystack[idx + 1..]))
}

fn strip_prefix_ignore_ascii_case<'a>(haystack: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    if haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&haystack[prefix.len()..])
    } else {
        None
    }
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(hi), Some(lo)) = (hex_val(input[i + 1]), hex_val(input[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed the whole input in one go.
    fn scan(input: &[u8]) -> Vec<ShellEvent> {
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(input, |e| events.push(e));
        events
    }

    /// Feed the input one byte at a time through a single sniffer, which forces every
    /// possible split point through the resumable path.
    fn scan_byte_by_byte(input: &[u8]) -> Vec<ShellEvent> {
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        for b in input {
            sniffer.feed(&[*b], |e| events.push(e));
        }
        events
    }

    /// Every chunking must produce the same events as feeding the input whole.
    fn scan_all_ways(input: &[u8]) -> Vec<ShellEvent> {
        let whole = scan(input);
        assert_eq!(scan_byte_by_byte(input), whole, "byte-by-byte differed");
        for split in 1..input.len() {
            let mut sniffer = OscSniffer::new();
            let mut events = Vec::new();
            sniffer.feed(&input[..split], |e| events.push(e));
            sniffer.feed(&input[split..], |e| events.push(e));
            assert_eq!(events, whole, "split at {split} differed");
        }
        whole
    }

    #[test]
    fn plain_text_yields_nothing() {
        assert!(scan_all_ways(b"hello world\r\n").is_empty());
    }

    #[test]
    fn osc7_bel_terminated() {
        assert_eq!(
            scan_all_ways(b"\x1b]7;file://myhost/home/iota\x07"),
            vec![ShellEvent::CwdChanged {
                host: Some("myhost".into()),
                path: "/home/iota".into()
            }]
        );
    }

    #[test]
    fn osc7_st_terminated() {
        assert_eq!(
            scan_all_ways(b"\x1b]7;file://myhost/tmp\x1b\\"),
            vec![ShellEvent::CwdChanged {
                host: Some("myhost".into()),
                path: "/tmp".into()
            }]
        );
    }

    #[test]
    fn osc7_empty_host_is_none() {
        assert_eq!(
            scan_all_ways(b"\x1b]7;file:///var/log\x07"),
            vec![ShellEvent::CwdChanged {
                host: None,
                path: "/var/log".into()
            }]
        );
    }

    #[test]
    fn osc7_percent_decodes_path() {
        assert_eq!(
            scan_all_ways(b"\x1b]7;file://h/home/my%20dir/caf%C3%A9\x07"),
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/home/my dir/café".into()
            }]
        );
    }

    #[test]
    fn osc7_scheme_is_case_insensitive() {
        assert_eq!(
            scan_all_ways(b"\x1b]7;FILE://h/srv\x07"),
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/srv".into()
            }]
        );
    }

    #[test]
    fn osc133_marks() {
        assert_eq!(
            scan_all_ways(b"\x1b]133;A\x07"),
            vec![ShellEvent::PromptStart]
        );
        assert_eq!(
            scan_all_ways(b"\x1b]133;B\x07"),
            vec![ShellEvent::PromptEnd]
        );
        assert_eq!(
            scan_all_ways(b"\x1b]133;C\x07"),
            vec![ShellEvent::CommandStart]
        );
    }

    #[test]
    fn osc133_command_end_status() {
        assert_eq!(
            scan_all_ways(b"\x1b]133;D;0\x07"),
            vec![ShellEvent::CommandEnd {
                exit_status: Some(0)
            }]
        );
        assert_eq!(
            scan_all_ways(b"\x1b]133;D;130\x07"),
            vec![ShellEvent::CommandEnd {
                exit_status: Some(130)
            }]
        );
        // No status reported.
        assert_eq!(
            scan_all_ways(b"\x1b]133;D\x07"),
            vec![ShellEvent::CommandEnd { exit_status: None }]
        );
        // Shells append extras after the status; they must not break parsing.
        assert_eq!(
            scan_all_ways(b"\x1b]133;D;7;aid=42\x07"),
            vec![ShellEvent::CommandEnd {
                exit_status: Some(7)
            }]
        );
    }

    #[test]
    fn ignores_uninteresting_osc_and_csi() {
        assert!(scan_all_ways(b"\x1b]0;my title\x07\x1b[31mred\x1b[0m").is_empty());
        // OSC 52 clipboard payloads must be skipped, not misread.
        assert!(scan_all_ways(b"\x1b]52;c;aGVsbG8=\x07").is_empty());
    }

    #[test]
    fn recognises_sequence_embedded_in_output() {
        let input = b"before\x1b[1mbold\x1b]7;file://h/opt\x07after\r\n";
        assert_eq!(
            scan_all_ways(input),
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/opt".into()
            }]
        );
    }

    #[test]
    fn handles_back_to_back_sequences() {
        let input = b"\x1b]133;D;0\x07\x1b]7;file://h/a\x07\x1b]133;A\x07\x1b]133;B\x07";
        assert_eq!(
            scan_all_ways(input),
            vec![
                ShellEvent::CommandEnd {
                    exit_status: Some(0)
                },
                ShellEvent::CwdChanged {
                    host: Some("h".into()),
                    path: "/a".into()
                },
                ShellEvent::PromptStart,
                ShellEvent::PromptEnd,
            ]
        );
    }

    #[test]
    fn osc_interrupted_by_new_escape_is_abandoned() {
        // An OSC cut short by a CSI must be dropped, and the following OSC still seen.
        let input = b"\x1b]7;file://h/never\x1b[0m\x1b]7;file://h/real\x07";
        assert_eq!(
            scan_all_ways(input),
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/real".into()
            }]
        );
    }

    #[test]
    fn malformed_sequences_are_ignored() {
        assert!(scan_all_ways(b"\x1b]7;not-a-url\x07").is_empty());
        assert!(scan_all_ways(b"\x1b]7;file://hostnoslash\x07").is_empty());
        // A bare root is still a legitimate directory.
        assert_eq!(
            scan_all_ways(b"\x1b]7;file://h/\x07"),
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/".into()
            }]
        );
        assert!(scan_all_ways(b"\x1b]133;Z\x07").is_empty());
        assert!(scan_all_ways(b"\x1b]\x07").is_empty());
    }

    #[test]
    fn oversized_payload_is_dropped_without_desync() {
        let mut input = Vec::from(*b"\x1b]52;c;");
        input.extend(std::iter::repeat_n(b'A', MAX_OSC_LEN * 2));
        input.extend_from_slice(b"\x07");
        // The giant sequence is discarded, and the stream stays in sync well enough to
        // recognise what follows.
        input.extend_from_slice(b"\x1b]7;file://h/after\x07");
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(&input, |e| events.push(e));
        assert_eq!(
            events,
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/after".into()
            }]
        );
    }

    #[test]
    fn reset_discards_partial_sequence() {
        let mut sniffer = OscSniffer::new();
        let mut events = Vec::new();
        sniffer.feed(b"\x1b]7;file://h/par", |e| events.push(e));
        sniffer.reset();
        sniffer.feed(b"tial\x07", |e| events.push(e));
        assert!(events.is_empty());
        // And the sniffer still works afterwards.
        sniffer.feed(b"\x1b]7;file://h/fresh\x07", |e| events.push(e));
        assert_eq!(
            events,
            vec![ShellEvent::CwdChanged {
                host: Some("h".into()),
                path: "/fresh".into()
            }]
        );
    }
}
