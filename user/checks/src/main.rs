//! Checks the process-building and memory-copy syscalls from Rust: what
//! they do, and how they fail. A kernel test runs it as the kernel runs
//! init, and a check that fails panics, which exits with status 101 after
//! logging where. A normal boot never runs it.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use thisos_rt::{Channel, Error, Handle, Memory, Process, Rights, Signals, USER_SPACE_END};

thisos_rt::entry!(main);

/// Where the children's code goes, lld's default base, as for programs.
const CODE: u64 = 0x20_0000;

/// Code that exits with its argument, the handle it started with, through
/// syscall 0: `xor eax, eax; syscall` on x86_64.
#[cfg(target_arch = "x86_64")]
const EXIT_WITH_ARG: &[u8] = &[0x31, 0xc0, 0x0f, 0x05];
/// `mov x8, #0; svc #0` on aarch64.
#[cfg(target_arch = "aarch64")]
const EXIT_WITH_ARG: &[u8] = &[0x08, 0x00, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4];

/// Code that raises an undefined-instruction exception: `ud2` on x86_64.
#[cfg(target_arch = "x86_64")]
const INVALID: &[u8] = &[0x0f, 0x0b];
/// `udf #0` on aarch64.
#[cfg(target_arch = "aarch64")]
const INVALID: &[u8] = &[0x00, 0x00, 0x00, 0x00];

/// The handle a new process's one handle gets: the first slot of an empty
/// table, at generation 1.
const FIRST_HANDLE: u64 = 1 << 16;

fn main(kernel: Handle<Channel>) -> u64 {
    let mut bytes = Vec::new();
    let handles = ok(kernel.receive(&mut bytes));
    drop(kernel);
    let Ok([_process, ramdisk]) = <[_; 2]>::try_from(handles) else {
        panic!("the kernel sent a message without two handles");
    };
    memory_copies();
    the_ramdisk_is_read_only(&ramdisk.cast());
    building_a_process();
    starting_a_process();
    process_ends();
    0
}

fn ok<T>(result: Result<T, Error>) -> T {
    match result {
        Ok(value) => value,
        Err(e) => panic!("{e:?}"),
    }
}

fn memory(size: usize) -> Handle<Memory> {
    ok(Handle::<Memory>::create(size))
}

fn channel() -> (Handle<Channel>, Handle<Channel>) {
    ok(Handle::<Channel>::create())
}

/// A process with `code` mapped at [`CODE`], not started.
fn process_with(code: &[u8]) -> (Handle<Process>, Handle<Memory>) {
    let process = ok(Handle::<Process>::create());
    let memory = memory(4096);
    ok(memory.write(0, code));
    ok(process.map_into(&memory, 0, 4096, CODE, Rights::READ | Rights::EXECUTE));
    (process, memory)
}

fn memory_copies() {
    let object = memory(8192);
    ok(object.write(4094, b"abcd"));
    let mut buf = [0xff; 6];
    ok(object.read(4093, &mut buf));
    assert_eq!(&buf, b"\0abcd\0", "a copy across frames");
    let mut stack = [0; 8192];
    let start = stack.as_ptr().addr();
    let edge = (start + 3).next_multiple_of(4096) - start;
    let across = &mut stack[edge - 3..edge + 3];
    across.copy_from_slice(b"ghijkl");
    ok(object.write(100, across));
    across.fill(0);
    ok(object.read(100, across));
    assert_eq!(&*across, b"ghijkl", "copies across stack pages");
    ok(object.read(8192, &mut []));
    assert_eq!(object.write(8190, b"abc"), Err(Error::OutOfRange));
    assert_eq!(object.read(8193, &mut []), Err(Error::OutOfRange));
    assert_eq!(object.read(usize::MAX, &mut [0]), Err(Error::OutOfRange));

    let read_only = ok(object.duplicate(Rights::READ));
    assert_eq!(read_only.write(0, b"a"), Err(Error::AccessDenied));
    let write_only = ok(object.duplicate(Rights::WRITE));
    assert_eq!(write_only.read(0, &mut [0]), Err(Error::AccessDenied));
    let (endpoint, _) = channel();
    let endpoint: Handle<Memory> = endpoint.into_unknown().cast();
    assert_eq!(endpoint.read(0, &mut [0]), Err(Error::WrongType));
}

fn the_ramdisk_is_read_only(ramdisk: &Handle<Memory>) {
    let mut magic = [0; 6];
    ok(ramdisk.read(257, &mut magic));
    assert_eq!(&magic, b"ustar\0", "the ramdisk's first header");
    assert_eq!(ramdisk.write(0, b"x"), Err(Error::AccessDenied));
}

fn building_a_process() {
    let (process, code) = process_with(EXIT_WITH_ARG);
    let map = |memory: &Handle<Memory>, offset, len, addr, rights| {
        process.map_into(memory, offset, len, addr, rights)
    };
    assert_eq!(
        map(&code, 0, 4096, CODE, Rights::READ),
        Err(Error::AddressInUse)
    );
    assert_eq!(
        map(&code, 4096, 4096, 0x30_0000, Rights::READ),
        Err(Error::OutOfRange)
    );
    assert_eq!(
        map(&code, 0, 4096, CODE + 1, Rights::READ),
        Err(Error::BadAddress)
    );
    assert_eq!(
        map(&code, 0, 4096, 0x30_0000, Rights::READ | Rights::WRITE),
        Err(Error::Conflict),
        "W^X across processes"
    );
    // Writing code that runs would get around W^X as a writable mapping
    // would.
    assert_eq!(code.write(0, INVALID), Err(Error::Conflict));
    let no_execute = ok(code.duplicate(Rights::READ | Rights::DUPLICATE));
    assert_eq!(
        map(
            &no_execute,
            0,
            4096,
            0x30_0000,
            Rights::READ | Rights::EXECUTE
        ),
        Err(Error::AccessDenied)
    );

    let reader = ok(process.duplicate(Rights::READ));
    assert_eq!(
        reader.map_into(&code, 0, 4096, 0x30_0000, Rights::READ),
        Err(Error::AccessDenied)
    );
    let (endpoint, _) = channel();
    let endpoint: Handle<Process> = endpoint.into_unknown().cast();
    assert_eq!(
        endpoint.map_into(&code, 0, 4096, 0x30_0000, Rights::READ),
        Err(Error::WrongType)
    );
    assert_eq!(process.exit_status(), Err(Error::ShouldWait));

    // A process that never starts frees its space with its last handle,
    // and its mappings with it, so the code may be written again.
    drop((process, reader));
    ok(code.write(0, INVALID));
}

fn starting_a_process() {
    let (process, _code) = process_with(EXIT_WITH_ARG);
    let (end, _) = channel();
    let (e, end) = process.start(0, USER_SPACE_END, end).unwrap_err();
    assert_eq!(e, Error::BadAddress, "an entry outside user space");
    let (e, end) = process.start(CODE, USER_SPACE_END + 1, end).unwrap_err();
    assert_eq!(e, Error::BadAddress, "a stack outside user space");
    let mut end = end;
    ok(end.restrict(Rights::READ | Rights::WRITE));
    let (e, end) = process.start(CODE, USER_SPACE_END, end).unwrap_err();
    assert_eq!(e, Error::AccessDenied, "a start handle without transfer");
    // It came back, and is still this process's.
    drop(end);
    let reader = ok(process.duplicate(Rights::READ));
    let (end, _) = channel();
    let (e, end) = reader.start(CODE, USER_SPACE_END, end).unwrap_err();
    assert_eq!(e, Error::AccessDenied, "a process handle without write");

    ok(process.start(CODE, USER_SPACE_END, end).map_err(|(e, _)| e));
    let (another, _) = channel();
    let (e, _) = process.start(CODE, USER_SPACE_END, another).unwrap_err();
    assert_eq!(e, Error::Started);
    let other = memory(4096);
    assert_eq!(
        process.map_into(&other, 0, 4096, 0x30_0000, Rights::READ),
        Err(Error::Started)
    );
    assert_eq!(ok(process.wait(Signals::ENDED, None)), Signals::ENDED);
    assert_eq!(
        process.exit_status(),
        Ok(FIRST_HANDLE),
        "it exits with its handle"
    );
}

fn process_ends() {
    let (process, _code) = process_with(INVALID);
    let (end, _) = channel();
    ok(process.start(CODE, USER_SPACE_END, end).map_err(|(e, _)| e));
    let writer = ok(process.duplicate(Rights::WRITE));
    assert_eq!(writer.exit_status(), Err(Error::AccessDenied));
    assert_eq!(writer.wait(Signals::ENDED, None), Err(Error::AccessDenied));
    assert_eq!(ok(process.wait(Signals::ENDED, None)), Signals::ENDED);
    assert_eq!(process.exit_status(), Err(Error::Killed));
    // ENDED stays.
    assert_eq!(
        ok(process.wait(Signals::ENDED, Some(core::time::Duration::ZERO))),
        Signals::ENDED
    );
}
