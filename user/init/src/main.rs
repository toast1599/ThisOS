//! The first process. It starts `hello`, sends it a line and waits for it
//! to exit, and starts `fault` and waits for the kernel to kill it. It
//! starts `ping` and `pong` with the ends of one channel, and prints the
//! round trips `ping` timed. Then it reports how each program ended.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod loader;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use thisos_rt::{Channel, Error, Handle, Memory, Process, Signals, println};

thisos_rt::entry!(main);

#[expect(clippy::similar_names, reason = "the programs are named ping and pong")]
fn main(kernel: Handle<Channel>) -> u64 {
    // The kernel sends one message, already queued: a handle to init's own
    // process, which it has no use for, and one to the ramdisk.
    let mut bytes = Vec::new();
    let handles = match kernel.receive(&mut bytes) {
        Ok(handles) => handles,
        Err(e) => panic!("receiving the kernel's handles failed: {e:?}"),
    };
    drop(kernel);
    let Ok([_process, ramdisk]) = <[_; 2]>::try_from(handles) else {
        panic!("the kernel sent init a message without two handles");
    };
    let ramdisk: Handle<Memory> = ramdisk.cast();

    let (to_hello, hello_end) = channel();
    let hello = start(&ramdisk, "hello", hello_end);
    if let Err((e, _)) = to_hello.send(b"world", Vec::new()) {
        panic!("sending hello its line failed: {e:?}");
    }
    let (_to_fault, fault_end) = channel();
    let fault = start(&ramdisk, "fault", fault_end);
    let (ping_end, pong_end) = channel();
    let (to_ping, init_end) = channel();
    let ping = start(&ramdisk, "ping", init_end);
    if let Err((e, _)) = to_ping.send(&[], vec![ping_end.into_unknown()]) {
        panic!("sending ping its endpoint failed: {e:?}");
    }
    let pong = start(&ramdisk, "pong", pong_end);

    let hello = match end(&hello) {
        Ok(status) => format!("hello exited with status {status:#x}"),
        Err(e) => panic!("hello's status: {e:?}"),
    };
    let fault = match end(&fault) {
        Ok(status) => format!("fault exited with status {status:#x}"),
        Err(Error::Killed) => String::from("fault was killed"),
        Err(e) => panic!("fault's status: {e:?}"),
    };
    print_round_trips(&to_ping);
    let talked = match (end(&ping), end(&pong)) {
        (Ok(ping), Ok(pong)) if ping == pong => {
            format!("ping and pong exited with status {ping:#x}")
        }
        (Ok(ping), Ok(pong)) => {
            format!("ping exited with status {ping:#x}, pong with status {pong:#x}")
        }
        (ping, pong) => panic!("ping's and pong's statuses: {ping:?}, {pong:?}"),
    };
    println!("init: {hello}, {fault}, {talked}");
    0
}

/// Receives ping's results, laid out as ping's documentation says, and
/// prints a line for each case.
fn print_round_trips(ping: &Handle<Channel>) {
    if let Err(e) = ping.wait(Signals::READABLE | Signals::PEER_CLOSED, None) {
        panic!("waiting for ping's results failed: {e:?}");
    }
    let mut bytes = Vec::new();
    if let Err(e) = ping.receive(&mut bytes) {
        panic!("receiving ping's results failed: {e:?}");
    }
    let (numbers, []) = bytes.as_chunks::<8>() else {
        panic!("ping's results aren't whole u64s");
    };
    let numbers: Vec<u64> = numbers.iter().map(|n| u64::from_le_bytes(*n)).collect();
    let Some((&hz, rest)) = numbers.split_first() else {
        panic!("ping's results are empty");
    };
    let Some((syscall, trips)) = rest.split_first_chunk::<3>() else {
        panic!("ping's results have no syscall timing");
    };
    let (trips, []) = trips.as_chunks::<5>() else {
        panic!("ping's results end in part of a round trip");
    };
    println!("init: null syscall: {}", spread(hz, *syscall));
    for &[len, count, min, median, max] in trips {
        let carrying = match count {
            0 => String::new(),
            1 => String::from(" and 1 handle"),
            n => format!(" and {n} handles"),
        };
        println!(
            "init: round trip of {len} bytes{carrying}: {}",
            spread(hz, [min, median, max])
        );
    }
}

/// Min, median and max in cycles, and in nanoseconds unless the TSC's
/// frequency `hz` is 0, which stands for unknown.
fn spread(hz: u64, [min, median, max]: [u64; 3]) -> String {
    let cycles = format!("min {min}, median {median}, max {max} cycles");
    if hz == 0 {
        return cycles;
    }
    let ns = |n: u64| u128::from(n) * 1_000_000_000 / u128::from(hz);
    format!("{cycles}; {}, {}, {} ns", ns(min), ns(median), ns(max))
}

fn channel() -> (Handle<Channel>, Handle<Channel>) {
    match Handle::<Channel>::create() {
        Ok(endpoints) => endpoints,
        Err(e) => panic!("creating a channel failed: {e:?}"),
    }
}

fn start(ramdisk: &Handle<Memory>, name: &str, handle: Handle<Channel>) -> Handle<Process> {
    match loader::start(ramdisk, name, handle) {
        Ok(process) => process,
        Err(e) => panic!("starting {name} failed: {e}"),
    }
}

/// Waits for `process` to end, and returns its exit status.
fn end(process: &Handle<Process>) -> Result<u64, Error> {
    if let Err(e) = process.wait(Signals::ENDED, None) {
        panic!("waiting for a process to end failed: {e:?}");
    }
    process.exit_status()
}
