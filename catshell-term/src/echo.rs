//! Removing a command's own echo from the output stream.
//!
//! Shell integration has to be *typed* into a remote shell: a server will not pass
//! environment variables through, and even if it did, an exported `PROMPT_COMMAND` is
//! overwritten by whatever the host's own `.bashrc` assigns. Typing it at the prompt is
//! what guarantees the hook runs last.
//!
//! The cost is that the remote echoes it — twice, in fact: once by the pseudoterminal
//! before bash starts, and again by readline as it consumes the line. Since catshell
//! knows the exact bytes it sent, it can take them back out of the stream before they
//! reach the grid, so the user never sees the plumbing.
//!
//! Matching is literal and exact. Anything else — a reformatted or wrapped echo — simply
//! fails to match and is shown, which is the old behaviour rather than a new failure.

/// How many copies of a typed line the remote is expected to echo.
///
/// The pseudoterminal echoes it once in canonical mode before the shell takes over, and
/// readline echoes it again when it reads the line.
pub const EXPECTED_ECHOES: usize = 2;

/// How much output to inspect before giving up and leaving the stream alone.
///
/// Without a limit the filter would watch a session forever, and a user who legitimately
/// typed the same text much later would see it vanish.
pub const DEFAULT_BUDGET: usize = 256 * 1024;

/// Removes known literal text from a byte stream.
#[derive(Debug)]
pub struct EchoFilter {
    pattern: Vec<u8>,
    /// Bytes matching a prefix of the pattern, withheld until the match resolves.
    held: Vec<u8>,
    /// Occurrences still to remove.
    occurrences: usize,
    /// Bytes still to inspect before giving up.
    budget: usize,
    /// Whether a line ending is still to be swallowed after a match, so removing the
    /// text does not leave a blank line exactly where it was.
    swallow: Swallow,
}

/// How much of the line ending following a match is still to be consumed.
///
/// Exactly one line ending, never more: a blank line after the echo belongs to the
/// program, not to us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swallow {
    Nothing,
    /// A match just ended; take a `\r`, an `\n`, or `\r\n`.
    LineEnding,
    /// A `\r` was taken; only its `\n` may follow.
    AfterCarriageReturn,
}

impl EchoFilter {
    pub fn new(pattern: impl Into<Vec<u8>>, occurrences: usize, budget: usize) -> Self {
        let pattern = pattern.into();
        // An empty pattern would match everywhere and remove nothing sensibly.
        let occurrences = if pattern.is_empty() { 0 } else { occurrences };
        Self {
            pattern,
            held: Vec::new(),
            occurrences,
            budget,
            swallow: Swallow::Nothing,
        }
    }

    /// Whether the filter has finished and is now passing everything through.
    pub fn finished(&self) -> bool {
        self.occurrences == 0 || self.budget == 0
    }

    /// Pass `input` through, writing what should be displayed into `out`.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        if self.finished() && self.swallow == Swallow::Nothing {
            out.append(&mut self.held);
            out.extend_from_slice(input);
            return;
        }

        for &byte in input {
            // Checked before `finished`, so the last match's trailing newline goes too.
            match (self.swallow, byte) {
                (Swallow::LineEnding, b'\r') => {
                    self.swallow = Swallow::AfterCarriageReturn;
                    continue;
                }
                (Swallow::LineEnding | Swallow::AfterCarriageReturn, b'\n') => {
                    self.swallow = Swallow::Nothing;
                    continue;
                }
                (Swallow::LineEnding | Swallow::AfterCarriageReturn, _) => {
                    self.swallow = Swallow::Nothing;
                }
                (Swallow::Nothing, _) => {}
            }

            if self.finished() {
                out.push(byte);
                continue;
            }
            self.budget = self.budget.saturating_sub(1);

            if self.pattern[self.held.len()] == byte {
                self.held.push(byte);
                if self.held.len() == self.pattern.len() {
                    self.held.clear();
                    self.occurrences -= 1;
                    self.swallow = Swallow::LineEnding;
                }
                continue;
            }

            // The run broke. Emit its first byte and look for a match starting later in
            // what was held — the text may begin again inside its own near-miss.
            self.held.push(byte);
            let mut pending = std::mem::take(&mut self.held);
            out.push(pending.remove(0));
            self.resync(pending, out);
        }

        if self.finished() {
            out.append(&mut self.held);
        }
    }

    /// Emit everything still withheld. For when the stream ends mid-candidate.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.held);
    }

    /// Find the longest suffix of `pending` that still starts the pattern, emitting what
    /// comes before it.
    fn resync(&mut self, mut pending: Vec<u8>, out: &mut Vec<u8>) {
        loop {
            if pending.is_empty() {
                self.held.clear();
                return;
            }
            if self.pattern.starts_with(&pending) {
                self.held = pending;
                return;
            }
            out.push(pending.remove(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed the whole input at once.
    fn filter(pattern: &str, input: &str) -> String {
        let mut filter = EchoFilter::new(pattern.as_bytes(), 2, DEFAULT_BUDGET);
        let mut out = Vec::new();
        filter.filter(input.as_bytes(), &mut out);
        filter.flush(&mut out);
        String::from_utf8(out).unwrap()
    }

    /// Feed one byte at a time, which forces every split through the resumable path.
    fn filter_byte_by_byte(pattern: &str, input: &str) -> String {
        let mut filter = EchoFilter::new(pattern.as_bytes(), 2, DEFAULT_BUDGET);
        let mut out = Vec::new();
        for byte in input.as_bytes() {
            filter.filter(&[*byte], &mut out);
        }
        filter.flush(&mut out);
        String::from_utf8(out).unwrap()
    }

    /// Every chunking must produce the same result.
    fn filter_all_ways(pattern: &str, input: &str) -> String {
        let whole = filter(pattern, input);
        assert_eq!(
            filter_byte_by_byte(pattern, input),
            whole,
            "byte-by-byte differed"
        );
        for split in 1..input.len() {
            let mut f = EchoFilter::new(pattern.as_bytes(), 2, DEFAULT_BUDGET);
            let mut out = Vec::new();
            f.filter(&input.as_bytes()[..split], &mut out);
            f.filter(&input.as_bytes()[split..], &mut out);
            f.flush(&mut out);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                whole,
                "split at {split} differed"
            );
        }
        whole
    }

    #[test]
    fn text_without_the_pattern_passes_through_untouched() {
        assert_eq!(
            filter_all_ways("SECRET", "hello world\r\n"),
            "hello world\r\n"
        );
    }

    #[test]
    fn an_occurrence_is_removed() {
        assert_eq!(
            filter_all_ways("SECRET", "before SECRET after"),
            "before  after"
        );
    }

    #[test]
    fn both_echoes_are_removed() {
        // The pseudoterminal echoes once and readline echoes again.
        assert_eq!(filter_all_ways("CMD", "CMD\r\nprompt$ CMD\r\n"), "prompt$ ");
    }

    #[test]
    fn a_third_occurrence_is_left_alone() {
        // Only the echoes catshell caused are its to remove; anything after that is the
        // user's own output.
        let out = filter_all_ways("CMD", "CMD\r\nCMD\r\nCMD\r\n");
        assert_eq!(out, "CMD\r\n");
    }

    #[test]
    fn the_line_ending_left_behind_is_removed_too() {
        // Otherwise taking the text out leaves a blank line exactly where it was.
        assert_eq!(filter_all_ways("CMD", "a\r\nCMD\r\nb"), "a\r\nb");
    }

    #[test]
    fn only_one_line_ending_is_swallowed() {
        // A deliberate blank line after the echo is the program's, not ours.
        assert_eq!(filter_all_ways("CMD", "CMD\r\n\r\nrest"), "\r\nrest");
    }

    #[test]
    fn a_partial_match_is_emitted_when_it_breaks() {
        assert_eq!(filter_all_ways("SECRET", "SECRabc"), "SECRabc");
    }

    #[test]
    fn a_match_can_start_inside_a_near_miss() {
        // "SESECRET" nearly matches at 0, then really matches at 2; a naive filter that
        // gave up on mismatch would miss it.
        assert_eq!(filter_all_ways("SECRET", "SESECRET!"), "SE!");
    }

    #[test]
    fn repeated_prefixes_resync_correctly() {
        assert_eq!(filter_all_ways("aab", "aaab"), "a");
        assert_eq!(filter_all_ways("aab", "aaaab"), "aa");
    }

    #[test]
    fn the_real_integration_snippet_is_removed() {
        // The actual thing, with the shape the shell echoes it in.
        let snippet = crate::integration::install_command(crate::integration::Shell::Bash);
        let typed = snippet.trim_end_matches('\n');
        let session = format!(
            "Last login: Tue Oct  6 18:36:08 2026 from 37.65.14.89\r\n\
             {typed}\r\n\
             [opc@fantasyhub ~]$ {typed}\r\n\
             [opc@fantasyhub ~]$ "
        );

        let out = filter_all_ways(typed, &session);
        assert!(
            !out.contains("__catshell_report"),
            "the snippet was still shown:\n{out}"
        );
        assert!(
            out.contains("Last login:"),
            "the login banner was eaten:\n{out}"
        );
        assert!(
            out.contains("[opc@fantasyhub ~]$"),
            "the prompt was eaten:\n{out}"
        );
    }

    #[test]
    fn a_budget_stops_the_filter_watching_forever() {
        // A user who types the same text much later must still see it.
        let mut filter = EchoFilter::new(b"CMD".to_vec(), 2, 4);
        let mut out = Vec::new();
        filter.filter(b"....CMD", &mut out);
        filter.flush(&mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "....CMD");
        assert!(filter.finished());
    }

    #[test]
    fn an_empty_pattern_removes_nothing() {
        assert_eq!(filter_all_ways("", "anything at all"), "anything at all");
    }

    #[test]
    fn a_finished_filter_passes_everything_through() {
        let mut filter = EchoFilter::new(b"CMD".to_vec(), 1, DEFAULT_BUDGET);
        let mut out = Vec::new();
        filter.filter(b"CMD", &mut out);
        assert!(filter.finished());

        out.clear();
        filter.filter(b"CMD and more", &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "CMD and more");
    }

    #[test]
    fn an_unfinished_candidate_is_not_lost_at_the_end_of_a_stream() {
        // Held bytes must come out eventually, or the tail of the output disappears.
        let mut filter = EchoFilter::new(b"SECRET".to_vec(), 2, DEFAULT_BUDGET);
        let mut out = Vec::new();
        filter.filter(b"tail SEC", &mut out);
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "tail ");

        filter.flush(&mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "tail SEC");
    }
}
