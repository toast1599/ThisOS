//! Sends every message it receives back, bytes and handles, until the
//! other end closes, then exits with status 0.

#![no_std]
#![no_main]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec;

use thisos_rt::{Channel, Error, Handle, MAX_MESSAGE_BYTES, Signals};

thisos_rt::entry!(main);

#[expect(clippy::needless_pass_by_value, reason = "entry! fixes the signature")]
fn main(ping: Handle<Channel>) -> u64 {
    let mut bytes = vec![0; MAX_MESSAGE_BYTES];
    loop {
        if let Err(e) = ping.wait(Signals::READABLE | Signals::PEER_CLOSED, None) {
            panic!("waiting for a message failed: {e:?}");
        }
        // A closed peer's queued messages come first, so none is lost.
        let (len, handles) = match ping.receive_into(&mut bytes) {
            Ok(message) => message,
            Err(Error::PeerClosed) => return 0,
            Err(e) => panic!("receiving a message failed: {e:?}"),
        };
        if let Err((e, _)) = ping.send(&bytes[..len], handles) {
            panic!("sending a message back failed: {e:?}");
        }
    }
}
