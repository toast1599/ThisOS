//! Overflows its stack into the unmapped page below it, so that the kernel
//! kills it, for its creator to see a process that a trap ended.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

use core::hint::black_box;

use thisos_rt::{Channel, Handle};

thisos_rt::entry!(main);

fn main(_creator: Handle<Channel>) -> u64 {
    deeper(0)
}

/// Takes 1 KiB of stack per call, and never stops calling itself.
#[expect(
    unconditional_recursion,
    reason = "it recurses until the stack runs out"
)]
fn deeper(depth: u64) -> u64 {
    let frame = black_box([depth; 128]);
    deeper(depth + 1) + frame[0]
}
