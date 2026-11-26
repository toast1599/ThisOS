//! Which CPU the kernel runs on, and what each CPU is doing, for the log
//! and the status cells.

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use thisos_hal::{CpuStart, Platform};
use thisos_locks::MAX_CPUS;

use crate::{Arch, paging};

const _: () = assert!(
    MAX_CPUS <= 1 << 8,
    "a CPU index must fit a log record's byte"
);

/// The running CPU's index.
pub fn id() -> usize {
    <Arch as thisos_locks::Platform>::cpu()
}

/// The running CPU's index as a log record holds it.
pub fn id_byte() -> u8 {
    let Ok(id) = u8::try_from(id()) else {
        unreachable!("MAX_CPUS fits a byte");
    };
    id
}

/// What a CPU is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Kernel,
    User,
    Idle,
}

impl State {
    fn byte(self) -> u8 {
        match self {
            State::Kernel => 0,
            State::User => 1,
            State::Idle => 2,
        }
    }

    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => State::User,
            2 => State::Idle,
            _ => State::Kernel,
        }
    }

    /// The state as a status cell shows it.
    pub fn name(self) -> &'static str {
        match self {
            State::Kernel => "kern",
            State::User => "user",
            State::Idle => "idle",
        }
    }
}

static STATES: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

/// Records what this CPU runs from now on.
pub fn set_state(state: State) {
    STATES[id()].store(state.byte(), Ordering::Relaxed);
}

/// What `cpu` runs, as last recorded.
pub fn state(cpu: usize) -> State {
    State::from_byte(STATES[cpu].load(Ordering::Relaxed))
}

/// How many CPUs run the kernel: 1 until [`start`] has started the rest.
pub fn count() -> usize {
    RUNNING.load(Ordering::Relaxed)
}

/// The CPUs running, the boot CPU included, as [`start`] found them.
static RUNNING: AtomicUsize = AtomicUsize::new(1);

/// The CPUs the firmware listed as ready besides the boot CPU.
#[cfg(test)]
static LISTED: AtomicUsize = AtomicUsize::new(0);
/// The CPUs that read back an index other than the one they were given.
#[cfg(test)]
static MISNUMBERED: AtomicUsize = AtomicUsize::new(0);

/// Starts the other CPUs, which park, and logs each step as it happens.
/// `kernel::init` calls this once, after the timer and the logger's
/// drainer.
pub fn start() {
    let mut listed = 0;
    let running = Arch::start_cpus(
        &mut |size| {
            let Some(stack) = paging::map_stack(size) else {
                panic!("no frames for a CPU's stack");
            };
            stack
        },
        &mut |step| match step {
            CpuStart::Skipped { id } => {
                log::info!("cpu: id {id} is only online-capable, so it stays off");
            }
            CpuStart::Starting {
                listed: n,
                timeout_ms,
            } => {
                listed = n;
                log::info!("cpu: starting {n} more CPUs, waiting up to {timeout_ms} ms for each");
            }
            CpuStart::Up {
                cpu,
                id,
                reported,
                reported_cpu,
                micros,
            } => {
                if reported != id {
                    log::warn!("cpu {cpu}: listed as id {id} but reads id {reported}");
                }
                if reported_cpu != cpu {
                    log::warn!("cpu {cpu}: reads its own index as {reported_cpu}");
                    #[cfg(test)]
                    MISNUMBERED.fetch_add(1, Ordering::Relaxed);
                }
                log::info!("cpu {cpu}: id {id} up and parked after {micros} us");
            }
            CpuStart::Silent { id } => {
                log::warn!("cpu: id {id} didn't check in, so it was stopped");
            }
        },
    );
    RUNNING.store(running, Ordering::Relaxed);
    if listed > 0 {
        log::info!("cpu: {running} of {} running", listed + 1);
    }
    #[cfg(test)]
    LISTED.store(listed, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn every_listed_cpu_started() {
        let listed = LISTED.load(Ordering::Relaxed);
        assert_eq!(count(), listed + 1);
    }

    #[test_case]
    fn every_cpu_reads_its_own_index() {
        assert_eq!(MISNUMBERED.load(Ordering::Relaxed), 0);
        assert_eq!(id(), 0, "tests run on the boot CPU");
    }
}
