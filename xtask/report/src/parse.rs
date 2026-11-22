//! Reading what a step's commands print: splitting it into lines, and the
//! progress some tools report in them.

/// How far a step is, from what its tools report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Progress {
    /// Tests of one libtest binary.
    Tests { done: u64, total: u64 },
    /// Bytes downloaded or written, out of `total` when it's known.
    Bytes { done: u64, total: Option<u64> },
}

/// Updates `progress` from one line of a command's output, ANSI codes
/// stripped. `expected` is the byte count a step said its command will
/// write, which dd's `status=progress` lines count towards.
pub fn update(line: &str, progress: &mut Option<Progress>, expected: Option<u64>) {
    if let Some(total) = running_tests(line) {
        *progress = Some(Progress::Tests { done: 0, total });
    } else if line.starts_with("test ") && line.contains(" ... ") {
        if let Some(Progress::Tests { done, total }) = progress {
            *done = (*done + 1).min(*total);
        }
    } else if let (Some(total), Some(done)) = (expected, dd_bytes(line)) {
        *progress = Some(Progress::Bytes {
            done,
            total: Some(total),
        });
    }
}

/// N from libtest's `running N tests`, which starts each test binary.
fn running_tests(line: &str) -> Option<u64> {
    let count = line.strip_prefix("running ")?;
    let count = count
        .strip_suffix(" tests")
        .or_else(|| count.strip_suffix(" test"))?;
    count.parse().ok()
}

/// The bytes copied so far from a dd `status=progress` line, such as
/// `1048576 bytes (1.0 MB, 1.0 MiB) copied, 1 s, 1.0 MB/s`.
fn dd_bytes(line: &str) -> Option<u64> {
    let (count, rest) = line.split_once(' ')?;
    if rest.starts_with("bytes") {
        count.parse().ok()
    } else {
        None
    }
}

/// `text` without its escape sequences. A control sequence is ESC `[`,
/// parameter bytes, intermediate bytes and a final byte (ECMA-48); edk2
/// sends ones like `ESC[=3h`, which console's stripper ends early at the
/// `=`. Any other escape is ESC and one byte.
pub fn strip(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            plain.push(c);
        } else if chars.next_if_eq(&'[').is_some() {
            while chars.next_if(|c| ('\x20'..='\x3f').contains(c)).is_some() {}
            chars.next_if(|c| ('\x40'..='\x7e').contains(c));
        } else {
            chars.next();
        }
    }
    plain
}

/// Splits a byte stream into lines at `\n`, `\r\n` and a lone `\r`, which
/// tools like dd end their progress lines with.
#[derive(Default)]
pub struct Lines {
    pending: Vec<u8>,
}

impl Lines {
    /// Adds `bytes` and returns the lines they complete.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n' || b == b'\r') {
            let len = match (self.pending[end], self.pending.get(end + 1)) {
                // A `\n` may come in the next read.
                (b'\r', None) => break,
                (b'\r', Some(b'\n')) => end + 2,
                _ => end + 1,
            };
            lines.push(String::from_utf8_lossy(&self.pending[..end]).into_owned());
            self.pending.drain(..len);
        }
        lines
    }

    /// The last line, when the stream didn't end with a line break.
    pub fn end(self) -> Option<String> {
        let line = self.pending.strip_suffix(b"\r").unwrap_or(&self.pending);
        (!line.is_empty()).then(|| String::from_utf8_lossy(line).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{Lines, Progress, strip, update};

    #[test]
    fn counts_libtest_results_per_binary() {
        let mut progress = None;
        for line in [
            "running 2 tests",
            "test a ... ok",
            "test b ... FAILED",
            "test result: FAILED. 1 passed; 1 failed",
            "running 1 test",
        ] {
            update(line, &mut progress, None);
        }
        assert_eq!(progress, Some(Progress::Tests { done: 0, total: 1 }));
        update("test c ... ok", &mut progress, None);
        assert_eq!(progress, Some(Progress::Tests { done: 1, total: 1 }));
    }

    #[test]
    fn counts_dd_bytes_only_when_a_total_is_expected() {
        let line = "1048576 bytes (1.0 MB, 1.0 MiB) copied, 1 s, 1.0 MB/s";
        let mut progress = None;
        update(line, &mut progress, None);
        assert_eq!(progress, None);
        update(line, &mut progress, Some(4 << 20));
        assert_eq!(
            progress,
            Some(Progress::Bytes {
                done: 1 << 20,
                total: Some(4 << 20)
            })
        );
    }

    #[test]
    fn strips_every_control_sequence() {
        assert_eq!(strip("\x1b[=3h\x1b[2J\x1b[01;01HBdsDxe"), "BdsDxe");
        assert_eq!(strip("\x1b[1m\x1b[32mCompiling\x1b[0m x"), "Compiling x");
        assert_eq!(strip("a\x1b7b\x1b"), "ab");
    }

    #[test]
    fn splits_lines_across_reads_and_line_ends() {
        let mut lines = Lines::default();
        assert_eq!(lines.push(b"one\r"), Vec::<String>::new());
        assert_eq!(
            lines.push(b"\ntwo\rthree\n\nfo"),
            ["one", "two", "three", ""]
        );
        assert_eq!(lines.push(b"ur"), Vec::<String>::new());
        assert_eq!(lines.end().as_deref(), Some("four"));
    }
}
