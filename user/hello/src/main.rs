//! Receives one line from its creator, prints it after a greeting, and
//! exits with its length.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

use thisos_rt::{Channel, Handle, Signals, println};

thisos_rt::entry!(main);

fn main(creator: Handle<Channel>) -> u64 {
    if let Err(e) = creator.wait(Signals::READABLE, None) {
        panic!("waiting for the line failed: {e:?}");
    }
    let mut line = Vec::new();
    if let Err(e) = creator.receive(&mut line) {
        panic!("receiving the line failed: {e:?}");
    }
    drop(creator);
    let text = core::str::from_utf8(&line).unwrap_or("bytes that aren't UTF-8");
    println!("hello, {text}");
    // At most MAX_MESSAGE_BYTES.
    u64::try_from(line.len()).unwrap_or(u64::MAX)
}
