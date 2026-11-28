//! The panic handler: stops every other CPU, then reports the panic with a
//! backtrace, and a line for each other CPU, to both log sinks, then
//! halts. In a test build it ends the test run with failure instead.
//! Traps the kernel can't handle end the same way. Both run with
//! interrupts off from their first instruction, and the other CPUs take
//! the stop as an NMI, which interrupts code with interrupts off too, so
//! nothing else runs during the report, nor panics and halts before it is
//! out. A CPU that panics while another reports joins the stopped ones.

use core::fmt;
use core::hint::spin_loop;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use thisos_hal::{BacktraceEnd, Cpu, Ipi, MAX_FRAMES, Platform, TestEnd, Trap, TrapFrame};
use thisos_locks::MAX_CPUS;

use crate::{Arch, cpu, logger};

/// The CPU that stopped the others to report, or [`NOBODY`].
static REPORTER: AtomicUsize = AtomicUsize::new(NOBODY);

const NOBODY: usize = usize::MAX;

/// Each CPU's stop: where the code it stopped in was.
static STOPS: [Stop; MAX_CPUS] = [const { Stop::new() }; MAX_CPUS];

/// The return addresses a stopped CPU's line shows after its IP.
const STOP_FRAMES: usize = 8;

/// What a [`Stop`] holds, in its `state`.
const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const STOPPED: u8 = 2;
const PANICKED: u8 = 3;

struct Stop {
    state: AtomicU8,
    len: AtomicUsize,
    addresses: [AtomicU64; STOP_FRAMES + 1],
}

impl Stop {
    const fn new() -> Self {
        Stop {
            state: AtomicU8::new(EMPTY),
            len: AtomicUsize::new(0),
            addresses: [const { AtomicU64::new(0) }; STOP_FRAMES + 1],
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    Arch::without_interrupts(|| {
        let mut backtrace = Backtrace::new();
        // Walked once, so both sinks show the same frames.
        backtrace.end = Arch::backtrace(|address| backtrace.frames.push(address));
        enter(&backtrace.frames);
        let report = Report(&backtrace);
        match info.location() {
            Some(location) => logger::panic(
                format_args!("PANIC at {location}: {}", info.message()),
                &report,
            ),
            None => logger::panic(format_args!("PANIC: {}", info.message()), &report),
        }
        #[cfg(test)]
        crate::testing::panicked(&info.message());
        #[cfg(not(test))]
        Arch::halt();
    })
}

/// Reports `trap` as a panic, with the registers in `frame` and a backtrace
/// of the trapped code, then ends as a panic does. Not through `panic!`,
/// whose backtrace would start in the handler, and for a trap on a stack of
/// its own stop there at once.
pub fn trap(trap: Trap, frame: &<Arch as Platform>::TrapFrame) -> ! {
    trapped(format_args!("{trap}"), frame)
}

/// As [`trap`], with `head` in place of the trap's name.
pub fn trapped(head: fmt::Arguments, frame: &<Arch as Platform>::TrapFrame) -> ! {
    Arch::without_interrupts(|| {
        let mut backtrace = Backtrace::new();
        backtrace.end = frame.backtrace(|address| backtrace.frames.push(address));
        enter(&backtrace.frames);
        logger::panic(
            format_args!("PANIC: {head}"),
            &format_args!("{frame}{}", Report(&backtrace)),
        );
        end()
    })
}

/// Stops every other CPU before the test run's summary, as a panic does,
/// unless this CPU has already. Returns whether one of them had panicked,
/// in the moment before its stop, which this reports with the other CPUs'
/// lines. Halts if another CPU is reporting a panic, as one stopped by it.
#[cfg(test)]
pub fn stop_others() -> bool {
    match claim() {
        Claim::First | Claim::Again => {
            let me = cpu::id();
            let panicked = (0..Arch::cpus_running())
                .any(|other| other != me && STOPS[other].state.load(Ordering::Acquire) == PANICKED);
            if panicked {
                logger::panic(format_args!("another CPU panicked"), &Others);
            }
            panicked
        }
        Claim::Other => {
            record(STOPPED, |push| {
                Arch::backtrace(push);
            });
            Arch::halt()
        }
    }
}

/// Halts as a stopped CPU if a panic is being reported, which may have
/// read the count of running CPUs before this one counted itself. An AP's
/// entry calls it first, with interrupts off.
pub fn stop_if_reporting() {
    // SeqCst, pairing with claim's: an AP counts itself before this, so
    // the reporter either counted it, and sends it a stop, or claimed
    // first, which this finds.
    if REPORTER.load(Ordering::SeqCst) != NOBODY {
        record(STOPPED, |push| {
            Arch::backtrace(push);
        });
        Arch::halt();
    }
}

/// Records where `frame` stopped and halts, the kernel's handler of
/// [`Trap::Stop`]. It takes no lock: the CPU may have stopped holding any.
pub fn stopped(frame: &<Arch as Platform>::TrapFrame) -> ! {
    record(STOPPED, |push| {
        frame.backtrace(push);
    });
    Arch::halt()
}

/// Makes this CPU the reporter and stops the others, or halts as a stopped
/// CPU, its `frames` recorded as a panic's, if another CPU reports.
fn enter(frames: &Frames) {
    match claim() {
        Claim::First => {}
        // A panic while reporting one would recurse, and the first report
        // is the one worth reading.
        Claim::Again => stop(),
        Claim::Other => {
            record(PANICKED, |push| {
                frames.addresses[..frames.len].iter().for_each(|&a| push(a));
            });
            Arch::halt()
        }
    }
}

enum Claim {
    /// This CPU is the reporter now, and the others have stopped.
    First,
    /// This CPU was the reporter already.
    Again,
    /// Another CPU is.
    Other,
}

fn claim() -> Claim {
    let me = cpu::id();
    // SeqCst, before stop_cpus reads the count; see stop_if_reporting.
    match REPORTER.compare_exchange(NOBODY, me, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => {
            stop_cpus(me);
            Claim::First
        }
        Err(reporter) if reporter == me => Claim::Again,
        Err(_) => Claim::Other,
    }
}

/// Sends every CPU but `me` a stop, and waits up to 100 ms for each to
/// record where it stopped. One that doesn't is reported as such: it may
/// be in an NMI's handler already, where the stop waits for that to end.
/// The CPUs are those that have started, not `cpu::count`, which counts
/// the APs only once they all run threads: a panic during their start
/// stops those up so far, and one that checks in later stops itself in
/// `stop_if_reporting`.
fn stop_cpus(me: usize) {
    let others = || (0..Arch::cpus_running()).filter(move |&other| other != me);
    for other in others() {
        Arch::send_ipi(other, Ipi::Stop);
    }
    // Without a rate no AP runs, so there is nothing to wait for.
    let limit = <Arch as thisos_locks::Platform>::counter_hz().map_or(0, |hz| hz / 10);
    let start = <Arch as thisos_locks::Platform>::counter();
    while others().any(|other| STOPS[other].state.load(Ordering::Acquire) < STOPPED)
        && <Arch as thisos_locks::Platform>::counter().wrapping_sub(start) < limit
    {
        spin_loop();
    }
}

/// Records this CPU's stop as `kind`, with the addresses `walk` pushes, of
/// which the first [`STOP_FRAMES`] + 1 are kept. Records nothing if the CPU
/// has a stop already: a CPU that panicked and halted takes the reporter's
/// stop later.
fn record(kind: u8, walk: impl FnOnce(&mut dyn FnMut(u64))) {
    let stop = &STOPS[cpu::id()];
    if stop
        .state
        .compare_exchange(EMPTY, WRITING, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let mut len = 0;
    walk(&mut |address| {
        if let Some(slot) = stop.addresses.get(len) {
            slot.store(address, Ordering::Relaxed);
            len += 1;
        }
    });
    stop.len.store(len, Ordering::Relaxed);
    // Release, pairing with stop_cpus and Others, for the addresses.
    stop.state.store(kind, Ordering::Release);
}

fn end() -> ! {
    #[cfg(test)]
    crate::testing::fail();
    #[cfg(not(test))]
    Arch::halt();
}

fn stop() -> ! {
    if cfg!(test) {
        Arch::exit_qemu(TestEnd::Failed);
    }
    Arch::halt()
}

/// A panic's backtrace, then a line for each other CPU.
struct Report<'a>(&'a Backtrace);

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.0, Others)
    }
}

/// A line for each CPU but the reporter: the thread it ran and the
/// addresses its stop recorded, or that it didn't stop.
struct Others;

impl fmt::Display for Others {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reporter = REPORTER.load(Ordering::Relaxed);
        let mut others = (0..Arch::cpus_running()).filter(|&other| other != reporter);
        let Some(first) = others.next() else {
            return Ok(());
        };
        writeln!(f, "other CPUs (link addresses):")?;
        for other in core::iter::once(first).chain(others) {
            let stop = &STOPS[other];
            let how = match stop.state.load(Ordering::Acquire) {
                STOPPED => "stopped",
                PANICKED => "panicked too",
                _ => {
                    writeln!(f, "  CPU {other} didn't stop")?;
                    continue;
                }
            };
            write!(f, "  CPU {other} {how} in ")?;
            match thisos_locks::thread(other) {
                Some(thread) => write!(f, "thread {thread}:")?,
                None => write!(f, "no thread:")?,
            }
            let len = stop.len.load(Ordering::Relaxed);
            for address in &stop.addresses[..len] {
                write!(f, " {:#x}", address.load(Ordering::Relaxed))?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

struct Backtrace {
    frames: Frames,
    end: BacktraceEnd,
}

impl Backtrace {
    fn new() -> Self {
        Backtrace {
            frames: Frames {
                addresses: [0; MAX_FRAMES],
                len: 0,
            },
            end: BacktraceEnd::Done,
        }
    }
}

/// The first [`MAX_FRAMES`] addresses pushed. A trap's backtrace reports
/// one more, and the last is dropped.
struct Frames {
    addresses: [u64; MAX_FRAMES],
    len: usize,
}

impl Frames {
    fn push(&mut self, address: u64) {
        if let Some(slot) = self.addresses.get_mut(self.len) {
            *slot = address;
            self.len += 1;
        }
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "backtrace (link addresses):")?;
        for (i, address) in self
            .frames
            .addresses
            .iter()
            .take(self.frames.len)
            .enumerate()
        {
            writeln!(f, "  #{i} {address:#x}")?;
        }
        match self.end {
            BacktraceEnd::Done => Ok(()),
            BacktraceEnd::TooDeep => writeln!(f, "  stopped: after {MAX_FRAMES} frames"),
            BacktraceEnd::OutsideStack(fp) => {
                writeln!(f, "  stopped: frame pointer {fp:#x} outside the stack")
            }
            BacktraceEnd::Misaligned(fp) => {
                writeln!(f, "  stopped: frame pointer {fp:#x} misaligned")
            }
            BacktraceEnd::NotIncreasing(fp) => {
                writeln!(f, "  stopped: frame pointer {fp:#x} not above the last")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::hint::{black_box, spin_loop};
    use core::sync::atomic::{AtomicBool, Ordering};

    use thisos_hal::{BacktraceEnd, Cpu, Platform};

    use super::{REPORTER, STOPPED, STOPS};
    use crate::testing::{self, ExpectPanic};
    use crate::{Arch, cpu, thread};

    // Interrupts are on here, so the runner's check that no tick arrives
    // during the panic finds whether the handler turned them off.
    #[test_case]
    const A_PANIC_TURNS_INTERRUPTS_OFF: ExpectPanic = ExpectPanic {
        name: "thisos_kernel::panic::tests::a_panic_turns_interrupts_off",
        run: || panic!("panicked with interrupts on"),
        message: "panicked with interrupts on",
        after: None,
    };

    // CPU 1 spins with interrupts off, so only an NMI stops it.
    #[test_case]
    const A_PANIC_STOPS_EVERY_OTHER_CPU: ExpectPanic = ExpectPanic {
        name: "thisos_kernel::panic::tests::a_panic_stops_every_other_cpu",
        run: || {
            static SPINNING: AtomicBool = AtomicBool::new(false);
            if cpu::count() > 1 {
                thread::spawn_on(1, || {
                    Arch::without_interrupts(|| {
                        SPINNING.store(true, Ordering::Release);
                        while SPINNING.load(Ordering::Relaxed) {
                            spin_loop();
                        }
                    });
                });
                testing::spin_until("CPU 1 spinning", || SPINNING.load(Ordering::Acquire));
            }
            panic!("stopping the other CPUs")
        },
        message: "stopping the other CPUs",
        after: Some(every_other_cpu_stopped),
    };

    fn every_other_cpu_stopped() -> bool {
        let reporter = REPORTER.load(Ordering::Relaxed);
        (0..Arch::cpus_running())
            .filter(|&other| other != reporter)
            .all(|other| {
                STOPS[other].state.load(Ordering::Acquire) == STOPPED
                    && STOPS[other].len.load(Ordering::Relaxed) > 0
            })
    }

    #[test_case]
    fn backtrace_reaches_the_entry() {
        let (frames, end) = outer();
        assert_eq!(end, BacktraceEnd::Done);
        assert!(frames >= 4, "{frames} frames");
    }

    // black_box keeps each call from becoming a tail call, which would
    // reuse the caller's frame and drop it from the chain.
    #[inline(never)]
    fn outer() -> (usize, BacktraceEnd) {
        black_box(middle())
    }

    #[inline(never)]
    fn middle() -> (usize, BacktraceEnd) {
        black_box(inner())
    }

    #[inline(never)]
    fn inner() -> (usize, BacktraceEnd) {
        let mut frames = 0;
        let end = Arch::backtrace(|_| frames += 1);
        (frames, end)
    }
}
