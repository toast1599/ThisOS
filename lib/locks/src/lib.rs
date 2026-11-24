//! The kernel's spin lock: a `spin::Mutex` with a rank, a record of the CPU
//! and thread that hold it, and a limit on how long anyone spins for it.
//!
//! Every lock has a [`Rank`], and a CPU may take a lock only if its rank is
//! above every lock it already holds, which rules out the lock-order
//! cycles that deadlock. Debug builds check this on every [`Lock::lock`].
//! [`Lock::try_lock`] isn't checked, since it can't wait, but its lock goes
//! on the list like any other.
//!
//! Each CPU keeps the list of locks held on it, at most [`MAX_HELD`]. The
//! list belongs to the thread running there: the scheduler swaps it with
//! [`switch_held`], and an interrupt handler's locks go on the interrupted
//! thread's list and come off before it returns.
//!
//! Spinning for a lock longer than a second panics in every build, naming
//! the lock, the spinning CPU and thread, and the holder. A spin past 10 ms
//! calls the hook set with [`on_long_spin`] once, so the kernel can show
//! the wait before the panic.
//!
//! The [`Platform`] gives the CPU's index, a counter and its rate, and a
//! way to keep interrupts off while a list changes. The `host` feature
//! adds one for host tests.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(any(test, feature = "host"))]
extern crate std;

#[cfg(any(test, feature = "host"))]
pub mod host;

use core::fmt;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};

/// The most CPUs the lists cover.
pub const MAX_CPUS: usize = 64;

/// The most locks one thread holds at once.
pub const MAX_HELD: usize = 8;

/// What the locks need from the machine.
pub trait Platform {
    /// The running CPU's index, below [`MAX_CPUS`].
    fn cpu() -> usize;
    /// A counter that goes up at [`Platform::counter_hz`].
    fn counter() -> u64;
    /// The counter's rate, or `None` while it's unknown, which leaves spins
    /// without a limit.
    fn counter_hz() -> Option<u64>;
    /// Runs `f` with interrupts off on this CPU, restoring them after.
    fn masked<R>(f: impl FnOnce() -> R) -> R;
}

/// A lock's place in the lock order, with a name for reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rank {
    number: u16,
    name: &'static str,
}

impl Rank {
    /// Kernel tests: a lock below every other.
    pub const TEST_OUTER: Self = Self::new(1, "TEST_OUTER");
    /// Kernel tests: a lock above [`Rank::TEST_OUTER`], below the kernel's.
    pub const TEST_INNER: Self = Self::new(2, "TEST_INNER");
    /// The test harness's record of the test running.
    pub const TEST_RUN: Self = Self::new(3, "TEST_RUN");
    /// The ramdisk and its memory-object copy.
    pub const RAMDISK: Self = Self::new(10, "RAMDISK");
    /// A process's life: its space, start and end, and who waits for it.
    pub const LIFE: Self = Self::new(20, "LIFE");
    /// A channel's two sides.
    pub const CHANNEL: Self = Self::new(30, "CHANNEL");
    /// A process's handle table.
    pub const HANDLES: Self = Self::new(40, "HANDLES");
    /// A memory object's count of writable and executable mappings.
    pub const USES: Self = Self::new(50, "USES");
    /// A user space's mappings.
    pub const MAPPINGS: Self = Self::new(60, "MAPPINGS");
    /// The kernel's page tables.
    pub const PAGE_TABLES: Self = Self::new(70, "PAGE_TABLES");
    /// The physical frame bitmap.
    pub const FRAMES: Self = Self::new(80, "FRAMES");
    /// The saved memory map.
    pub const MEMORY: Self = Self::new(90, "MEMORY");
    /// User writes waiting for room in the log.
    pub const WAITERS: Self = Self::new(100, "WAITERS");
    /// Kernel tests: a lock taken with interrupts off, after the rest.
    pub const TEST_LATE: Self = Self::new(110, "TEST_LATE");
    /// How a process ended. Taken with interrupts off, from a user fault's
    /// trap among others, so it and every rank above rank above every lock
    /// taken with interrupts on: a trap can arrive while any of those is
    /// held.
    pub const PROCESS_END: Self = Self::new(200, "PROCESS_END");
    /// The test harness's expected trap, taken from the trap handler.
    pub const TEST_EXPECTED: Self = Self::new(205, "TEST_EXPECTED");
    /// The scheduler.
    pub const SCHEDULER: Self = Self::new(210, "SCHEDULER");
    /// The log's serial port and console.
    pub const SINKS: Self = Self::new(220, "SINKS");
    /// The log's ring.
    pub const RING: Self = Self::new(230, "RING");

    const ALL: &[Self] = &[
        Self::TEST_OUTER,
        Self::TEST_INNER,
        Self::TEST_RUN,
        Self::RAMDISK,
        Self::LIFE,
        Self::CHANNEL,
        Self::HANDLES,
        Self::USES,
        Self::MAPPINGS,
        Self::PAGE_TABLES,
        Self::FRAMES,
        Self::MEMORY,
        Self::WAITERS,
        Self::TEST_LATE,
        Self::PROCESS_END,
        Self::TEST_EXPECTED,
        Self::SCHEDULER,
        Self::SINKS,
        Self::RING,
    ];

    const fn new(number: u16, name: &'static str) -> Self {
        assert!(number != NONE, "rank 0 means no lock");
        Self { number, name }
    }

    /// The rank's name, as in the table.
    pub fn name(self) -> &'static str {
        self.name
    }

    fn numbered(number: u16) -> Option<Self> {
        Self::ALL.iter().copied().find(|rank| rank.number == number)
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.number)
    }
}

/// A list entry or rank number meaning no lock.
const NONE: u16 = 0;

/// A holder CPU meaning the lock isn't held through a listed guard.
const NOBODY: usize = usize::MAX;

/// A thread id meaning none was set.
const NO_THREAD: u64 = u64::MAX;

/// One CPU's state. Only that CPU changes its list and the rank it spins
/// for, with interrupts off; any CPU reads them.
struct PerCpu {
    held: [AtomicU16; MAX_HELD],
    len: AtomicUsize,
    thread: AtomicU64,
    spinning: AtomicU16,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            held: [const { AtomicU16::new(NONE) }; MAX_HELD],
            len: AtomicUsize::new(0),
            thread: AtomicU64::new(NO_THREAD),
            spinning: AtomicU16::new(NONE),
        }
    }

    #[cfg(any(test, feature = "host"))]
    fn reset(&self) {
        self.load(&Saved::default());
        self.thread.store(NO_THREAD, Ordering::Relaxed);
        self.spinning.store(NONE, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Saved {
        let mut saved = Saved {
            len: self.len.load(Ordering::Relaxed).min(MAX_HELD),
            ..Saved::default()
        };
        for (to, from) in saved.held.iter_mut().zip(&self.held).take(saved.len) {
            *to = from.load(Ordering::Relaxed);
        }
        saved.spinning = self.spinning.load(Ordering::Relaxed);
        saved
    }

    fn load(&self, saved: &Saved) {
        for (to, &from) in self.held.iter().zip(&saved.held) {
            to.store(from, Ordering::Relaxed);
        }
        self.len.store(saved.len, Ordering::Relaxed);
        self.spinning.store(saved.spinning, Ordering::Relaxed);
    }

    fn push(&self, rank: Rank) {
        let len = self.len.load(Ordering::Relaxed);
        assert!(
            len < MAX_HELD,
            "taking {rank} with {MAX_HELD} locks held already: {}",
            self.snapshot()
        );
        self.held[len].store(rank.number, Ordering::Relaxed);
        self.len.store(len + 1, Ordering::Relaxed);
    }

    /// Takes `rank`'s last entry off the list, keeping the order of the
    /// rest, since guards can drop in any order.
    fn remove(&self, rank: Rank) {
        let len = self.len.load(Ordering::Relaxed);
        let Some(at) = (0..len)
            .rev()
            .find(|&i| self.held[i].load(Ordering::Relaxed) == rank.number)
        else {
            panic!(
                "dropping {rank}, which isn't on the held list {}",
                self.snapshot()
            );
        };
        for i in at..len - 1 {
            let next = self.held[i + 1].load(Ordering::Relaxed);
            self.held[i].store(next, Ordering::Relaxed);
        }
        self.held[len - 1].store(NONE, Ordering::Relaxed);
        self.len.store(len - 1, Ordering::Relaxed);
    }

    /// The highest-ranked lock held, if any.
    fn highest(&self) -> Option<u16> {
        let len = self.len.load(Ordering::Relaxed);
        self.held[..len]
            .iter()
            .map(|entry| entry.load(Ordering::Relaxed))
            .max()
    }

    fn thread(&self) -> Option<u64> {
        let thread = self.thread.load(Ordering::Relaxed);
        (thread != NO_THREAD).then_some(thread)
    }
}

static CPUS: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

static ON_LONG_SPIN: spin::Once<fn(usize)> = spin::Once::new();

/// Sets the hook a spin calls once when it passes 10 ms, with the spinning
/// CPU's index. Only the first call sets it.
pub fn on_long_spin(hook: fn(usize)) {
    ON_LONG_SPIN.call_once(|| hook);
}

/// Records `thread` as the one running on this CPU, for reports.
pub fn running<P: Platform>(thread: u64) {
    CPUS[P::cpu()].thread.store(thread, Ordering::Relaxed);
}

/// The locks a thread held when it last left a CPU, and the one it was
/// spinning for, since a tick can preempt a spin. A new thread holds none.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Saved {
    held: [u16; MAX_HELD],
    len: usize,
    spinning: u16,
}

impl fmt::Display for Saved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len == 0 {
            return f.write_str("[]");
        }
        f.write_str("[")?;
        for (i, &number) in self.held[..self.len].iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            match Rank::numbered(number) {
                Some(rank) => f.write_str(rank.name)?,
                None => write!(f, "rank {number}")?,
            }
        }
        f.write_str("]")
    }
}

/// Swaps the running thread's held locks and spin for `next`'s, keeping the
/// innermost entry on the list: the lock the scheduler holds while it
/// switches, whose guard drops before the switch. Returns the running
/// thread's list without that entry.
pub fn switch_held<P: Platform>(next: &Saved) -> Saved {
    P::masked(|| {
        let cpu = &CPUS[P::cpu()];
        let mut left = cpu.snapshot();
        let Some(len) = left.len.checked_sub(1) else {
            panic!("switch_held with no lock held");
        };
        let switching = left.held[len];
        left.held[len] = NONE;
        left.len = len;
        assert!(
            next.len < MAX_HELD,
            "a thread left holding {MAX_HELD} locks: {next}"
        );
        let mut arriving = *next;
        arriving.held[arriving.len] = switching;
        arriving.len += 1;
        cpu.load(&arriving);
        left
    })
}

/// The thread last recorded as running on `cpu`, if any.
pub fn thread(cpu: usize) -> Option<u64> {
    CPUS[cpu].thread()
}

/// The locks held on `cpu` right now.
pub fn held(cpu: usize) -> Saved {
    CPUS[cpu].snapshot()
}

/// The innermost lock held on `cpu`, if any.
pub fn innermost(cpu: usize) -> Option<Rank> {
    let held = held(cpu);
    held.held[..held.len]
        .last()
        .and_then(|&number| Rank::numbered(number))
}

/// The lock `cpu` is spinning for, if any.
pub fn spinning(cpu: usize) -> Option<Rank> {
    Rank::numbered(CPUS[cpu].spinning.load(Ordering::Relaxed))
}

/// A ranked spin lock holding a `T`, for a [`Platform`] `P`.
pub struct Lock<T, P> {
    rank: Rank,
    mutex: spin::Mutex<T>,
    /// The CPU that holds the lock through a listed guard, or [`NOBODY`].
    holder_cpu: AtomicUsize,
    /// The thread that holds it, valid while `holder_cpu` is.
    holder_thread: AtomicU64,
    platform: PhantomData<fn() -> P>,
}

impl<T, P: Platform> Lock<T, P> {
    /// A lock of `rank` holding `value`.
    pub const fn new(rank: Rank, value: T) -> Self {
        Self {
            rank,
            mutex: spin::Mutex::new(value),
            holder_cpu: AtomicUsize::new(NOBODY),
            holder_thread: AtomicU64::new(NO_THREAD),
            platform: PhantomData,
        }
    }

    /// The lock's rank.
    pub fn rank(&self) -> Rank {
        self.rank
    }

    /// The value, from a lock nothing else can reach.
    pub fn into_inner(self) -> T {
        self.mutex.into_inner()
    }

    /// Takes the lock, spinning until it's free. Panics in debug builds if
    /// a lock of this rank or above is held, when [`MAX_HELD`] are held,
    /// and after spinning for a second.
    pub fn lock(&self) -> Guard<'_, T, P> {
        if cfg!(debug_assertions) {
            P::masked(|| self.check_order(&CPUS[P::cpu()]));
        }
        let guard = match self.mutex.try_lock() {
            Some(guard) => guard,
            None => self.spin(),
        };
        self.listed(guard)
    }

    /// Takes the lock if it's free, whatever is held: a try can't wait, so
    /// it can't deadlock. Panics when [`MAX_HELD`] are held.
    pub fn try_lock(&self) -> Option<Guard<'_, T, P>> {
        self.mutex.try_lock().map(|guard| self.listed(guard))
    }

    /// Takes the lock if it's free, leaving it off the held list and
    /// without a holder, for the panic path, which must not panic again
    /// over a full list or a stale holder.
    pub fn try_lock_unlisted(&self) -> Option<Guard<'_, T, P>> {
        Some(Guard {
            lock: self,
            inner: self.mutex.try_lock()?,
            listed: false,
        })
    }

    fn check_order(&self, cpu: &PerCpu) {
        if let Some(highest) = cpu.highest()
            && highest >= self.rank.number
        {
            let highest = Rank::numbered(highest).unwrap_or(Rank {
                number: highest,
                name: "?",
            });
            panic!(
                "taking {} while holding {highest}: held {}",
                self.rank,
                cpu.snapshot()
            );
        }
    }

    fn listed<'a>(&'a self, guard: spin::MutexGuard<'a, T>) -> Guard<'a, T, P> {
        let cpu = P::masked(|| {
            let cpu = P::cpu();
            CPUS[cpu].push(self.rank);
            cpu
        });
        self.holder_thread
            .store(CPUS[cpu].thread.load(Ordering::Relaxed), Ordering::Relaxed);
        self.holder_cpu.store(cpu, Ordering::Relaxed);
        Guard {
            lock: self,
            inner: guard,
            listed: true,
        }
    }

    // The CPU is read afresh each time, since a tick can move the spinning
    // thread, whose spin goes with it.
    fn spin(&self) -> spin::MutexGuard<'_, T> {
        P::masked(|| {
            CPUS[P::cpu()]
                .spinning
                .store(self.rank.number, Ordering::Relaxed);
        });
        let started = P::counter();
        let hz = P::counter_hz().filter(|&hz| hz != 0);
        let mut told = false;
        loop {
            spin_loop();
            if !self.mutex.is_locked()
                && let Some(guard) = self.mutex.try_lock()
            {
                P::masked(|| {
                    CPUS[P::cpu()].spinning.store(NONE, Ordering::Relaxed);
                });
                return guard;
            }
            let Some(hz) = hz else {
                continue;
            };
            let spun = P::counter().wrapping_sub(started);
            if !told && spun >= hz / 100 {
                told = true;
                if let Some(hook) = ON_LONG_SPIN.get() {
                    hook(P::cpu());
                }
            }
            if spun >= hz {
                self.stuck(P::cpu());
            }
        }
    }

    fn stuck(&self, cpu: usize) -> ! {
        let spinner = &CPUS[cpu];
        panic!(
            "spun 1 s for {} on CPU {cpu}, {} holding {}; {}",
            self.rank,
            Thread(spinner.thread()),
            spinner.snapshot(),
            Holder {
                cpu: self.holder_cpu.load(Ordering::Relaxed),
                thread: self.holder_thread.load(Ordering::Relaxed),
            }
        );
    }
}

/// A thread id for reports, if one was set.
struct Thread(Option<u64>);

impl fmt::Display for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(id) => write!(f, "thread {id}"),
            None => f.write_str("no thread"),
        }
    }
}

/// A lock's holder as last recorded, read without locking.
struct Holder {
    cpu: usize,
    thread: u64,
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(on) = CPUS.get(self.cpu) else {
            return f.write_str("no holder recorded");
        };
        let thread = Thread((self.thread != NO_THREAD).then_some(self.thread));
        write!(f, "held by CPU {} {thread}", self.cpu)?;
        if on.thread() == thread.0 {
            write!(f, ", which holds {}", on.snapshot())
        } else {
            f.write_str(", which isn't running, so its list is saved")
        }
    }
}

/// A held [`Lock`]. Dropping it takes the lock off the list and frees it.
pub struct Guard<'a, T, P: Platform> {
    lock: &'a Lock<T, P>,
    inner: spin::MutexGuard<'a, T>,
    listed: bool,
}

impl<T, P: Platform> Deref for Guard<'_, T, P> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T, P: Platform> DerefMut for Guard<'_, T, P> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T, P: Platform> Drop for Guard<'_, T, P> {
    fn drop(&mut self) {
        if self.listed {
            self.lock.holder_cpu.store(NOBODY, Ordering::Relaxed);
            P::masked(|| CPUS[P::cpu()].remove(self.lock.rank));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;
    use std::string::String;
    use std::thread;

    use super::host::Host;
    use super::*;

    static TOLD: AtomicU64 = AtomicU64::new(0);
    static SPINNER: AtomicUsize = AtomicUsize::new(0);

    type TestLock = Lock<u32, Host>;

    fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
        match payload.downcast::<String>() {
            Ok(text) => *text,
            Err(payload) => String::from(*payload.downcast::<&str>().unwrap()),
        }
    }

    #[test]
    fn locks_in_rank_order_go_on_and_off_the_list() {
        let outer = TestLock::new(Rank::LIFE, 1);
        let inner = TestLock::new(Rank::FRAMES, 2);
        let cpu = Host::cpu();
        {
            let a = outer.lock();
            let b = inner.lock();
            assert_eq!(*a + *b, 3);
            assert_eq!(std::format!("{}", held(cpu)), "[LIFE, FRAMES]");
            assert_eq!(innermost(cpu), Some(Rank::FRAMES));
            drop(a);
            assert_eq!(std::format!("{}", held(cpu)), "[FRAMES]");
        }
        assert_eq!(held(cpu), Saved::default());
    }

    #[test]
    #[should_panic(expected = "taking LIFE (20) while holding FRAMES (80): held [FRAMES]")]
    fn taking_a_lower_rank_panics() {
        let outer = TestLock::new(Rank::LIFE, 0);
        let inner = TestLock::new(Rank::FRAMES, 0);
        let _inner = inner.lock();
        let _outer = outer.lock();
    }

    #[test]
    #[should_panic(expected = "taking FRAMES (80) while holding FRAMES (80)")]
    fn taking_an_equal_rank_panics() {
        let a = TestLock::new(Rank::FRAMES, 0);
        let b = TestLock::new(Rank::FRAMES, 0);
        let _a = a.lock();
        let _b = b.lock();
    }

    #[test]
    fn a_try_out_of_order_is_allowed_and_listed() {
        let outer = TestLock::new(Rank::LIFE, 0);
        let inner = TestLock::new(Rank::FRAMES, 0);
        let _inner = inner.lock();
        let outer = outer.try_lock().unwrap();
        assert_eq!(std::format!("{}", held(Host::cpu())), "[FRAMES, LIFE]");
        drop(outer);
        assert_eq!(std::format!("{}", held(Host::cpu())), "[FRAMES]");
    }

    #[test]
    fn a_try_on_a_held_lock_fails() {
        let lock = TestLock::new(Rank::LIFE, 0);
        let _held = lock.lock();
        assert!(lock.try_lock().is_none());
        assert!(lock.try_lock_unlisted().is_none());
    }

    #[test]
    fn an_unlisted_try_stays_off_the_list() {
        let lock = TestLock::new(Rank::LIFE, 0);
        let _guard = lock.try_lock_unlisted().unwrap();
        assert_eq!(held(Host::cpu()), Saved::default());
    }

    #[test]
    #[should_panic(expected = "taking TEST_INNER (2) with 8 locks held already: [")]
    fn a_ninth_lock_panics() {
        let locks: std::vec::Vec<TestLock> =
            (0..9).map(|_| TestLock::new(Rank::TEST_INNER, 0)).collect();
        let _guards: std::vec::Vec<_> = locks.iter().map(Lock::try_lock).collect();
    }

    #[test]
    fn a_spin_past_a_second_panics_naming_the_holder() {
        let lock = TestLock::new(Rank::FRAMES, 0);
        let outer = TestLock::new(Rank::LIFE, 0);
        running::<Host>(7);
        let _outer = outer.lock();
        let _held = lock.lock();
        on_long_spin(|cpu| {
            TOLD.fetch_or(1 << cpu, Ordering::Relaxed);
        });
        let text = thread::scope(|scope| {
            scope
                .spawn(|| {
                    running::<Host>(9);
                    let spinner = Host::cpu();
                    SPINNER.store(spinner, Ordering::Relaxed);
                    drop(lock.lock());
                })
                .join()
                .map_err(panic_text)
                .unwrap_err()
        });
        let (spinner, text) = (SPINNER.load(Ordering::Relaxed), text);
        let holder = Host::cpu();
        assert_eq!(
            text,
            std::format!(
                "spun 1 s for FRAMES (80) on CPU {spinner}, thread 9 holding []; \
                 held by CPU {holder} thread 7, which holds [LIFE, FRAMES]"
            )
        );
        assert_ne!(TOLD.load(Ordering::Relaxed) & (1 << spinner), 0);
    }

    #[test]
    fn a_spin_names_a_holder_that_left_its_cpu() {
        let lock = TestLock::new(Rank::FRAMES, 0);
        let switching = TestLock::new(Rank::SCHEDULER, 0);
        running::<Host>(3);
        let _held = lock.lock();
        let guard = switching.lock();
        let left = switch_held::<Host>(&Saved::default());
        drop(guard);
        running::<Host>(4);
        let text = thread::scope(|scope| {
            scope
                .spawn(|| drop(lock.lock()))
                .join()
                .map_err(panic_text)
                .unwrap_err()
        });
        assert!(
            text.ends_with(&std::format!(
                "held by CPU {} thread 3, which isn't running, so its list is saved",
                Host::cpu()
            )),
            "{text}"
        );
        assert_eq!(std::format!("{left}"), "[FRAMES]");
        // Back to the thread that holds the lock, so its guard drops.
        let guard = switching.lock();
        switch_held::<Host>(&left);
        drop(guard);
    }

    #[test]
    fn a_switch_swaps_lists_and_keeps_the_switching_lock() {
        let outer = TestLock::new(Rank::LIFE, 0);
        let switching = TestLock::new(Rank::SCHEDULER, 0);
        let cpu = Host::cpu();
        let held_by_first = outer.lock();
        let guard = switching.lock();
        let first = switch_held::<Host>(&Saved::default());
        assert_eq!(std::format!("{}", held(cpu)), "[SCHEDULER]");
        drop(guard);
        assert_eq!(held(cpu), Saved::default());

        let guard = switching.lock();
        let second = switch_held::<Host>(&first);
        assert_eq!(std::format!("{}", held(cpu)), "[LIFE, SCHEDULER]");
        assert_eq!(second, Saved::default());
        drop(guard);
        drop(held_by_first);
        assert_eq!(held(cpu), Saved::default());
    }

    #[test]
    fn a_spin_goes_with_its_thread() {
        let switching = TestLock::new(Rank::SCHEDULER, 0);
        let cpu = Host::cpu();
        CPUS[cpu]
            .spinning
            .store(Rank::FRAMES.number, Ordering::Relaxed);
        let guard = switching.lock();
        let spinner = switch_held::<Host>(&Saved::default());
        drop(guard);
        assert_eq!(spinning(cpu), None);
        let guard = switching.lock();
        switch_held::<Host>(&spinner);
        drop(guard);
        assert_eq!(spinning(cpu), Some(Rank::FRAMES));
        CPUS[cpu].spinning.store(NONE, Ordering::Relaxed);
    }

    #[test]
    fn rank_numbers_are_unique_and_rise_through_the_table() {
        for pair in Rank::ALL.windows(2) {
            assert!(pair[0].number < pair[1].number, "{} {}", pair[0], pair[1]);
        }
    }
}
