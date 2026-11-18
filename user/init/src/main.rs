//! The first process. It starts `hello`, sends it a line and waits for it
//! to exit, and starts `fault` and waits for the kernel to kill it, then
//! reports both ends.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

mod loader;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use thisos_rt::{Channel, Error, Handle, Memory, Process, Signals, println};

thisos_rt::entry!(main);

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

    let hello = match end(&hello) {
        Ok(status) => format!("hello exited with status {status:#x}"),
        Err(e) => panic!("hello's status: {e:?}"),
    };
    let fault = match end(&fault) {
        Ok(status) => format!("fault exited with status {status:#x}"),
        Err(Error::Killed) => String::from("fault was killed"),
        Err(e) => panic!("fault's status: {e:?}"),
    };
    println!("init: {hello}, {fault}");
    0
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
