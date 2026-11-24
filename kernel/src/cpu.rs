//! Which CPU the kernel runs on, and what each CPU is doing, for the log
//! and the status cells. There is one CPU until SMP starts the others.

use core::sync::atomic::{AtomicU8, Ordering};

use thisos_locks::MAX_CPUS;

use crate::Arch;

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

/// How many CPUs run the kernel.
pub fn count() -> usize {
    1
}
