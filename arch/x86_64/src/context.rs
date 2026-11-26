//! Kernel threads' contexts, and switching between them. A suspended
//! thread's callee-saved registers sit on its own stack, under the address
//! its switch returns to, and its context holds the stack pointer to them.

use core::arch::{asm, global_asm};
use core::mem::offset_of;
use core::ops::Range;

use x86_64::VirtAddr;
use x86_64::instructions::interrupts;

use crate::paging::Stack;
use crate::{backtrace, trap};

/// A suspended thread: the stack pointer its registers were pushed at,
/// and the stack it runs on, which it holds: a `Stack`'s range, or the
/// boot thread's. A switch hands the leaving thread's on to the thread it
/// resumes, which makes its context.
#[derive(Debug)]
pub struct Context {
    rsp: u64,
    stack: Range<u64>,
}

// thisos_switch takes the stack pointer to resume in rdi, and returns the
// one it left in rax, in the thread it resumes. It leaves rsi and rdx
// alone, so the thread that resumes finds in them what the one that left
// put there: its stack. DS, ES, FS and GS go with the thread, since ring 3
// can load them and would otherwise read what another process left there.
// Loading GS's selector clears its base on Intel, so the load lands on user
// code's base, between two SWAPGS; interrupts are off, and an NMI or #MC
// meanwhile swaps by IA32_GS_BASE (see percpu).
// A new thread's stack holds a Start, so the switch loads null into the
// four, pops entry into r12 and zero into rbp, the end of the frame-pointer
// chain, and returns into thisos_thread_start, which calls start with the
// stack 16-aligned.
global_asm!(
    ".pushsection .text.thisos_switch, \"ax\", @progbits",
    ".globl thisos_switch",
    ".hidden thisos_switch",
    "thisos_switch:",
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov ecx, ds",
    "push rcx",
    "mov ecx, es",
    "push rcx",
    "mov ecx, fs",
    "push rcx",
    "mov ecx, gs",
    "push rcx",
    "mov rax, rsp",
    "mov rsp, rdi",
    "pop rcx",
    "swapgs",
    "mov gs, ecx",
    "swapgs",
    "pop rcx",
    "mov fs, ecx",
    "pop rcx",
    "mov es, ecx",
    "pop rcx",
    "mov ds, ecx",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    ".globl thisos_thread_start",
    ".hidden thisos_thread_start",
    "thisos_thread_start:",
    "mov rdi, rax",
    "mov rcx, r12",
    "call {start}",
    "ud2",
    ".popsection",
    start = sym start,
);

unsafe extern "C" {
    fn thisos_thread_start();
}

/// What `new` writes at the top of a new thread's stack: the registers
/// `thisos_switch` pops, in the order it pops them, its return address,
/// and two words that make the stack 16-aligned at `call start`.
#[repr(C)]
struct Start {
    gs: u64,
    fs: u64,
    es: u64,
    ds: u64,
    r15: u64,
    r14: u64,
    r13: u64,
    r12: fn(Context) -> !,
    rbx: u64,
    rbp: u64,
    ret: unsafe extern "C" fn(),
    pad: [u64; 2],
}

// After the switch's ret, rsp is the stack's page-aligned end less this.
const _: () = assert!((size_of::<Start>() - offset_of!(Start, pad)) % 16 == 0);

/// A thread that runs `entry` on `stack` once switched to; see
/// `Platform::new_context`.
pub fn new(stack: Stack, entry: fn(Context) -> !) -> Context {
    let stack = stack.into_range();
    // The address comes from a page map_stack mapped in the reserved level
    // 4 entry.
    let top = VirtAddr::new(stack.end)
        .as_mut_ptr::<Start>()
        .wrapping_sub(1);
    let start = Start {
        gs: 0,
        fs: 0,
        es: 0,
        ds: 0,
        r15: 0,
        r14: 0,
        r13: 0,
        r12: entry,
        rbx: 0,
        rbp: 0,
        ret: thisos_thread_start,
        pad: [0; 2],
    };
    // SAFETY: Only map_stack makes a Stack, mapped writable, and this
    // consumed it, so the Start at its top is mapped, and nothing else
    // reads or writes it. map_stack's stacks are whole pages, so top is
    // aligned for a Start.
    unsafe {
        top.write(start);
    }
    Context {
        rsp: VirtAddr::from_ptr(top).as_u64(),
        stack,
    }
}

/// The stack `context` held, unless it is the boot thread's; see
/// `Platform::into_stack`.
pub fn into_stack(context: Context) -> Option<Stack> {
    Stack::of_suspended(context.stack)
}

/// Suspends the running thread and resumes `to`; see `Platform::switch`.
pub fn switch(to: Context) -> Context {
    assert!(
        !interrupts::are_enabled(),
        "switching threads with interrupts on"
    );
    let from = backtrace::stack();
    trap::set_kernel_stack(to.stack.end);
    backtrace::set_stack(to.stack);
    let (rsp, start, end): (u64, u64, u64);
    // SAFETY: to came from new or from a switch, which consumed it, so
    // to.rsp points at a Start or at the registers a switch pushed, on a
    // stack nothing has run on since, and this is the only resumption.
    // thisos_switch pushes the callee-saved registers and the four data
    // segment registers on this thread's stack, and returns here only when
    // a later switch resumes it, with those registers and rsp as they were;
    // the C ABI clobbers cover the rest, rcx among them. A selector it loads
    // is null or one this thread's ring 3 code loaded, which the kernel can
    // load too. The kernel's own accesses use only GS's base, which the GS
    // load leaves alone, since it lands between two SWAPGS on user code's.
    // Interrupts are off, so no handler runs on either stack between the
    // two halves of the switch, and only an NMI, #MC or #DB can come
    // between the SWAPGS, whose stubs swap by IA32_GS_BASE.
    unsafe {
        asm!(
            "call thisos_switch",
            in("rdi") to.rsp,
            inout("rsi") from.start => start,
            inout("rdx") from.end => end,
            lateout("rax") rsp,
            clobber_abi("C"),
        );
    }
    Context {
        rsp,
        stack: start..end,
    }
}

/// A new thread's first Rust frame, which `thisos_thread_start` calls with
/// what the switch that resumed it left: the context of the thread that
/// switched to it, and the `entry` that `new` put in r12.
#[expect(
    improper_ctypes_definitions,
    reason = "only thisos_thread_start calls it, passing on the Rust fn pointer new stored"
)]
extern "C" fn start(rsp: u64, stack_start: u64, stack_end: u64, entry: fn(Context) -> !) -> ! {
    entry(Context {
        rsp,
        stack: stack_start..stack_end,
    })
}
