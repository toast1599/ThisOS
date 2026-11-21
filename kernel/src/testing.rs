//! The test runner for the kernel's `#[test_case]` functions. It ends on a
//! summary of the results with a large PASS or FAIL, which is all the
//! laptop shows, then exits QEMU. Serial gets every test's result; the
//! console gets counts, so that a failure's report stays on the laptop's
//! screen above them.

use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use spin::Mutex;
use thisos_hal::{Cpu, PANIC_TEST_FILE, Platform, Probe, TestEnd, Trap, TrapFrame};

use crate::{Arch, logger, panic, timer, user};

/// The tests and the index of the one running, or of none once all passed,
/// for the panic handler's summary. `None` until the tests start.
static RUN: Mutex<Option<Run>> = Mutex::new(None);

#[derive(Clone, Copy)]
struct Run {
    tests: &'static [&'static dyn Test],
    current: usize,
    /// The test that expects a panic that this boot runs, after all the
    /// others.
    last: Option<usize>,
    /// Whether this boot runs that test alone, as every boot but the first
    /// does.
    alone: bool,
    /// The tick the running test started at.
    started: u64,
}

/// How many tests this boot has started.
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// How long a test may run, in seconds, before the watchdog stops it.
const LIMIT: u64 = 10;

/// What ends the probe the running test runs, set while it runs.
static EXPECTED: Mutex<Option<Expectation>> = Mutex::new(None);

/// Whether the trap handler that stopped the last [`expect_timeout`]
/// probe found user access blocked.
static BLOCKED_AT_TIMEOUT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
enum Expectation {
    /// A trap the function accepts.
    Trap(fn(Trap) -> bool),
    /// The first timer interrupt at or after this tick.
    Timeout(u64),
}

/// A `#[test_case]` function, or an [`ExpectPanic`].
pub trait Test: Sync {
    /// The function's path.
    fn name(&self) -> &'static str;
    /// Calls the function.
    fn run(&self);
    /// What the message of the panic the test passes by contains, for a
    /// test that passes by panicking.
    fn expected_panic(&self) -> Option<&'static str> {
        None
    }
    /// What else must hold once that panic is reported.
    fn after_panic(&self) -> Option<fn() -> bool> {
        None
    }
}

/// A `#[test_case]` const for a test that passes by panicking with a
/// message that contains `message`, with no tick arriving while the panic
/// handler runs. A panic ends the boot, so each runs on
/// a boot of its own: the first after every other test, and each later one
/// alone.
pub struct ExpectPanic {
    /// The test's path, as a function's would read.
    pub name: &'static str,
    /// The test.
    pub run: fn(),
    /// Part of the panic message it expects.
    pub message: &'static str,
    /// What else must hold once the panic is reported, for the test to
    /// pass.
    pub after: Option<fn() -> bool>,
}

impl Test for ExpectPanic {
    fn name(&self) -> &'static str {
        self.name
    }

    fn run(&self) {
        (self.run)();
    }

    fn expected_panic(&self) -> Option<&'static str> {
        Some(self.message)
    }

    fn after_panic(&self) -> Option<fn() -> bool> {
        self.after
    }
}

impl<T: Fn() + Sync> Test for T {
    fn name(&self) -> &'static str {
        core::any::type_name::<T>()
    }

    fn run(&self) {
        self();
    }
}

/// Runs the tests in order, then reports success. Of the tests that expect
/// a panic it runs one, last: the one [`PANIC_TEST_FILE`] names, or the
/// first, and for any but the first it runs no other test. A failing test
/// panics, and the panic handler calls [`panicked`].
pub fn run(tests: &'static [&'static dyn Test]) -> ! {
    let selected = panic_test();
    let last = (0..tests.len())
        .filter(|&i| expects_panic(tests, i))
        .nth(selected);
    assert!(
        selected == 0 || last.is_some(),
        "there is no panic test {selected}"
    );
    let alone = selected > 0;
    let order = (0..tests.len())
        .filter(|&i| !alone && !expects_panic(tests, i))
        .chain(last);
    for (current, test) in order.filter_map(|i| Some((i, *tests.get(i)?))) {
        *RUN.lock() = Some(Run {
            tests,
            current,
            last,
            alone,
            started: timer::ticks(),
        });
        log::info!("test {}", test.name());
        STARTED.fetch_add(1, Ordering::Relaxed);
        test.run();
        assert!(
            Some(current) != last,
            "the test returned without the panic it expects"
        );
    }
    // Before the log line, so that a panic in it doesn't blame the last test.
    *RUN.lock() = Some(Run {
        tests,
        current: tests.len(),
        last,
        alone,
        started: timer::ticks(),
    });
    log::info!("{} tests passed", tests.len());
    finish(true)
}

/// Which test that expects a panic [`PANIC_TEST_FILE`] names, or 0 if the
/// ramdisk has no such file.
fn panic_test() -> usize {
    let Some(file) = user::find_in_ramdisk(PANIC_TEST_FILE) else {
        return 0;
    };
    let Some(selected) = str::from_utf8(file).ok().and_then(|s| s.parse().ok()) else {
        panic!("the ramdisk's {PANIC_TEST_FILE} isn't a number");
    };
    selected
}

/// Whether test `i` expects a panic.
fn expects_panic(tests: &[&dyn Test], i: usize) -> bool {
    tests
        .get(i)
        .is_some_and(|test| test.expected_panic().is_some())
}

/// Names the failed test after its panic report, then reports failure.
pub fn fail() -> ! {
    finish(false)
}

/// Ends the run after a panic's report: with success when the running test
/// expects a panic whose message contains what it names, its `after` check
/// holds if it has one, and no tick arrives, which also makes it the last,
/// and with failure otherwise.
pub fn panicked(message: &dyn fmt::Display) -> ! {
    let passed = RUN.try_lock().is_some_and(|mut run| {
        let Some(run) = run.as_mut() else {
            return false;
        };
        let Some(test) = run.tests.get(run.current) else {
            return false;
        };
        let passed = test
            .expected_panic()
            .is_some_and(|expected| contains(message, expected))
            && test.after_panic().is_none_or(|after| after())
            && quiet();
        if passed {
            run.current = run.tests.len();
        }
        passed
    });
    finish(passed)
}

/// Whether no tick arrives within [`MAX_SPINS`] spins, as none should
/// while the panic handler runs. Says so if one does.
fn quiet() -> bool {
    let start = timer::ticks();
    let ticked = (0..MAX_SPINS).any(|_| {
        spin_loop();
        timer::ticks() != start
    });
    if ticked {
        logger::panic(format_args!("a tick arrived during the panic"), &"");
    }
    !ticked
}

/// Whether `message`, as far as its first 256 bytes, contains `part`.
/// Without the heap, which a panic may have left locked.
fn contains(message: &dyn fmt::Display, part: &str) -> bool {
    struct Prefix {
        bytes: [u8; 256],
        len: usize,
    }
    impl Write for Prefix {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for &byte in s.as_bytes() {
                if let Some(slot) = self.bytes.get_mut(self.len) {
                    *slot = byte;
                    self.len += 1;
                }
            }
            Ok(())
        }
    }
    let mut prefix = Prefix {
        bytes: [0; 256],
        len: 0,
    };
    let _ = write!(prefix, "{message}");
    let written = prefix.bytes.get(..prefix.len).unwrap_or_default();
    part.is_empty()
        || written
            .windows(part.len())
            .any(|window| window == part.as_bytes())
}

/// Far more spins than 100 ms takes under TCG, and well under the
/// watchdog's 10 s on the test laptop, where a spin costs 83 to 123 TSC
/// cycles at 2995 MHz: 3.7 to 5.5 s.
const MAX_SPINS: u64 = 1 << 27;

/// Spins until `done` holds, and panics after [`MAX_SPINS`].
pub fn spin_until(what: &str, done: impl Fn() -> bool) {
    for _ in 0..MAX_SPINS {
        if done() {
            return;
        }
        spin_loop();
    }
    panic!("{what} didn't happen within {MAX_SPINS} spins");
}

/// Takes every free byte of the kernel heap, in buffers that halve from
/// 1 MiB down to one byte, and returns them; dropping them gives the heap
/// back. The largest goes first, so each size after it fits at most a few
/// times, and the list of buffers stays short.
pub fn fill_heap() -> Vec<Vec<u8>> {
    let mut filler = Vec::new();
    assert!(
        filler.try_reserve_exact(256).is_ok(),
        "no room for the list"
    );
    let mut size = 1 << 20;
    while size > 0 {
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(size).is_err() {
            size /= 2;
            continue;
        }
        assert!(filler.len() < filler.capacity(), "the heap is in pieces");
        filler.push(buffer);
    }
    filler
}

/// Runs `probe`, and returns once it traps with a trap `expected` accepts.
/// Any other trap fails the test as usual, and a probe that never traps
/// runs into the watchdog, except one that runs with interrupts off.
pub fn expect_trap(expected: fn(Trap) -> bool, probe: Probe) {
    catch(Expectation::Trap(expected), probe);
}

/// Runs `probe`, and returns once it has run for `seconds`, as the watchdog
/// would stop a hung test.
pub fn expect_timeout(seconds: u64, probe: Probe) {
    let at = timer::ticks() + seconds * u64::from(timer::HZ);
    BLOCKED_AT_TIMEOUT.store(false, Ordering::Relaxed);
    catch(Expectation::Timeout(at), probe);
}

/// Whether the trap handler that stopped the last [`expect_timeout`] probe
/// found user access blocked.
pub fn user_access_blocked_at_timeout() -> bool {
    BLOCKED_AT_TIMEOUT.load(Ordering::Relaxed)
}

fn catch(expectation: Expectation, probe: Probe) {
    *EXPECTED.lock() = Some(expectation);
    Arch::catch(probe);
    *EXPECTED.lock() = None;
}

/// Abandons the running probe when `trap` is what its test expects, and
/// returns whether it did. The kernel's trap handler calls this first.
/// Through `try_lock`, because a timer interrupt can come while `catch`
/// holds the lock; no probe runs then.
pub(crate) fn caught(trap: Trap, frame: &mut <Arch as Platform>::TrapFrame) -> bool {
    let Some(expected) = EXPECTED.try_lock().and_then(|expected| *expected) else {
        return false;
    };
    let matches = match expected {
        Expectation::Trap(accepts) => accepts(trap),
        Expectation::Timeout(at) => {
            let due = trap == Trap::Timer && timer::ticks() >= at;
            if due {
                BLOCKED_AT_TIMEOUT.store(Arch::user_access_blocked(), Ordering::Relaxed);
            }
            due
        }
    };
    matches && frame.abandon()
}

/// Stops the running test with a report of where it was, once it has run
/// past the watchdog's limit. The kernel's trap handler calls this on every
/// timer interrupt.
pub(crate) fn watchdog(frame: &<Arch as Platform>::TrapFrame) {
    if let Some(test) = overdue() {
        panic::trapped(
            format_args!("test {} timed out after {LIMIT} s", test.name()),
            frame,
        );
    }
}

/// The running test, if it has run past the watchdog's limit. Through
/// `try_lock`, because it runs in the timer's handler, which may interrupt
/// `run` holding the lock; that tick then doesn't check.
fn overdue() -> Option<&'static dyn Test> {
    let run = (*RUN.try_lock()?)?;
    let test = *run.tests.get(run.current)?;
    let limit = LIMIT * u64::from(timer::HZ);
    (timer::ticks() - run.started >= limit).then_some(test)
}

fn finish(pass: bool) -> ! {
    if let Some(run) = RUN.try_lock().map(|run| *run) {
        if let Some(test) = run
            .and_then(|run| run.tests.get(run.current))
            .filter(|_| !pass)
        {
            // The logger may be the thing that panicked, so this goes
            // through the panic path.
            logger::panic(format_args!("test {} failed", test.name()), &"");
        }
        let summary = Summary { run, pass };
        logger::test_summary(&summary, &Brief(&summary), pass);
        if pass && run.is_some_and(|run| run.more()) {
            Arch::exit_qemu(TestEnd::PassedWithMore);
        }
    }
    Arch::exit_qemu(if pass {
        TestEnd::Passed
    } else {
        TestEnd::Failed
    })
}

struct Summary {
    run: Option<Run>,
    pass: bool,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "summary:")?;
        let Some(run) = self.run else {
            return writeln!(f, "  panic before the tests started");
        };
        for (i, test) in run.tests.iter().enumerate() {
            writeln!(f, "  {} {}", run.result(i), test.name())?;
        }
        if !self.pass && run.current == run.tests.len() {
            writeln!(f, "  panic after the last test")?;
        }
        Ok(())
    }
}

/// The summary for the console: how many tests passed, failed and didn't
/// run, and the failed one's name, in at most three lines.
struct Brief<'a>(&'a Summary);

impl fmt::Display for Brief<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(run) = self.0.run else {
            return writeln!(f, "summary: panic before the tests started");
        };
        let (mut ok, mut not_run) = (0, 0);
        let mut failed = None;
        for (i, test) in run.tests.iter().enumerate() {
            match run.result(i) {
                "ok" => ok += 1,
                "FAILED" => failed = Some(test.name()),
                _ => not_run += 1,
            }
        }
        write!(f, "summary: {ok} ok")?;
        if failed.is_some() {
            write!(f, ", 1 FAILED")?;
        }
        if not_run > 0 {
            write!(f, ", {not_run} not run")?;
        }
        writeln!(f)?;
        if let Some(name) = failed {
            writeln!(f, "  FAILED {name}")?;
        }
        if !self.0.pass && run.current == run.tests.len() {
            writeln!(f, "  panic after the last test")?;
        }
        Ok(())
    }
}

impl Run {
    /// What the summary says of test `i`.
    fn result(&self, i: usize) -> &'static str {
        if i == self.current {
            return "FAILED";
        }
        let Some(place) = self.place(i) else {
            return "not run";
        };
        let passed = self.current == self.tests.len()
            || self
                .place(self.current)
                .is_some_and(|current| place < current);
        if passed { "ok" } else { "not run" }
    }

    /// Where test `i` comes in this boot's order, if the boot runs it.
    fn place(&self, i: usize) -> Option<usize> {
        if Some(i) == self.last {
            Some(self.tests.len())
        } else if self.alone || expects_panic(self.tests, i) {
            None
        } else {
            Some(i)
        }
    }

    /// Whether another test that expects a panic follows the one this
    /// boot runs.
    fn more(&self) -> bool {
        self.last
            .is_some_and(|last| (last + 1..self.tests.len()).any(|i| expects_panic(self.tests, i)))
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use thisos_hal::Probe;

    use core::sync::atomic::Ordering;

    use super::{Brief, ExpectPanic, Run, STARTED, Summary, Test};

    fn brief(run: Option<Run>, pass: bool) -> alloc::string::String {
        format!("{}", Brief(&Summary { run, pass }))
    }

    // Four tests, the last of which expects a panic, so it runs last.
    #[test_case]
    fn the_console_summary_counts_tests_and_names_the_failed_one() {
        static TESTS: [&dyn Test; 4] = [
            &|| {},
            &|| {},
            &|| {},
            &ExpectPanic {
                name: "panics",
                run: || {},
                message: "",
                after: None,
            },
        ];
        let run = |current| Run {
            tests: &TESTS,
            current,
            last: Some(3),
            alone: false,
            started: 0,
        };
        let second = TESTS[1].name();
        assert_eq!(
            brief(Some(run(1)), false),
            format!("summary: 1 ok, 1 FAILED, 2 not run\n  FAILED {second}\n")
        );
        assert_eq!(
            brief(Some(run(3)), false),
            "summary: 3 ok, 1 FAILED\n  FAILED panics\n"
        );
        assert_eq!(brief(Some(run(4)), true), "summary: 4 ok\n");
        assert_eq!(
            brief(Some(run(4)), false),
            "summary: 4 ok\n  panic after the last test\n"
        );
        assert_eq!(
            brief(None, false),
            "summary: panic before the tests started\n"
        );
        let alone = Run {
            alone: true,
            ..run(4)
        };
        assert_eq!(brief(Some(alone), true), "summary: 1 ok, 3 not run\n");
    }

    // The logger's panic test comes first in the test list, so this one
    // runs on a boot of its own, where it is the only test.
    #[test_case]
    const A_LATER_PANIC_TEST_RUNS_ALONE: ExpectPanic = ExpectPanic {
        name: "thisos_kernel::testing::tests::a_later_panic_test_runs_alone",
        run: || panic!("the second panic test"),
        message: "the second panic test",
        after: Some(|| STARTED.load(Ordering::Relaxed) == 1),
    };

    // The watchdog's own report ends the run, so this stands in a 1 s
    // timeout for its 10 s one and checks the timer stops the probe.
    #[test_case]
    fn the_watchdog_stops_a_hung_test() {
        super::expect_timeout(1, Probe::Spin);
    }
}
