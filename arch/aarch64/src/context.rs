//! Kernel threads' contexts, and switching between them. A suspended
//! thread's callee-saved registers and its `TPIDR_EL0` sit on its own
//! stack, and its context holds the stack pointer to them.

use core::arch::{asm, global_asm};
use core::mem::offset_of;
use core::ops::Range;

use aarch64_cpu::registers::{DAIF, Readable};

use crate::paging::Stack;
use crate::{backtrace, ptr_at, to_u64};

/// A suspended thread: the stack pointer its registers were saved at, and
/// the stack it runs on, which it holds: a `Stack`'s range, or the boot
/// thread's. A switch hands the leaving thread's on to the thread it
/// resumes, which makes its context.
#[derive(Debug)]
pub struct Context {
    sp: u64,
    stack: Range<u64>,
}

// thisos_switch takes the stack pointer to resume in x0, and returns the
// one it left in x0, in the thread it resumes. It leaves x1 and x2 alone,
// so the thread that resumes finds in them what the one that left put
// there: its stack. TPIDR_EL0 goes with the thread, since EL0 can write it
// and would otherwise read what another process left there. A new
// thread's stack holds a Start, so the switch loads entry into x19 and
// zero into x29, the end of the frame-record chain, and returns into
// thisos_thread_start, which calls start.
global_asm!(
    ".pushsection .text.thisos_switch, \"ax\", %progbits",
    ".globl thisos_switch",
    ".hidden thisos_switch",
    "thisos_switch:",
    "sub sp, sp, #{size}",
    "stp x19, x20, [sp, #0x00]",
    "stp x21, x22, [sp, #0x10]",
    "stp x23, x24, [sp, #0x20]",
    "stp x25, x26, [sp, #0x30]",
    "stp x27, x28, [sp, #0x40]",
    "stp x29, x30, [sp, #0x50]",
    "mrs x9, tpidr_el0",
    "str x9, [sp, #0x60]",
    "mov x9, sp",
    "mov sp, x0",
    "mov x0, x9",
    "ldr x9, [sp, #0x60]",
    "msr tpidr_el0, x9",
    "ldp x19, x20, [sp, #0x00]",
    "ldp x21, x22, [sp, #0x10]",
    "ldp x23, x24, [sp, #0x20]",
    "ldp x25, x26, [sp, #0x30]",
    "ldp x27, x28, [sp, #0x40]",
    "ldp x29, x30, [sp, #0x50]",
    "add sp, sp, #{size}",
    "ret",
    ".globl thisos_thread_start",
    ".hidden thisos_thread_start",
    "thisos_thread_start:",
    "mov x3, x19",
    "bl {start}",
    "udf #0",
    ".popsection",
    size = const size_of::<Start>(),
    start = sym start,
);

unsafe extern "C" {
    fn thisos_thread_start();
}

/// What `new` writes at the top of a new thread's stack: the registers
/// `thisos_switch` loads, where it loads them from.
#[repr(C)]
struct Start {
    x19: fn(Context) -> !,
    x20_x28: [u64; 9],
    x29: u64,
    x30: unsafe extern "C" fn(),
    tpidr_el0: u64,
    pad: u64,
}

// The switch's offsets, and SP 16-aligned at the stack's top after its add.
const _: () = assert!(
    offset_of!(Start, x29) == 0x50
        && offset_of!(Start, tpidr_el0) == 0x60
        && size_of::<Start>() % 16 == 0
);

/// A thread that runs `entry` on `stack` once switched to; see
/// `Platform::new_context`.
pub fn new(stack: Stack, entry: fn(Context) -> !) -> Context {
    let stack = stack.into_range();
    let sp = stack.end - to_u64(size_of::<Start>());
    let start = Start {
        x19: entry,
        x20_x28: [0; 9],
        x29: 0,
        x30: thisos_thread_start,
        tpidr_el0: 0,
        pad: 0,
    };
    // SAFETY: Only map_stack makes a Stack, mapped writable, and this
    // consumed it, so the Start at its top is mapped, and nothing else
    // reads or writes it. map_stack's stacks are whole pages, so sp is
    // aligned for a Start.
    unsafe {
        ptr_at::<Start>(sp).write(start);
    }
    Context { sp, stack }
}

/// The stack `context` held, unless it is the boot thread's; see
/// `Platform::into_stack`.
pub fn into_stack(context: Context) -> Option<Stack> {
    Stack::of_suspended(context.stack)
}

/// Suspends the running thread and resumes `to`; see `Platform::switch`.
pub fn switch(to: Context) -> Context {
    assert!(DAIF.is_set(DAIF::I), "switching threads with interrupts on");
    let from = backtrace::stack();
    backtrace::set_stack(to.stack);
    let (sp, start, end): (u64, u64, u64);
    // SAFETY: to came from new or from a switch, which consumed it, so
    // to.sp points at a Start or at the registers a switch saved, on a
    // stack nothing has run on since, and this is the only resumption.
    // thisos_switch saves the callee-saved registers on this thread's
    // stack, and returns here only when a later switch resumes it, with
    // those registers and SP as they were; the C ABI clobbers cover the
    // rest, x30 included. Interrupts are off, so no handler runs on either
    // stack between the two halves of the switch.
    unsafe {
        asm!(
            "bl thisos_switch",
            inlateout("x0") to.sp => sp,
            inout("x1") from.start => start,
            inout("x2") from.end => end,
            clobber_abi("C"),
        );
    }
    Context {
        sp,
        stack: start..end,
    }
}

/// A new thread's first Rust frame, which `thisos_thread_start` calls with
/// what the switch that resumed it left: the context of the thread that
/// switched to it, and the `entry` that `new` put in x19.
#[expect(
    improper_ctypes_definitions,
    reason = "only thisos_thread_start calls it, passing on the Rust fn pointer new stored"
)]
extern "C" fn start(sp: u64, stack_start: u64, stack_end: u64, entry: fn(Context) -> !) -> ! {
    entry(Context {
        sp,
        stack: stack_start..stack_end,
    })
}
