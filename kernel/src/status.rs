//! The CPUs' status cells: fixed-width cells packed in rows at the top of
//! the screen, one per CPU, with the log scrolling below them. A CPU draws
//! its own cell, taking no lock, on every tick and when a lock spin passes
//! 10 ms, so a CPU that hangs leaves its cell showing where. A cell reads
//! `0     1234 t42 kern FRAMES`: the CPU, how many ticks it has drawn (the
//! last 7 digits), the thread running, whether it runs the kernel or user
//! code or idles, and the innermost lock it holds, or in yellow `!NAME`
//! for the one it spins on.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use thisos_console::Color;
use thisos_hal::{Framebuffer, Platform};
use thisos_locks::MAX_CPUS;
use thisos_logring::Line;

use crate::Arch;
use crate::cpu::{self, State};

type Pixels = <Arch as Platform>::Pixels;

/// The characters in a cell, a space after the last included.
pub const CELL: usize = 32;

static SCREEN: spin::Once<Framebuffer<Pixels>> = spin::Once::new();

/// The ticks each CPU has drawn its cell on.
static TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Each CPU's cell as drawn: a character per byte, with [`YELLOW`] set
/// when it is drawn yellow, and 0 before it is drawn. Each CPU writes only
/// its own, so a redraw draws only the characters that changed.
static SHOWN: [[AtomicU8; CELL]; MAX_CPUS] =
    [const { [const { AtomicU8::new(0) }; CELL] }; MAX_CPUS];

const YELLOW: u8 = 0x80;

/// Keeps `fb` for the cells, and returns how many rows of the console's
/// grid they take at the top. Only `logger::init` calls this, once, before
/// the console clears the screen.
pub fn init(fb: Framebuffer<Pixels>) -> usize {
    let (cols, _) = thisos_console::grid(&fb);
    let rows = cpu::count().div_ceil(per_row(cols));
    SCREEN.call_once(|| fb);
    thisos_locks::on_long_spin(redraw);
    rows
}

/// How many cells a row of `cols` holds: one, cut short, on a screen too
/// narrow for a whole one.
fn per_row(cols: usize) -> usize {
    (cols / CELL).max(1)
}

/// Counts a tick on this CPU and redraws its cell, showing `state`, what
/// the tick interrupted. The trap handler calls this on every tick.
pub fn tick(state: State) {
    let cpu = cpu::id();
    let ticks = TICKS[cpu].fetch_add(1, Ordering::Relaxed) + 1;
    draw(cpu, &text(cpu, state, ticks));
}

/// Redraws `cpu`'s cell with what it runs now. Locks call this through
/// their hook when a spin passes 10 ms, on the spinning CPU.
fn redraw(cpu: usize) {
    let ticks = TICKS[cpu].load(Ordering::Relaxed);
    draw(cpu, &text(cpu, cpu::state(cpu), ticks));
}

/// What `cpu`'s cell shows, given `state` and `ticks`, and whether it is
/// yellow: when it spins.
pub fn text(cpu: usize, state: State, ticks: u64) -> (Line<CELL>, bool) {
    let mut line = Line::new();
    let _ = write!(line, "{cpu} {:7} ", ticks % 10_000_000);
    let _ = match thisos_locks::thread(cpu) {
        Some(id) => write!(line, "t{id} "),
        None => write!(line, "t? "),
    };
    let _ = write!(line, "{} ", state.name());
    let spinning = thisos_locks::spinning(cpu);
    let _ = match (spinning, thisos_locks::innermost(cpu)) {
        (Some(rank), _) => write!(line, "!{}", rank.name()),
        (None, Some(rank)) => write!(line, "{}", rank.name()),
        (None, None) => write!(line, "-"),
    };
    (line, spinning.is_some())
}

fn draw(cpu: usize, (line, spinning): &(Line<CELL>, bool)) {
    let Some(fb) = SCREEN.get() else {
        return;
    };
    let (cols, _) = thisos_console::grid(fb);
    let per_row = per_row(cols);
    let (left, row) = ((cpu % per_row) * CELL, cpu / per_row);
    let (color, mark) = if *spinning {
        (Color::Yellow, YELLOW)
    } else {
        (Color::Gray, 0)
    };
    // The last character stays a space, between this cell and the next.
    let text = line.as_str().bytes().chain(core::iter::repeat(b' '));
    for (i, (shown, byte)) in SHOWN[cpu].iter().zip(text).take(CELL - 1).enumerate() {
        if shown.swap(byte | mark, Ordering::Relaxed) != byte | mark {
            thisos_console::draw_char(fb, left + i, row, char::from(byte), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use thisos_locks::Rank;

    use super::*;
    use crate::{Lock, thread};

    #[test_case]
    fn a_cell_shows_the_thread_its_state_and_the_innermost_lock() {
        static HELD: Lock<()> = Lock::new(Rank::TEST_OUTER, ());
        let id = thread::id();
        let cpu = cpu::id();
        let guard = HELD.lock();
        let (held, spinning) = text(cpu, State::Kernel, 12_345_678);
        drop(guard);
        assert_eq!(
            held.as_str(),
            format!("{cpu} 2345678 t{id} kern TEST_OUTER")
        );
        assert!(!spinning);
        let (none, _) = text(cpu, State::User, 5);
        assert_eq!(none.as_str(), format!("{cpu}       5 t{id} user -"));
    }
}
