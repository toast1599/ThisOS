//! Kernel threads and the scheduler. Each CPU has a scheduler of its own,
//! under a lock of its own: the thread it runs, a round-robin ready queue,
//! its blocked threads, an idle thread that runs only when no thread is
//! ready, and a reaper, a thread like any other, that frees the threads
//! that exit there. A thread runs on one CPU for life: [`spawn`] keeps it on
//! the CPU that calls it, and [`new_user`] puts it on the CPU with the
//! fewest threads. Nothing moves a thread, so a thread can't change CPUs
//! partway through changing its CPU's list of held locks, and a process,
//! which has one thread, uses its space on one CPU at a time. A thread's id
//! holds its CPU's index in its low bits, so [`unpark`] takes only that
//! CPU's lock, and no code holds two schedulers' locks.
//!
//! The boot thread runs on the loader's stack, and each AP's idle thread on
//! the stack the AP started on; every other thread has a stack from
//! `paging::map_stack`. A user thread is a kernel thread that shares an
//! address space and belongs to a process, and has left for user code in
//! it; every switch loads the next thread's space, or the kernel's tables.
//!
//! A thread blocks with [`park`] or [`park_until`] until [`unpark`] names
//! it or its deadline passes. Each CPU's tick readies the blocked threads
//! whose deadline has passed before it switches, and an unpark that
//! readies a thread on another CPU that runs its idle thread wakes it with
//! an IPI.
//!
//! A scheduler's lock is only ever held with interrupts off, so the timer's
//! handler never finds its own CPU's held, and nothing allocates or frees
//! while holding it: a tick can preempt a thread that holds the heap's
//! lock, and the heap would then spin with interrupts off for good. Making a
//! thread reserves room in its CPU's queues for it, so moving one never
//! allocates. A switch also swaps the CPU's list of held locks
//! (`thisos_locks::switch_held`), so each thread's list goes with it.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::mem;
use core::sync::atomic::{AtomicU64, Ordering};

use thisos_abi::Error;
use thisos_hal::{AddressSpace as _, Cpu, Ipi, Platform};
use thisos_locks::{MAX_CPUS, Rank, Saved};

use crate::Lock;
use crate::cpu::{self, State};
use crate::process::Process;
use crate::{Arch, paging, panic, timer};

type Context = <Arch as Platform>::Context;

/// The size of every thread's stack but the boot thread's and the APs'
/// idle threads'. aarch64's trap entries tell an overflow by bit 16 of the
/// stack pointer, which needs 64 KiB stacks aligned to 128 KiB, as
/// `map_stack` aligns them.
pub const STACK_SIZE: usize = 64 << 10;

/// The low bits of a thread's id, which hold its CPU's index.
const CPU_BITS: u32 = 6;

const _: () = assert!(
    MAX_CPUS == 1 << CPU_BITS,
    "a thread id's CPU bits cover every CPU"
);

/// The CPU the thread `id` names runs on.
fn cpu_of(id: u64) -> usize {
    let Ok(cpu) = usize::try_from(id % (1 << CPU_BITS)) else {
        unreachable!("the CPU bits fit a usize");
    };
    cpu
}

struct Thread {
    /// Names it to [`unpark`]: a count of its CPU's threads, then the CPU.
    /// Never reused, so a stale id unparks nothing.
    id: u64,
    /// Where it stopped, while it isn't running, which holds its stack.
    context: Option<Context>,
    /// What it runs, until it starts.
    entry: Option<Entry>,
    /// The address space it runs user code in, or `None` for a kernel
    /// thread.
    space: Option<Arc<paging::Space>>,
    /// The process it belongs to, or `None` for a kernel thread.
    process: Option<Arc<Process>>,
    /// The tick at which it stops being blocked, while it is.
    deadline: Option<u64>,
    /// Why its next park returns, once something has woken it: set by an
    /// unpark or a deadline that comes while it is blocked, and by an
    /// unpark that comes before it parks.
    wake: Option<Wake>,
    /// The locks it held when it last left the CPU, which the CPU's list
    /// holds while it runs.
    held: Saved,
}

impl Thread {
    /// A thread of CPU `cpu`.
    fn new(
        cpu: usize,
        context: Option<Context>,
        entry: Option<Entry>,
        space: Option<Arc<paging::Space>>,
        process: Option<Arc<Process>>,
    ) -> Self {
        static NEXT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
        let Ok(index) = u64::try_from(cpu) else {
            unreachable!("a CPU index fits a u64");
        };
        Self {
            id: NEXT[cpu].fetch_add(1, Ordering::Relaxed) << CPU_BITS | index,
            context,
            entry,
            space,
            process,
            deadline: None,
            wake: None,
            held: Saved::default(),
        }
    }
}

/// What a thread runs when it starts.
enum Entry {
    /// A kernel thread's closure.
    Kernel(Box<dyn FnOnce() + Send>),
    /// User code at `ip`, with stack pointer `sp` and `arg` as its first
    /// argument, which [`Unstarted::start`] sets.
    User { ip: u64, sp: u64, arg: u64 },
}

/// Why a park returned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wake {
    Unparked,
    TimedOut,
}

/// Where the thread that switches away goes.
#[derive(Clone, Copy)]
enum Place {
    Ready,
    Blocked,
    Dead,
    Idle,
}

/// One CPU's scheduler.
struct Scheduler {
    running: Option<Box<Thread>>,
    ready: VecDeque<Box<Thread>>,
    /// Parked threads, in order of id.
    #[expect(
        clippy::vec_box,
        reason = "threads move between this and the ready queue as boxes, which never allocates"
    )]
    blocked: Vec<Box<Thread>>,
    /// The idle thread, or `None` while it runs.
    idle: Option<Box<Thread>>,
    /// Threads that exited, until the reaper frees them.
    #[expect(
        clippy::vec_box,
        reason = "unboxing a thread under the scheduler's lock would free the box there"
    )]
    dead: Vec<Box<Thread>>,
    /// The thread that just switched away, without its context, until the
    /// thread it switched to puts it in its place with that context.
    leaving: Option<(Box<Thread>, Place)>,
    /// Threads that aren't freed yet, the boot thread and the reaper
    /// included and the idle thread not: the room `ready`, `blocked` and
    /// `dead` each need.
    threads: usize,
    /// The reaper's id, which a thread's death unparks.
    reaper: u64,
}

impl Scheduler {
    /// A scheduler running `running`, with `idle` as its idle thread unless
    /// that is the one running, and `reaper` ready.
    fn new(running: Box<Thread>, idle: Option<Box<Thread>>, reaper: Box<Thread>) -> Self {
        // The boot thread counts, an AP's idle thread doesn't.
        let threads = if idle.is_some() { 2 } else { 1 };
        let mut scheduler = Self {
            running: Some(running),
            ready: VecDeque::with_capacity(threads),
            blocked: Vec::with_capacity(threads),
            idle,
            dead: Vec::with_capacity(threads),
            leaving: None,
            threads,
            reaper: reaper.id,
        };
        scheduler.ready.push_back(reaper);
        scheduler
    }
}

static SCHEDULERS: [Lock<Option<Scheduler>>; MAX_CPUS] =
    [const { Lock::new(Rank::SCHEDULER, None) }; MAX_CPUS];

/// Runs `f` on CPU `cpu`'s scheduler with interrupts off, or returns
/// `None` if that CPU has none. `f` must not allocate or free.
fn try_on<T>(cpu: usize, f: impl FnOnce(&mut Scheduler) -> T) -> Option<T> {
    Arch::without_interrupts(|| SCHEDULERS.get(cpu)?.lock().as_mut().map(f))
}

/// [`try_on`], for a CPU that has a scheduler.
fn on<T>(cpu: usize, f: impl FnOnce(&mut Scheduler) -> T) -> T {
    let Some(value) = try_on(cpu, f) else {
        panic!("CPU {cpu} has no scheduler");
    };
    value
}

/// [`on`] this CPU.
fn with<T>(f: impl FnOnce(&mut Scheduler) -> T) -> T {
    on(cpu::id(), f)
}

/// A stack for a thread the kernel makes for itself, `what`.
fn own_stack(what: &str) -> paging::Stack {
    let Some(stack) = paging::map_stack(STACK_SIZE) else {
        panic!("no frames for {what}'s stack on CPU {}", cpu::id());
    };
    stack
}

/// This CPU's reaper.
fn new_reaper() -> Box<Thread> {
    Box::new(Thread::new(
        cpu::id(),
        Some(Arch::new_context(own_stack("the reaper"), start)),
        Some(Entry::Kernel(Box::new(reaper))),
        None,
        None,
    ))
}

/// Makes the running code the boot thread, and makes the boot CPU's idle
/// thread and reaper. `kernel::init` calls this once, after `heap::init`
/// and before the timer starts.
pub fn init() {
    let idle = Box::new(Thread::new(
        0,
        Some(Arch::new_context(own_stack("the idle thread"), idle)),
        None,
        None,
        None,
    ));
    let reaper = new_reaper();
    let boot = Box::new(Thread::new(0, None, None, None, None));
    thisos_locks::running::<Arch>(boot.id);
    let scheduler = Scheduler::new(boot, Some(idle), reaper);
    Arch::without_interrupts(|| *SCHEDULERS[0].lock() = Some(scheduler));
    log::info!(
        "threads: boot, idle and reaper, {} KiB stacks with guard pages",
        STACK_SIZE >> 10
    );
}

/// Makes the running code this AP's idle thread, makes its reaper, and
/// runs the idle thread. An AP runs this once it has checked in, with
/// interrupts off.
pub fn run_ap() -> ! {
    panic::stop_if_reporting();
    let idle = Box::new(Thread::new(cpu::id(), None, None, None, None));
    let reaper = new_reaper();
    thisos_locks::running::<Arch>(idle.id);
    let scheduler = Scheduler::new(idle, None, reaper);
    Arch::without_interrupts(|| *SCHEDULERS[cpu::id()].lock() = Some(scheduler));
    run_idle()
}

/// Waits until CPUs 1 to `count` - 1 have their schedulers, which each
/// makes in [`run_ap`] after the boot CPU has seen it check in.
pub fn wait_for_cpus(count: usize) {
    while (1..count).any(|cpu| try_on(cpu, |_| ()).is_none()) {
        spin_loop();
    }
}

/// Starts a thread on this CPU that runs `run`, then exits. Panics if
/// kernel space or the frames run out.
pub fn spawn(run: impl FnOnce() + Send + 'static) {
    spawn_in(cpu::id(), None, None, run);
}

/// [`spawn`], on CPU `cpu`.
#[cfg(test)]
pub fn spawn_on(cpu: usize, run: impl FnOnce() + Send + 'static) {
    spawn_in(cpu, None, None, run);
}

/// Starts a kernel thread on CPU `cpu` that runs `run` in `space` and
/// `process`, which only tests give. Panics if kernel space or the frames
/// run out.
fn spawn_in(
    cpu: usize,
    space: Option<Arc<paging::Space>>,
    process: Option<Arc<Process>>,
    run: impl FnOnce() + Send + 'static,
) {
    match new_thread(cpu, space, process, Entry::Kernel(Box::new(run))) {
        Ok(thread) => thread.start(0),
        Err(e) => panic!("starting a kernel thread failed: {e:?}"),
    }
}

/// A thread of `process` that will run user code at `ip` in `space`, with
/// stack pointer `sp`, once [`Unstarted::start`] gives its argument, on the
/// CPU with the fewest threads. It ends through `user::trap`. Fails with
/// [`Error::NoMemory`] if kernel space, the frames or the heap run short.
pub fn new_user(
    process: Arc<Process>,
    space: Arc<paging::Space>,
    ip: u64,
    sp: u64,
) -> Result<Unstarted, Error> {
    let entry = Entry::User { ip, sp, arg: 0 };
    new_thread(least_loaded(), Some(space), Some(process), entry)
}

/// The CPU with the fewest threads, the lowest of those that tie. Another
/// CPU's count may change meanwhile, which only makes the choice older.
fn least_loaded() -> usize {
    (0..cpu::count())
        .filter_map(|cpu| try_on(cpu, |scheduler| (scheduler.threads, cpu)))
        .min()
        .map_or(0, |(_, cpu)| cpu)
}

/// A thread that is made but not ready: it holds its stack and room in its
/// CPU's queues, which dropping it gives back.
pub struct Unstarted(Option<Box<Thread>>);

impl Unstarted {
    /// Makes the thread ready, with `arg` as a user thread's first
    /// argument.
    pub fn start(mut self, arg: u64) {
        let Some(mut thread) = self.0.take() else {
            unreachable!("only start and drop take the thread");
        };
        if let Some(Entry::User { arg: user_arg, .. }) = &mut thread.entry {
            *user_arg = arg;
        }
        let cpu = cpu_of(thread.id);
        // new_thread reserved the room, so this doesn't allocate.
        let idle = on(cpu, |scheduler| {
            scheduler.ready.push_back(thread);
            scheduler.idle.is_none()
        });
        wake_if_idle(cpu, idle);
    }

    /// The CPU the thread will run on.
    #[cfg(test)]
    fn cpu(&self) -> Option<usize> {
        self.0.as_ref().map(|thread| cpu_of(thread.id))
    }
}

impl Drop for Unstarted {
    fn drop(&mut self) {
        if let Some(mut thread) = self.0.take() {
            free_stack(&mut thread);
            on(cpu_of(thread.id), |scheduler| scheduler.threads -= 1);
        }
    }
}

/// Wakes `cpu` if it is another CPU and `idle` says it was running its
/// idle thread when a thread became ready there.
fn wake_if_idle(cpu: usize, idle: bool) {
    if idle && cpu != cpu::id() {
        Arch::send_ipi(cpu, Ipi::Wake);
    }
}

/// Makes a thread of CPU `cpu` that runs `entry`, with its stack and room
/// in that CPU's queues.
fn new_thread(
    cpu: usize,
    space: Option<Arc<paging::Space>>,
    process: Option<Arc<Process>>,
    entry: Entry,
) -> Result<Unstarted, Error> {
    let thread = Thread::new(cpu, None, Some(entry), space, process);
    let mut thread = Box::try_new(thread).map_err(|_| Error::NoMemory)?;
    let stack = paging::map_stack(STACK_SIZE).ok_or(Error::NoMemory)?;
    thread.context = Some(Arch::new_context(stack, start));
    if let Err(e) = reserve(cpu) {
        free_stack(&mut thread);
        return Err(e);
    }
    Ok(Unstarted(Some(thread)))
}

/// Counts one more thread on CPU `cpu`, after giving its `ready`, `blocked`
/// and `dead` room for it.
fn reserve(cpu: usize) -> Result<(), Error> {
    loop {
        let short = on(cpu, |scheduler| {
            let need = scheduler.threads + 1;
            if scheduler.ready.capacity() < need
                || scheduler.blocked.capacity() < need
                || scheduler.dead.capacity() < need
            {
                return Some(need);
            }
            scheduler.threads = need;
            None
        });
        match short {
            Some(need) => grow(cpu, need)?,
            None => return Ok(()),
        }
    }
}

/// Gives CPU `cpu`'s `ready`, `blocked` and `dead` room for `need` threads
/// each. Another thread may grow them meanwhile, so the caller checks
/// again.
fn grow(cpu: usize, need: usize) -> Result<(), Error> {
    let mut ready = VecDeque::new();
    let mut blocked = Vec::new();
    let mut dead = Vec::new();
    if ready.try_reserve_exact(need).is_err()
        || blocked.try_reserve_exact(need).is_err()
        || dead.try_reserve_exact(need).is_err()
    {
        return Err(Error::NoMemory);
    }
    on(cpu, |scheduler| {
        if scheduler.ready.capacity() < need {
            ready.append(&mut scheduler.ready);
            mem::swap(&mut scheduler.ready, &mut ready);
        }
        if scheduler.blocked.capacity() < need {
            blocked.append(&mut scheduler.blocked);
            mem::swap(&mut scheduler.blocked, &mut blocked);
        }
        if scheduler.dead.capacity() < need {
            dead.append(&mut scheduler.dead);
            mem::swap(&mut scheduler.dead, &mut dead);
        }
    });
    // Whichever of each pair the scheduler doesn't keep is empty, and is
    // freed here with interrupts on.
    Ok(())
}

/// Unmaps the stack of `thread`, which isn't running.
fn free_stack(thread: &mut Thread) {
    if let Some(stack) = thread.context.take().and_then(Arch::into_stack) {
        paging::unmap_stack(stack);
    }
}

/// The running thread's id.
pub fn id() -> u64 {
    with(|scheduler| running(scheduler).id)
}

/// The running thread's process, or `None` for a kernel thread.
pub fn process() -> Option<Arc<Process>> {
    with(|scheduler| running(scheduler).process.clone())
}

/// The running thread's address space, or `None` for a kernel thread.
pub fn space() -> Option<Arc<paging::Space>> {
    with(|scheduler| running(scheduler).space.clone())
}

fn running(scheduler: &mut Scheduler) -> &mut Thread {
    let Some(running) = scheduler.running.as_mut() else {
        panic!("no thread is running");
    };
    running
}

/// Blocks the running thread until [`unpark`] names it, or returns at once
/// if an unpark named it since its last park. May return without either,
/// so callers check what they wait for again.
pub fn park() -> Wake {
    park_with(None)
}

/// [`park`], but also returns once [`timer::ticks`] reaches `deadline`.
pub fn park_until(deadline: u64) -> Wake {
    park_with(Some(deadline))
}

fn park_with(deadline: Option<u64>) -> Wake {
    Arch::without_interrupts(|| {
        with(|scheduler| {
            assert!(scheduler.idle.is_some(), "the idle thread parked");
            let thread = running(scheduler);
            if thread.wake.is_none() && deadline.is_some_and(|deadline| deadline <= timer::ticks())
            {
                thread.wake = Some(Wake::TimedOut);
            }
            thread.deadline = deadline;
        });
        switch(Place::Blocked);
        with(|scheduler| {
            let thread = running(scheduler);
            thread.deadline = None;
            let Some(wake) = thread.wake.take() else {
                panic!("a parked thread resumed with nothing to wake it");
            };
            wake
        })
    })
}

/// Blocks the running thread for at least `ticks` ticks.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "nothing outside the tests sleeps or yields")
)]
pub fn sleep(ticks: u64) {
    let deadline = timer::ticks() + ticks;
    while timer::ticks() < deadline {
        park_until(deadline);
    }
}

/// Readies the thread `id` names if it is parked, or makes its next park
/// return at once if it isn't. Does nothing if no live thread has that id.
pub fn unpark(id: u64) {
    let cpu = cpu_of(id);
    if let Some(idle) = try_on(cpu, |scheduler| scheduler.unpark(id)) {
        wake_if_idle(cpu, idle);
    }
}

impl Scheduler {
    /// [`unpark`] on this scheduler. Returns whether it readied a thread
    /// while the idle thread runs.
    fn unpark(&mut self, id: u64) -> bool {
        if let Ok(i) = self.blocked.binary_search_by_key(&id, |thread| thread.id) {
            let mut thread = self.blocked.remove(i);
            thread.wake = Some(Wake::Unparked);
            self.ready.push_back(thread);
            return self.idle.is_none();
        }
        // The leaving thread too, which another CPU can unpark between its
        // switch and the next thread's arrival.
        let thread = self
            .running
            .iter_mut()
            .chain(self.ready.iter_mut())
            .chain(self.leaving.iter_mut().map(|(thread, _)| thread))
            .find(|thread| thread.id == id);
        if let Some(thread) = thread {
            thread.wake = Some(Wake::Unparked);
        }
        false
    }
}

/// Lets the next ready thread run, if there is one.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "nothing outside the tests sleeps or yields")
)]
pub fn yield_now() {
    Arch::without_interrupts(|| switch(Place::Ready));
}

/// Hands the CPU to the next ready thread. The kernel's trap handler calls
/// this on every timer tick, with interrupts off.
pub fn tick() {
    with(|scheduler| {
        let now = timer::ticks();
        let due =
            |thread: &mut Box<Thread>| thread.deadline.is_some_and(|deadline| deadline <= now);
        // One pass, since removing each one shifts the rest every time.
        for mut thread in scheduler.blocked.extract_if(.., due) {
            thread.wake = Some(Wake::TimedOut);
            scheduler.ready.push_back(thread);
        }
    });
    switch(Place::Ready);
}

/// Switches to a ready thread if this CPU runs its idle thread. The
/// kernel's trap handler calls this on [`Ipi::Wake`], with interrupts off.
pub fn wake() {
    #[cfg(test)]
    tests::WAKES[cpu::id()].fetch_add(1, Ordering::Relaxed);
    if with(|scheduler| scheduler.idle.is_none()) {
        switch(Place::Ready);
    }
}

/// Ends the running thread. The reaper frees it, since nothing can free a
/// thread's stack while the thread runs on it.
pub fn exit() -> ! {
    Arch::without_interrupts(|| switch(Place::Dead));
    panic!("a thread ran after it exited");
}

/// Ends the running thread from a trap handler, with interrupts still off:
/// the trap may have come in on a stack the CPU starts the next such trap
/// on, so nothing may preempt the thread there. The reaper frees it.
pub fn exit_from_trap() -> ! {
    switch(Place::Dead);
    panic!("a thread ran after it exited");
}

/// Switches from the running thread, which goes to `place`, or back to its
/// slot if it is the idle thread, to the next ready thread, or to the idle
/// thread if none is ready and the running one is blocked or dead. Returns
/// at once if none is ready and the running one isn't blocked or dead, or
/// if it would block with a wake already set. Interrupts are off.
fn switch(place: Place) {
    let to = with(|scheduler| {
        let place = if scheduler.idle.is_none() {
            assert!(!matches!(place, Place::Dead), "the idle thread exited");
            Place::Idle
        } else {
            place
        };
        if matches!(place, Place::Blocked) && running(scheduler).wake.is_some() {
            return None;
        }
        let mut next = match (scheduler.ready.pop_front(), place) {
            (Some(next), _) => next,
            (None, Place::Blocked | Place::Dead) => scheduler.idle.take()?,
            (None, Place::Ready | Place::Idle) => return None,
        };
        let Some(to) = next.context.take() else {
            panic!("a thread that isn't running has no context");
        };
        let held = thisos_locks::switch_held::<Arch>(&next.held);
        thisos_locks::running::<Arch>(next.id);
        // The timer's handler runs this too, so the kernel's tables come
        // through the HAL call that takes no lock.
        match &next.space {
            Some(space) => space.activate(),
            None => Arch::activate_kernel_tables(),
        }
        let Some(mut leaving) = scheduler.running.replace(next) else {
            panic!("no thread was running");
        };
        leaving.held = held;
        scheduler.leaving = Some((leaving, place));
        Some(to)
    });
    if let Some(to) = to {
        arrived(Arch::switch(to));
    }
}

/// Puts the thread that switched to this one in its place, with `from`, the
/// context it left. Every thread runs this first when it resumes.
fn arrived(from: Context) {
    cpu::set_state(State::Kernel);
    with(|scheduler| {
        let Some((mut thread, place)) = scheduler.leaving.take() else {
            panic!("a thread resumed with none leaving");
        };
        thread.context = Some(from);
        match place {
            Place::Ready => scheduler.ready.push_back(thread),
            // Unparked while it left.
            Place::Blocked if thread.wake.is_some() => scheduler.ready.push_back(thread),
            Place::Blocked => {
                let (Ok(i) | Err(i)) = scheduler
                    .blocked
                    .binary_search_by_key(&thread.id, |blocked| blocked.id);
                scheduler.blocked.insert(i, thread);
            }
            Place::Dead => {
                scheduler.dead.push(thread);
                let reaper = scheduler.reaper;
                scheduler.unpark(reaper);
            }
            Place::Idle => scheduler.idle = Some(thread),
        }
    });
}

/// Frees every thread that exited on this CPU: unmaps its stack and frees
/// its frames, closes its process's handles and lets go of its address
/// space, then drops it. A process has one thread, so that thread's end is
/// the process's: it lets go of its space too, which the last holder
/// destroys, then its waiters see it ended. Interrupts are on, since the
/// page tables', the frames' and the heap's locks may be held by a thread a
/// tick preempted. Only the CPU's reaper calls this: a tick that preempts
/// it while it holds one of those locks puts it back in the ready queue,
/// where the idle thread would wait for every other thread to block.
fn reap() {
    while let Some(mut thread) = with(|scheduler| {
        let thread = scheduler.dead.pop()?;
        scheduler.threads -= 1;
        Some(thread)
    }) {
        free_stack(&mut thread);
        drop(thread.space.take());
        if let Some(process) = thread.process.take() {
            process.close_handles();
            process.reaped();
        }
        #[cfg(test)]
        tests::REAPED[cpu::id()].fetch_add(1, Ordering::Relaxed);
    }
}

/// A spawned thread's entry.
fn start(from: Context) -> ! {
    arrived(from);
    let entry = with(|scheduler| scheduler.running.as_mut()?.entry.take());
    Arch::enable_interrupts();
    match entry {
        Some(Entry::Kernel(run)) => run(),
        Some(Entry::User { ip, sp, arg }) => {
            cpu::set_state(State::User);
            Arch::enter_user(ip, sp, arg)
        }
        None => {}
    }
    exit()
}

/// What a reaper runs. A death that comes after its last look at the dead
/// threads leaves it a wake, so its park returns at once.
fn reaper() {
    loop {
        reap();
        park();
    }
}

/// The boot CPU's idle thread's entry.
fn idle(from: Context) -> ! {
    arrived(from);
    run_idle()
}

/// What an idle thread runs. It sleeps until the next interrupt, whose
/// tick or wake switches to a ready thread if there is one. It takes no
/// lock, since no tick puts it back in the ready queue to release one.
fn run_idle() -> ! {
    Arch::enable_interrupts();
    loop {
        cpu::set_state(State::Idle);
        Arch::wait_for_interrupt();
    }
}

/// Whether the thread `id` names is parked.
#[cfg(test)]
pub fn is_blocked(id: u64) -> bool {
    try_on(cpu_of(id), |scheduler| {
        scheduler
            .blocked
            .binary_search_by_key(&id, |thread| thread.id)
            .is_ok()
    })
    .unwrap_or(false)
}

/// Waits until each CPU's reaper has freed every thread there but the boot
/// thread and the logger's drainer on the boot CPU, and itself, and parked
/// again, so that the next test starts with those alone. A reaper takes a
/// thread off the dead ones before it frees it, so only its park says that
/// it is done.
#[cfg(test)]
pub fn wait_for_exits() {
    crate::testing::spin_until("the reapers to free every thread", || {
        (0..cpu::count()).all(|cpu| {
            let kept = if cpu == 0 { 3 } else { 1 };
            let (threads, dead, reaper) = on(cpu, |scheduler| {
                (scheduler.threads, scheduler.dead.len(), scheduler.reaper)
            });
            (threads, dead) == (kept, 0) && is_blocked(reaper)
        })
    });
}

#[cfg(test)]
mod tests {
    use core::hint::spin_loop;
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use thisos_abi::Rights;
    use thisos_hal::{BacktraceEnd, Probe, Trap};

    use super::*;
    use crate::frames;
    use crate::testing::{self, ExpectPanic, spin_until};

    static OUTER: Lock<()> = Lock::new(Rank::TEST_OUTER, ());
    static INNER: Lock<()> = Lock::new(Rank::TEST_INNER, ());

    /// The wake interrupts each CPU has taken.
    pub(super) static WAKES: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
    /// The threads each CPU's reaper has freed.
    pub(super) static REAPED: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

    #[test_case]
    const AN_ORDER_INVERSION_PANICS: ExpectPanic = ExpectPanic {
        name: "thisos_kernel::thread::tests::an_order_inversion_panics",
        run: || {
            let _inner = INNER.lock();
            let _outer = OUTER.lock();
        },
        message: "taking TEST_OUTER (1) while holding TEST_INNER (2): held [TEST_INNER]",
        after: None,
    };

    // The holder parks with the lock, so ticks switch between it and the
    // spinner, and each keeps its own held list and spin.
    #[test_case]
    const A_LOCK_HELD_PAST_THE_SPIN_LIMIT_PANICS: ExpectPanic = ExpectPanic {
        name: "thisos_kernel::thread::tests::a_lock_held_past_the_spin_limit_panics",
        run: || {
            static HELD: AtomicBool = AtomicBool::new(false);
            spawn(|| {
                let _held = OUTER.lock();
                HELD.store(true, Ordering::Release);
                loop {
                    park();
                }
            });
            spin_until("the holder to take the lock", || {
                HELD.load(Ordering::Acquire)
            });
            drop(OUTER.lock());
        },
        message: "spun 1 s for TEST_OUTER (1) on CPU 0, thread ",
        after: Some(|| thisos_locks::spinning(0) == Some(Rank::TEST_OUTER)),
    };

    // Neither the spawned threads nor this one yields, so all three make
    // progress only if ticks preempt them.
    #[test_case]
    fn ticks_preempt_threads_that_never_yield() {
        static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
        static STOP: AtomicBool = AtomicBool::new(false);
        for count in &COUNTS {
            spawn(move || {
                while !STOP.load(Ordering::Relaxed) {
                    count.fetch_add(1, Ordering::Relaxed);
                    spin_loop();
                }
            });
        }
        spin_until("both threads to run", || {
            COUNTS.iter().all(|count| count.load(Ordering::Relaxed) > 0)
        });
        STOP.store(true, Ordering::Relaxed);
        wait_for_exits();
    }

    // This thread and the spinner are always ready, so the idle thread
    // never runs, and nothing else exits after the thread that dies.
    #[test_case]
    fn a_dead_thread_is_freed_while_others_stay_ready() {
        static STOP: AtomicBool = AtomicBool::new(false);
        static STACK: AtomicU64 = AtomicU64::new(0);
        let run = || {
            STOP.store(false, Ordering::Relaxed);
            let free = frames::free_frames();
            let tables = paging::stack_table_frames();
            let Some(space) = paging::new_space() else {
                panic!("no free frame for a space");
            };
            let Ok(process) = Process::new(Arc::new(space)) else {
                panic!("no memory for a process");
            };
            assert!(process.insert_self(Rights::READ).is_ok());
            spawn(|| {
                while !STOP.load(Ordering::Relaxed) {
                    spin_loop();
                }
            });
            spawn_in(cpu::id(), None, Some(Arc::clone(&process)), || {
                STACK.store(Arch::guard_page() + 4096, Ordering::Relaxed);
            });
            // The thread holds the process, and so does the process's
            // handle to itself, until the thread is freed and its handles
            // close.
            spin_until("the dead thread's handles to close", || {
                Arc::strong_count(&process) == 1
            });
            STOP.store(true, Ordering::Relaxed);
            wait_for_exits();
            let stack = STACK.load(Ordering::Relaxed);
            assert_eq!(paging::translate(stack), None, "its stack at {stack:#x}");
            let tables = paging::stack_table_frames() - tables;
            assert_eq!(frames::free_frames(), free - tables);
        };
        // The first run grows the queues to fit, which they keep.
        run();
        let allocated = Arch::heap_stats().allocated;
        run();
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }

    // Interrupts stay off in this thread, so no tick lets the spawned one
    // run in its place.
    #[test_case]
    fn yield_runs_the_next_ready_thread() {
        static RAN: AtomicBool = AtomicBool::new(false);
        spawn(|| RAN.store(true, Ordering::Relaxed));
        Arch::without_interrupts(|| {
            spin_until("the thread to run", || {
                yield_now();
                RAN.load(Ordering::Relaxed)
            });
        });
        wait_for_exits();
    }

    #[test_case]
    fn a_threads_stack_is_guarded_and_its_backtrace_ends_at_its_entry() {
        static DONE: AtomicBool = AtomicBool::new(false);
        static GUARD: AtomicU64 = AtomicU64::new(0);
        static FRAMES: AtomicUsize = AtomicUsize::new(0);
        static END: Lock<Option<BacktraceEnd>> = Lock::new(Rank::TEST_OUTER, None);
        spawn(|| {
            GUARD.store(Arch::guard_page(), Ordering::Relaxed);
            let mut frames = 0;
            let end = Arch::backtrace(|_| frames += 1);
            FRAMES.store(frames, Ordering::Relaxed);
            *END.lock() = Some(end);
            DONE.store(true, Ordering::Relaxed);
        });
        spin_until("the thread to finish", || DONE.load(Ordering::Relaxed));
        let guard = GUARD.load(Ordering::Relaxed);
        assert_eq!(paging::translate(guard), None, "guard page {guard:#x}");
        assert_eq!(*END.lock(), Some(BacktraceEnd::Done));
        let frames = FRAMES.load(Ordering::Relaxed);
        assert!(frames >= 2, "{frames} frames");
        wait_for_exits();
    }

    // Unparks the middle one of three parked threads first, so the blocked
    // set has to find it between the others and keep them parked.
    #[test_case]
    fn unpark_wakes_only_the_thread_it_names() {
        static IDS: [AtomicU64; 3] = [const { AtomicU64::new(u64::MAX) }; 3];
        static WOKEN: [AtomicBool; 3] = [const { AtomicBool::new(false) }; 3];
        let run = || {
            for (id, woken) in IDS.iter().zip(&WOKEN) {
                woken.store(false, Ordering::Relaxed);
                spawn(move || {
                    id.store(super::id(), Ordering::Relaxed);
                    assert_eq!(park(), Wake::Unparked);
                    woken.store(true, Ordering::Relaxed);
                });
            }
            spin_until("all three threads to park", || {
                IDS.iter().all(|id| is_blocked(id.load(Ordering::Relaxed)))
            });
            unpark(IDS[1].load(Ordering::Relaxed));
            spin_until("the middle thread to wake", || {
                WOKEN[1].load(Ordering::Relaxed)
            });
            assert!(!WOKEN[0].load(Ordering::Relaxed));
            assert!(!WOKEN[2].load(Ordering::Relaxed));
            for id in [&IDS[0], &IDS[2]] {
                unpark(id.load(Ordering::Relaxed));
            }
            wait_for_exits();
            assert!(WOKEN.iter().all(|woken| woken.load(Ordering::Relaxed)));
        };
        // The first run grows the queues to fit, which they keep.
        run();
        let allocated = Arch::heap_stats().allocated;
        run();
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }

    // The thread is ready, not parked, when the unpark comes, which
    // interrupts being off in this thread guarantees.
    #[test_case]
    fn an_unpark_before_the_park_isnt_lost() {
        static WAKE: Lock<Option<Wake>> = Lock::new(Rank::TEST_OUTER, None);
        Arch::without_interrupts(|| {
            spawn(|| *WAKE.lock() = Some(park()));
            let Some(id) = with(|scheduler| scheduler.ready.back().map(|thread| thread.id)) else {
                panic!("the spawned thread isn't ready");
            };
            unpark(id);
        });
        spin_until("the thread to return from its park", || {
            WAKE.lock().is_some()
        });
        assert_eq!(*WAKE.lock(), Some(Wake::Unparked));
        wait_for_exits();
    }

    // The boot thread parks here, so the idle thread runs until the tick
    // that wakes it.
    #[test_case]
    fn park_until_and_sleep_wait_for_their_deadline() {
        let deadline = timer::ticks() + 5;
        assert_eq!(park_until(deadline), Wake::TimedOut);
        assert!(timer::ticks() >= deadline);
        assert_eq!(park_until(0), Wake::TimedOut);
        let start = timer::ticks();
        sleep(10);
        assert!(timer::ticks() >= start + 10);
    }

    // The outer two of three parked threads time out on the same tick,
    // so that tick has to take them from either side of the middle one,
    // which has no deadline and stays parked.
    #[test_case]
    fn a_tick_wakes_only_the_threads_whose_deadline_passed() {
        static DEADLINE: AtomicU64 = AtomicU64::new(0);
        static IDS: [AtomicU64; 3] = [const { AtomicU64::new(u64::MAX) }; 3];
        static WAKES: Lock<[Option<Wake>; 3]> = Lock::new(Rank::TEST_OUTER, [None; 3]);
        // Far more than three spawns and parks take under TCG.
        DEADLINE.store(timer::ticks() + 50, Ordering::Relaxed);
        for (i, id) in IDS.iter().enumerate() {
            spawn(move || {
                id.store(super::id(), Ordering::Relaxed);
                let wake = if i == 1 {
                    park()
                } else {
                    park_until(DEADLINE.load(Ordering::Relaxed))
                };
                WAKES.lock()[i] = Some(wake);
            });
        }
        spin_until("all three threads to park", || {
            IDS.iter().all(|id| is_blocked(id.load(Ordering::Relaxed)))
        });
        assert!(
            timer::ticks() < DEADLINE.load(Ordering::Relaxed),
            "the deadline passed before all three parked"
        );
        spin_until("the outer threads to time out", || {
            let wakes = WAKES.lock();
            wakes[0].is_some() && wakes[2].is_some()
        });
        assert!(is_blocked(IDS[1].load(Ordering::Relaxed)));
        unpark(IDS[1].load(Ordering::Relaxed));
        wait_for_exits();
        assert_eq!(
            *WAKES.lock(),
            [
                Some(Wake::TimedOut),
                Some(Wake::Unparked),
                Some(Wake::TimedOut)
            ]
        );
    }

    // Shows the double fault's stack switch works from a thread's stack as
    // from the boot stack, and that the thread carries on afterwards.
    #[test_case]
    fn a_threads_stack_overflow_double_faults() {
        static DONE: AtomicBool = AtomicBool::new(false);
        spawn(|| {
            testing::expect_trap(|trap| trap == Trap::DoubleFault, Probe::OverflowStack);
            DONE.store(true, Ordering::Relaxed);
        });
        spin_until("the thread's overflow", || DONE.load(Ordering::Relaxed));
        wait_for_exits();
    }

    // The boot CPU has the boot thread, the logger's drainer and its reaper,
    // and each AP only its reaper, so user threads made one after another
    // go to the APs in turn.
    #[test_case]
    fn user_threads_go_to_the_cpus_with_the_fewest_threads() {
        if !testing::several_cpus() {
            return;
        }
        let Some(space) = paging::new_space() else {
            panic!("no free frame for a space");
        };
        let space = Arc::new(space);
        let Ok(process) = Process::new(Arc::clone(&space)) else {
            panic!("no memory for a process");
        };
        let threads: Vec<_> = (1..cpu::count())
            .map(
                |_| match new_user(Arc::clone(&process), Arc::clone(&space), 0, 0) {
                    Ok(thread) => thread,
                    Err(e) => panic!("making a user thread failed: {e:?}"),
                },
            )
            .collect();
        let cpus: Vec<_> = threads.iter().filter_map(Unstarted::cpu).collect();
        assert_eq!(cpus, (1..cpu::count()).collect::<Vec<_>>());
    }

    // CPU 1 runs only its reaper, which is parked, so it idles until the
    // unpark. A tick there may run the thread before the IPI arrives, but
    // the IPI has to arrive all the same.
    #[test_case]
    fn an_unpark_wakes_an_idle_cpu_with_an_ipi() {
        static ID: AtomicU64 = AtomicU64::new(u64::MAX);
        static WAKE: Lock<Option<Wake>> = Lock::new(Rank::TEST_OUTER, None);
        if !testing::several_cpus() {
            return;
        }
        *WAKE.lock() = None;
        spawn_on(1, || {
            ID.store(super::id(), Ordering::Relaxed);
            let wake = park();
            *WAKE.lock() = Some(wake);
        });
        spin_until("the thread on CPU 1 to park and CPU 1 to idle", || {
            is_blocked(ID.load(Ordering::Relaxed)) && cpu::state(1) == State::Idle
        });
        let wakes = WAKES[1].load(Ordering::Relaxed);
        unpark(ID.load(Ordering::Relaxed));
        spin_until("the thread on CPU 1 to wake", || WAKE.lock().is_some());
        assert_eq!(*WAKE.lock(), Some(Wake::Unparked));
        spin_until("CPU 1 to take a wake interrupt", || {
            WAKES[1].load(Ordering::Relaxed) > wakes
        });
        wait_for_exits();
    }

    // The first run grows CPU 1's queues, which may grow the heap, so only
    // the second counts frames.
    #[test_case]
    fn a_thread_that_dies_on_an_ap_is_freed_there() {
        static STACK: AtomicU64 = AtomicU64::new(0);
        if !testing::several_cpus() {
            return;
        }
        for run in 1..=2 {
            let free = frames::free_frames();
            let tables = paging::stack_table_frames();
            let reaped = REAPED[1].load(Ordering::Relaxed);
            spawn_on(1, || {
                STACK.store(Arch::guard_page() + 4096, Ordering::Relaxed);
            });
            wait_for_exits();
            assert_eq!(REAPED[1].load(Ordering::Relaxed), reaped + 1, "run {run}");
            let stack = STACK.load(Ordering::Relaxed);
            assert_eq!(paging::translate(stack), None, "its stack at {stack:#x}");
            if run == 2 {
                let tables = paging::stack_table_frames() - tables;
                assert_eq!(frames::free_frames(), free - tables);
            }
        }
    }
}
