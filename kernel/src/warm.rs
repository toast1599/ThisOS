//! The warm-reset experiment (issue #107, spike branch only). One flash,
//! three boots. The first finds no blocks, writes them and lets the TCO
//! expire. The second shows what survived the TCO reset, writes the blocks
//! again and resets through CF9. The third shows what survived that, with
//! the TCO summary, clears the headers and halts. Where the 83DA's chipset
//! isn't there (QEMU), the first boot goes straight to the CF9 test.

use spin::Once;
use thisos_arch_x86_64::warm::{self, BITMAP, BLOCK, INDICES, STRIDE, bit, set};
use thisos_hal::{Cpu, MemoryKind, Platform};

use crate::{Arch, timer};

type MemoryMap = <Arch as Platform>::MemoryMap;

const WORDS: u64 = BLOCK / 8;
/// Header words at the start of every block.
const HEADER: u64 = 32;
const HEADER_WORDS: usize = 32;
const MAGIC: u64 = u64::from_le_bytes(*b"THISOSWR");
/// TCO ticks of 0.6 s before the first timeout.
const TCO_TICKS: u16 = 16;
/// Seconds the TCO result stays on screen before the CF9 reset.
const COUNTDOWN: u64 = 60;
/// Most lines of damaged or gone blocks to list.
const LIST: usize = 24;

/// The reset a run's blocks were written to face.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tco,
    Cf9,
}

impl Kind {
    /// The number a header stores.
    fn code(self) -> u64 {
        match self {
            Kind::Tco => 1,
            Kind::Cf9 => 2,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Tco => "TCO timeout",
            Kind::Cf9 => "CF9 reset",
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Summary {
    written: u64,
    intact: u64,
    damaged: u64,
    gone: u64,
    hpr_cause0: u64,
    gen_pmcon_a: u64,
}

enum Next {
    ArmTco,
    ResetCf9(Option<Summary>),
    Done,
}

struct Run {
    next: Next,
    bitmap: [u64; BITMAP],
}

static RUN: Once<Run> = Once::new();

fn expected(phys: u64, seed: u64) -> u64 {
    phys.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ seed
}

fn fold(words: &[u64]) -> u64 {
    words.iter().fold(0xcbf2_9ce4_8422_2325, |h, &w| {
        (h ^ w).wrapping_mul(0x0100_0000_01b3)
    })
}

fn header(at: u64) -> [u64; HEADER_WORDS] {
    let mut words = [0; HEADER_WORDS];
    for (word, w) in words.iter_mut().zip(0..HEADER) {
        *word = warm::read(at + 8 * w);
    }
    words
}

fn valid(words: &[u64; HEADER_WORDS]) -> bool {
    words[0] == MAGIC && words[31] == fold(&words[..31])
}

/// Block addresses that lie wholly in regions of `memory` that `keep`
/// accepts, below the bitmap's limit, skipping the first 64 MiB's block at
/// address 0.
fn blocks(memory: MemoryMap, keep: impl Fn(MemoryKind) -> bool) -> impl Iterator<Item = u64> {
    memory.filter(move |r| keep(r.kind)).flat_map(|r| {
        let first = r.start.div_ceil(STRIDE).max(1);
        (first..INDICES)
            .map(|i| i * STRIDE)
            .take_while(move |&at| at + BLOCK <= r.end)
    })
}

fn reset_causes() -> Option<(u32, u32)> {
    warm::on_83da().then(warm::reset_causes)
}

/// Shows what the last boot's blocks look like and decides this boot's
/// step. `kernel::init` calls this before the frame allocator starts.
pub fn check(memory: MemoryMap) {
    log::info!(
        "warm: issue #107 experiment, {} KiB blocks every {} MiB",
        BLOCK >> 10,
        STRIDE >> 20
    );
    let causes = reset_causes();
    match causes {
        Some((hpr, pmcon)) => log::info!(
            "warm: HPR_CAUSE0 {hpr:#x} (CF9_ES {}, TCO_WDT {}), GEN_PMCON_A {pmcon:#x} (MEM_SR {}, GBL_RST_STS {})",
            hpr >> 1 & 1,
            hpr >> 6 & 1,
            pmcon >> 21 & 1,
            pmcon >> 24 & 1
        ),
        None => {
            log::info!("warm: not the 83DA's chipset, or its PMC isn't uncacheable: CF9 test only");
        }
    }
    let ram = |kind| matches!(kind, MemoryKind::Usable | MemoryKind::Loader);
    let found = blocks(memory.clone(), ram).map(header).find(valid);
    let mut fresh = [0; BITMAP];
    for at in blocks(memory.clone(), |kind| kind == MemoryKind::Usable) {
        set(&mut fresh, at / STRIDE);
    }
    let next = match found {
        None => {
            log::info!("warm: no blocks from an earlier boot: starting a run");
            if causes.is_some() {
                Next::ArmTco
            } else {
                Next::ResetCf9(None)
            }
        }
        Some(h) => {
            let kind = if h[1] == Kind::Tco.code() {
                Kind::Tco
            } else {
                Kind::Cf9
            };
            let mut summary = survey(kind, &h);
            if let Some((hpr, pmcon)) = causes {
                summary.hpr_cause0 = u64::from(hpr);
                summary.gen_pmcon_a = u64::from(pmcon);
            }
            match kind {
                Kind::Tco => Next::ResetCf9(Some(summary)),
                Kind::Cf9 => {
                    if h[20] != 0 {
                        log::info!(
                            "warm: earlier TCO timeout: {} written, {} intact, {} damaged, {} gone; HPR_CAUSE0 {:#x}, GEN_PMCON_A {:#x}",
                            h[20],
                            h[21],
                            h[22],
                            h[23],
                            h[24],
                            h[25]
                        );
                    }
                    for at in blocks(memory, ram) {
                        if valid(&header(at)) {
                            warm::write(at, 0);
                        }
                    }
                    log::info!(
                        "warm: run complete; headers cleared, so the next boot starts a new run"
                    );
                    Next::Done
                }
            }
        }
    };
    if !matches!(next, Next::Done) {
        warm::reserve(fresh);
    }
    RUN.call_once(|| Run {
        next,
        bitmap: fresh,
    });
}

/// Compares every block the run header `run` lists with what it held, and
/// logs the totals and the blocks that changed: for each, the payload
/// words changed, a mask of the header words changed, and the first
/// changed header word's index and value.
fn survey(kind: Kind, run: &[u64; HEADER_WORDS]) -> Summary {
    let seed = run[2];
    let mut bitmap = [0; BITMAP];
    bitmap.copy_from_slice(&run[4..20]);
    let mut summary = Summary::default();
    let mut listed = 0;
    for i in (0..INDICES).filter(|&i| bit(&bitmap, i)) {
        let at = i * STRIDE;
        summary.written += 1;
        let h = header(at);
        let mut want = *run;
        want[3] = i;
        want[31] = fold(&want[..31]);
        let (mask, first) = h.iter().zip(&want).zip(0..HEADER).fold(
            (0_u64, None),
            |(mask, first), ((found, want), w)| {
                if found == want {
                    (mask, first)
                } else {
                    (mask | 1 << w, first.or(Some((w, *found))))
                }
            },
        );
        let bad = (HEADER..WORDS)
            .filter(|w| warm::read(at + 8 * w) != expected(at + 8 * w, seed))
            .fold(0, |n, _| n + 1);
        let header_ok = mask == 0;
        let state = if bad == 0 && header_ok {
            summary.intact += 1;
            continue;
        } else if bad == WORDS - HEADER {
            summary.gone += 1;
            "gone"
        } else {
            summary.damaged += 1;
            "damaged"
        };
        if listed < LIST {
            match first {
                Some((w, found)) => log::info!(
                    "warm:   {at:#011x}: {state}, {bad} words, header mask {mask:#x}, w{w} {found:#018x}"
                ),
                None => log::info!("warm:   {at:#011x}: {state}, {bad} words, header intact"),
            }
        }
        listed += 1;
    }
    if listed > LIST {
        log::info!("warm:   and {} more", listed - LIST);
    }
    log::info!(
        "warm: after the {}: {} blocks written, {} intact, {} damaged, {} gone",
        kind.name(),
        summary.written,
        summary.intact,
        summary.damaged,
        summary.gone
    );
    summary
}

/// Writes every reserved block for the reset `kind`, with `tco` saved in
/// each header.
fn write_blocks(bitmap: &[u64; BITMAP], kind: Kind, tco: Summary) -> u64 {
    let seed = warm::seed();
    let mut h = [0; HEADER_WORDS];
    h[0] = MAGIC;
    h[1] = kind.code();
    h[2] = seed;
    h[4..20].copy_from_slice(bitmap);
    h[20..26].copy_from_slice(&[
        tco.written,
        tco.intact,
        tco.damaged,
        tco.gone,
        tco.hpr_cause0,
        tco.gen_pmcon_a,
    ]);
    let mut count = 0;
    for i in (0..INDICES).filter(|&i| bit(bitmap, i)) {
        let at = i * STRIDE;
        h[3] = i;
        h[31] = fold(&h[..31]);
        for (&word, w) in h.iter().zip(0..HEADER) {
            warm::write(at + 8 * w, word);
        }
        for w in HEADER..WORDS {
            warm::write(at + 8 * w, expected(at + 8 * w, seed));
        }
        count += 1;
    }
    count
}

fn wait(seconds: u64) {
    let end = timer::ticks() + seconds * u64::from(timer::HZ);
    while timer::ticks() < end {
        core::hint::spin_loop();
    }
}

/// Takes this boot's step: arms the TCO and hangs, or counts down and
/// resets through CF9, or halts once the run is over. `kmain` calls this
/// after `kernel::init`, in place of starting init.
pub fn act() -> ! {
    let Some(run) = RUN.get() else {
        panic!("warm::check didn't run");
    };
    match run.next {
        Next::ArmTco => {
            let n = write_blocks(&run.bitmap, Kind::Tco, Summary::default());
            log::info!(
                "warm: wrote {n} blocks; arming the TCO for {TCO_TICKS} ticks and hanging with interrupts off"
            );
            log::info!("warm: expect a freeze, then a reset, in about 10 s; leave the stick in");
            wait(1);
            warm::arm_tco(TCO_TICKS);
            warm::hang()
        }
        Next::ResetCf9(tco) => {
            log::info!("warm: photograph this screen; CF9 reset in {COUNTDOWN} s");
            let mut left = COUNTDOWN;
            while left > 0 {
                wait(10);
                left -= 10;
                log::info!("warm: {left} s");
            }
            let n = write_blocks(&run.bitmap, Kind::Cf9, tco.unwrap_or_default());
            log::info!("warm: wrote {n} blocks; resetting through CF9");
            wait(1);
            warm::reset_cf9()
        }
        Next::Done => {
            log::info!("warm: done");
            Arch::halt()
        }
    }
}
