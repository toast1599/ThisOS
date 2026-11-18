//! The kernel log's buffer: a ring of records, each a one-byte tag and a
//! line of text, which writers append whole and one reader takes back in
//! pieces of at most [`PIECE`] bytes, never splitting a character. The
//! kernel adds the lock, what a tag means and the thread that draws the
//! pieces. [`Line`] is a fixed buffer that formats a record before it goes
//! in, cutting it short when it doesn't fit.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt::{self, Write};
use core::str;

/// The bytes a record takes in the ring besides its text: the tag and the
/// text's length.
pub const HEADER: usize = 5;

/// The most text one [`Ring::pop`] gives.
pub const PIECE: usize = 32;

/// A ring of `N` bytes holding records oldest first, with a count of the
/// records dropped for want of room.
pub struct Ring<const N: usize> {
    bytes: [u8; N],
    /// Where the oldest byte not yet popped is.
    start: usize,
    /// How many bytes are in use, from `start` on, wrapping.
    len: usize,
    /// How much of the oldest record's text is left after a pop that took
    /// part of it, which also took its header. Zero at a record's start.
    left: usize,
    dropped: u64,
}

/// What one [`Ring::pop`] gives: a piece of a record's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    /// The record's tag, when this piece starts it.
    pub tag: Option<u8>,
    /// Whether this piece ends the record.
    pub end: bool,
    bytes: [u8; PIECE],
    len: usize,
}

impl Piece {
    /// The piece's text.
    pub fn text(&self) -> &str {
        // Records go in as text, and pop cuts only at character
        // boundaries.
        str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }
}

impl<const N: usize> Ring<N> {
    /// An empty ring with nothing dropped.
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            start: 0,
            len: 0,
            left: 0,
            dropped: 0,
        }
    }

    /// Whether nothing is left to pop.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many bytes are free: a record fits when its text and
    /// [`HEADER`] do.
    pub const fn room(&self) -> usize {
        N - self.len
    }

    /// Appends a record of `tag` and `text`, and returns whether it fit. A
    /// record that doesn't, or whose formatting fails, changes nothing.
    pub fn push(&mut self, tag: u8, text: &dyn fmt::Display) -> bool {
        if self.room() < HEADER {
            return false;
        }
        let at = (self.start + self.len) % N;
        let mut writer = Writer {
            bytes: &mut self.bytes,
            at: (at + HEADER) % N,
            written: 0,
            room: N - self.len - HEADER,
        };
        if write!(writer, "{text}").is_err() {
            return false;
        }
        let written = writer.written;
        // The ring is smaller than 4 GiB wherever it is used.
        let Ok(length) = u32::try_from(written) else {
            return false;
        };
        let mut header = [0; HEADER];
        header[0] = tag;
        header[1..].copy_from_slice(&length.to_le_bytes());
        for (i, byte) in header.into_iter().enumerate() {
            self.bytes[(at + i) % N] = byte;
        }
        self.len += HEADER + written;
        true
    }

    /// Counts a record dropped for want of room.
    pub fn count_dropped(&mut self) {
        self.dropped += 1;
    }

    /// The records dropped since the last call.
    pub fn take_dropped(&mut self) -> u64 {
        core::mem::take(&mut self.dropped)
    }

    /// Takes up to [`PIECE`] bytes of the oldest record's text, ending
    /// before a character that doesn't fit, and frees them. `None` when the
    /// ring is empty.
    pub fn pop(&mut self) -> Option<Piece> {
        if self.is_empty() {
            return None;
        }
        let mut tag = None;
        if self.left == 0 {
            let mut header = [0; HEADER];
            for byte in &mut header {
                *byte = self.take_byte();
            }
            let [first, length @ ..] = header;
            tag = Some(first);
            // Fits: push wrote it from a usize.
            self.left = usize::try_from(u32::from_le_bytes(length)).unwrap_or(0);
        }
        let mut len = self.left.min(PIECE);
        // A byte 0b10xx_xxxx continues a character, so a cut before one
        // would split it.
        while len < self.left && len > 0 && self.bytes[(self.start + len) % N] & 0xc0 == 0x80 {
            len -= 1;
        }
        let mut bytes = [0; PIECE];
        for byte in &mut bytes[..len] {
            *byte = self.take_byte();
        }
        self.left -= len;
        Some(Piece {
            tag,
            end: self.left == 0,
            bytes,
            len,
        })
    }

    fn take_byte(&mut self) -> u8 {
        let byte = self.bytes[self.start];
        self.start = (self.start + 1) % N;
        self.len -= 1;
        byte
    }
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Writes into the free part of a ring, failing once `room` runs out.
struct Writer<'a> {
    bytes: &'a mut [u8],
    at: usize,
    written: usize,
    room: usize,
}

impl Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if s.len() > self.room - self.written {
            return Err(fmt::Error);
        }
        let n = self.bytes.len();
        for &byte in s.as_bytes() {
            self.bytes[self.at] = byte;
            self.at = (self.at + 1) % n;
        }
        self.written += s.len();
        Ok(())
    }
}

/// How many bytes `text` formats to, so that a writer can see whether
/// several records fit before it pushes any.
pub fn size(text: &dyn fmt::Display) -> usize {
    struct Count(usize);
    impl Write for Count {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = write!(count, "{text}");
    count.0
}

/// What [`Line`] ends a record it cut short with.
pub const CUT: &str = "...";

/// A record's text formatted into `N` bytes. Text that doesn't fit is cut
/// at a character boundary and ends in [`CUT`], within the `N` bytes.
pub struct Line<const N: usize> {
    bytes: [u8; N],
    len: usize,
    cut: bool,
}

impl<const N: usize> Line<N> {
    /// An empty line.
    pub const fn new() -> Self {
        const { assert!(N >= CUT.len(), "no room for the mark") };
        Self {
            bytes: [0; N],
            len: 0,
            cut: false,
        }
    }

    /// The text written so far.
    pub fn as_str(&self) -> &str {
        // Only whole strings and prefixes cut at character boundaries go
        // in.
        str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }

    fn append(&mut self, s: &str) {
        self.bytes[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
    }
}

impl<const N: usize> Default for Line<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Line<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.cut {
            return Ok(());
        }
        if s.len() <= N - self.len {
            self.append(s);
            return Ok(());
        }
        let keep = N - CUT.len();
        if self.len < keep {
            self.append(&s[..s.floor_char_boundary(keep - self.len)]);
        } else {
            self.len = keep;
            while self.len > 0 && self.bytes[self.len] & 0xc0 == 0x80 {
                self.len -= 1;
            }
        }
        self.append(CUT);
        self.cut = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::String;
    use std::vec::Vec;

    use super::*;

    /// Pops everything, joining each record's pieces, and checks that no
    /// piece is longer than [`PIECE`] or empty but for an empty record.
    fn drain<const N: usize>(ring: &mut Ring<N>) -> Vec<(u8, String)> {
        let mut records: Vec<(u8, String)> = Vec::new();
        while let Some(piece) = ring.pop() {
            assert!(piece.text().len() <= PIECE);
            if let Some(tag) = piece.tag {
                records.push((tag, String::new()));
            } else {
                assert!(!piece.text().is_empty(), "{piece:?}");
            }
            let Some((_, text)) = records.last_mut() else {
                panic!("a piece before any record's start");
            };
            text.push_str(piece.text());
        }
        records
    }

    #[test]
    fn records_come_out_whole_and_in_order_across_the_wrap() {
        let mut ring = Ring::<64>::new();
        for round in 0..20 {
            let first = std::format!("record {round}");
            let second = "x".repeat(round);
            assert!(ring.push(1, &first));
            assert!(ring.push(2, &second));
            assert_eq!(drain(&mut ring), [(1, first), (2, second)]);
            assert!(ring.is_empty());
            assert_eq!(ring.room(), 64);
        }
    }

    #[test]
    fn a_record_that_doesnt_fit_changes_nothing() {
        let mut ring = Ring::<32>::new();
        assert!(ring.push(1, &"a".repeat(20)));
        assert_eq!(ring.room(), 32 - HEADER - 20);
        assert!(!ring.push(2, &"bbb"));
        assert_eq!(ring.room(), 32 - HEADER - 20);
        assert!(ring.push(2, &"b".repeat(2)));
        assert_eq!(ring.room(), 0);
        assert!(!ring.push(3, &""));
        assert_eq!(drain(&mut ring), [(1, "a".repeat(20)), (2, "bb".into())]);
    }

    #[test]
    fn pieces_free_room_as_they_go_and_never_split_a_character() {
        // Two bytes each, so a cut at 32 bytes would land in one when the
        // text starts one byte in.
        let text = std::format!("a{}", "é".repeat(40));
        let mut ring = Ring::<128>::new();
        assert!(ring.push(7, &text));
        let first = ring.pop().unwrap_or_else(|| panic!("empty"));
        assert_eq!(first.tag, Some(7));
        assert!(!first.end);
        assert_eq!(first.text().len(), PIECE - 1);
        assert_eq!(ring.room(), 128 - (text.len() - (PIECE - 1)));
        let mut rest = String::new();
        while let Some(piece) = ring.pop() {
            assert_eq!(piece.tag, None);
            rest.push_str(piece.text());
            assert_eq!(piece.end, ring.is_empty());
        }
        assert_eq!(std::format!("{}{rest}", first.text()), text);
    }

    #[test]
    fn an_empty_record_is_one_empty_piece() {
        let mut ring = Ring::<16>::new();
        assert!(ring.push(4, &""));
        let piece = ring.pop().unwrap_or_else(|| panic!("empty"));
        assert_eq!((piece.tag, piece.end, piece.text()), (Some(4), true, ""));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn dropped_records_are_counted_until_taken() {
        let mut ring = Ring::<16>::new();
        ring.count_dropped();
        ring.count_dropped();
        assert_eq!(ring.take_dropped(), 2);
        assert_eq!(ring.take_dropped(), 0);
    }

    #[test]
    fn size_counts_formatted_bytes() {
        assert_eq!(size(&format_args!("{}-{}", "é", 12)), 5);
        assert_eq!(size(&""), 0);
    }

    /// A line after one `write_str` per part, as `write!` makes them.
    fn line<const N: usize>(parts: &[&str]) -> Line<N> {
        let mut line = Line::new();
        for part in parts {
            assert!(line.write_str(part).is_ok());
        }
        line
    }

    #[test]
    fn a_line_that_fits_is_kept_whole() {
        assert_eq!(line::<8>(&["abcd", "efgh"]).as_str(), "abcdefgh");
    }

    #[test]
    fn a_long_line_is_cut_at_a_character_boundary_with_a_mark() {
        assert_eq!(line::<10>(&["abcde", "éééé"]).as_str(), "abcdeé...");
        assert_eq!(line::<10>(&["abcdefé", "zz", "more"]).as_str(), "abcdef...");
        assert_eq!(line::<10>(&[&"x".repeat(20)]).as_str(), "xxxxxxx...");
    }
}
