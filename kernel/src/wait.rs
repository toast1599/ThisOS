//! Waiting on several objects at once. The running thread adds its id to
//! each object's waiters and parks; a change that gives one of them a
//! signal unparks it, and it takes its id off every list before it looks
//! again. Since an unpark that comes before the park makes the park return
//! at once, a signal that arrives between the check and the park isn't
//! lost. Only channel endpoints have signals so far.

use alloc::sync::Arc;

use thisos_abi::{Error, Signals};

use crate::channel::Endpoint;
use crate::{thread, timer};

/// The tick a wait of `timeout` nanoseconds ends at: now for 0, never for
/// `u64::MAX`, and otherwise late enough that at least `timeout` passes,
/// though the tick now under way may be nearly over.
pub fn deadline(timeout: u64) -> Option<u64> {
    let now = timer::ticks();
    let nanos_per_tick = 1_000_000_000 / u64::from(timer::HZ);
    match timeout {
        0 => Some(now),
        u64::MAX => None,
        _ => Some(now.saturating_add(timeout.div_ceil(nanos_per_tick) + 1)),
    }
}

/// Blocks until one of the endpoints in `items` has one of the signals
/// paired with it, or until [`timer::ticks`] reaches `deadline`, then
/// writes each endpoint's signals at the same place in `observed` and
/// returns how many items have a wanted one. Fails with
/// [`Error::NoMemory`] if a waiter list can't grow, or
/// [`Error::TimedOut`] once the deadline has passed with none.
pub fn wait(
    items: &[(Arc<Endpoint>, Signals)],
    observed: &mut [Signals],
    deadline: Option<u64>,
) -> Result<usize, Error> {
    assert_eq!(items.len(), observed.len(), "one observed per item");
    let id = thread::id();
    let expired = || deadline.is_some_and(|deadline| timer::ticks() >= deadline);
    loop {
        let mut ready = false;
        for (i, (endpoint, wanted)) in items.iter().enumerate() {
            match endpoint.add_waiter(id) {
                Ok(signals) => ready |= signals.intersects(*wanted),
                Err(e) => {
                    for (endpoint, _) in &items[..i] {
                        endpoint.remove_waiter(id);
                    }
                    return Err(e);
                }
            }
        }
        if !ready && !expired() {
            match deadline {
                Some(deadline) => thread::park_until(deadline),
                None => thread::park(),
            };
        }
        let mut count = 0;
        for ((endpoint, wanted), observed) in items.iter().zip(observed.iter_mut()) {
            *observed = endpoint.remove_waiter(id);
            if observed.intersects(*wanted) {
                count += 1;
            }
        }
        if count > 0 {
            return Ok(count);
        }
        if expired() {
            return Err(Error::TimedOut);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, Ordering};

    use spin::Mutex;
    use thisos_channels::Message;

    use super::*;
    use crate::channel;
    use crate::testing::spin_until;

    fn ping(endpoint: &Endpoint) {
        let sent = endpoint.send(|| {
            Ok(Message {
                bytes: Vec::from(*b"ping"),
                handles: Vec::new(),
            })
        });
        assert!(sent.is_ok(), "sending failed");
    }

    #[test_case]
    fn a_waiter_sleeps_until_its_endpoint_gets_a_message() {
        static STARTED: AtomicBool = AtomicBool::new(false);
        static DONE: AtomicBool = AtomicBool::new(false);
        type Outcome = (Result<usize, Error>, [Signals; 2]);
        static RESULT: Mutex<Option<Outcome>> = Mutex::new(None);
        let (a, b) = channel::pair();
        let (a, b) = (Arc::new(a), Arc::new(b));
        let (c, _d) = channel::pair();
        let items = [
            (Arc::clone(&b), Signals::READABLE),
            (Arc::new(c), Signals::READABLE | Signals::PEER_CLOSED),
        ];
        thread::spawn(move || {
            STARTED.store(true, Ordering::Relaxed);
            let mut observed = [Signals::NONE; 2];
            let result = wait(&items, &mut observed, None);
            *RESULT.lock() = Some((result, observed));
            DONE.store(true, Ordering::Release);
        });
        // Long enough for the waiter to park, so that the send below wakes
        // it rather than finding it not yet waiting.
        thread::sleep(5);
        assert!(STARTED.load(Ordering::Relaxed), "the waiter never ran");
        assert!(!DONE.load(Ordering::Acquire), "the wait returned early");
        ping(&a);
        spin_until("the waiter to wake", || DONE.load(Ordering::Acquire));
        let Some((result, observed)) = RESULT.lock().take() else {
            unreachable!("DONE is set after RESULT");
        };
        assert_eq!(result, Ok(1));
        assert_eq!(
            observed,
            [Signals::READABLE | Signals::WRITABLE, Signals::WRITABLE]
        );
        thread::wait_for_exits();
        assert!(b.receive(Ok).is_ok());
    }

    #[test_case]
    fn a_wait_times_out_and_a_zero_timeout_only_checks() {
        let (_a, b) = channel::pair();
        let items = [(Arc::new(b), Signals::READABLE)];
        let mut observed = [Signals::NONE];
        let start = timer::ticks();
        assert_eq!(
            wait(&items, &mut observed, deadline(0)),
            Err(Error::TimedOut)
        );
        let deadline = deadline(30_000_000);
        assert_eq!(wait(&items, &mut observed, deadline), Err(Error::TimedOut));
        // 30 ms is three ticks, and one more covers the tick under way.
        assert!(timer::ticks() >= start + 4, "the wait ended early");
        let mut none = [];
        assert_eq!(
            wait(&[], &mut none, Some(timer::ticks() + 1)),
            Err(Error::TimedOut)
        );
    }

    #[test_case]
    fn a_wait_returns_at_once_for_a_signal_already_set() {
        let (a, b) = channel::pair();
        ping(&a);
        let items = [
            (Arc::new(b), Signals::READABLE),
            (Arc::new(a), Signals::WRITABLE),
        ];
        let mut observed = [Signals::NONE; 2];
        assert_eq!(wait(&items, &mut observed, None), Ok(2));
    }
}
