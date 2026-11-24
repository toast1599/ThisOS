//! Channels: two endpoints sharing one [`Channel`] under one lock, so a
//! send and its peer's close never race. An endpoint closes when the last
//! hold on it goes: its one handle, or the message carrying it. A change
//! that gives an endpoint a signal unparks the threads waiting on it under
//! the lock, so the waiter lists keep their room.

use alloc::sync::Arc;

use thisos_abi::{Error, Rights, Signals};
use thisos_channels::{Channel, Message, Side};
use thisos_locks::Rank;

use crate::Lock;
use crate::process::Object;
use crate::thread;

/// The rights a new endpoint's handle has.
pub const ENDPOINT_RIGHTS: Rights = Rights::READ.union(Rights::WRITE).union(Rights::TRANSFER);

/// What a message carries for each handle sent.
pub type Carried = (Object, Rights);

pub struct Endpoint {
    side: Side,
    /// Taken with interrupts on, and never by a trap handler: sending and
    /// receiving allocate, and take a process's handle-table lock inside
    /// it, and unparking takes the scheduler's.
    channel: Arc<Lock<Channel<Carried>>>,
    /// The next endpoint on the stack of closed ones a drop drains, while
    /// this one is on it.
    next: Option<Arc<Endpoint>>,
}

/// A new channel's two endpoints. Fails with [`Error::NoMemory`].
pub fn create() -> Result<(Endpoint, Endpoint), Error> {
    let channel =
        Arc::try_new(Lock::new(Rank::CHANNEL, Channel::new())).map_err(|_| Error::NoMemory)?;
    let first = Endpoint {
        side: Side::First,
        channel: Arc::clone(&channel),
        next: None,
    };
    let second = Endpoint {
        side: Side::Second,
        channel,
        next: None,
    };
    Ok((first, second))
}

/// A new channel's endpoints, for tests, which leave the heap room.
#[cfg(test)]
pub fn pair() -> (Endpoint, Endpoint) {
    let Ok(pair) = create() else {
        panic!("no memory for a channel");
    };
    pair
}

impl Endpoint {
    /// Whether `other` is this endpoint or its peer.
    pub fn same_channel(&self, other: &Endpoint) -> bool {
        Arc::ptr_eq(&self.channel, &other.channel)
    }

    /// Sends the message `make` returns to the peer; see [`Channel::send`].
    /// `make` runs under the channel's lock, so it must drop no object.
    pub fn send(
        &self,
        make: impl FnOnce() -> Result<Message<Carried>, Error>,
    ) -> Result<(), Error> {
        self.channel.lock().send(self.side, make, thread::unpark)
    }

    /// Hands the oldest message waiting here to `take`; see
    /// [`Channel::receive`]. `take` runs under the channel's lock, so it
    /// must drop no object.
    pub fn receive<R>(
        &self,
        take: impl FnOnce(Message<Carried>) -> Result<R, (Error, Message<Carried>)>,
    ) -> Result<R, Error> {
        self.channel.lock().receive(self.side, take, thread::unpark)
    }

    /// Adds the thread `id` to this endpoint's waiters, and returns its
    /// signals; see [`Channel::add_waiter`].
    pub fn add_waiter(&self, id: u64) -> Result<Signals, Error> {
        self.channel.lock().add_waiter(self.side, id)
    }

    /// Takes the thread `id` off this endpoint's waiters, and returns its
    /// signals; see [`Channel::remove_waiter`].
    pub fn remove_waiter(&self, id: u64) -> Signals {
        self.channel.lock().remove_waiter(self.side, id)
    }
}

impl Drop for Endpoint {
    /// Closes this side and drops what waited here. Endpoints in those
    /// messages that nothing else holds close too, and go on a stack
    /// linked through the Arcs that held them, whose messages drop in the
    /// same loop. So a chain of endpoints each queued at the last drops at
    /// a constant stack depth, and nothing allocates.
    fn drop(&mut self) {
        self.channel.lock().close(self.side, thread::unpark);
        let mut stack = drain(&self.channel, self.side, None);
        while let Some(top) = stack {
            let Some(mut endpoint) = Arc::into_inner(top) else {
                panic!("an endpoint on the drop stack has another holder");
            };
            stack = drain(&endpoint.channel, endpoint.side, endpoint.next.take());
            // Its own drop now finds nothing queued.
        }
    }
}

/// Drops the messages waiting at the closed `side` of `channel`, and
/// returns `stack` with the endpoints in them that nothing else held
/// pushed on, closed. Takes one message at a time, so that its objects
/// drop after the channel's lock.
fn drain(
    channel: &Lock<Channel<Carried>>,
    side: Side,
    mut stack: Option<Arc<Endpoint>>,
) -> Option<Arc<Endpoint>> {
    loop {
        let Some(message) = channel.lock().discard(side) else {
            return stack;
        };
        for (object, _) in message.handles {
            if let Object::Channel(mut held) = object
                && let Some(endpoint) = Arc::get_mut(&mut held)
            {
                endpoint.channel.lock().close(endpoint.side, thread::unpark);
                endpoint.next = stack;
                stack = Some(held);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use thisos_abi::MAX_QUEUED;
    use thisos_hal::Platform;

    use super::*;
    use crate::Arch;
    use crate::testing::fill_heap;

    fn carry(endpoint: Endpoint) -> Message<Carried> {
        Message {
            bytes: Vec::new(),
            handles: vec![(Object::Channel(Arc::new(endpoint)), ENDPOINT_RIGHTS)],
        }
    }

    #[test_case]
    fn closing_an_endpoint_drops_what_it_held() {
        let allocated = Arch::heap_stats().allocated;
        let (a, b) = pair();
        let (c, d) = pair();
        assert!(a.send(|| Ok(carry(c))).is_ok());
        drop(a);
        // b still receives what a sent.
        let Ok(message) = b.receive(Ok) else {
            panic!("b lost a's message");
        };
        let [(Object::Channel(c), _)] = &message.handles[..] else {
            panic!("the message lost its endpoint");
        };
        // Made and its peer dropped outside send, which holds the lock.
        let (carried, peer) = pair();
        drop(peer);
        assert!(c.send(|| Ok(carry(carried))).is_ok());
        drop(message);
        // d drains what c sent before it closed.
        assert!(d.receive(Ok).is_ok());
        assert!(matches!(d.receive(Ok), Err(Error::PeerClosed)));
        assert!(matches!(b.receive(Ok), Err(Error::PeerClosed)));
        drop((b, d));
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }

    #[test_case]
    fn a_full_heap_makes_no_channel() {
        let allocated = Arch::heap_stats().allocated;
        let filler = fill_heap();
        assert!(matches!(create(), Err(Error::NoMemory)));
        drop(filler);
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }

    #[test_case]
    fn a_full_heap_drops_queued_endpoints() {
        let allocated = Arch::heap_stats().allocated;
        let (a, b) = pair();
        let (c, d) = pair();
        for _ in 0..MAX_QUEUED {
            let empty = || {
                Ok(Message {
                    bytes: Vec::new(),
                    handles: Vec::new(),
                })
            };
            assert!(c.send(empty).is_ok());
        }
        assert!(a.send(|| Ok(carry(d))).is_ok());
        drop(a);
        let filler = fill_heap();
        // b's close reaches d, and d's close drops what c sent.
        drop(b);
        drop(filler);
        drop(c);
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }

    #[test_case]
    fn a_long_chain_of_queued_endpoints_drops_without_recursing() {
        let allocated = Arch::heap_stats().allocated;
        // Each new channel's first endpoint holds the chain so far at its
        // second, which is all that holds the second.
        let (mut head, tail) = pair();
        drop(tail);
        for _ in 0..2000 {
            let (first, second) = pair();
            assert!(first.send(|| Ok(carry(head))).is_ok());
            drop(first);
            head = second;
        }
        drop(head);
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }
}
