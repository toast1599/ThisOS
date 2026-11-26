//! The kernel's trap handler.

use thisos_hal::{Platform, Trap, TrapFrame as _};

use crate::cpu::{self, State};
use crate::{Arch, paging, panic, status, thread, timer, user};

/// Moves the stacks the CPU switches to for some traps, x86_64's for
/// double faults, NMIs and machine checks and aarch64's for a stack
/// overflow, to stacks with an unmapped page below each.
/// `kernel::init` calls this once, after `paging::init`.
pub fn init() {
    let mut count = 0;
    Arch::move_trap_stacks(&mut |size| {
        count += 1;
        let Some(stack) = paging::map_stack(size) else {
            panic!("no frames for a trap stack");
        };
        stack
    });
    log::info!("trap stacks: {count}, each with a guard page");
}

/// The kernel's trap handler, which every entry point passes to `entry!`.
/// A trap user code raised goes to `user::trap`. Otherwise a breakpoint and
/// a spurious interrupt return, and a timer interrupt counts a tick and
/// switches to the next ready thread; any other trap is a kernel bug, and
/// panics.
pub fn trap(trap: Trap, frame: &mut <Arch as Platform>::TrapFrame) {
    // Restored on the way out, so a tick shows what it interrupted. A
    // thread a tick switches away from restores its own when it resumes.
    let interrupted = cpu::state(cpu::id());
    cpu::set_state(State::Kernel);
    handle(trap, frame, interrupted);
    cpu::set_state(interrupted);
}

fn handle(trap: Trap, frame: &mut <Arch as Platform>::TrapFrame, interrupted: State) {
    if frame.raised_by_user() {
        return user::trap(trap, frame);
    }
    #[cfg(test)]
    if crate::testing::caught(trap, frame) {
        return;
    }
    match trap {
        Trap::Breakpoint | Trap::Spurious => {}
        Trap::Timer => {
            #[cfg(test)]
            crate::testing::watchdog(frame);
            timer::tick();
            status::tick(interrupted);
            thread::tick();
        }
        _ => panic::trap(trap, frame),
    }
}

#[cfg(test)]
mod tests {
    use thisos_hal::{Cpu, Platform, Probe, Trap};

    use crate::{Arch, paging, testing, timer};

    // Passes by returning: with no IDT entry the CPU triple-faults, and a
    // handler that panics fails the run.
    #[test_case]
    fn breakpoint_returns() {
        Arch::breakpoint();
    }

    // A trap from code that opened user access, as a user copy does, runs
    // the handler with it blocked, and the catch blocks it again for the
    // code after it.
    #[test_case]
    fn traps_block_user_access() {
        testing::expect_timeout(1, Probe::SpinUserAccessOpen);
        assert!(
            testing::user_access_blocked_at_timeout(),
            "the timer handler ran with user access open"
        );
        assert!(
            Arch::user_access_blocked(),
            "user access stayed open after the catch"
        );
    }

    // Passes by returning, as breakpoint_returns does.
    #[test_case]
    fn a_spurious_interrupt_returns() {
        Arch::raise_spurious();
    }

    // The kernel can't resume from an FIQ, so once the handler returns, the
    // entry has to report it rather than halt with nothing on screen.
    #[cfg(target_arch = "aarch64")]
    #[test_case]
    const A_RETURNED_FIQ_PANICS: crate::testing::ExpectPanic = crate::testing::ExpectPanic {
        name: "thisos_kernel::trap::tests::a_returned_fiq_panics",
        run: thisos_arch_aarch64::raise_spurious_fiq,
        message: "returned from EL1h FIQ",
        after: None,
    };

    #[test_case]
    fn legacy_interrupts_stay_masked() {
        assert!(Arch::legacy_interrupts_masked());
    }

    #[test_case]
    fn trap_stacks_have_guard_pages() {
        let mut count = 0;
        for stack in Arch::trap_stacks() {
            count += 1;
            let align = (2 * (stack.end - stack.start)).next_power_of_two();
            assert_eq!(stack.start % align, 0, "stack {stack:#x?} alignment");
            let guard = stack.start - 4096;
            assert_eq!(paging::translate(guard), None, "guard page {guard:#x}");
            for page in stack.step_by(4096) {
                let Some(t) = paging::translate(page) else {
                    panic!("trap stack page {page:#x} isn't mapped");
                };
                assert!(t.writable && !t.executable, "{page:#x}: {t:?}");
            }
        }
        assert!(count > 0, "no trap stacks");
    }

    // The CPU can't push the page fault's frame onto a full stack, so the
    // overflow reaches the handler as a double fault, on a stack of its own.
    // The tests after this one show the kernel carries on after it. The
    // probe runs with interrupts off, and the timer ticking after it shows
    // they came back on.
    #[test_case]
    fn a_stack_overflow_double_faults() {
        testing::expect_trap(|trap| trap == Trap::DoubleFault, Probe::OverflowStack);
        let start = timer::ticks();
        testing::spin_until("a tick after the overflow", || timer::ticks() > start);
    }

    // An NMI can arrive on a SYSCALL's first instruction, in ring 0 with
    // user code's GS base, where the saved CS says nothing about which base
    // is active. Its handler must still find this CPU's index, and its
    // return must leave user code's base active, which the catch checks.
    #[cfg(target_arch = "x86_64")]
    #[test_case]
    fn an_nmi_on_user_gs_finds_its_cpu() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::cpu;

        static SEEN: AtomicUsize = AtomicUsize::new(usize::MAX);
        testing::expect_trap(
            |trap| {
                SEEN.store(cpu::id(), Ordering::Relaxed);
                trap == Trap::Other {
                    vector: 2,
                    error: 0,
                }
            },
            Probe::NmiWithUserGs,
        );
        assert_eq!(SEEN.load(Ordering::Relaxed), cpu::id());
    }
}
