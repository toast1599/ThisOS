//! A channel's state: two queues of messages, one toward each endpoint,
//! whether each endpoint is closed, and the ids of the threads waiting on
//! each. A message carries bytes and handles of type `T`. The kernel adds
//! the lock, the user copies, what a handle is, and the parking and waking
//! of the ids.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::mem;

use thisos_abi::{Error, MAX_QUEUED, Signals};

/// One of a channel's two endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The endpoint listed first when the channel was made.
    First,
    /// The other one.
    Second,
}

impl Side {
    /// The other endpoint.
    #[must_use]
    pub const fn peer(self) -> Side {
        match self {
            Side::First => Side::Second,
            Side::Second => Side::First,
        }
    }

    const fn index(self) -> usize {
        match self {
            Side::First => 0,
            Side::Second => 1,
        }
    }
}

/// What one send carries.
#[derive(Debug, PartialEq, Eq)]
pub struct Message<T> {
    /// What the sender wrote.
    pub bytes: Vec<u8>,
    /// What moved out of the sender's handle table.
    pub handles: Vec<T>,
}

/// The ids of the waiters a change to a channel wakes: every waiter on
/// each endpoint whose [`Signals`] gained one, which the change took off
/// that endpoint's list.
#[derive(Debug, Default, PartialEq, Eq)]
#[must_use = "a waiter that isn't woken may wait for good"]
pub struct Woken([Vec<u64>; 2]);

impl Woken {
    /// The ids, an id once for each time it was added.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().flatten().copied()
    }
}

/// A channel's two queues, each holding at most [`MAX_QUEUED`] messages.
pub struct Channel<T> {
    /// The messages waiting at each endpoint, oldest first.
    queues: [VecDeque<Message<T>>; 2],
    closed: [bool; 2],
    /// The ids waiting on each endpoint, in no order.
    waiters: [Vec<u64>; 2],
}

impl<T> Channel<T> {
    /// A channel with both endpoints open, nothing queued and no waiters,
    /// which allocates nothing.
    pub const fn new() -> Self {
        Self {
            queues: [VecDeque::new(), VecDeque::new()],
            closed: [false, false],
            waiters: [Vec::new(), Vec::new()],
        }
    }

    /// `side`'s signals: [`Signals::READABLE`] while a message waits
    /// there, [`Signals::PEER_CLOSED`] once its peer is closed, and
    /// [`Signals::WRITABLE`] while its peer is open with fewer than
    /// [`MAX_QUEUED`] messages waiting.
    pub fn signals(&self, side: Side) -> Signals {
        let mut signals = Signals::NONE;
        if !self.queues[side.index()].is_empty() {
            signals = signals | Signals::READABLE;
        }
        let peer = side.peer().index();
        if self.closed[peer] {
            signals = signals | Signals::PEER_CLOSED;
        } else if self.queues[peer].len() < MAX_QUEUED {
            signals = signals | Signals::WRITABLE;
        }
        signals
    }

    /// Adds `id` to `side`'s waiters, until a change that gives `side` a
    /// signal it lacked takes it off, or [`Channel::remove_waiter`] does,
    /// and returns `side`'s signals. Fails with [`Error::NoMemory`], and
    /// then adds nothing.
    pub fn add_waiter(&mut self, side: Side, id: u64) -> Result<Signals, Error> {
        let waiters = &mut self.waiters[side.index()];
        waiters.try_reserve(1).map_err(|_| Error::NoMemory)?;
        waiters.push(id);
        Ok(self.signals(side))
    }

    /// Takes `id` off `side`'s waiters once, if it is there, and returns
    /// `side`'s signals.
    pub fn remove_waiter(&mut self, side: Side, id: u64) -> Signals {
        let waiters = &mut self.waiters[side.index()];
        if let Some(i) = waiters.iter().position(|&waiter| waiter == id) {
            waiters.swap_remove(i);
        }
        self.signals(side)
    }

    fn both_signals(&self) -> [Signals; 2] {
        [self.signals(Side::First), self.signals(Side::Second)]
    }

    /// Takes the waiters off each side that has a signal now that it
    /// lacked in `before`.
    fn woken(&mut self, before: [Signals; 2]) -> Woken {
        let mut woken = Woken::default();
        for side in [Side::First, Side::Second] {
            let i = side.index();
            if !before[i].contains(self.signals(side)) {
                woken.0[i] = mem::take(&mut self.waiters[i]);
            }
        }
        woken
    }

    /// Queues the message `make` returns at `from`'s peer, and returns the
    /// waiters that wakes. Calls `make` only once the message has a place,
    /// so a send that fails with [`Error::PeerClosed`],
    /// [`Error::ShouldWait`] if [`MAX_QUEUED`] messages wait there, or
    /// [`Error::NoMemory`], never calls it. A failure from `make` sends
    /// nothing.
    pub fn send(
        &mut self,
        from: Side,
        make: impl FnOnce() -> Result<Message<T>, Error>,
    ) -> Result<Woken, Error> {
        let before = self.both_signals();
        let to = from.peer();
        if self.closed[to.index()] {
            return Err(Error::PeerClosed);
        }
        let queue = &mut self.queues[to.index()];
        if queue.len() >= MAX_QUEUED {
            return Err(Error::ShouldWait);
        }
        queue.try_reserve(1).map_err(|_| Error::NoMemory)?;
        queue.push_back(make()?);
        Ok(self.woken(before))
    }

    /// Hands the oldest message waiting at `at` to `take`, and returns what
    /// it returns and the waiters that wakes. A failed `take` hands the
    /// message back, and it stays the oldest. Fails with
    /// [`Error::ShouldWait`] if no message waits, or [`Error::PeerClosed`]
    /// if none waits and the peer is closed.
    pub fn receive<R>(
        &mut self,
        at: Side,
        take: impl FnOnce(Message<T>) -> Result<R, (Error, Message<T>)>,
    ) -> Result<(R, Woken), Error> {
        let before = self.both_signals();
        let Some(message) = self.queues[at.index()].pop_front() else {
            return Err(if self.closed[at.peer().index()] {
                Error::PeerClosed
            } else {
                Error::ShouldWait
            });
        };
        match take(message) {
            Ok(taken) => Ok((taken, self.woken(before))),
            Err((e, message)) => {
                // Popping left its place free.
                self.queues[at.index()].push_front(message);
                Err(e)
            }
        }
    }

    /// Closes `side`, and returns the messages that waited there, which
    /// nothing can receive now, and the waiters that wakes. Messages it
    /// sent stay for its peer to receive. Closing it again returns
    /// nothing.
    pub fn close(&mut self, side: Side) -> (VecDeque<Message<T>>, Woken) {
        let before = self.both_signals();
        self.closed[side.index()] = true;
        let queued = mem::take(&mut self.queues[side.index()]);
        (queued, self.woken(before))
    }
}

impl<T> Default for Channel<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn message(byte: u8, handles: &[char]) -> Message<char> {
        Message {
            bytes: vec![byte],
            handles: handles.to_vec(),
        }
    }

    /// Sends a message of `byte` and `handles`, and returns the ids it
    /// woke.
    fn send(channel: &mut Channel<char>, from: Side, byte: u8, handles: &[char]) -> Vec<u64> {
        let woken = channel.send(from, || Ok(message(byte, handles))).unwrap();
        woken.ids().collect()
    }

    fn receive(channel: &mut Channel<char>, at: Side) -> Result<Message<char>, Error> {
        channel.receive(at, Ok).map(|(message, _)| message)
    }

    fn fill(channel: &mut Channel<char>, from: Side) {
        for i in 0..MAX_QUEUED {
            send(channel, from, u8::try_from(i).unwrap(), &[]);
        }
    }

    #[test]
    fn messages_arrive_in_order_at_the_peer() {
        let mut channel = Channel::new();
        send(&mut channel, Side::First, 1, &['a']);
        send(&mut channel, Side::First, 2, &[]);
        send(&mut channel, Side::Second, 3, &[]);
        assert_eq!(receive(&mut channel, Side::Second), Ok(message(1, &['a'])));
        assert_eq!(receive(&mut channel, Side::Second), Ok(message(2, &[])));
        assert_eq!(receive(&mut channel, Side::Second), Err(Error::ShouldWait));
        assert_eq!(receive(&mut channel, Side::First), Ok(message(3, &[])));
    }

    #[test]
    fn a_full_queue_fails_without_making_the_message() {
        let mut channel = Channel::new();
        fill(&mut channel, Side::First);
        let refused = channel.send(Side::First, || panic!("made a message for a full queue"));
        assert_eq!(refused, Err(Error::ShouldWait));
        // The other direction has its own room.
        send(&mut channel, Side::Second, 0, &[]);
        receive(&mut channel, Side::Second).unwrap();
        send(&mut channel, Side::First, 0, &[]);
    }

    #[test]
    fn a_failed_make_sends_nothing() {
        let mut channel = Channel::new();
        let sent = channel.send(Side::First, || Err(Error::AccessDenied));
        assert_eq!(sent, Err(Error::AccessDenied));
        assert_eq!(receive(&mut channel, Side::Second), Err(Error::ShouldWait));
    }

    #[test]
    fn a_failed_take_leaves_the_message_oldest() {
        let mut channel = Channel::new();
        send(&mut channel, Side::First, 1, &['a']);
        send(&mut channel, Side::First, 2, &[]);
        let taken = channel.receive(Side::Second, |m| Err::<(), _>((Error::BadAddress, m)));
        assert_eq!(taken, Err(Error::BadAddress));
        assert_eq!(receive(&mut channel, Side::Second), Ok(message(1, &['a'])));
    }

    #[test]
    fn a_closed_peer_is_drained_before_it_fails() {
        let mut channel = Channel::new();
        send(&mut channel, Side::First, 1, &[]);
        send(&mut channel, Side::Second, 2, &['b']);
        let (dropped, _) = channel.close(Side::First);
        assert_eq!(dropped, [message(2, &['b'])]);
        assert!(channel.close(Side::First).0.is_empty());
        let refused = channel.send(Side::Second, || panic!("made a message for a closed peer"));
        assert_eq!(refused, Err(Error::PeerClosed));
        assert_eq!(receive(&mut channel, Side::Second), Ok(message(1, &[])));
        assert_eq!(receive(&mut channel, Side::Second), Err(Error::PeerClosed));
    }

    #[test]
    fn signals_follow_the_queues_and_a_close() {
        let mut channel = Channel::new();
        assert_eq!(channel.signals(Side::First), Signals::WRITABLE);
        send(&mut channel, Side::First, 1, &[]);
        assert_eq!(
            channel.signals(Side::Second),
            Signals::READABLE | Signals::WRITABLE
        );
        fill(&mut channel, Side::Second);
        assert_eq!(channel.signals(Side::Second), Signals::READABLE);
        assert_eq!(
            channel.signals(Side::First),
            Signals::READABLE | Signals::WRITABLE
        );
        drop(channel.close(Side::First));
        assert_eq!(
            channel.signals(Side::Second),
            Signals::READABLE | Signals::PEER_CLOSED
        );
    }

    #[test]
    fn a_change_wakes_only_the_side_that_gained_a_signal() {
        let mut channel = Channel::new();
        channel.add_waiter(Side::First, 1).unwrap();
        channel.add_waiter(Side::Second, 2).unwrap();
        // Second gains READABLE; First loses nothing it is woken for.
        assert_eq!(send(&mut channel, Side::First, 0, &[]), [2]);
        // Second had READABLE already.
        channel.add_waiter(Side::Second, 2).unwrap();
        assert_eq!(send(&mut channel, Side::First, 0, &[]), Vec::<u64>::new());
        // Second losing a message gains it nothing, nor First while its
        // peer's queue had room.
        let (_, woken) = channel.receive(Side::Second, Ok).unwrap();
        assert_eq!(woken, Woken::default());
        // First gains WRITABLE when its full peer's queue frees a place.
        let mut channel = Channel::new();
        fill(&mut channel, Side::First);
        channel.add_waiter(Side::First, 3).unwrap();
        let (_, woken) = channel.receive(Side::Second, Ok).unwrap();
        assert_eq!(woken.ids().collect::<Vec<_>>(), [3]);
        // A close wakes the peer, which gains PEER_CLOSED.
        channel.add_waiter(Side::Second, 4).unwrap();
        channel.add_waiter(Side::Second, 5).unwrap();
        let (_, woken) = channel.close(Side::First);
        let mut ids: Vec<_> = woken.ids().collect();
        ids.sort_unstable();
        assert_eq!(ids, [4, 5]);
    }

    #[test]
    fn removing_a_waiter_takes_one_of_its_entries() {
        let mut channel = Channel::new();
        channel.add_waiter(Side::Second, 7).unwrap();
        channel.add_waiter(Side::Second, 7).unwrap();
        channel.add_waiter(Side::Second, 8).unwrap();
        assert_eq!(channel.remove_waiter(Side::Second, 7), Signals::WRITABLE);
        channel.remove_waiter(Side::Second, 8);
        channel.remove_waiter(Side::Second, 9);
        assert_eq!(send(&mut channel, Side::First, 0, &[]), [7]);
    }
}
