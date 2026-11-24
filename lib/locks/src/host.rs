//! A [`Platform`] for host tests, where tests run on threads in parallel:
//! each thread claims a CPU slot of its own on first use and frees it when
//! it exits, so parallel tests never share a held list. The counter moves
//! a [`STEP`] on every read, so a spin reaches its limit after a hundred
//! reads.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{CPUS, MAX_CPUS, Platform};

/// The counter's rate.
pub const HZ: u64 = 1000;

/// How far the counter moves on each read.
pub const STEP: u64 = 10;

static TAKEN: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// A claimed CPU slot, freed when its thread exits.
struct Slot(usize);

impl Slot {
    fn claim() -> Self {
        let Some(cpu) = (0..MAX_CPUS).find(|&cpu| {
            TAKEN[cpu]
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        }) else {
            panic!("more than {MAX_CPUS} test threads hold a CPU slot");
        };
        // A thread that panicked holding locks left its list behind.
        CPUS[cpu].reset();
        Self(cpu)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        TAKEN[self.0].store(false, Ordering::Release);
    }
}

std::thread_local! {
    static SLOT: Slot = Slot::claim();
    static NOW: Cell<u64> = const { Cell::new(0) };
}

/// The host-test platform.
pub struct Host;

impl Platform for Host {
    fn cpu() -> usize {
        SLOT.with(|slot| slot.0)
    }

    fn counter() -> u64 {
        NOW.with(|now| {
            let read = now.get();
            now.set(read + STEP);
            read
        })
    }

    fn counter_hz() -> Option<u64> {
        Some(HZ)
    }

    fn masked<R>(f: impl FnOnce() -> R) -> R {
        f()
    }
}
