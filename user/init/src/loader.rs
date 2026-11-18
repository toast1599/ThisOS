//! The first loader (ADR 0004): starts programs from the ramdisk, a ustar
//! archive in a memory object, in processes of their own. Each segment and
//! the stack are a memory object, written through the memory-write syscall
//! before they are mapped, so no object is ever writable and executable.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use thisos_elf::Elf;
use thisos_rt::{Handle, Memory, Process, Rights, USER_SPACE_END};
use thisos_ustar::{BLOCK, Header};

/// The size of a program's stack, which ends where user space does.
const STACK_SIZE: usize = 64 << 10;

/// Why a program didn't start.
pub enum Error {
    /// The ramdisk isn't an archive the loader reads.
    Ramdisk(thisos_ustar::Error),
    /// The ramdisk has no file of that name.
    Missing,
    /// The file isn't an executable the loader loads.
    Elf(thisos_elf::Error),
    /// A syscall failed.
    Syscall(thisos_rt::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Ramdisk(e) => write!(f, "the ramdisk isn't an archive init reads: {e:?}"),
            Error::Missing => write!(f, "the ramdisk has no such file"),
            Error::Elf(e) => write!(f, "not an executable init loads: {e:?}"),
            Error::Syscall(e) => write!(f, "a syscall failed: {e:?}"),
        }
    }
}

impl From<thisos_rt::Error> for Error {
    fn from(e: thisos_rt::Error) -> Self {
        Error::Syscall(e)
    }
}

/// Starts the program `name` from `ramdisk` in a new process, with `handle`
/// as its one handle, and returns the process. The handle is gone either
/// way.
pub fn start<T>(
    ramdisk: &Handle<Memory>,
    name: &str,
    handle: Handle<T>,
) -> Result<Handle<Process>, Error> {
    let file = read(ramdisk, name)?;
    let elf = Elf::new(&file).map_err(Error::Elf)?;
    let process = Handle::<Process>::create()?;
    for segment in elf.segments() {
        let pages = segment.page_range();
        let len = size(pages.end - pages.start);
        let memory = Handle::<Memory>::create(len)?;
        memory.write(size(segment.address - pages.start), segment.data)?;
        // Elf::new rejects segments both writable and executable.
        let rights = if segment.executable {
            Rights::READ | Rights::EXECUTE
        } else if segment.writable {
            Rights::READ | Rights::WRITE
        } else {
            Rights::READ
        };
        process.map_into(&memory, 0, len, pages.start, rights)?;
    }
    let stack = Handle::<Memory>::create(STACK_SIZE)?;
    let stack_start = USER_SPACE_END - u64::try_from(STACK_SIZE).unwrap_or(0);
    process.map_into(
        &stack,
        0,
        STACK_SIZE,
        stack_start,
        Rights::READ | Rights::WRITE,
    )?;
    process
        .start(elf.entry(), USER_SPACE_END, handle)
        .map_err(|(e, _)| e)?;
    Ok(process)
}

/// The bytes of the file `name` in the archive in `ramdisk`.
fn read(ramdisk: &Handle<Memory>, name: &str) -> Result<Vec<u8>, Error> {
    let mut offset = 0;
    loop {
        let mut block = [0; BLOCK];
        ramdisk.read(offset, &mut block)?;
        let Some(header) = Header::parse(&block).map_err(Error::Ramdisk)? else {
            return Err(Error::Missing);
        };
        if header.name() == name {
            let mut file = vec![0; size(header.size())];
            ramdisk.read(offset + BLOCK, &mut file)?;
            return Ok(file);
        }
        let span = header
            .span()
            .ok_or(Error::Ramdisk(thisos_ustar::Error::Truncated))?;
        offset += size(span);
    }
}

fn size(bytes: u64) -> usize {
    let Ok(size) = usize::try_from(bytes) else {
        unreachable!("x86_64 addresses are 64 bits");
    };
    size
}
