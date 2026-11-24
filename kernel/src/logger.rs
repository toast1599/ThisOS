//! The `log` backend, which writes to the serial port, when there is one,
//! and to the framebuffer console. Until its drainer thread runs, it writes
//! each record as it comes. From then on records go into a ring, and the
//! drainer draws them a piece at a time, so that a record's caller doesn't
//! wait for the polled serial port or a console scroll with interrupts off.

use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::{Level, LevelFilter, Log, Metadata, Record};
use spin::Mutex;
use thisos_abi::{Error, MAX_WRITE};
use thisos_console::{Color, Console};
use thisos_hal::{Cpu, Framebuffer, Platform};
use thisos_logring::{HEADER, Line, Piece, Ring};

use crate::{Arch, thread};

type Serial = <Arch as Platform>::Serial;
type Pixels = <Arch as Platform>::Pixels;

struct Logger {
    // One lock for both, so that records reach them in the same order.
    sinks: Mutex<Sinks>,
}

struct Sinks {
    serial: Option<Serial>,
    console: Console<Pixels>,
}

static LOGGER: Logger = Logger {
    sinks: Mutex::new(Sinks {
        serial: None,
        console: Console::new(),
    }),
};

/// Whether `init` has given the logger its sinks.
static READY: AtomicBool = AtomicBool::new(false);

/// The sinks a panic uses when it can't use the logger's: a second serial
/// handle and a console on a second framebuffer handle. The first panic
/// that needs them sets them up. A static, because the console's grid is
/// too large for the stack.
static PANIC_SINKS: Mutex<Sinks> = Mutex::new(Sinks {
    serial: None,
    console: Console::new(),
});

static PANIC_SINKS_READY: AtomicBool = AtomicBool::new(false);

/// The ring's size in bytes.
const RING_SIZE: usize = 128 * 1024;

/// The most a user write takes in the ring: a record per line, at most
/// `MAX_WRITE + 1` of them, each a header and `user: `, and text that
/// escapes make up to six bytes per byte. A write waits until it fits, so
/// a smaller ring would leave it waiting for good.
const _: () = assert!(RING_SIZE >= (MAX_WRITE + 1) * (HEADER + USER.len()) + 6 * MAX_WRITE);

/// What starts the text of a user write's records.
const USER: &str = "user: ";

/// The most bytes of a kernel record's text, which is cut short past that.
const LINE: usize = 512;

/// The records the drainer hasn't drawn yet. Taken with interrupts off, so
/// that an interrupt handler can log.
static RING: Mutex<Ring<RING_SIZE>> = Mutex::new(Ring::new());

/// The threads whose user writes wait for room in the ring, by id, each
/// with the room its write needs. Taken with interrupts on, and never by an
/// interrupt handler.
static WAITERS: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

/// Whether the drainer runs, so that records go into the ring.
static DRAINING: AtomicBool = AtomicBool::new(false);

/// The drainer's thread id, once `DRAINING` is set.
static DRAINER: AtomicU64 = AtomicU64::new(0);

// The lock is held only with interrupts off, so an interrupt handler that
// logs never finds it held by the code it interrupted. An exception, NMI or
// machine check inside the logger can still find it held, which is what
// report_sinks is for.
fn with_sinks<R>(f: impl FnOnce(&mut Sinks) -> R) -> R {
    Arch::without_interrupts(|| f(&mut LOGGER.sinks.lock()))
}

/// Runs `f` on the logger's sinks, or on the panic sinks when the logger
/// has none yet or its lock is held: then the panic came before
/// `logger::init`, or from inside the logger, where the console may be half
/// updated. Once a panic has used the panic sinks, every later report of
/// that panic finds the logger's lock still held and uses them too.
fn report_sinks<R>(f: impl FnOnce(&mut Sinks) -> R) -> Option<R> {
    Arch::without_interrupts(|| {
        if READY.load(Ordering::Acquire)
            && let Some(mut sinks) = LOGGER.sinks.try_lock()
        {
            return Some(f(&mut sinks));
        }
        let mut sinks = PANIC_SINKS.try_lock()?;
        if !PANIC_SINKS_READY.swap(true, Ordering::Relaxed) {
            sinks.serial = Arch::panic_serial();
            if let Some(framebuffer) = Arch::panic_framebuffer() {
                sinks.console.attach(framebuffer);
            }
        }
        Some(f(&mut sinks))
    })
}

/// Installs the logger. Only `kernel::init` calls this, once.
pub fn init(serial: Option<Serial>, framebuffer: Option<Framebuffer<Pixels>>) {
    with_sinks(|sinks| {
        sinks.serial = serial;
        if let Some(framebuffer) = framebuffer {
            sinks.console.attach(framebuffer);
        }
    });
    READY.store(true, Ordering::Release);
    // This fails only when a logger is already set, and this is the only
    // place that sets one.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(if cfg!(debug_assertions) {
        LevelFilter::Trace
    } else {
        LevelFilter::Info
    });
}

/// Starts the drainer. Only `kernel::init` calls this, once, after
/// `thread::init`; records go into the ring once the drainer first runs.
pub fn start() {
    thread::spawn(drain);
}

/// What the drainer runs: it draws the ring's records to the sinks a
/// piece at a time, and parks while the ring is empty.
fn drain() {
    DRAINER.store(thread::id(), Ordering::Relaxed);
    DRAINING.store(true, Ordering::Release);
    loop {
        #[cfg(test)]
        if tests::PAUSED.load(Ordering::Acquire) {
            thread::park();
            continue;
        }
        // A piece at a time with interrupts off, which bounds how long
        // they stay off, and leaves the sinks free for a panic on another
        // thread between pieces.
        let (drew, room) = Arch::without_interrupts(|| {
            let (dropped, piece, room) = {
                let mut ring = RING.lock();
                let dropped = ring.take_dropped();
                let piece = ring.pop();
                (dropped, piece, ring.room())
            };
            let mut sinks = LOGGER.sinks.lock();
            write_dropped(&mut sinks, dropped);
            if let Some(piece) = &piece {
                write_piece(&mut sinks, piece);
            }
            (piece.is_some(), room)
        });
        // Also before a park, since the ring may have had room made
        // without a piece drawn: a panic's flush, or a test emptying it.
        // Then the ring is empty, which every waiting write fits.
        wake_fitting(room);
        if !drew {
            thread::park();
        }
    }
}

/// Logs the lines of a user write, one Info record each with the target
/// `user`, all together once there is room for all of them, and waits for
/// that room. Fails with [`Error::NoMemory`], logging nothing, when the
/// heap has no room to note that the thread waits.
pub fn write_user<D: fmt::Display>(lines: impl Iterator<Item = D> + Clone) -> Result<(), Error> {
    if !DRAINING.load(Ordering::Acquire) {
        for line in lines {
            log::info!(target: "user", "{line}");
        }
        return Ok(());
    }
    let size: usize = lines
        .clone()
        .map(|line| HEADER + thisos_logring::size(&format_args!("{USER}{line}")))
        .sum();
    let id = thread::id();
    loop {
        let written = Arch::without_interrupts(|| {
            let mut ring = RING.lock();
            if ring.room() < size {
                return false;
            }
            for line in lines.clone() {
                ring.push(tag(Level::Info), &format_args!("{USER}{line}"));
            }
            true
        });
        if written {
            thread::unpark(DRAINER.load(Ordering::Relaxed));
            return Ok(());
        }
        {
            let mut waiters = WAITERS.lock();
            waiters.try_reserve(1).map_err(|_| Error::NoMemory)?;
            waiters.push((id, size));
        }
        // The drainer may have made room before the id went on the list.
        if Arch::without_interrupts(|| RING.lock().room()) < size {
            thread::park();
        }
        WAITERS.lock().retain(|&(waiter, _)| waiter != id);
    }
}

/// Unparks the threads in [`WAITERS`] whose write fits in `room` bytes of
/// the ring, and takes them off it. Several may fit only one at a time;
/// the others find too little room, and wait again.
fn wake_fitting(room: usize) {
    WAITERS.lock().retain(|&(id, size)| {
        let fits = size <= room;
        if fits {
            thread::unpark(id);
        }
        !fits
    });
}

/// Writes a panic report: `head` as an Error record, then `body` as it is,
/// to the logger's sinks or the panic sinks (see `report_sinks`), after the
/// records still in the ring.
pub fn panic(head: fmt::Arguments, body: &dyn fmt::Display) {
    let record = Record::builder()
        .level(Level::Error)
        .target(env!("CARGO_CRATE_NAME"))
        .args(head)
        .build();
    report_sinks(|sinks| {
        flush(sinks);
        let Sinks { serial, console } = sinks;
        if let Some(serial) = serial {
            write_serial(serial, &record, body);
        }
        write_console(console, &record, body);
    });
}

/// Draws what is left in the ring to `sinks`, unless the ring is locked:
/// then the panic that called this came from code holding it.
fn flush(sinks: &mut Sinks) {
    let Some(mut ring) = RING.try_lock() else {
        return;
    };
    write_dropped(sinks, ring.take_dropped());
    while let Some(piece) = ring.pop() {
        #[cfg(test)]
        if piece.tag.is_some() {
            tests::FLUSHED.fetch_add(1, Ordering::Relaxed);
        }
        write_piece(sinks, &piece);
    }
}

/// Writes a test summary, then PASS or FAIL: `full` and the word plain on
/// serial, and `brief` and the word at four times the font's size on the
/// console. It goes where the panic report went, when there was one.
#[cfg(test)]
pub fn test_summary(full: &dyn fmt::Display, brief: &dyn fmt::Display, pass: bool) {
    let (word, color) = if pass {
        ("PASS", Color::Green)
    } else {
        ("FAIL", Color::Red)
    };
    report_sinks(|sinks| {
        flush(sinks);
        let Sinks { serial, console } = sinks;
        if let Some(serial) = serial {
            let _ = writeln!(serial, "{full}{word}");
        }
        console.set_color(Color::Gray);
        let _ = write!(console, "{brief}");
        console.set_color(color);
        console.write_large(word, 4);
    });
}

// A record that fails to format has nowhere else to be reported, so this
// and write_console drop the results of their writes.
fn write_serial(serial: &mut Serial, record: &Record, body: &dyn fmt::Display) {
    let _ = write!(
        serial,
        "{} {}: {}\n{body}",
        record.level(),
        record.target(),
        record.args()
    );
}

fn write_console(console: &mut Console<Pixels>, record: &Record, body: &dyn fmt::Display) {
    console.set_color(color(record.level()));
    let _ = write!(console, "{}", record.level());
    console.set_color(Color::Gray);
    let _ = write!(console, " {}: {}\n{body}", record.target(), record.args());
}

/// Writes a piece of a record from the ring: the level first when the
/// piece starts the record, and a newline when it ends it.
fn write_piece(Sinks { serial, console }: &mut Sinks, piece: &Piece) {
    let level = piece.tag.map(level);
    if let Some(serial) = serial {
        if let Some(level) = level {
            let _ = write!(serial, "{level} ");
        }
        let _ = serial.write_str(piece.text());
        if piece.end {
            let _ = serial.write_char('\n');
        }
    }
    if let Some(level) = level {
        console.set_color(color(level));
        let _ = write!(console, "{level}");
    }
    // On every piece, since a panic report may have changed the color
    // since the last one.
    console.set_color(Color::Gray);
    if level.is_some() {
        let _ = console.write_char(' ');
    }
    let _ = console.write_str(piece.text());
    if piece.end {
        let _ = console.write_char('\n');
    }
}

/// Logs that `dropped` records didn't fit in the ring, if any didn't.
fn write_dropped(Sinks { serial, console }: &mut Sinks, dropped: u64) {
    if dropped == 0 {
        return;
    }
    let records = if dropped == 1 { "record" } else { "records" };
    let args = format_args!("{dropped} {records} dropped: the log was full");
    let record = Record::builder()
        .level(Level::Warn)
        .target(module_path!())
        .args(args)
        .build();
    if let Some(serial) = serial {
        write_serial(serial, &record, &"");
    }
    write_console(console, &record, &"");
    #[cfg(test)]
    tests::DROPS_LOGGED.fetch_add(dropped, Ordering::Relaxed);
}

/// The tag a record of `level` has in the ring.
fn tag(level: Level) -> u8 {
    match level {
        Level::Error => 1,
        Level::Warn => 2,
        Level::Info => 3,
        Level::Debug => 4,
        Level::Trace => 5,
    }
}

/// The level of a record whose tag is `tag`.
fn level(tag: u8) -> Level {
    match tag {
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        4 => Level::Debug,
        // Only tag makes tags.
        _ => Level::Trace,
    }
}

fn color(level: Level) -> Color {
    match level {
        Level::Error => Color::Red,
        Level::Warn => Color::Yellow,
        Level::Info => Color::Green,
        Level::Debug => Color::Blue,
        Level::Trace => Color::DarkGray,
    }
}

impl Log for Logger {
    // The log macros have already checked the level against max_level.
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        // Interrupts stay off from the check to the write, so the drainer
        // can't start in between and draw later records first.
        let written = Arch::without_interrupts(|| {
            if DRAINING.load(Ordering::Acquire) {
                return false;
            }
            with_sinks(|Sinks { serial, console }| {
                if let Some(serial) = serial {
                    write_serial(serial, record, &"");
                }
                write_console(console, record, &"");
            });
            true
        });
        if written {
            return;
        }
        // Formatted with no lock held, so that a panic in a record's
        // Display finds the ring free and flushes it.
        let mut line = Line::<LINE>::new();
        let _ = write!(line, "{}: {}", record.target(), record.args());
        Arch::without_interrupts(|| {
            let mut ring = RING.lock();
            if !ring.push(tag(record.level()), &line.as_str()) {
                ring.count_dropped();
            }
        });
        thread::unpark(DRAINER.load(Ordering::Relaxed));
    }

    fn flush(&self) {}
}

#[cfg(test)]
pub(crate) mod tests {
    use core::fmt::{self, Write};
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use log::Level;
    use thisos_hal::Cpu;
    use thisos_logring::HEADER;

    use super::{DRAINER, LINE, RING, USER, WAITERS, tag, wake_fitting, with_sinks};
    use crate::testing::{ExpectPanic, spin_until};
    use crate::{Arch, thread};

    /// Whether the drainer leaves the ring alone.
    pub(super) static PAUSED: AtomicBool = AtomicBool::new(false);

    /// The records the panic path drew from the ring.
    pub(super) static FLUSHED: AtomicUsize = AtomicUsize::new(0);

    /// The dropped records the drainer and the panic path have logged.
    pub(super) static DROPS_LOGGED: AtomicU64 = AtomicU64::new(0);

    /// Waits until the drainer has drawn everything in the ring, then
    /// parks it until [`resume`], so that records stay in the ring.
    pub fn pause() {
        spin_until("the drainer to empty the ring", || {
            Arch::without_interrupts(|| RING.lock().is_empty())
        });
        PAUSED.store(true, Ordering::Release);
        spin_until("the drainer to park", || {
            thread::is_blocked(DRAINER.load(Ordering::Relaxed))
        });
    }

    /// Lets the drainer draw the ring again.
    pub fn resume() {
        PAUSED.store(false, Ordering::Release);
        thread::unpark(DRAINER.load(Ordering::Relaxed));
    }

    /// Fills the ring, which the drainer must leave alone, to its last
    /// byte, then logs a record that it drops.
    pub fn fill() {
        let room = || Arch::without_interrupts(|| RING.lock().room());
        while room() >= HEADER + LINE {
            log::info!("{:LINE$}", "filling the ring");
        }
        Arch::without_interrupts(|| {
            let mut ring = RING.lock();
            // Below a header's size nothing fits already.
            if let Some(rest) = ring.room().checked_sub(HEADER) {
                assert!(ring.push(tag(Level::Info), &format_args!("{:rest$}", "")));
            }
        });
        log::info!("dropped");
    }

    /// Empties the ring without drawing it, keeping its count of dropped
    /// records, so that a test that fills it doesn't wait for the sinks.
    pub fn discard() {
        Arch::without_interrupts(|| {
            let mut ring = RING.lock();
            while ring.pop().is_some() {}
        });
    }

    /// How many threads wait for room in the ring.
    pub fn waiting() -> usize {
        WAITERS.lock().len()
    }

    // The drainer runs the same check after every piece it draws; here the
    // test runs it, with the drainer paused, so that the room is exact.
    #[test_case]
    fn a_waiting_write_wakes_only_once_it_fits() {
        static ID: AtomicU64 = AtomicU64::new(u64::MAX);
        static DONE: AtomicBool = AtomicBool::new(false);
        const TEXT: &str = "a user write that waits for its room in the ring";
        DONE.store(false, Ordering::Relaxed);
        let needed = HEADER + thisos_logring::size(&format_args!("{USER}{TEXT}"));
        pause();
        Arch::without_interrupts(|| {
            let mut ring = RING.lock();
            // Leaves one byte less than the write needs, in records of at
            // most a format width's 65535, none left shorter than a header.
            let mut fill = ring.room() - (needed - 1);
            while fill > 0 {
                let len = if fill > 60_000 + 2 * HEADER {
                    60_000
                } else {
                    fill - HEADER
                };
                assert!(ring.push(tag(Level::Info), &format_args!("{:len$}", "")));
                fill -= HEADER + len;
            }
        });
        thread::spawn(|| {
            ID.store(thread::id(), Ordering::Relaxed);
            assert!(super::write_user(core::iter::once(TEXT)).is_ok());
            DONE.store(true, Ordering::Release);
        });
        spin_until("the write to wait", || {
            waiting() == 1 && thread::is_blocked(ID.load(Ordering::Relaxed))
        });
        wake_fitting(Arch::without_interrupts(|| RING.lock().room()));
        assert_eq!(waiting(), 1, "woken with a byte too little");
        assert!(thread::is_blocked(ID.load(Ordering::Relaxed)));
        discard();
        wake_fitting(Arch::without_interrupts(|| RING.lock().room()));
        spin_until("the write to land", || DONE.load(Ordering::Acquire));
        assert_eq!(waiting(), 0);
        thread::wait_for_exits();
        discard();
        resume();
    }

    #[test_case]
    fn a_full_ring_drops_kernel_records_and_the_count_is_logged() {
        pause();
        let logged = DROPS_LOGGED.load(Ordering::Relaxed);
        fill();
        log::info!("dropped too");
        discard();
        resume();
        spin_until("the drainer to log the drops", || {
            DROPS_LOGGED.load(Ordering::Relaxed) == logged + 2
        });
        log::info!("logged after the drops");
        spin_until("the drainer to empty the ring", || {
            Arch::without_interrupts(|| RING.lock().is_empty())
        });
    }

    struct PanicsWhenShown;

    impl fmt::Display for PanicsWhenShown {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            panic!("panicked inside the logger");
        }
    }

    // The console is written with the sinks' lock held, as the drainer
    // writes it, so the report has to go through the panic sinks, after
    // the record still in the ring. On the laptop it is the report on the
    // cleared screen above the summary.
    #[test_case]
    const A_PANIC_INSIDE_THE_LOGGER_IS_REPORTED_AFTER_THE_RING: ExpectPanic = ExpectPanic {
        name: "thisos_kernel::logger::tests::a_panic_inside_the_logger_is_reported_after_the_ring",
        run: || {
            pause();
            log::info!("logged before the panic");
            with_sinks(|sinks| {
                let _ = write!(sinks.console, "{PanicsWhenShown}");
            });
        },
        message: "panicked inside the logger",
        after: Some(|| FLUSHED.load(Ordering::Relaxed) == 1),
    };
}
