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
    /// lacked in `before`, and calls `wake` with each, an id once for each
    /// time it was added. The lists keep their room, so the next wait
    /// doesn't allocate it again.
    fn wake(&mut self, before: [Signals; 2], mut wake: impl FnMut(u64)) {
        for side in [Side::First, Side::Second] {
            let i = side.index();
            if !before[i].contains(self.signals(side)) {
                self.waiters[i].drain(..).for_each(&mut wake);
            }
        }
    }

    /// Queues the message `make` returns at `from`'s peer, and calls
    /// `wake` with the waiters that wakes. Calls `make` only once the
    /// message has a place,
    /// so a send that fails with [`Error::PeerClosed`],
    /// [`Error::ShouldWait`] if [`MAX_QUEUED`] messages wait there, or
    /// [`Error::NoMemory`], never calls it. A failure from `make` sends
    /// nothing.
    pub fn send(
        &mut self,
        from: Side,
        make: impl FnOnce() -> Result<Message<T>, Error>,
        wake: impl FnMut(u64),
    ) -> Result<(), Error> {
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
        self.wake(before, wake);
        Ok(())
    }

    /// Hands the oldest message waiting at `at` to `take`, returns what it
    /// returns, and calls `wake` with the waiters that wakes. A failed
    /// `take` hands the
    /// message back, and it stays the oldest. Fails with
    /// [`Error::ShouldWait`] if no message waits, or [`Error::PeerClosed`]
    /// if none waits and the peer is closed.
    pub fn receive<R>(
        &mut self,
        at: Side,
        take: impl FnOnce(Message<T>) -> Result<R, (Error, Message<T>)>,
        wake: impl FnMut(u64),
    ) -> Result<R, Error> {
        let before = self.both_signals();
        let Some(message) = self.queues[at.index()].pop_front() else {
            return Err(if self.closed[at.peer().index()] {
                Error::PeerClosed
            } else {
                Error::ShouldWait
            });
        };
        match take(message) {
            Ok(taken) => {
                self.wake(before, wake);
                Ok(taken)
            }
            Err((e, message)) => {
                // Popping left its place free.
                self.queues[at.index()].push_front(message);
                Err(e)
            }
        }
    }

    /// Closes `side`, and calls `wake` with the waiters that wakes. The
    /// messages
    /// waiting there stay until [`Channel::discard`] takes them, and
    /// messages it sent stay for its peer to receive. Closing it again
    /// wakes nobody.
    pub fn close(&mut self, side: Side, wake: impl FnMut(u64)) {
        let before = self.both_signals();
        self.closed[side.index()] = true;
        self.wake(before, wake);
    }

    /// Takes the oldest message waiting at `side`, which must be closed,
    /// so that taking it wakes nobody.
    pub fn discard(&mut self, side: Side) -> Option<Message<T>> {
        self.queues[side.index()].pop_front()
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
        let mut woken = Vec::new();
        channel
            .send(from, || Ok(message(byte, handles)), |id| woken.push(id))
            .unwrap();
        woken
    }

    fn receive(channel: &mut Channel<char>, at: Side) -> Result<Message<char>, Error> {
        channel.receive(at, Ok, |_| {})
    }

    /// Receives a message at `at`, and returns the ids it woke.
    fn receive_waking(channel: &mut Channel<char>, at: Side) -> Vec<u64> {
        let mut woken = Vec::new();
        channel.receive(at, Ok, |id| woken.push(id)).unwrap();
        woken
    }

    /// Closes `side`, and returns the ids it woke.
    fn close(channel: &mut Channel<char>, side: Side) -> Vec<u64> {
        let mut woken = Vec::new();
        channel.close(side, |id| woken.push(id));
        woken
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
        let refused = channel.send(
            Side::First,
            || panic!("made a message for a full queue"),
            |_| {},
        );
        assert_eq!(refused, Err(Error::ShouldWait));
        // The other direction has its own room.
        send(&mut channel, Side::Second, 0, &[]);
        receive(&mut channel, Side::Second).unwrap();
        send(&mut channel, Side::First, 0, &[]);
    }

    #[test]
    fn a_failed_make_sends_nothing() {
        let mut channel = Channel::new();
        let sent = channel.send(Side::First, || Err(Error::AccessDenied), |_| {});
        assert_eq!(sent, Err(Error::AccessDenied));
        assert_eq!(receive(&mut channel, Side::Second), Err(Error::ShouldWait));
    }

    #[test]
    fn a_failed_take_leaves_the_message_oldest() {
        let mut channel = Channel::new();
        send(&mut channel, Side::First, 1, &['a']);
        send(&mut channel, Side::First, 2, &[]);
        let taken = channel.receive(
            Side::Second,
            |m| Err::<(), _>((Error::BadAddress, m)),
            |_| {},
        );
        assert_eq!(taken, Err(Error::BadAddress));
        assert_eq!(receive(&mut channel, Side::Second), Ok(message(1, &['a'])));
    }

    #[test]
    fn a_closed_peer_is_drained_before_it_fails() {
        let mut channel = Channel::new();
        send(&mut channel, Side::First, 1, &[]);
        send(&mut channel, Side::Second, 2, &['b']);
        assert_eq!(close(&mut channel, Side::First), []);
        assert_eq!(channel.discard(Side::First), Some(message(2, &['b'])));
        assert_eq!(channel.discard(Side::First), None);
        let refused = channel.send(
            Side::Second,
            || panic!("made a message for a closed peer"),
            |_| {},
        );
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
        close(&mut channel, Side::First);
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
        assert_eq!(receive_waking(&mut channel, Side::Second), []);
        // First gains WRITABLE when its full peer's queue frees a place.
        let mut channel = Channel::new();
        fill(&mut channel, Side::First);
        channel.add_waiter(Side::First, 3).unwrap();
        assert_eq!(receive_waking(&mut channel, Side::Second), [3]);
        // A close wakes the peer, which gains PEER_CLOSED.
        channel.add_waiter(Side::Second, 4).unwrap();
        channel.add_waiter(Side::Second, 5).unwrap();
        let mut ids = close(&mut channel, Side::First);
        ids.sort_unstable();
        assert_eq!(ids, [4, 5]);
    }

    // A list taken by a wake would make every wait allocate it again.
    #[test]
    fn a_wake_keeps_the_waiter_list() {
        let mut channel = Channel::new();
        for id in 0..4 {
            channel.add_waiter(Side::Second, id).unwrap();
        }
        let capacity = channel.waiters[1].capacity();
        assert_eq!(send(&mut channel, Side::First, 0, &[]), [0, 1, 2, 3]);
        assert_eq!(channel.waiters[1], Vec::<u64>::new());
        assert_eq!(channel.waiters[1].capacity(), capacity);
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
