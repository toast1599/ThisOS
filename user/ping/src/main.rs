//! Times round trips to `pong` and sends the results to its creator.
//!
//! Its creator sends it one message carrying its endpoint of a channel to
//! `pong`. It times a near-empty syscall, then round trips of several
//! sizes, the last carrying a handle, and closes the channel, which ends
//! `pong`. The results go to the creator as little-endian `u64`s: the
//! cycle counter's frequency in Hz, or 0 where it isn't known; the syscall's min,
//! median and max in cycles; and for each round trip its bytes, its
//! handles, and its min, median and max.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::mem;

use thisos_rt::{Channel, Handle, MAX_MESSAGE_BYTES, Memory, Signals, Unknown, cycles};

thisos_rt::entry!(main);

/// Runs before the timed ones, uncounted, so caches and TLBs are warm.
const WARM_UP: usize = 100;

/// Counted runs of each case.
const RUNS: usize = 1000;

/// The round trips timed, as bytes and handles a message carries.
const TRIPS: [(usize, usize); 5] = [(16, 0), (1 << 10, 0), (4 << 10, 0), (64 << 10, 0), (16, 1)];

#[expect(clippy::needless_pass_by_value, reason = "entry! fixes the signature")]
fn main(creator: Handle<Channel>) -> u64 {
    let pong = receive_pong(&creator);

    let mut results = Vec::new();
    results.push(thisos_rt::cycles_hz().unwrap_or(0));
    results.extend(time(|| {
        let started = cycles();
        thisos_rt::null_syscall();
        cycles().wrapping_sub(started)
    }));

    let sent = vec![0x5a; MAX_MESSAGE_BYTES];
    let mut received = vec![0; MAX_MESSAGE_BYTES];
    let mut carried = Vec::new();
    for (len, count) in TRIPS {
        // A memory object, the cheapest object to make, stands for any
        // handle a message carries.
        while carried.len() < count {
            match Handle::<Memory>::create(4096) {
                Ok(memory) => carried.push(memory.into_unknown()),
                Err(e) => panic!("creating a memory object to send failed: {e:?}"),
            }
        }
        results.extend([len, count].map(|n| u64::try_from(n).unwrap_or(u64::MAX)));
        results.extend(time(|| {
            let started = cycles();
            carried = round_trip(&pong, &sent[..len], mem::take(&mut carried), &mut received);
            cycles().wrapping_sub(started)
        }));
    }
    drop(pong);

    let bytes: Vec<u8> = results.iter().flat_map(|n| n.to_le_bytes()).collect();
    if let Err((e, _)) = creator.send(&bytes, Vec::new()) {
        panic!("sending the results failed: {e:?}");
    }
    0
}

/// Receives the creator's one message, and returns the endpoint it
/// carries.
fn receive_pong(creator: &Handle<Channel>) -> Handle<Channel> {
    if let Err(e) = creator.wait(Signals::READABLE | Signals::PEER_CLOSED, None) {
        panic!("waiting for the endpoint to pong failed: {e:?}");
    }
    let handles = match creator.receive(&mut Vec::new()) {
        Ok(handles) => handles,
        Err(e) => panic!("receiving the endpoint to pong failed: {e:?}"),
    };
    let Ok([pong]) = <[_; 1]>::try_from(handles) else {
        panic!("the creator's message didn't carry one handle");
    };
    pong.cast()
}

/// Sends `bytes` and `handles` to pong and receives them back into
/// `received`, and returns the handles.
fn round_trip(
    pong: &Handle<Channel>,
    bytes: &[u8],
    handles: Vec<Handle<Unknown>>,
    received: &mut [u8],
) -> Vec<Handle<Unknown>> {
    let count = handles.len();
    if let Err((e, _)) = pong.send(bytes, handles) {
        panic!("sending to pong failed: {e:?}");
    }
    if let Err(e) = pong.wait(Signals::READABLE | Signals::PEER_CLOSED, None) {
        panic!("waiting for pong's answer failed: {e:?}");
    }
    let (len, handles) = match pong.receive_into(received) {
        Ok(message) => message,
        Err(e) => panic!("receiving pong's answer failed: {e:?}"),
    };
    assert!(
        len == bytes.len() && handles.len() == count,
        "pong answered {len} bytes and {} handles to {} and {count}",
        handles.len(),
        bytes.len(),
    );
    handles
}

/// Runs `run`, which returns the cycles it took, [`WARM_UP`] times
/// uncounted and [`RUNS`] times counted, and returns the counted runs'
/// min, median and max.
fn time(mut run: impl FnMut() -> u64) -> [u64; 3] {
    for _ in 0..WARM_UP {
        run();
    }
    let mut samples: Vec<u64> = (0..RUNS).map(|_| run()).collect();
    samples.sort_unstable();
    [samples[0], samples[RUNS / 2], samples[RUNS - 1]]
}
