//! TLB shootdowns for kernel pages. An invlpg reaches only the CPU that
//! runs it, and kernel pages are global, so loading CR3 doesn't drop them
//! either: before an unmapped kernel page's frame goes back, every other
//! running CPU takes an interrupt that drops the page from its TLB, which
//! may hold it even if that CPU never touched the page, from a speculative
//! walk.
//!
//! One shootdown runs at a time: only the kernel's `PageTables` starts
//! them, from methods that take it by `&mut`.

use core::fmt;
use core::hint::spin_loop;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};

use thisos_locks::MAX_CPUS;
use x86_64::VirtAddr;
use x86_64::instructions::tlb;

use crate::{apic, percpu, smp, tsc};

/// The pages of the shootdown that runs.
static START: AtomicU64 = AtomicU64::new(0);
static END: AtomicU64 = AtomicU64::new(0);

/// The CPUs that haven't dropped those pages yet, a bit per index.
static PENDING: AtomicU64 = AtomicU64::new(0);

const _: () = assert!(MAX_CPUS <= 64, "PENDING has a bit per CPU");

/// Drops `pages` from every other running CPU's TLB, and returns once each
/// has. The caller drops them from its own. Panics, naming them, if any
/// hasn't within a second: one spinning that long with interrupts off is
/// already a bug, as a lock spin that long is.
#[cfg_attr(
    test,
    expect(dead_code, reason = "host tests flush through paging's fake CPU")
)]
pub fn run(pages: Range<u64>) {
    let running = smp::running();
    if running == 1 {
        return;
    }
    let me = percpu::index();
    let others = (u64::MAX >> (64 - running)) & !(1 << me);
    START.store(pages.start, Ordering::Relaxed);
    END.store(pages.end, Ordering::Relaxed);
    // Release, so that a CPU that finds its bit finds the pages too.
    PENDING.store(others, Ordering::Release);
    for cpu in (0..running).filter(|&cpu| cpu != me) {
        apic::send_shootdown(smp::apic_id(cpu));
    }
    let Some(hz) = tsc::rate() else {
        unreachable!("smp::start runs no AP without the TSC's rate");
    };
    let started = tsc::read();
    loop {
        let left = PENDING.load(Ordering::Acquire);
        if left == 0 {
            return;
        }
        assert!(
            tsc::read().wrapping_sub(started) < hz,
            "{} didn't answer a TLB shootdown of {:#x}..{:#x} within 1 s",
            Cpus(left),
            pages.start,
            pages.end
        );
        spin_loop();
    }
}

/// Drops the pages of the shootdown that runs from this CPU's TLB, and
/// tells its starter. The shootdown vector's handler.
pub fn answer() {
    let bit = 1 << percpu::index();
    // Acquire, pairing with run's store, for the pages.
    assert!(
        PENDING.load(Ordering::Acquire) & bit != 0,
        "a TLB shootdown interrupt with no shootdown for this CPU"
    );
    let pages = START.load(Ordering::Relaxed)..END.load(Ordering::Relaxed);
    for page in pages.step_by(4096) {
        tlb::flush(VirtAddr::new(page));
    }
    PENDING.fetch_and(!bit, Ordering::Release);
}

/// CPU indices, from a bit per index.
#[cfg_attr(
    test,
    expect(dead_code, reason = "host tests flush through paging's fake CPU")
)]
struct Cpus(u64);

impl fmt::Display for Cpus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CPU")?;
        let mut first = true;
        for cpu in (0..64).filter(|cpu| self.0 & 1 << cpu != 0) {
            write!(f, "{}{cpu}", if first { " " } else { ", " })?;
            first = false;
        }
        Ok(())
    }
}
