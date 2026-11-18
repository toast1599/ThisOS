//! A reader for the ramdisk: a POSIX ustar archive of regular files with
//! names of at most 100 bytes, which xtask writes. The kernel finds init in
//! it with [`find`]; init, which reads the ramdisk through syscalls, parses
//! one header block at a time with [`Header::parse`].

#![no_std]
#![forbid(unsafe_code)]

/// The size of a header and of the blocks a file's bytes are padded to.
pub const BLOCK: usize = 512;

const NAME: usize = 0;
const NAME_SIZE: usize = 100;
const SIZE: usize = 124;
const SIZE_SIZE: usize = 12;
const CHECKSUM: usize = 148;
const CHECKSUM_SIZE: usize = 8;
const TYPE: usize = 156;
const MAGIC: usize = 257;
const PREFIX: usize = 345;
const PREFIX_SIZE: usize = 155;

/// The magic and version fields of a POSIX ustar header.
const USTAR: &[u8] = b"ustar\x0000";

/// What is wrong with an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The archive ends inside a header or a file's bytes, or before the
    /// zero block that ends it.
    Truncated,
    /// A header's checksum doesn't match its bytes.
    BadChecksum,
    /// A header isn't a POSIX ustar header.
    NotUstar,
    /// A header is ustar but not what the ramdisk holds, as the named field
    /// shows.
    Unsupported(&'static str),
    /// A numeric field isn't octal digits, or overflows.
    BadNumber(&'static str),
    /// A name is empty or isn't UTF-8.
    BadName,
}

/// A checked header of a regular file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    name: &'a str,
    size: u64,
}

impl<'a> Header<'a> {
    /// The header in `block`, or `None` for the zero block that ends the
    /// archive.
    pub fn parse(block: &'a [u8; BLOCK]) -> Result<Option<Header<'a>>, Error> {
        if block.iter().all(|&b| b == 0) {
            return Ok(None);
        }
        if &block[MAGIC..MAGIC + USTAR.len()] != USTAR {
            return Err(Error::NotUstar);
        }
        // The sum of every byte, with the checksum field's own as spaces.
        let sum: u64 = block
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (CHECKSUM..CHECKSUM + CHECKSUM_SIZE).contains(&i) {
                    u64::from(b' ')
                } else {
                    u64::from(b)
                }
            })
            .sum();
        if octal(&block[CHECKSUM..CHECKSUM + CHECKSUM_SIZE], "checksum")? != sum {
            return Err(Error::BadChecksum);
        }
        if !matches!(block[TYPE], b'0' | 0) {
            return Err(Error::Unsupported("type"));
        }
        if block[PREFIX..PREFIX + PREFIX_SIZE].iter().any(|&b| b != 0) {
            return Err(Error::Unsupported("prefix"));
        }
        let name = &block[NAME..NAME + NAME_SIZE];
        let name = name.split(|&b| b == 0).next().unwrap_or(name);
        let name = str::from_utf8(name).map_err(|_| Error::BadName)?;
        if name.is_empty() {
            return Err(Error::BadName);
        }
        let size = octal(&block[SIZE..SIZE + SIZE_SIZE], "size")?;
        Ok(Some(Header { name, size }))
    }

    /// The file's name.
    pub fn name(&self) -> &'a str {
        self.name
    }

    /// The file's size in bytes, which follow the header.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The bytes from the start of this header to the next one's: the
    /// header and the file's bytes padded to whole blocks. `None` if that
    /// overflows.
    pub fn span(&self) -> Option<u64> {
        let block = u64::try_from(BLOCK).ok()?;
        self.size
            .checked_next_multiple_of(block)?
            .checked_add(block)
    }
}

/// The value of an octal field: digits, which leading spaces may precede,
/// ended by a NUL, a space or the field's end.
fn octal(field: &[u8], name: &'static str) -> Result<u64, Error> {
    let digits = field.trim_ascii_start();
    let end = digits
        .iter()
        .position(|&b| b == 0 || b == b' ')
        .unwrap_or(digits.len());
    if end == 0 {
        return Err(Error::BadNumber(name));
    }
    digits[..end].iter().try_fold(0u64, |value, &b| {
        if !(b'0'..=b'7').contains(&b) {
            return Err(Error::BadNumber(name));
        }
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add(u64::from(b - b'0')))
            .ok_or(Error::BadNumber(name))
    })
}

/// The bytes of the file `name` in `archive`, or `None` if the archive has
/// no such file. Checks every header up to that file's.
pub fn find<'a>(archive: &'a [u8], name: &str) -> Result<Option<&'a [u8]>, Error> {
    let mut rest = archive;
    loop {
        let (block, after) = rest.split_first_chunk::<BLOCK>().ok_or(Error::Truncated)?;
        let Some(header) = Header::parse(block)? else {
            return Ok(None);
        };
        let size = usize::try_from(header.size()).map_err(|_| Error::Truncated)?;
        if header.name() == name {
            return after.get(..size).map(Some).ok_or(Error::Truncated);
        }
        let span = header
            .span()
            .and_then(|span| usize::try_from(span).ok())
            .ok_or(Error::Truncated)?;
        rest = rest.get(span..).ok_or(Error::Truncated)?;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec;
    use std::vec::Vec;

    use super::*;

    /// A ustar header for a regular file, as xtask writes them.
    fn header(name: &str, size: usize) -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        block[..name.len()].copy_from_slice(name.as_bytes());
        block[100..108].copy_from_slice(b"0000644\0");
        let size = std::format!("{size:011o}\0");
        block[SIZE..SIZE + SIZE_SIZE].copy_from_slice(size.as_bytes());
        block[TYPE] = b'0';
        block[MAGIC..MAGIC + USTAR.len()].copy_from_slice(USTAR);
        seal(&mut block);
        block
    }

    /// Writes `block`'s checksum.
    fn seal(block: &mut [u8; BLOCK]) {
        block[CHECKSUM..CHECKSUM + CHECKSUM_SIZE].fill(b' ');
        let sum: u32 = block.iter().map(|&b| u32::from(b)).sum();
        let sum = std::format!("{sum:06o}\0 ");
        block[CHECKSUM..CHECKSUM + CHECKSUM_SIZE].copy_from_slice(sum.as_bytes());
    }

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        for (name, bytes) in files {
            archive.extend(header(name, bytes.len()));
            archive.extend(*bytes);
            archive.resize(archive.len().next_multiple_of(BLOCK), 0);
        }
        archive.extend([0; 2 * BLOCK]);
        archive
    }

    #[test]
    fn finds_each_file_by_name() {
        let block = vec![7; BLOCK];
        let longer = vec![9; BLOCK + 1];
        let files: [(&str, &[u8]); 4] = [
            ("init", b"init's bytes"),
            ("empty", b""),
            ("block", &block),
            ("longer", &longer),
        ];
        let archive = archive(&files);
        for (name, bytes) in files {
            assert_eq!(find(&archive, name), Ok(Some(bytes)), "{name}");
        }
        assert_eq!(find(&archive, "missing"), Ok(None));
        assert_eq!(find(&archive, "ini"), Ok(None));
    }

    #[test]
    fn rejects_what_isnt_a_ramdisk() {
        let good = archive(&[("init", b"bytes")]);
        let mut first = [0; BLOCK];
        first.copy_from_slice(&good[..BLOCK]);
        let with = |change: fn(&mut [u8; BLOCK])| {
            let mut block = first;
            change(&mut block);
            let mut archive = good.clone();
            archive[..BLOCK].copy_from_slice(&block);
            find(&archive, "missing").map(|found| found.map(<[u8]>::len))
        };
        assert_eq!(with(|b| b[0] = b'j'), Err(Error::BadChecksum));
        assert_eq!(with(|b| b[MAGIC + 5] = b' '), Err(Error::NotUstar));
        assert_eq!(
            with(|b| {
                b[TYPE] = b'5';
                seal(b);
            }),
            Err(Error::Unsupported("type"))
        );
        assert_eq!(
            with(|b| {
                b[PREFIX] = b'd';
                seal(b);
            }),
            Err(Error::Unsupported("prefix"))
        );
        assert_eq!(
            with(|b| {
                b[SIZE] = b'8';
                seal(b);
            }),
            Err(Error::BadNumber("size"))
        );
        assert_eq!(
            with(|b| {
                b[..4].copy_from_slice(&[0xff; 4]);
                seal(b);
            }),
            Err(Error::BadName)
        );
        // Cut inside the file's bytes, and before the end's zero block.
        assert_eq!(find(&good[..BLOCK + 3], "init"), Err(Error::Truncated));
        assert_eq!(find(&good[..2 * BLOCK], "missing"), Err(Error::Truncated));
    }
}
