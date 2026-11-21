//! The runtime every ThisOS program links: its entry point, its syscalls,
//! [`Handle`], [`wait`], [`print!`] and [`println!`], its panic handler,
//! and a heap behind `alloc`. A program names its main function with
//! [`entry!`].

#![no_std]

extern crate alloc;

// Host tests run on std's allocator.
#[cfg(not(test))]
mod heap;

#[cfg_attr(target_arch = "x86_64", path = "arch/x86_64.rs")]
#[cfg_attr(target_arch = "aarch64", path = "arch/aarch64.rs")]
mod arch;

use alloc::vec::Vec;
use core::fmt::{self, Write as _};
use core::marker::PhantomData;
use core::mem;
#[cfg(not(test))]
use core::panic::PanicInfo;
#[cfg(not(test))]
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

pub use thisos_abi::{Error, Rights, Signals};
pub use thisos_abi::{MAX_MESSAGE_BYTES, MAX_MESSAGE_HANDLES, MAX_WAIT_ITEMS, USER_SPACE_END};
use thisos_abi::{MAX_WRITE, Syscall};

/// Makes `main`, a `fn(Handle<Channel>) -> u64`, the program's main
/// function. It gets the handle its creator started it with, by convention
/// a channel endpoint over which the creator sends the rest, and the
/// process ends through [`exit`] with the status it returns.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        // Inside an anonymous const, so the program can't name the exported
        // function and call it a second time.
        const _: () = {
            #[unsafe(export_name = "__thisos_main")]
            fn __thisos_main(arg: u64) -> u64 {
                let main: fn($crate::Handle<$crate::Channel>) -> u64 = $main;
                main($crate::Handle::__from_start(arg))
            }
        };
    };
}

// Safe to call: only entry! defines the symbol, with this signature, since
// defining it any other way takes unsafe code, which programs forbid. Host
// tests link no program, so they leave out everything that names it, and
// the panic handler, which std has.
#[cfg(not(test))]
unsafe extern "Rust" {
    /// The function `entry!` exports.
    safe fn __thisos_main(arg: u64) -> u64;
}

pub use arch::{ELF_MACHINE, cycles, cycles_hz, exit};

/// The first Rust code, which the architecture's `_start` calls.
#[cfg(not(test))]
extern "C" fn start(arg: u64) -> ! {
    exit(__thisos_main(arg))
}

/// Logs `text` in the kernel's log, one record per line, without a newline
/// at its end. Fails with [`Error::TooLong`] for more than [`MAX_WRITE`]
/// bytes.
pub fn write(text: &str) -> Result<(), Error> {
    match write_bytes(text.as_bytes()) {
        0 => Ok(()),
        code => Err(error(code)),
    }
}

/// Makes a syscall that does next to nothing, close(0), which fails with
/// [`Error::BadHandle`] since 0 is never a handle: the baseline a timing
/// of other syscalls is set against. Panics if the kernel answers
/// otherwise.
pub fn null_syscall() {
    match syscall(Syscall::Close, [0]) {
        Err(Error::BadHandle) => {}
        other => panic!("close(0) answered {other:?}"),
    }
}

/// Makes the write syscall with `bytes`, and returns its error code.
fn write_bytes(bytes: &[u8]) -> u64 {
    // SAFETY: By `Syscall::Write`'s docs, write reads the bytes.len()
    // bytes at bytes.as_ptr(), which bytes holds, and writes no user
    // memory.
    let (error, _) = unsafe {
        arch::syscall_reads(
            Syscall::Write.number(),
            [address(bytes.as_ptr()), length(bytes.len()), 0, 0, 0, 0],
        )
    };
    error
}

/// A process, as [`Handle<Process>`] names it.
#[derive(Debug)]
pub enum Process {}

/// A memory object, as [`Handle<Memory>`] names it.
#[derive(Debug)]
pub enum Memory {}

/// A channel endpoint, as [`Handle<Channel>`] names it.
#[derive(Debug)]
pub enum Channel {}

/// An object of a type the program hasn't claimed, as a message's handles
/// arrive; [`Handle::cast`] claims one.
#[derive(Debug)]
pub enum Unknown {}

/// A handle to a kernel object of type `T`, which closes when it drops.
/// The type is the program's claim: the kernel checks the object's type on
/// each syscall, and fails with [`Error::WrongType`] if the claim is
/// wrong.
#[derive(Debug)]
pub struct Handle<T> {
    /// 0, which is never a handle, once the value has moved elsewhere and
    /// the handle no longer closes it.
    value: u32,
    kind: PhantomData<T>,
}

impl<T> Handle<T> {
    fn new(value: u32) -> Self {
        Handle {
            value,
            kind: PhantomData,
        }
    }

    /// The handle with `value`, which the kernel returned.
    fn from_kernel(value: u64) -> Self {
        let Ok(value) = u32::try_from(value) else {
            panic!("the kernel returned {value:#x}, which isn't a handle");
        };
        Handle::new(value)
    }

    /// The handle's value in the process's table.
    pub fn value(&self) -> u32 {
        self.value
    }

    /// The value, which this no longer closes.
    fn into_value(mut self) -> u32 {
        mem::take(&mut self.value)
    }

    /// Makes another handle to the same object with `rights`, which must
    /// be ones this handle has. Needs [`Rights::DUPLICATE`].
    pub fn duplicate(&self, rights: Rights) -> Result<Handle<T>, Error> {
        let value = syscall(
            Syscall::Duplicate,
            [u64::from(self.value), u64::from(rights.bits()), 0, 0, 0],
        )?;
        Ok(Handle::from_kernel(value))
    }

    /// Leaves this handle with only `rights`, which must be ones it has.
    pub fn restrict(&mut self, rights: Rights) -> Result<(), Error> {
        syscall(
            Syscall::Restrict,
            [u64::from(self.value), u64::from(rights.bits()), 0, 0, 0],
        )
        .map(|_| ())
    }

    /// The same handle, with its type forgotten, as a message carries it.
    pub fn into_unknown(self) -> Handle<Unknown> {
        Handle::new(self.into_value())
    }
}

impl Handle<Channel> {
    /// The handle the program's creator started it with, in `arg`. Only
    /// [`entry!`] calls this.
    #[doc(hidden)]
    pub fn __from_start(arg: u64) -> Handle<Channel> {
        let Ok(value) = u32::try_from(arg) else {
            panic!("the program started with {arg:#x}, which isn't a handle");
        };
        Handle::new(value)
    }
}

impl Handle<Process> {
    /// Makes a process with an empty address space and no thread, and
    /// returns a handle to it with [`Rights::READ`], [`Rights::WRITE`],
    /// [`Rights::DUPLICATE`] and [`Rights::TRANSFER`].
    pub fn create() -> Result<Handle<Process>, Error> {
        let value = syscall(Syscall::ProcessCreate, [])?;
        Ok(Handle::from_kernel(value))
    }

    /// Maps `len` bytes of `memory` from `offset` at `addr` in the
    /// process, with `rights`: [`Rights::READ`], with [`Rights::WRITE`] or
    /// [`Rights::EXECUTE`] or neither. Works only before [`start`], and
    /// needs [`Rights::WRITE`] here.
    ///
    /// [`start`]: Handle::start
    pub fn map_into(
        &self,
        memory: &Handle<Memory>,
        offset: usize,
        len: usize,
        addr: u64,
        rights: Rights,
    ) -> Result<(), Error> {
        let (Ok(offset), Ok(len)) = (u64::try_from(offset), u64::try_from(len)) else {
            unreachable!("ThisOS's addresses are 64 bits");
        };
        syscall(
            Syscall::MapInto,
            [
                u64::from(self.value),
                u64::from(memory.value),
                offset,
                len,
                addr,
                u64::from(rights.bits()),
            ],
        )
        .map(|_| ())
    }

    /// Starts the process with one thread at `ip`, with stack pointer `sp`,
    /// and moves `handle`, which needs [`Rights::TRANSFER`], into it, where
    /// the thread gets it as its argument. Needs [`Rights::WRITE`] here. On
    /// failure nothing starts, and the handle comes back.
    pub fn start<T>(&self, ip: u64, sp: u64, handle: Handle<T>) -> Result<(), (Error, Handle<T>)> {
        match syscall(
            Syscall::ProcessStart,
            [u64::from(self.value), ip, sp, u64::from(handle.value)],
        ) {
            Ok(_) => {
                // It moved to the process, so it no longer closes here.
                handle.into_value();
                Ok(())
            }
            Err(e) => Err((e, handle)),
        }
    }

    /// The process's exit status, once it has [`Signals::ENDED`]. Needs
    /// [`Rights::READ`]. Fails with [`Error::ShouldWait`] before that, and
    /// with [`Error::Killed`] if a trap ended it.
    pub fn exit_status(&self) -> Result<u64, Error> {
        syscall(Syscall::ProcessEnd, [u64::from(self.value)])
    }
}

impl Handle<Unknown> {
    /// The same handle, claimed to name an object of type `T`.
    pub fn cast<T>(self) -> Handle<T> {
        Handle::new(self.into_value())
    }
}

impl Handle<Memory> {
    /// Makes a memory object of `size` zeroed bytes, a multiple of 4096,
    /// with every right.
    pub fn create(size: usize) -> Result<Handle<Memory>, Error> {
        let Ok(size) = u64::try_from(size) else {
            unreachable!("ThisOS's addresses are 64 bits");
        };
        let value = syscall(Syscall::MemoryCreate, [size])?;
        Ok(Handle::from_kernel(value))
    }

    /// Copies the object's bytes from `offset` into `buf`. Needs
    /// [`Rights::READ`]. Fails with [`Error::OutOfRange`] if they run past
    /// the object's end.
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> Result<(), Error> {
        let Ok(offset) = u64::try_from(offset) else {
            unreachable!("ThisOS's addresses are 64 bits");
        };
        // SAFETY: By `Syscall::MemoryRead`'s docs, memory-read writes at
        // most buf.len() bytes at buf.as_mut_ptr(), which buf holds, and
        // reads no user memory.
        let (code, _) = unsafe {
            arch::syscall_writes(
                Syscall::MemoryRead.number(),
                [
                    u64::from(self.value),
                    offset,
                    address(buf.as_mut_ptr()),
                    length(buf.len()),
                    0,
                    0,
                ],
            )
        };
        result(code, 0).map(|_| ())
    }

    /// Copies `bytes` into the object from `offset`. Needs
    /// [`Rights::WRITE`]. Fails with [`Error::OutOfRange`] if they run past
    /// the object's end, and with [`Error::Conflict`] while the object is
    /// mapped executable anywhere.
    pub fn write(&self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        let Ok(offset) = u64::try_from(offset) else {
            unreachable!("ThisOS's addresses are 64 bits");
        };
        // SAFETY: By `Syscall::MemoryWrite`'s docs, memory-write reads the
        // bytes.len() bytes at bytes.as_ptr(), which bytes holds, and
        // writes no user memory.
        let (code, _) = unsafe {
            arch::syscall_reads(
                Syscall::MemoryWrite.number(),
                [
                    u64::from(self.value),
                    offset,
                    address(bytes.as_ptr()),
                    length(bytes.len()),
                    0,
                    0,
                ],
            )
        };
        result(code, 0).map(|_| ())
    }
}

impl Handle<Channel> {
    /// Makes a channel, and returns its two endpoints.
    pub fn create() -> Result<(Handle<Channel>, Handle<Channel>), Error> {
        let value = syscall(Syscall::ChannelCreate, [])?;
        Ok((
            Handle::from_kernel(value & 0xffff_ffff),
            Handle::from_kernel(value >> 32),
        ))
    }

    /// Sends `bytes` and `handles` to the other endpoint. Needs
    /// [`Rights::WRITE`] here and [`Rights::TRANSFER`] on each handle. On
    /// failure nothing is sent, and the handles come back.
    pub fn send(
        &self,
        bytes: &[u8],
        handles: Vec<Handle<Unknown>>,
    ) -> Result<(), (Error, Vec<Handle<Unknown>>)> {
        if handles.len() > MAX_MESSAGE_HANDLES {
            return Err((Error::TooLong, handles));
        }
        let mut values = [0; MAX_MESSAGE_HANDLES];
        for (value, handle) in values.iter_mut().zip(&handles) {
            *value = handle.value;
        }
        // SAFETY: By `Syscall::Send`'s docs, send reads the bytes.len()
        // bytes at bytes.as_ptr() and the handles.len() u32s at values,
        // which those hold, and writes no user memory.
        let (code, _) = unsafe {
            arch::syscall_reads(
                Syscall::Send.number(),
                [
                    u64::from(self.value),
                    address(bytes.as_ptr()),
                    length(bytes.len()),
                    address(values.as_ptr()),
                    length(handles.len()),
                    0,
                ],
            )
        };
        match result(code, 0) {
            Ok(_) => {
                // They moved to the receiver, so they no longer close here.
                for handle in handles {
                    handle.into_value();
                }
                Ok(())
            }
            Err(e) => Err((e, handles)),
        }
    }

    /// Receives the oldest message waiting here: its bytes replace what
    /// `bytes` held, and its handles are returned. Needs [`Rights::READ`].
    /// `bytes` grows to [`MAX_MESSAGE_BYTES`] for the call, zeroing what
    /// grows, and is left empty on failure; a loop that receives many
    /// messages calls [`receive_into`](Self::receive_into) instead.
    pub fn receive(&self, bytes: &mut Vec<u8>) -> Result<Vec<Handle<Unknown>>, Error> {
        bytes.resize(MAX_MESSAGE_BYTES, 0);
        match self.receive_into(bytes) {
            Ok((len, handles)) => {
                bytes.truncate(len);
                Ok(handles)
            }
            Err(e) => {
                bytes.clear();
                Err(e)
            }
        }
    }

    /// Receives the oldest message waiting here into the start of `bytes`,
    /// and returns its length and its handles. Needs [`Rights::READ`].
    /// Fails with [`Error::BufferTooSmall`], leaving the message queued,
    /// when `bytes` is shorter than the message.
    pub fn receive_into(&self, bytes: &mut [u8]) -> Result<(usize, Vec<Handle<Unknown>>), Error> {
        let mut values = [0u32; MAX_MESSAGE_HANDLES];
        // SAFETY: By `Syscall::Receive`'s docs, receive writes at most
        // bytes.len() bytes at bytes.as_mut_ptr() and at most
        // MAX_MESSAGE_HANDLES u32s at values, which those hold, and reads
        // no user memory.
        let (code, value) = unsafe {
            arch::syscall_writes(
                Syscall::Receive.number(),
                [
                    u64::from(self.value),
                    address(bytes.as_mut_ptr()),
                    length(bytes.len()),
                    address(values.as_mut_ptr()),
                    length(MAX_MESSAGE_HANDLES),
                    0,
                ],
            )
        };
        let value = result(code, value)?;
        let (Ok(len), Ok(count)) = (
            usize::try_from(value & 0xffff_ffff),
            usize::try_from(value >> 32),
        ) else {
            unreachable!("ThisOS's addresses are 64 bits");
        };
        let Some(values) = values.get(..count) else {
            panic!("the kernel received {count} handles, more than a message holds");
        };
        Ok((
            len,
            values.iter().map(|&value| Handle::new(value)).collect(),
        ))
    }
}

impl<T> Handle<T> {
    /// Waits until this handle's object has one of `signals`, for at most
    /// `timeout`, or for good if it is `None`, and returns its signals;
    /// see [`wait`].
    pub fn wait(&self, signals: Signals, timeout: Option<Duration>) -> Result<Signals, Error> {
        let mut items = [WaitItem::new(self, signals)];
        crate::wait(&mut items, timeout)?;
        Ok(items[0].observed())
    }
}

impl<T> Drop for Handle<T> {
    fn drop(&mut self) {
        if self.value == 0 {
            return;
        }
        if let Err(e) = syscall(Syscall::Close, [u64::from(self.value)]) {
            panic!("closing handle {:#x} failed: {e:?}", self.value);
        }
    }
}

/// One object [`wait`] waits on, by a handle it borrows, and the signals
/// it waits for.
#[derive(Debug)]
pub struct WaitItem<'a> {
    handle: u32,
    wanted: Signals,
    observed: Signals,
    borrowed: PhantomData<&'a ()>,
}

impl<'a> WaitItem<'a> {
    /// An item that waits for any of `wanted` on `handle`'s object.
    pub fn new<T>(handle: &'a Handle<T>, wanted: Signals) -> Self {
        WaitItem {
            handle: handle.value,
            wanted,
            observed: Signals::NONE,
            borrowed: PhantomData,
        }
    }

    /// The object's signals when the last [`wait`] on this item returned
    /// successfully, or none before that.
    pub fn observed(&self) -> Signals {
        self.observed
    }
}

/// Waits until one of `items`' objects has one of the item's wanted
/// signals, for at most `timeout`, rounded up to the scheduler's ticks, or
/// for good if it is `None`. Records each object's signals in its item,
/// and returns how many items have a wanted one. A zero timeout only
/// checks, and no items waits out the timeout. Fails with
/// [`Error::TimedOut`] when the timeout passes, and with
/// [`Error::TooLong`] for more than [`MAX_WAIT_ITEMS`] items. Each handle
/// needs [`Rights::READ`]; channel endpoints and processes have signals,
/// and each item wants at least one, all of them its object's:
/// [`Signals::ENDPOINT`] or [`Signals::PROCESS`], or the wait fails with
/// [`Error::BadSignals`].
pub fn wait(items: &mut [WaitItem<'_>], timeout: Option<Duration>) -> Result<usize, Error> {
    if items.len() > MAX_WAIT_ITEMS {
        return Err(Error::TooLong);
    }
    // Three u32s an item, as the kernel reads them.
    let mut raw = [[0u32; 3]; MAX_WAIT_ITEMS];
    for (raw, item) in raw.iter_mut().zip(items.iter()) {
        *raw = [item.handle, item.wanted.bits(), 0];
    }
    // u64::MAX is for good, which a timeout of 584 years is as well.
    let timeout = timeout.map_or(u64::MAX, |timeout| {
        u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX)
    });
    // SAFETY: By `Syscall::Wait`'s docs, wait reads and writes at most
    // items.len() items of three u32s at raw, which raw holds.
    let (code, value) = unsafe {
        arch::syscall_writes(
            Syscall::Wait.number(),
            [
                address(raw.as_mut_ptr()),
                length(items.len()),
                timeout,
                0,
                0,
                0,
            ],
        )
    };
    let ready = result(code, value)?;
    for (item, raw) in items.iter_mut().zip(&raw) {
        let Some(observed) = Signals::from_bits(raw[2]) else {
            panic!(
                "the kernel observed signals {:#x}, which no signal has",
                raw[2]
            );
        };
        item.observed = observed;
    }
    let Ok(ready) = usize::try_from(ready) else {
        unreachable!("ThisOS's addresses are 64 bits");
    };
    Ok(ready)
}

/// Makes `number`, a syscall that reads, writes, maps and unmaps no memory
/// of this process, with `args` and zeros for the rest of its six
/// arguments, and returns its value.
fn syscall<const N: usize>(number: Syscall, args: [u64; N]) -> Result<u64, Error> {
    const { assert!(N <= 6, "a syscall takes at most six arguments") };
    let mut all = [0; 6];
    all[..N].copy_from_slice(&args);
    // SAFETY: The caller passes a syscall that touches no user memory.
    let (code, value) = unsafe { arch::syscall_nomem(number.number(), all) };
    result(code, value)
}

/// Makes the map syscall with `args`, and returns its value. Unlike
/// [`syscall`], the compiler may not move memory accesses across it, since
/// it makes memory appear.
#[cfg_attr(test, expect(dead_code, reason = "only the heap maps memory"))]
fn map_syscall(args: [u64; 5]) -> Result<u64, Error> {
    let mut all = [0; 6];
    all[..5].copy_from_slice(&args);
    // SAFETY: Map only adds pages where none were mapped, so no memory
    // Rust reaches changes.
    let (code, value) = unsafe { arch::syscall_writes(Syscall::Map.number(), all) };
    result(code, value)
}

/// `ptr`'s address, as a syscall argument.
fn address<T>(ptr: *const T) -> u64 {
    length(ptr.addr())
}

/// `len`, as a syscall argument.
fn length(len: usize) -> u64 {
    let Ok(len) = u64::try_from(len) else {
        unreachable!("ThisOS's addresses are 64 bits");
    };
    len
}

fn result(code: u64, value: u64) -> Result<u64, Error> {
    match code {
        0 => Ok(value),
        code => Err(error(code)),
    }
}

/// The error with `code`, which the kernel returned.
fn error(code: u64) -> Error {
    let Some(error) = Error::from_code(code) else {
        panic!("the kernel returned unknown error code {code}");
    };
    error
}

/// Prints to the kernel's log, as `format!` formats: one record per line,
/// without its newline, and one for what is left after the last newline.
/// A line longer than [`MAX_WRITE`] bytes goes out as more records, split
/// between characters.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::print_fmt(format_args!($($arg)*))
    };
}

/// Prints as [`print!`] does, with a newline added.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print_fmt(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::print_fmt(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// What [`print!`] calls. Panics if a write fails.
#[doc(hidden)]
pub fn print_fmt(args: fmt::Arguments) {
    print_to(args, |record| {
        let code = write_bytes(record);
        assert!(code == 0, "write failed: {:?}", error(code));
    });
}

/// Formats `args` and hands each record [`print!`] makes to `write`.
fn print_to(args: fmt::Arguments, write: impl FnMut(&[u8])) {
    let mut print = Print {
        buffer: Buffer::new(),
        write,
    };
    // Print's write_str never fails, so only a Display impl can.
    assert!(print.write_fmt(args).is_ok(), "formatting failed");
    if print.buffer.len != 0 {
        print.flush();
    }
}

/// Up to [`MAX_WRITE`] bytes of text, cut only between characters.
struct Buffer {
    bytes: [u8; MAX_WRITE],
    len: usize,
}

impl Buffer {
    fn new() -> Self {
        Buffer {
            bytes: [0; MAX_WRITE],
            len: 0,
        }
    }

    /// Appends as many whole characters of `text` as fit, and returns the
    /// rest.
    fn push<'a>(&mut self, text: &'a str) -> &'a str {
        let mut n = (MAX_WRITE - self.len).min(text.len());
        while !text.is_char_boundary(n) {
            n -= 1;
        }
        let (head, rest) = text.split_at(n);
        self.bytes[self.len..self.len + n].copy_from_slice(head.as_bytes());
        self.len += n;
        rest
    }
}

/// Hands its buffer to `write` as a record at each newline, without the
/// newline, and when it is full and more of the line arrives. A full
/// buffer waits for that, so that a line of exactly [`MAX_WRITE`] bytes
/// and its newline make one record, not that line and an empty one.
struct Print<W> {
    buffer: Buffer,
    write: W,
}

impl<W: FnMut(&[u8])> Print<W> {
    fn flush(&mut self) {
        (self.write)(&self.buffer.bytes[..self.buffer.len]);
        self.buffer.len = 0;
    }
}

impl<W: FnMut(&[u8])> fmt::Write for Print<W> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                self.flush();
            }
            let mut rest = self.buffer.push(line);
            while !rest.is_empty() {
                self.flush();
                rest = self.buffer.push(rest);
            }
        }
        Ok(())
    }
}

/// Keeps as much as fits in its buffer, and drops the rest.
struct Truncate(Buffer);

impl fmt::Write for Truncate {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.push(text);
        Ok(())
    }
}

/// Writes the panic's location and message, cut to [`MAX_WRITE`] bytes,
/// then ends the thread with status 101, Rust's own exit code for a panic.
/// It neither uses [`print!`] nor checks the write, so that a panic there
/// doesn't come back here. A panic while formatting the message would only
/// panic again, so it exits at once.
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    static PANICKING: AtomicBool = AtomicBool::new(false);
    if !PANICKING.swap(true, Ordering::Relaxed) {
        let mut message = Truncate(Buffer::new());
        let _ = write!(message, "{info}");
        write_bytes(&message.0.bytes[..message.0.len]);
    }
    exit(101)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::format;
    use std::string::String;
    use std::vec::Vec;

    use super::*;

    /// The records [`print!`] makes of `args`.
    fn records(args: fmt::Arguments) -> Vec<String> {
        let mut records = Vec::new();
        print_to(args, |record| {
            records.push(String::from_utf8(record.to_vec()).unwrap());
        });
        records
    }

    #[test]
    fn a_full_line_and_its_newline_make_one_record() {
        let line = "a".repeat(MAX_WRITE);
        // The newline comes in a write_str of its own, after the line has
        // filled the buffer.
        assert_eq!(records(format_args!("{line}\n")), [line]);
    }

    #[test]
    fn each_line_is_a_record() {
        assert_eq!(records(format_args!("\n")), [""]);
        assert_eq!(records(format_args!("")), Vec::<String>::new());
        assert_eq!(records(format_args!("one\n\ntwo")), ["one", "", "two"]);
        assert_eq!(records(format_args!("{}{}\n", "par", "ts")), ["parts"]);
    }

    #[test]
    fn a_long_line_splits_between_characters() {
        // Two-byte characters, starting one byte in, so that the buffer
        // fills one byte short of a character.
        let line = format!("a{}", "é".repeat(MAX_WRITE));
        let records = records(format_args!("{line}\n"));
        assert_eq!(records.concat(), line);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].len(), MAX_WRITE - 1);
    }

    #[test]
    fn a_truncated_message_ends_on_a_character() {
        let mut message = Truncate(Buffer::new());
        let text = format!("a{}", "é".repeat(MAX_WRITE));
        write!(message, "{text}").unwrap();
        assert_eq!(message.0.len, MAX_WRITE - 1);
        assert_eq!(
            &message.0.bytes[..message.0.len],
            &text.as_bytes()[..MAX_WRITE - 1]
        );
    }
}
