//! User code: the ramdisk, starting init from it, and the traps user code
//! raises, its syscalls and the faults that end it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::{fmt, str};

use thisos_abi::{
    Error, MAX_MESSAGE_BYTES, MAX_MESSAGE_HANDLES, MAX_WAIT_ITEMS, MAX_WRITE, Rights, Signals,
    Syscall, WAIT_ITEM_SIZE,
};
use thisos_channels::Message;
use thisos_elf::Elf;
use thisos_hal::{
    AddressSpace as _, Cpu, MapErrorKind, Platform, Protection, Trap, TrapFrame as _,
};
use thisos_locks::Rank;

use crate::Lock;
use crate::channel::{self, ENDPOINT_RIGHTS, Endpoint};
use crate::memory::{Mapping, MemoryObject};
use crate::paging::Space;
use crate::process::{End, Object, PROCESS_RIGHTS, Process};
use crate::wait::Waitable;
use crate::{Arch, frames, logger, paging, thread, wait};

/// The size of a program's stack, which ends where user space does.
const STACK_SIZE: u64 = 64 << 10;

/// The rights of a program's handle to the ramdisk.
const RAMDISK_RIGHTS: Rights = Rights::READ
    .union(Rights::DUPLICATE)
    .union(Rights::TRANSFER);

/// The records the last write syscall logged, joined by newlines, for
/// tests. Taken with interrupts off, inside the syscall's big lock.
#[cfg(test)]
static WRITTEN: Lock<Written> = Lock::new(
    Rank::TEST_LATE,
    Written {
        bytes: [0; WRITTEN_SIZE],
        len: 0,
    },
);

/// Escapes make a record up to six bytes per byte of text, as `\u{1b}`.
#[cfg(test)]
const WRITTEN_SIZE: usize = 6 * MAX_WRITE;

#[cfg(test)]
struct Written {
    bytes: [u8; WRITTEN_SIZE],
    len: usize,
}

#[cfg(test)]
impl fmt::Write for Written {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// The loader's ramdisk: the ustar archive the kernel finds init in, and a
/// memory object holding a copy, which programs read.
struct Ramdisk {
    archive: &'static [u8],
    object: Arc<MemoryObject>,
}

static RAMDISK: Lock<Option<Ramdisk>> = Lock::new(Rank::RAMDISK, None);

/// Logs the size of the loader's ramdisk, copies it into a memory object,
/// and keeps both for [`start_init`]. Panics if there is no memory for the
/// copy.
pub fn init(ramdisk: Option<&'static [u8]>) {
    let Some(archive) = ramdisk else {
        log::warn!("no ramdisk, so no init to run");
        return;
    };
    log::info!("ramdisk is {} bytes", archive.len());
    let Ok(mut object) = MemoryObject::new(archive.len().div_ceil(4096)) else {
        panic!("no memory for a copy of the {}-byte ramdisk", archive.len());
    };
    for (frame, bytes) in object.frames_mut().iter_mut().zip(archive.chunks(4096)) {
        Arch::fill_frame(frame, 0, bytes);
    }
    *RAMDISK.lock() = Some(Ramdisk {
        archive,
        object: Arc::new(object),
    });
}

/// The ramdisk's archive and its copy, if there is a ramdisk.
fn ramdisk() -> Option<(&'static [u8], Arc<MemoryObject>)> {
    let ramdisk = RAMDISK.lock();
    let ramdisk = ramdisk.as_ref()?;
    Some((ramdisk.archive, Arc::clone(&ramdisk.object)))
}

/// The file `name` in the ramdisk, if there is a ramdisk and it holds one.
/// Panics if the archive isn't one the kernel reads.
#[cfg(test)]
pub fn find_in_ramdisk(name: &str) -> Option<&'static [u8]> {
    let archive = RAMDISK.lock().as_ref()?.archive;
    match thisos_ustar::find(archive, name) {
        Ok(file) => file,
        Err(e) => panic!("the ramdisk isn't an archive the kernel reads: {e:?}"),
    }
}

/// The file `name` in the ramdisk's `archive`. Panics if there is none, or
/// the archive isn't one the kernel reads, since xtask builds both.
fn ramdisk_file(archive: &'static [u8], name: &str) -> &'static [u8] {
    match thisos_ustar::find(archive, name) {
        Ok(Some(file)) => file,
        Ok(None) => panic!("the ramdisk has no {name}"),
        Err(e) => panic!("the ramdisk isn't an archive the kernel reads: {e:?}"),
    }
}

/// Starts init from the ramdisk, if there is one. Panics if it doesn't
/// start.
pub fn start_init() {
    let Some((archive, object)) = ramdisk() else {
        return;
    };
    if let Err(e) = start_with_ramdisk(ramdisk_file(archive, "init"), object) {
        panic!("starting init failed: {e}");
    }
}

/// Starts the executable in `file` as a user-space loader starts one: its
/// one handle is the receiving end of a channel whose other end is closed,
/// where one message waits, with a handle to the process itself with
/// [`PROCESS_RIGHTS`] and one to `ramdisk` with [`RAMDISK_RIGHTS`]. The
/// message is sent once the program runs, since it holds the process.
fn start_with_ramdisk(file: &[u8], ramdisk: Arc<MemoryObject>) -> Result<Arc<Process>, LoadError> {
    let (ours, theirs) = channel::create().map_err(LoadError::Start)?;
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(2)
        .map_err(|_| LoadError::NoMemory)?;
    let process = start_program(file, theirs)?;
    handles.push((Object::Process(Arc::clone(&process)), PROCESS_RIGHTS));
    handles.push((Object::Memory(ramdisk), RAMDISK_RIGHTS));
    // Fails with PeerClosed only if the program ended without its message.
    ours.send(|| {
        Ok(Message {
            bytes: Vec::new(),
            handles,
        })
    })
    .map_err(LoadError::Start)?;
    Ok(process)
}

/// Why [`start_program`] failed.
enum LoadError {
    /// The file isn't an executable the kernel loads.
    Elf(thisos_elf::Error),
    /// The pages at `addr` couldn't be mapped.
    Map { addr: u64, kind: MapErrorKind },
    /// No memory was free.
    NoMemory,
    /// The loaded program didn't start.
    Start(Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Elf(e) => write!(f, "not an executable the kernel loads: {e:?}"),
            LoadError::Map { addr, kind } => write!(f, "mapping {addr:#x} failed: {kind:?}"),
            LoadError::NoMemory => write!(f, "no free memory"),
            LoadError::Start(e) => write!(f, "starting it failed: {e:?}"),
        }
    }
}

/// A program loaded into an address space of its own, with a stack.
struct Program {
    space: Arc<Space>,
    entry: u64,
    stack_top: u64,
}

/// Loads the executable in `file` and starts it in a process of its own,
/// whose one handle is `endpoint`, with [`ENDPOINT_RIGHTS`].
fn start_program(file: &[u8], endpoint: Endpoint) -> Result<Arc<Process>, LoadError> {
    let program = load(file)?;
    let process = Process::new(program.space).map_err(LoadError::Start)?;
    let endpoint = Arc::try_new(endpoint).map_err(|_| LoadError::NoMemory)?;
    process
        .start(program.entry, program.stack_top, || {
            Ok((Object::Channel(endpoint), ENDPOINT_RIGHTS))
        })
        .map_err(LoadError::Start)?;
    Ok(process)
}

/// Loads the executable in `file` into a new address space, with a zeroed
/// stack at the end of user space. Each segment and the stack are a memory
/// object of their own.
fn load(file: &[u8]) -> Result<Program, LoadError> {
    let elf = Elf::new(file, Arch::ELF_MACHINE).map_err(LoadError::Elf)?;
    let space = paging::new_space().ok_or(LoadError::NoMemory)?;
    // On failure the space drops, and destroys itself.
    let stack_top = map_program(&elf, &space)?;
    Ok(Program {
        space: Arc::try_new(space).map_err(|_| LoadError::NoMemory)?,
        entry: elf.entry(),
        stack_top,
    })
}

/// Maps `elf`'s segments and a stack into `space`, and returns the stack's
/// top.
fn map_program(elf: &Elf, space: &Space) -> Result<u64, LoadError> {
    for segment in elf.segments() {
        // Elf::new rejects segments both writable and executable.
        let protection = if segment.executable {
            Protection::ReadExecute
        } else if segment.writable {
            Protection::ReadWrite
        } else {
            Protection::ReadOnly
        };
        let range = segment.page_range();
        let mut object = new_object(range.end - range.start)?;
        for (frame, page) in object.frames_mut().iter_mut().zip(segment.pages()) {
            Arch::fill_frame(frame, page.offset, page.data);
        }
        map_object(space, range.start, object, protection)?;
    }
    let top = space.user_space().end;
    // User-space loaders put stacks there too.
    assert_eq!(
        top,
        thisos_abi::USER_SPACE_END,
        "the HAL's user space ends elsewhere than the ABI says"
    );
    let stack = new_object(STACK_SIZE)?;
    map_object(space, top - STACK_SIZE, stack, Protection::ReadWrite)?;
    Ok(top)
}

/// A memory object of `size` bytes, a multiple of 4096.
fn new_object(size: u64) -> Result<MemoryObject, LoadError> {
    let pages = usize::try_from(size / 4096).map_err(|_| LoadError::NoMemory)?;
    MemoryObject::new(pages).map_err(|_| LoadError::NoMemory)
}

/// Maps all of `object`, which nothing else holds, at `addr` in `space`.
fn map_object(
    space: &Space,
    addr: u64,
    object: MemoryObject,
    protection: Protection,
) -> Result<(), LoadError> {
    let pages = object.pages();
    let Ok(mapping) = Mapping::new(Arc::new(object), protection) else {
        unreachable!("a new object has no mappings to conflict with");
    };
    space
        .map(addr, mapping, 0, pages, protection, &mut frames::alloc)
        .map_err(|e| LoadError::Map { addr, kind: e.kind })
}

/// Handles a trap that [`TrapFrame::raised_by_user`] says user code raised:
/// runs a syscall, and ends the process on any other trap, or on the exit
/// syscall.
///
/// [`TrapFrame::raised_by_user`]: thisos_hal::TrapFrame::raised_by_user
pub fn trap(trap: Trap, frame: &mut <Arch as Platform>::TrapFrame) {
    if trap != Trap::Syscall {
        end(End::Killed {
            trap,
            ip: frame.ip(),
        });
        // A #DB comes in on an IST stack, which the next one starts at the
        // top of again, so interrupts stay off until the thread is gone
        // from it.
        thread::exit_from_trap();
    }
    // Syscalls run with interrupts on, so that they can allocate and free,
    // and a tick may preempt them. The SYSCALL stub turns interrupts off
    // again before it returns to user code.
    Arch::enable_interrupts();
    let mut big = Big::lock();
    let (number, args) = frame.syscall();
    match syscall(number, args, &mut big) {
        Ok(value) => frame.set_syscall_result(0, value),
        Err(Failure { error, value }) => frame.set_syscall_result(error.code(), value),
    }
}

/// The big kernel lock, which every syscall holds through its body, one
/// syscall at a time across CPUs, over whatever the object locks don't
/// cover. A tick that preempts the holder doesn't release it, since the
/// holder may hold object locks that another CPU's holder of the big lock
/// would spin for.
static BIG: Lock<()> = Lock::new(Rank::BIG, ());

/// The big lock as a syscall holds it, which [`Big::unlocked`] gives up
/// while the syscall parks.
pub struct Big(Option<thisos_locks::Guard<'static, (), Arch>>);

impl Big {
    /// Takes the lock, with interrupts on and no lock held.
    fn lock() -> Self {
        Self(Some(BIG.lock()))
    }

    /// Holds nothing, for kernel code that calls what a syscall calls.
    #[cfg(test)]
    pub fn none() -> Self {
        Self(None)
    }

    /// Runs `f`, which parks, without the lock, and takes it again after if
    /// it was held.
    pub fn unlocked<T>(&mut self, f: impl FnOnce() -> T) -> T {
        let held = self.0.take().is_some();
        let value = f();
        // With interrupts on, as the syscall has them: a holder that a tick
        // preempted on this CPU has to run to let go of it.
        if held {
            *self = Self::lock();
        }
        value
    }
}

/// Why a syscall failed, and the value it returns with the error: 0, but
/// for receive's [`Error::BufferTooSmall`], which gives the sizes it needed.
struct Failure {
    error: Error,
    value: u64,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self { error, value: 0 }
    }
}

/// Runs the syscall `number` with `args`, holding `big`, and returns its
/// value.
fn syscall(number: u64, args: [u64; 6], big: &mut Big) -> Result<u64, Failure> {
    let result = match Syscall::try_from(number)? {
        Syscall::Exit => {
            end(End::Exited(args[0]));
            // The thread never returns to drop it.
            big.0 = None;
            thread::exit()
        }
        Syscall::Write => write(args[0], args[1], big),
        Syscall::Close => process().close(handle(args[0])?).map(|()| 0),
        Syscall::Duplicate => process()
            .duplicate(handle(args[0])?, rights(args[1])?)
            .map(u64::from),
        Syscall::Restrict => process()
            .restrict(handle(args[0])?, rights(args[1])?)
            .map(|()| 0),
        Syscall::MemoryCreate => memory_create(args[0]),
        Syscall::Map => map(&space(), args[0], args[1], args[2], args[3], args[4]),
        Syscall::Unmap => space().unmap(args[0]).map(|_| 0).ok_or(Error::BadAddress),
        Syscall::ChannelCreate => channel_create(),
        Syscall::Send => send(args[0], args[1], args[2], args[3], args[4]),
        Syscall::Receive => return receive(args[0], args[1], args[2], args[3], args[4]),
        Syscall::Wait => wait(args[0], args[1], args[2], big),
        Syscall::ProcessCreate => process_create(),
        Syscall::MapInto => target(args[0], Rights::WRITE)?
            .map_into(|space| map(space, args[1], args[2], args[3], args[4], args[5])),
        Syscall::ProcessStart => process_start(args[0], args[1], args[2], args[3]),
        Syscall::ProcessEnd => target(args[0], Rights::READ)?.exit_status(),
        Syscall::MemoryRead => memory_read(args[0], args[1], args[2], args[3]),
        Syscall::MemoryWrite => memory_write(args[0], args[1], args[2], args[3]),
    };
    result.map_err(Failure::from)
}

/// Logs how the running thread's process ended, and records it there.
fn end(end: End) {
    match end {
        End::Exited(status) => log::info!("user process exited with status {status:#x}"),
        End::Killed { trap, ip } => log::warn!("user process killed by {trap} at {ip:#x}"),
    }
    process().set_end(end);
}

/// The running thread's process.
fn process() -> Arc<Process> {
    let Some(process) = thread::process() else {
        panic!("a kernel thread trapped as user code");
    };
    process
}

/// The running thread's address space.
fn space() -> Arc<Space> {
    let Some(space) = thread::space() else {
        panic!("a kernel thread trapped as user code");
    };
    space
}

fn handle(arg: u64) -> Result<u32, Error> {
    u32::try_from(arg).map_err(|_| Error::BadHandle)
}

fn rights(arg: u64) -> Result<Rights, Error> {
    u32::try_from(arg)
        .ok()
        .and_then(Rights::from_bits)
        .ok_or(Error::BadRights)
}

/// The pages in `len` bytes, which must be a nonzero multiple of 4096.
fn pages(len: u64) -> Result<usize, Error> {
    if len == 0 || len % 4096 != 0 {
        return Err(Error::OutOfRange);
    }
    usize::try_from(len / 4096).map_err(|_| Error::OutOfRange)
}

/// Makes a memory object of `size` bytes, as the memory-create syscall
/// does, and returns a handle to it.
fn memory_create(size: u64) -> Result<u64, Error> {
    let object = MemoryObject::new(pages(size)?)?;
    let object = Arc::try_new(object).map_err(|_| Error::NoMemory)?;
    process()
        .insert(Object::Memory(object), Rights::ALL)
        .map(u64::from)
}

/// Maps `len` bytes of the memory object `handle` names from `offset` at
/// `addr` in `space`, with the rights in `rights`, as the map syscall does.
fn map(
    space: &Space,
    handle_arg: u64,
    offset: u64,
    len: u64,
    addr: u64,
    rights_arg: u64,
) -> Result<u64, Error> {
    let (object, held) = process().get(handle(handle_arg)?)?;
    let Object::Memory(object) = object else {
        return Err(Error::WrongType);
    };
    let wanted = rights(rights_arg)?;
    let protection = if wanted == Rights::READ {
        Protection::ReadOnly
    } else if wanted == Rights::READ | Rights::WRITE {
        Protection::ReadWrite
    } else if wanted == Rights::READ | Rights::EXECUTE {
        Protection::ReadExecute
    } else {
        return Err(Error::BadRights);
    };
    if !held.contains(wanted) {
        return Err(Error::AccessDenied);
    }
    if offset % 4096 != 0 {
        return Err(Error::BadAddress);
    }
    let pages = pages(len)?;
    let first = usize::try_from(offset / 4096).map_err(|_| Error::OutOfRange)?;
    if first
        .checked_add(pages)
        .is_none_or(|end| end > object.pages())
    {
        return Err(Error::OutOfRange);
    }
    let mapping = Mapping::new(object, protection)?;
    space
        .map(addr, mapping, first, pages, protection, &mut frames::alloc)
        .map_err(|e| match e.kind {
            MapErrorKind::Mapped => Error::AddressInUse,
            MapErrorKind::NoFrames | MapErrorKind::NoMemory => Error::NoMemory,
            MapErrorKind::Unaligned
            | MapErrorKind::OutsideUserSpace
            | MapErrorKind::OutsideKernelSpace
            | MapErrorKind::OutOfRange => Error::BadAddress,
        })?;
    Ok(0)
}

/// The process `handle_arg` names, if its handle has `rights`.
fn target(handle_arg: u64, rights: Rights) -> Result<Arc<Process>, Error> {
    let (object, held) = process().get(handle(handle_arg)?)?;
    let Object::Process(target) = object else {
        return Err(Error::WrongType);
    };
    if !held.contains(rights) {
        return Err(Error::AccessDenied);
    }
    Ok(target)
}

/// Makes a process with an empty address space, as the process-create
/// syscall does, and returns a handle to it.
fn process_create() -> Result<u64, Error> {
    let space = paging::new_space().ok_or(Error::NoMemory)?;
    let space = Arc::try_new(space).map_err(|_| Error::NoMemory)?;
    let created = Process::new(space)?;
    process()
        .insert(Object::Process(created), PROCESS_RIGHTS)
        .map(u64::from)
}

/// Starts the process `target_arg` names at `ip` with stack pointer `sp`,
/// moving the caller's handle `start_arg` into it, as the process-start
/// syscall does.
fn process_start(target_arg: u64, ip: u64, sp: u64, start_arg: u64) -> Result<u64, Error> {
    let target = target(target_arg, Rights::WRITE)?;
    let start = handle(start_arg)?;
    let caller = process();
    target.start(ip, sp, || {
        let mut taken = caller.take(&[start], |_, rights| {
            if rights.contains(Rights::TRANSFER) {
                Ok(())
            } else {
                Err(Error::AccessDenied)
            }
        })?;
        let Some(taken) = taken.pop() else {
            unreachable!("take took the one handle or failed");
        };
        Ok(taken)
    })?;
    Ok(0)
}

/// The memory object `handle_arg` names, if its handle has `rights`, and
/// `offset` and `len` as sizes.
fn memory(
    handle_arg: u64,
    rights: Rights,
    offset: u64,
    len: u64,
) -> Result<(Arc<MemoryObject>, usize, usize), Error> {
    let (object, held) = process().get(handle(handle_arg)?)?;
    let Object::Memory(object) = object else {
        return Err(Error::WrongType);
    };
    if !held.contains(rights) {
        return Err(Error::AccessDenied);
    }
    let (Ok(offset), Ok(len)) = (usize::try_from(offset), usize::try_from(len)) else {
        return Err(Error::OutOfRange);
    };
    Ok((object, offset, len))
}

/// `addr` plus `done`, or `None` past the end of the address space.
fn at(addr: u64, done: usize) -> Option<u64> {
    addr.checked_add(u64::try_from(done).ok()?)
}

/// Copies `len` bytes from `offset` in the memory object `handle_arg` names
/// to `addr`, as the memory-read syscall does.
fn memory_read(handle_arg: u64, offset: u64, addr: u64, len: u64) -> Result<u64, Error> {
    let (object, offset, len) = memory(handle_arg, Rights::READ, offset, len)?;
    let space = space();
    let copied = object.read(offset, len, |done, piece| {
        at(addr, done).is_some_and(|addr| copy_to_user(&space, addr, piece))
    })?;
    if copied {
        Ok(0)
    } else {
        Err(Error::BadAddress)
    }
}

/// Copies `len` bytes from `addr` into the memory object `handle_arg` names
/// from `offset`, as the memory-write syscall does.
fn memory_write(handle_arg: u64, offset: u64, addr: u64, len: u64) -> Result<u64, Error> {
    let (object, offset, len) = memory(handle_arg, Rights::WRITE, offset, len)?;
    let space = space();
    let copied = object.write(offset, len, |done, piece| {
        at(addr, done).is_some_and(|addr| copy_from_user(&space, addr, piece))
    })?;
    if copied {
        Ok(0)
    } else {
        Err(Error::BadAddress)
    }
}

/// Makes a channel, as the channel-create syscall does, and returns its
/// endpoints' handles, the first in the low 32 bits.
fn channel_create() -> Result<u64, Error> {
    let (first, second) = channel::create()?;
    let mut endpoints = Vec::new();
    endpoints
        .try_reserve_exact(2)
        .map_err(|_| Error::NoMemory)?;
    for endpoint in [first, second] {
        let endpoint = Arc::try_new(endpoint).map_err(|_| Error::NoMemory)?;
        endpoints.push((Object::Channel(endpoint), ENDPOINT_RIGHTS));
    }
    // On failure the endpoints drop here, after the table's lock is gone.
    process()
        .insert_all(endpoints, |handles| {
            Ok(u64::from(handles[1]) << 32 | u64::from(handles[0]))
        })
        .map_err(|(e, _)| e)
}

/// The channel endpoint `handle_arg` names, if its handle has `rights`.
fn endpoint(handle_arg: u64, rights: Rights) -> Result<Arc<Endpoint>, Error> {
    let (object, held) = process().get(handle(handle_arg)?)?;
    let Object::Channel(endpoint) = object else {
        return Err(Error::WrongType);
    };
    if !held.contains(rights) {
        return Err(Error::AccessDenied);
    }
    Ok(endpoint)
}

/// A count from user code, if it is at most `max`.
fn count(arg: u64, max: usize) -> Result<usize, Error> {
    usize::try_from(arg)
        .ok()
        .filter(|&n| n <= max)
        .ok_or(Error::TooLong)
}

/// Sends the `len` bytes at `buf` and the `count` handles at `handles`
/// over the endpoint `handle_arg` names, as the send syscall does.
fn send(handle_arg: u64, buf: u64, len: u64, handles: u64, count_arg: u64) -> Result<u64, Error> {
    let endpoint = endpoint(handle_arg, Rights::WRITE)?;
    let len = count(len, MAX_MESSAGE_BYTES)?;
    let count = count(count_arg, MAX_MESSAGE_HANDLES)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
    bytes.resize(len, 0);
    let space = space();
    if !copy_from_user(&space, buf, &mut bytes) {
        return Err(Error::BadAddress);
    }
    let mut raw = [0; 4 * MAX_MESSAGE_HANDLES];
    let raw = &mut raw[..4 * count];
    if !copy_from_user(&space, handles, raw) {
        return Err(Error::BadAddress);
    }
    let mut numbers = [0; MAX_MESSAGE_HANDLES];
    for (number, chunk) in numbers.iter_mut().zip(raw.as_chunks::<4>().0) {
        *number = u32::from_le_bytes(*chunk);
    }
    let process = process();
    endpoint.send(|| {
        let handles = process.take(&numbers[..count], |object, rights| {
            if !rights.contains(Rights::TRANSFER) {
                return Err(Error::AccessDenied);
            }
            match object {
                Object::Channel(carried) if carried.same_channel(&endpoint) => Err(Error::Cycle),
                _ => Ok(()),
            }
        })?;
        Ok(Message { bytes, handles })
    })?;
    Ok(0)
}

/// Receives the oldest message at the endpoint `handle_arg` names into the
/// `len` bytes at `buf` and the `count` handles at `handles`, as the
/// receive syscall does, and returns the message's sizes; see [`sizes`].
/// Fails with those sizes as its value if the buffers are too small.
fn receive(handle_arg: u64, buf: u64, len: u64, handles: u64, count: u64) -> Result<u64, Failure> {
    let endpoint = endpoint(handle_arg, Rights::READ)?;
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    let process = process();
    let space = space();
    let mut needed = 0;
    let received = endpoint.receive(|message| {
        if message.bytes.len() > len || message.handles.len() > count {
            needed = sizes(&message);
            return Err((Error::BufferTooSmall, message));
        }
        let value = sizes(&message);
        let Message {
            bytes,
            handles: objects,
        } = message;
        process
            .insert_all(objects, |numbers| {
                let mut raw = [0; 4 * MAX_MESSAGE_HANDLES];
                for (chunk, number) in raw.as_chunks_mut::<4>().0.iter_mut().zip(numbers) {
                    chunk.copy_from_slice(&number.to_le_bytes());
                }
                if copy_to_user(&space, buf, &bytes)
                    && copy_to_user(&space, handles, &raw[..4 * numbers.len()])
                {
                    Ok(value)
                } else {
                    Err(Error::BadAddress)
                }
            })
            .map_err(|(e, handles)| (e, Message { bytes, handles }))
    });
    received.map_err(|error| Failure {
        error,
        value: if error == Error::BufferTooSmall {
            needed
        } else {
            0
        },
    })
}

/// `message`'s byte count in the low 32 bits and its handle count in the
/// high 32, as receive returns them.
fn sizes<T>(message: &Message<T>) -> u64 {
    // Both are at most the limits, so they fit 32 bits.
    u64::try_from(message.handles.len()).unwrap_or(0) << 32
        | u64::try_from(message.bytes.len()).unwrap_or(0)
}

/// Waits on the `count` items at `items` for at most `timeout`
/// nanoseconds, as the wait syscall does.
fn wait(items: u64, count_arg: u64, timeout: u64, big: &mut Big) -> Result<u64, Error> {
    let count = count(count_arg, MAX_WAIT_ITEMS)?;
    // Nothing could end it, and no syscall ends another process.
    if count == 0 && timeout == u64::MAX {
        return Err(Error::BadSignals);
    }
    let mut raw = [0; WAIT_ITEM_SIZE * MAX_WAIT_ITEMS];
    let raw = &mut raw[..WAIT_ITEM_SIZE * count];
    // Written back unchanged, so that items the call can't write its result
    // into fail before it blocks rather than after.
    let space = space();
    if !copy_from_user(&space, items, raw) || !copy_to_user(&space, items, raw) {
        return Err(Error::BadAddress);
    }
    let mut waits = Vec::new();
    waits
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let process = process();
    for item in raw.as_chunks::<WAIT_ITEM_SIZE>().0 {
        let [handle, wanted] = [0, 4]
            .map(|at| u32::from_le_bytes([item[at], item[at + 1], item[at + 2], item[at + 3]]));
        let (object, held) = process.get(handle)?;
        let object = Waitable::new(object).ok_or(Error::WrongType)?;
        if !held.contains(Rights::READ) {
            return Err(Error::AccessDenied);
        }
        // A signal the object never has would leave the wait to its
        // timeout, and with none, blocked for good.
        let wanted = Signals::from_bits(wanted)
            .filter(|&wanted| wanted != Signals::NONE && object.signals().contains(wanted))
            .ok_or(Error::BadSignals)?;
        waits.push((object, wanted));
    }
    let mut observed = [Signals::NONE; MAX_WAIT_ITEMS];
    let observed = &mut observed[..count];
    let ready = wait::wait(&waits, observed, wait::deadline(timeout), big)?;
    for (item, signals) in raw
        .as_chunks_mut::<WAIT_ITEM_SIZE>()
        .0
        .iter_mut()
        .zip(observed)
    {
        item[8..].copy_from_slice(&signals.bits().to_le_bytes());
    }
    if !copy_to_user(&space, items, raw) {
        return Err(Error::BadAddress);
    }
    // At most MAX_WAIT_ITEMS.
    Ok(u64::try_from(ready).unwrap_or(0))
}

/// Logs the `len` bytes of text at `buf` in user space as the write
/// syscall does, one record per line, and returns `len`. Copies the text
/// into a buffer on the stack first, and waits while the log has no room
/// for all of it.
fn write(buf: u64, len: u64, big: &mut Big) -> Result<u64, Error> {
    let mut bytes = [0; MAX_WRITE];
    let bytes = usize::try_from(len)
        .ok()
        .and_then(|len| bytes.get_mut(..len))
        .ok_or(Error::TooLong)?;
    if !copy_from_user(&space(), buf, bytes) {
        return Err(Error::BadAddress);
    }
    let text = str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    logger::write_user(text.split('\n').map(Escaped), big)?;
    #[cfg(test)]
    Arch::without_interrupts(|| {
        use fmt::Write as _;
        let mut written = WRITTEN.lock();
        written.len = 0;
        for (i, line) in text.split('\n').enumerate() {
            let newline = if i == 0 { "" } else { "\n" };
            assert!(
                write!(written, "{newline}{}", Escaped(line)).is_ok(),
                "WRITTEN is full"
            );
        }
    });
    Ok(len)
}

/// Copies `bytes` to `addr` in `space`, as
/// [`thisos_hal::AddressSpace::copy_to_user`] does, a page at a time; see
/// [`by_page`].
fn copy_to_user(space: &Space, addr: u64, bytes: &[u8]) -> bool {
    by_page(addr, bytes.len(), |addr, range| {
        bytes
            .get(range)
            .is_some_and(|chunk| space.copy_to_user(addr, chunk))
    })
}

/// Copies the bytes at `addr` in `space` into `bytes`, as
/// [`thisos_hal::AddressSpace::copy_from_user`] does, a page at a time;
/// see [`by_page`].
fn copy_from_user(space: &Space, addr: u64, bytes: &mut [u8]) -> bool {
    by_page(addr, bytes.len(), |addr, range| {
        bytes
            .get_mut(range)
            .is_some_and(|chunk| space.copy_from_user(addr, chunk))
    })
}

/// Splits the `len` bytes at `addr` in user space at page boundaries and
/// runs `copy` with each piece's address and its range in the `len`
/// bytes, in order, until it returns false. Returns whether every `copy`
/// returned true.
fn by_page(addr: u64, len: usize, mut copy: impl FnMut(u64, Range<usize>) -> bool) -> bool {
    let mut addr = addr;
    let mut done = 0;
    while done < len {
        let in_page = 4096 - addr % 4096;
        let end = len.min(done.saturating_add(usize::try_from(in_page).unwrap_or(usize::MAX)));
        if !copy(addr, done..end) {
            return false;
        }
        // The page was user space, so the next one's address fits.
        addr += in_page;
        done = end;
    }
    true
}

/// A line of user text with its control characters shown as escapes, such
/// as `\u{1b}`, so that it can't move the terminal's cursor.
struct Escaped<'a>(&'a str);

impl fmt::Display for Escaped<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            if c.is_control() {
                write!(f, "{}", c.escape_default())?;
            } else {
                fmt::Write::write_char(f, c)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::ops::Range;
    use core::ptr;
    use core::sync::atomic::AtomicU64;

    use thisos_hal::{Access, Cpu, UserProgram};

    use super::*;
    use crate::cpu;
    use crate::testing::{self, spin_until};

    /// The records of the last write syscall, joined by newlines, which it
    /// then forgets.
    fn take_written() -> String {
        Arch::without_interrupts(|| {
            let mut written = WRITTEN.lock();
            let text = String::from_utf8_lossy(&written.bytes[..written.len]).into_owned();
            written.len = 0;
            text
        })
    }

    /// Runs `program` in a new process and space, with its code in the
    /// first user page and a stack page after it, and the argument `arg`
    /// returns given the stack page's address and the process. Returns how
    /// it ended and where its code was; see [`finish`].
    fn run(program: UserProgram, arg: impl FnOnce(u64, &Arc<Process>) -> u64) -> (End, u64) {
        run_with_stack(program, |stack, process| (Vec::new(), arg(stack, process)))
    }

    /// Runs `program` as [`run`] does, with the bytes `setup` returns at
    /// the start of its stack page, and the argument it returns.
    fn run_with_stack(
        program: UserProgram,
        setup: impl FnOnce(u64, &Arc<Process>) -> (Vec<u8>, u64),
    ) -> (End, u64) {
        run_at(program, |user| (user.start, user.start + 4096), setup)
    }

    /// Runs `program` as [`run_with_stack`] does, with its code and stack
    /// pages where `place` puts them in user space.
    fn run_at(
        program: UserProgram,
        place: impl FnOnce(Range<u64>) -> (u64, u64),
        setup: impl FnOnce(u64, &Arc<Process>) -> (Vec<u8>, u64),
    ) -> (End, u64) {
        let before = frames::free_frames();
        let tables = paging::stack_table_frames();
        let (process, code) = start_at(program, place, setup);
        (finish(&process, before, tables, code), code)
    }

    /// Starts `program` as [`run_at`] does, and returns its process and
    /// where its code is.
    fn start_at(
        program: UserProgram,
        place: impl FnOnce(Range<u64>) -> (u64, u64),
        setup: impl FnOnce(u64, &Arc<Process>) -> (Vec<u8>, u64),
    ) -> (Arc<Process>, u64) {
        let Some(space) = paging::new_space() else {
            panic!("no free frame for a space");
        };
        let space = Arc::new(space);
        let (code, stack) = place(space.user_space());
        let Ok(process) = Process::new(Arc::clone(&space)) else {
            panic!("no memory for a process");
        };
        let (bytes, arg) = setup(stack, &process);
        let mut code_object = page_object();
        Arch::write_user_program(&mut code_object.frames_mut()[0], program);
        let mut stack_object = page_object();
        Arch::fill_frame(&mut stack_object.frames_mut()[0], 0, &bytes);
        for (page, object, protection) in [
            (code, code_object, Protection::ReadExecute),
            (stack, stack_object, Protection::ReadWrite),
        ] {
            if let Err(e) = map_object(&space, page, object, protection) {
                panic!("{e}");
            }
        }
        // The process and its thread hold the space from here, so that it
        // goes with them.
        drop(space);
        process.start_with(code, stack + 4096, arg);
        (process, code)
    }

    /// Runs the program `name` from the ramdisk as the kernel runs init,
    /// and returns how it ended; see [`finish`].
    fn run_program(name: &str) -> End {
        run_program_by(name, start_with_ramdisk, || {})
    }

    /// Runs the program `name` from the ramdisk with its one handle
    /// `endpoint`, whose peer is closed, and returns how it ended; see
    /// [`finish`].
    fn run_program_alone(name: &str, endpoint: Endpoint) -> End {
        run_program_by(name, |file, _| start_program(file, endpoint), || {})
    }

    /// Starts the program `name` from the ramdisk by `start`, given its
    /// file and the ramdisk's copy, runs `meanwhile`, and returns how the
    /// program ended; see [`finish`].
    fn run_program_by(
        name: &str,
        start: impl FnOnce(&[u8], Arc<MemoryObject>) -> Result<Arc<Process>, LoadError>,
        meanwhile: impl FnOnce(),
    ) -> End {
        let Some((archive, object)) = ramdisk() else {
            panic!("no ramdisk");
        };
        let file = ramdisk_file(archive, name);
        let entry = match Elf::new(file, Arch::ELF_MACHINE) {
            Ok(elf) => elf.entry(),
            Err(e) => panic!("{name} isn't an executable: {e:?}"),
        };
        let before = frames::free_frames();
        let tables = paging::stack_table_frames();
        let process = match start(file, object) {
            Ok(process) => process,
            Err(e) => panic!("starting {name} failed: {e}"),
        };
        meanwhile();
        finish(&process, before, tables, entry)
    }

    /// Checks that a ramdisk program panicked with `message`: that it
    /// exited with 101, rt's panic status, and that the last record it
    /// wrote, its panic report, ends with `message`.
    fn assert_panicked(end: End, message: &str) {
        let written = take_written();
        assert_eq!(end, End::Exited(101), "wrote {written:?}");
        assert!(written.ends_with(message), "wrote {written:?}");
    }

    /// Waits until `process`, whose code starts at `code`, has ended and
    /// its thread is freed, and returns how it ended. Checks that it closed
    /// its handles, and that every frame taken since there were `before`
    /// free came back: those of its space and pages, and those of its
    /// threads' stacks. Kernel-space tables the stacks needed, which
    /// [`paging::stack_table_frames`] counted from `tables`, stay, so those
    /// are taken off. It blocks while it waits, so that a program that
    /// passes the CPU back and forth, as ping and pong do, doesn't wait out
    /// this thread's slice each time, and checks that this thread, a kernel
    /// thread, runs on the kernel's tables when it wakes.
    fn finish(process: &Arc<Process>, before: usize, tables: usize, code: u64) -> End {
        wait_for_end(process);
        assert_eq!(Arch::translate_active(code), None, "user code's page");
        thread::wait_for_exits();
        let tables = paging::stack_table_frames() - tables;
        assert_eq!(frames::free_frames(), before - tables, "frames leaked");
        let Some(end) = process.end() else {
            panic!("the process ended with no end recorded");
        };
        // Handles to itself are closed, and its thread is gone.
        assert_eq!(Arc::strong_count(process), 1);
        end
    }

    /// Blocks until `process` has ended, for up to 9 s, under the
    /// watchdog's 10.
    fn wait_for_end(process: &Arc<Process>) {
        let items = [(Waitable::Process(Arc::clone(process)), Signals::ENDED)];
        let deadline = wait::deadline(9_000_000_000);
        match wait::wait(&items, &mut [Signals::NONE], deadline, &mut Big::none()) {
            Ok(_) => {}
            Err(Error::TimedOut) => panic!("the user process didn't end within 9 s"),
            Err(e) => panic!("waiting for the user process to end failed: {e:?}"),
        }
    }

    /// A handle to `process` itself, with [`PROCESS_RIGHTS`], as the
    /// argument of a program that checks syscalls on its own handles.
    fn self_handle(_: u64, process: &Arc<Process>) -> u64 {
        let Ok(handle) = process.insert_self(PROCESS_RIGHTS) else {
            panic!("a new handle table is full");
        };
        u64::from(handle)
    }

    fn page_object() -> MemoryObject {
        let Ok(object) = MemoryObject::new(1) else {
            panic!("no memory for a page");
        };
        object
    }

    fn static_address(target: &AtomicU64) -> u64 {
        let Ok(address) = u64::try_from(ptr::from_ref(target).addr()) else {
            panic!("a static's address doesn't fit a u64");
        };
        address
    }

    #[test_case]
    fn init_starts_its_programs_and_reports_their_ends() {
        take_written();
        assert_eq!(run_program("init"), End::Exited(0));
        assert_eq!(
            take_written(),
            "init: hello exited with status 0x5, fault was killed, ping and pong exited with status 0x0"
        );
    }

    // by_page splits every copy at page boundaries, so the HAL's copies
    // check only one page; one that crossed would check too little.
    #[test_case]
    const A_USER_COPY_ACROSS_A_PAGE_PANICS: crate::testing::ExpectPanic =
        crate::testing::ExpectPanic {
            name: "thisos_kernel::user::tests::a_user_copy_across_a_page_panics",
            run: || {
                let Some(space) = paging::new_space() else {
                    panic!("no free frame for a space");
                };
                space.copy_from_user(0x1_0fff, &mut [0; 2]);
            },
            message: "copying 2 bytes at 0x10fff crosses a page",
            after: None,
        };

    // by_page copies a page at a time, so a write that runs into a page it
    // can't write has written the bytes before that page.
    #[test_case]
    fn a_user_copy_writes_up_to_the_first_bad_page() {
        let Some(space) = paging::new_space() else {
            panic!("no free frame for a space");
        };
        for (page, protection) in [
            (0x40_0000, Protection::ReadWrite),
            (0x40_1000, Protection::ReadOnly),
        ] {
            if let Err(e) = map_object(&space, page, page_object(), protection) {
                panic!("mapping {page:#x} failed: {e}");
            }
        }
        assert!(!copy_to_user(&space, 0x40_0fff, b"ab"));
        let mut bytes = [0; 2];
        assert!(copy_from_user(&space, 0x40_0fff, &mut bytes));
        assert_eq!(&bytes, b"a\0");
    }

    // checks tests the process-building, memory-copy and wait syscalls' error
    // paths from Rust, and panics, exiting with 101, at the first that
    // fails.
    #[test_case]
    fn the_process_and_memory_checks_pass() {
        assert_eq!(run_program("checks"), End::Exited(0));
    }

    // A creator that closes without sending, or a pong that closes without
    // answering, sends nothing more, so a wait for READABLE alone would
    // never return.
    #[test_case]
    fn hello_ends_when_its_creator_closes_without_a_line() {
        let (creator, hello) = channel::pair();
        drop(creator);
        take_written();
        assert_panicked(
            run_program_alone("hello", hello),
            "receiving the line failed: PeerClosed",
        );
    }

    #[test_case]
    fn ping_ends_when_its_creator_closes_without_pong() {
        let (creator, ping) = channel::pair();
        drop(creator);
        take_written();
        assert_panicked(
            run_program_alone("ping", ping),
            "receiving the endpoint to pong failed: PeerClosed",
        );
    }

    #[test_case]
    fn ping_ends_when_pong_closes_without_answering() {
        let (creator, to_creator) = channel::pair();
        let (pong, to_pong) = channel::pair();
        let handles = vec![(Object::Channel(Arc::new(to_pong)), ENDPOINT_RIGHTS)];
        let sent = creator.send(|| {
            Ok(Message {
                bytes: Vec::new(),
                handles,
            })
        });
        assert!(sent.is_ok(), "sending ping its endpoint to pong failed");
        take_written();
        let end = run_program_by(
            "ping",
            |file, _| start_program(file, to_creator),
            || {
                let items = [(Waitable::Channel(Arc::new(pong)), Signals::READABLE)];
                // Once ping has sent, it waits for the answer, and pong
                // closes with the message still queued.
                let deadline = wait::deadline(5_000_000_000);
                let first = wait::wait(&items, &mut [Signals::NONE], deadline, &mut Big::none());
                assert_eq!(first, Ok(1), "waiting for ping's first message");
            },
        );
        assert_panicked(end, "receiving pong's answer failed: PeerClosed");
        drop(creator);
    }

    #[test_case]
    fn handle_syscalls_check_handles_and_rights() {
        let (end, _) = run(UserProgram::Handles, self_handle);
        assert_eq!(end, End::Exited(0));
    }

    #[test_case]
    fn memory_syscalls_create_map_share_and_unmap() {
        let (end, _) = run(UserProgram::Memory, self_handle);
        assert_eq!(end, End::Exited(0));
    }

    #[test_case]
    fn channel_syscalls_move_bytes_and_handles() {
        let (end, _) = run(UserProgram::Channel, self_handle);
        assert_eq!(end, End::Exited(0));
    }

    #[test_case]
    fn wait_syscall_checks_its_items_and_observes_signals() {
        let (end, _) = run(UserProgram::Wait, self_handle);
        assert_eq!(end, End::Exited(0));
    }

    // Programs whose syscalls cover handles, memory objects, channels and
    // waits run at once, as many of each as there are CPUs, spread over
    // them, and each passes its checks.
    #[test_case]
    fn syscalls_on_every_cpu_at_once_pass_their_checks() {
        if !testing::several_cpus() {
            return;
        }
        let before = frames::free_frames();
        let tables = paging::stack_table_frames();
        let programs = [
            UserProgram::Handles,
            UserProgram::Memory,
            UserProgram::Channel,
            UserProgram::Wait,
        ];
        let started: Vec<_> = (0..cpu::count())
            .flat_map(|_| programs)
            .map(|program| {
                let place = |user: Range<u64>| (user.start, user.start + 4096);
                let setup =
                    |stack, process: &Arc<Process>| (Vec::new(), self_handle(stack, process));
                (program, start_at(program, place, setup).0)
            })
            .collect();
        for (program, process) in &started {
            wait_for_end(process);
            assert_eq!(process.end(), Some(End::Exited(0)), "{program:?}");
        }
        thread::wait_for_exits();
        let tables = paging::stack_table_frames() - tables;
        assert_eq!(frames::free_frames(), before - tables, "frames leaked");
    }

    // A user thread never returns from its entry, so what started it must
    // be freed before it enters user code.
    #[test_case]
    fn a_user_thread_leaves_the_heap_as_it_found_it() {
        // The first run grows the scheduler's queues to fit, which they
        // keep.
        run(UserProgram::Syscalls, |_, _| 0);
        for program in [UserProgram::Syscalls, UserProgram::Privileged] {
            let allocated = Arch::heap_stats().allocated;
            run(program, |_, _| 0);
            assert_eq!(Arch::heap_stats().allocated, allocated, "{program:?}");
        }
    }

    #[test_case]
    fn a_syscall_keeps_registers_and_an_unknown_one_fails() {
        let (end, _) = run(UserProgram::Syscalls, |_, _| 0);
        assert_eq!(end, End::Exited(Error::UnknownSyscall.code()));
    }

    /// Runs [`UserProgram::Write`] with `len` and the buffer `buf` returns,
    /// given where `text` is in the stack page, and returns its exit
    /// status and what it wrote.
    fn run_write(buf: impl FnOnce(u64) -> u64, len: usize, text: &[u8]) -> (End, String) {
        run_write_after(|| {}, buf, len, text)
    }

    /// [`run_write`], calling `first` before the program starts but after
    /// [`run_at`] counts the free frames, so that a thread `first` starts
    /// counts among those it checks.
    fn run_write_after(
        first: impl FnOnce(),
        buf: impl FnOnce(u64) -> u64,
        len: usize,
        text: &[u8],
    ) -> (End, String) {
        take_written();
        let (end, _) = run_with_stack(UserProgram::Write, |stack, _| {
            first();
            let at = stack + 16;
            let mut bytes = Vec::new();
            bytes.extend(buf(at).to_le_bytes());
            bytes.extend(u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
            bytes.extend(text);
            (bytes, stack)
        });
        (end, take_written())
    }

    #[test_case]
    fn write_logs_text_without_its_newline() {
        let text = b"written from user code\n";
        let (end, written) = run_write(|at| at, text.len(), text);
        assert_eq!(end, End::Exited(0));
        assert_eq!(written, "written from user code");
        let long = [b'a'; MAX_WRITE];
        let (end, written) = run_write(|at| at, long.len(), &long);
        assert_eq!(end, End::Exited(0));
        assert_eq!(written.len(), MAX_WRITE);
    }

    #[test_case]
    fn write_logs_a_record_per_line_with_control_characters_escaped() {
        let text = b"one\n\ntwo\r\x1b[2J\t\n";
        let (end, written) = run_write(|at| at, text.len(), text);
        assert_eq!(end, End::Exited(0));
        assert_eq!(written, "one\n\ntwo\\r\\u{1b}[2J\\t");
        // A zero-length write reads nothing, so even a null pointer logs
        // its empty line.
        assert_eq!(run_write(|_| 0, 0, b""), (End::Exited(0), String::new()));
    }

    // The thread empties the ring only once the write waits, so the write
    // can't have found room before.
    #[test_case]
    fn a_write_waits_for_room_in_the_log() {
        logger::tests::pause();
        logger::tests::fill();
        let empty_when_waiting = || {
            thread::spawn(|| {
                spin_until("the write to wait", || logger::tests::waiting() == 1);
                logger::tests::discard();
                logger::tests::resume();
            });
        };
        let text = b"written once the log had room\n";
        let (end, written) = run_write_after(empty_when_waiting, |at| at, text.len(), text);
        assert_eq!(end, End::Exited(0));
        assert_eq!(written, "written once the log had room");
        assert_eq!(logger::tests::waiting(), 0);
    }

    #[test_case]
    fn write_rejects_bad_buffers_and_writes_nothing() {
        static TARGET: AtomicU64 = AtomicU64::new(0);
        let failed = |error: Error| (End::Exited(error.code()), String::new());
        let kernel = static_address(&TARGET);
        assert_eq!(run_write(|_| kernel, 1, b""), failed(Error::BadAddress));
        // The last 4 bytes of the stack page, and 4 of the unmapped page
        // after it.
        let across = |at: u64| at - 16 + 4096 - 4;
        assert_eq!(run_write(across, 8, b""), failed(Error::BadAddress));
        assert_eq!(run_write(|at| at, 2, b"\xff\xfe"), failed(Error::NotUtf8));
        assert_eq!(
            run_write(|at| at, MAX_WRITE + 1, b""),
            failed(Error::TooLong)
        );
    }

    #[test_case]
    fn a_syscall_returning_to_user_spaces_end_kills_the_thread() {
        let (end, code) = run_at(
            UserProgram::LastSyscall,
            |user| (user.end - 4096, user.end - 8192),
            |_, _| (Vec::new(), 0),
        );
        let trap = Trap::PageFault {
            address: code + 4096,
            access: Access::Execute,
        };
        assert_eq!(
            end,
            End::Killed {
                trap,
                ip: code + 4096
            }
        );
    }

    #[test_case]
    fn user_code_reads_its_own_stack() {
        let (end, _) = run(UserProgram::Read, |stack, _| stack);
        assert_eq!(end, End::Exited(0));
    }

    #[test_case]
    fn reading_kernel_memory_kills_the_thread() {
        static TARGET: AtomicU64 = AtomicU64::new(0);
        let address = static_address(&TARGET);
        let (end, code) = run(UserProgram::Read, |_, _| address);
        let trap = Trap::PageFault {
            address,
            access: Access::Read,
        };
        assert_eq!(end, End::Killed { trap, ip: code });
    }

    #[test_case]
    fn a_privileged_instruction_kills_the_thread() {
        let (end, code) = run(UserProgram::Privileged, |_, _| 0);
        assert!(
            matches!(end, End::Killed { trap: Trap::Other { .. }, ip } if ip == code),
            "{end:?}"
        );
    }

    // User code that masked interrupts would never be preempted, and would
    // keep the only CPU for good.
    #[test_case]
    fn masking_interrupts_kills_the_thread() {
        let (end, code) = run(UserProgram::MaskInterrupts, |_, _| 0);
        assert!(
            matches!(end, End::Killed { trap: Trap::Other { .. }, ip } if ip == code),
            "{end:?}"
        );
    }

    // A descriptor table's address tells user code where the kernel keeps
    // each CPU's state. x86_64 hides it only where the CPU has UMIP, which
    // CI's CPU model and the test laptop do.
    #[test_case]
    fn reading_a_descriptor_table_kills_the_thread() {
        let (end, code) = run(UserProgram::DescriptorTable, |_, _| 0);
        if end == End::Exited(2) {
            log::warn!("the CPU has no UMIP, so user code can read the GDT's address");
            return;
        }
        assert!(
            matches!(end, End::Killed { trap: Trap::Other { .. }, ip } if ip >= code),
            "{end:?}"
        );
    }

    // The second run would start with the value the first left, were they
    // not kept per thread. 0x1b is x86_64's user data selector, the one
    // value ring 3 can load that survives an interrupt: returning to ring 3
    // through iretq zeroes a null selector whatever its RPL.
    #[test_case]
    fn thread_registers_stay_with_their_thread() {
        for run_number in 1..=2 {
            let (end, _) = run(UserProgram::ThreadRegisters, |_, _| 0x1b);
            assert_eq!(end, End::Exited(0), "run {run_number}");
        }
    }

    // On x86_64 the exception arrives on the stack every user thread's
    // debug exceptions share. ICEBP raises it as a trap, after the
    // instruction, so the CPU reports the byte after it. aarch64's is a
    // brk, which the arch crate reports as a breakpoint after it, as it
    // does the kernel's own.
    #[test_case]
    fn a_debug_exception_kills_the_thread() {
        let (end, code) = run(UserProgram::Debug, |_, _| 0);
        #[cfg(target_arch = "x86_64")]
        let (trap, ip) = (
            Trap::Other {
                vector: 1,
                error: 0,
            },
            code + 1,
        );
        #[cfg(target_arch = "aarch64")]
        let (trap, ip) = (Trap::Breakpoint, code + 4);
        assert_eq!(end, End::Killed { trap, ip });
    }

    // The kernel saves no SIMD state, so user code may not use it.
    #[test_case]
    fn simd_kills_the_thread() {
        let (end, code) = run(UserProgram::Simd, |_, _| 0);
        assert!(
            matches!(end, End::Killed { trap: Trap::Other { .. }, ip } if ip == code),
            "{end:?}"
        );
    }

    // The user thread spins through many ticks, each of which preempts it
    // in ring 3 and switches to this thread.
    #[test_case]
    fn ticks_preempt_user_code() {
        let (end, _) = run(UserProgram::Spin, |_, _| 0);
        assert_eq!(end, End::Exited(0));
    }
}
