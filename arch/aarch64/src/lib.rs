//! ThisOS's aarch64 support: the implementation of `thisos-hal` for aarch64
//! under QEMU's virt machine, booted by Limine. Together with the x86_64
//! arch crate it is the only code allowed unsafe code, and the only crate
//! that may depend on `aarch64-cpu` and `limine`.
//!
//! Not every HAL call is ported yet: those that aren't panic with
//! `<call> isn't ported to aarch64`, and the types behind them are the
//! stand-ins in `unported`, which nothing can construct.

#![no_std]
#![deny(clippy::unwrap_used, clippy::expect_used)]

// paging's tests run on the host, under Miri too. The crate's assembly
// doesn't assemble there, so test builds have only paging, the heap it
// maps and the Once it records the stacks' window in.
#[cfg(test)]
extern crate std;

#[cfg(not(test))]
mod backtrace;
#[cfg(not(test))]
mod boot;
#[cfg(not(test))]
mod catch;
#[cfg(not(test))]
mod context;
#[cfg(not(test))]
mod devices;
#[cfg(not(test))]
mod framebuffer;
#[cfg(not(test))]
mod gic;
mod heap;
#[cfg(not(test))]
mod memory;
mod once;
mod paging;
#[cfg(not(test))]
mod sysreg;
#[cfg(not(test))]
mod timer;
#[cfg(not(test))]
mod trap;
#[cfg(not(test))]
mod unported;
#[cfg(not(test))]
mod user;

#[cfg(not(test))]
use core::arch::asm;
#[cfg(not(test))]
use core::ops::Range;
#[cfg(not(test))]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(test))]
use aarch64_cpu::asm::{barrier, wfi};
#[cfg(not(test))]
use aarch64_cpu::registers::{DAIF, ReadWriteable, Readable, Writeable};
#[cfg(not(test))]
use thisos_frames::Frame;
#[cfg(not(test))]
use thisos_hal::{
    BacktraceEnd, Cpu, Framebuffer, HeapStats, Platform, Probe, Stack, TestEnd, TimerFrequencies,
    TimerFrequency, Translation, UserProgram,
};

#[cfg(not(test))]
pub use context::Context;
#[cfg(not(test))]
pub use framebuffer::FramebufferPixels;
#[cfg(not(test))]
pub use memory::MemoryMap;
pub use paging::{KernelPageTables, UserSpace};
#[cfg(not(test))]
pub use trap::{TrapFrame, raise_spurious_fiq};
#[cfg(not(test))]
pub use unported::NoSerial;

/// The aarch64 architecture, as the kernel sees it through `thisos-hal`.
#[cfg(not(test))]
#[derive(Debug)]
pub struct Aarch64;

#[cfg(not(test))]
impl Cpu for Aarch64 {
    fn halt() -> ! {
        DAIF.write(DAIF::D::Masked + DAIF::A::Masked + DAIF::I::Masked + DAIF::F::Masked);
        // The framebuffer is non-cacheable, so the last pixels may still
        // sit in a write buffer; this drains them to the display.
        barrier::dsb(barrier::SY);
        // Masked interrupts still end wfi, so go straight back.
        loop {
            wfi();
        }
    }

    // Not inlined, so that x29 here is this function's own frame and the
    // first return address is in the caller.
    #[inline(never)]
    fn backtrace(f: impl FnMut(u64)) -> BacktraceEnd {
        let fp: u64;
        // SAFETY: Copying x29 into a register reads no memory and changes no
        // other state.
        unsafe {
            asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags));
        }
        backtrace::walk(fp, f)
    }

    fn breakpoint() {
        // SAFETY: brk traps, and the trap handler returns past it with
        // every register as it was.
        unsafe {
            asm!("brk #0xf000", options(nomem, nostack, preserves_flags));
        }
    }

    fn enable_interrupts() {
        DAIF.modify(DAIF::I::Unmasked);
    }

    // Masked, so that an interrupt between the caller's check and the wfi
    // still ends it: a pending interrupt ends wfi whether or not it is
    // masked. The isb takes it before this returns.
    fn wait_for_interrupt() {
        DAIF.modify(DAIF::I::Masked);
        wfi();
        DAIF.modify(DAIF::I::Unmasked);
        barrier::isb(barrier::SY);
    }

    fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
        let masked = DAIF.is_set(DAIF::I);
        DAIF.modify(DAIF::I::Masked);
        let r = f();
        if !masked {
            DAIF.modify(DAIF::I::Unmasked);
        }
        r
    }

    fn catch(probe: Probe) {
        catch::catch(probe);
    }

    fn user_access_blocked() -> bool {
        // PSTATE.PAN reads back in bit 22.
        sysreg::read_pan() & (1 << 22) != 0
    }
    fn enter_user(ip: u64, sp: u64, arg: u64) -> ! {
        user::enter_user(ip, sp, arg)
    }
}

#[cfg(not(test))]
impl Platform for Aarch64 {
    type Serial = NoSerial;
    type Pixels = FramebufferPixels;
    type TrapFrame = TrapFrame;
    type MemoryMap = MemoryMap;
    type PageTables = KernelPageTables;
    type Context = Context;

    const ELF_MACHINE: u16 = 183;

    fn take_page_tables(alloc: &mut dyn FnMut() -> Option<Frame>) -> Option<KernelPageTables> {
        // SAFETY: boot_info recorded where Limine mapped all physical memory
        // and the framebuffer, and found the GIC's registers in the device
        // tree, and linker.ld puts every function in .text.
        // Limine's TTBR0 maps nothing the kernel uses: _start left Limine's
        // stack, and everything else is in the upper half.
        unsafe {
            paging::take(
                boot::phys_offset(),
                Self::guard_page(),
                boot::text(),
                boot::framebuffer_range(),
                &devices::get().registers(),
                alloc,
            )
        }
    }

    fn activate_kernel_tables() {
        paging::activate_kernel();
    }

    fn move_trap_stacks(stack: &mut dyn FnMut(usize) -> Stack<Self>) {
        trap::move_stack(stack);
    }

    fn trap_stacks() -> impl Iterator<Item = Range<u64>> {
        trap::stacks()
    }

    fn new_context(stack: Stack<Self>, entry: fn(Context) -> !) -> Context {
        context::new(stack, entry)
    }

    fn into_stack(context: Context) -> Option<Stack<Self>> {
        context::into_stack(context)
    }

    fn switch(to: Context) -> Context {
        context::switch(to)
    }

    fn guard_page() -> u64 {
        backtrace::stack_bottom() - 4096
    }

    fn probe_target(probe: Probe) -> Option<u64> {
        catch::target(probe)
    }

    fn panic_serial() -> Option<NoSerial> {
        None
    }

    fn panic_framebuffer() -> Option<Framebuffer<FramebufferPixels>> {
        boot::PANIC_FRAMEBUFFER.try_lock()?.take()
    }

    fn heap_stats() -> HeapStats {
        heap::stats()
    }

    fn start_timer(hz: u32) -> TimerFrequency {
        let devices = devices::get();
        gic::init(boot::phys_offset(), &devices);
        timer::start(hz, devices.timer)
    }

    // CNTFRQ_EL0 is all there is: QEMU's virt machine has no second clock
    // fine enough to check it against.
    fn timer_frequencies() -> TimerFrequencies {
        TimerFrequencies {
            reported: None,
            measured: None,
        }
    }

    // The GIC is the only interrupt controller.
    fn legacy_interrupts_masked() -> bool {
        true
    }

    fn translate_active(addr: u64) -> Option<Translation> {
        paging::translate_active(boot::phys_offset(), addr)
    }

    fn fill_frame(frame: &mut Frame, offset: usize, bytes: &[u8]) {
        paging::fill_frame(boot::phys_offset(), frame, offset, bytes);
    }

    fn read_frame(frame: &Frame, offset: usize, buf: &mut [u8]) {
        paging::read_frame(boot::phys_offset(), frame, offset, buf);
    }

    fn write_frame(frame: &Frame, offset: usize, bytes: &[u8]) {
        paging::write_frame(boot::phys_offset(), frame, offset, bytes);
    }

    fn copy_from_user(addr: u64, buf: &mut [u8]) -> bool {
        paging::copy_from_user(boot::phys_offset(), addr, buf)
    }

    fn copy_to_user(addr: u64, buf: &[u8]) -> bool {
        paging::copy_to_user(boot::phys_offset(), addr, buf)
    }

    fn write_return(frame: &mut Frame) {
        paging::write_return(boot::phys_offset(), frame);
    }

    fn raise_spurious() {
        trap::raise_spurious();
    }

    fn write_user_program(frame: &mut Frame, program: UserProgram) {
        user::write_program(boot::phys_offset(), frame, program);
    }

    // Through semihosting's SYS_EXIT_EXTENDED, which QEMU turns into its
    // exit status, the same as x86_64's debug exit device gives. Without
    // -semihosting-config the hlt is undefined, and the panic path its trap
    // takes calls this again, which then only halts.
    fn exit_qemu(end: TestEnd) -> ! {
        static EXITING: AtomicBool = AtomicBool::new(false);
        if devices::qemu_virt() && !EXITING.swap(true, Ordering::Relaxed) {
            const SYS_EXIT_EXTENDED: u64 = 0x20;
            const APPLICATION_EXIT: u64 = 0x2_0026;
            let block = [APPLICATION_EXIT, u64::from(end.status())];
            // SAFETY: Under QEMU with semihosting, the call reads the two
            // words at x1 and ends the process; without it, the hlt traps as
            // an undefined instruction, and EXITING keeps it from running
            // again. On real hardware the root isn't QEMU's, so it never
            // runs.
            unsafe {
                asm!(
                    "hlt #0xf000",
                    inout("x0") SYS_EXIT_EXTENDED => _,
                    in("x1") block.as_ptr(),
                    options(nostack, readonly),
                );
            }
        }
        Self::halt()
    }
}

/// Panics for a HAL call that has no aarch64 implementation yet.
#[cfg(not(test))]
#[track_caller]
fn not_ported(call: &str) -> ! {
    panic!("{call} isn't ported to aarch64")
}

/// The pointer at `addr`. The one place the crate turns an integer into a
/// pointer, for an address that reaches it as an integer from a register,
/// from the loader, from a symbol the crate's own assembly defines, or from
/// the page tables, as the heap's base does.
#[expect(
    clippy::disallowed_methods,
    reason = "the address comes from the hardware, the loader or the page tables, never from a Rust pointer"
)]
fn ptr_at<T>(addr: u64) -> *mut T {
    let Ok(addr) = usize::try_from(addr) else {
        unreachable!("aarch64 addresses are 64 bits");
    };
    core::ptr::with_exposed_provenance_mut(addr)
}

/// `addr` as a `u64`, which on aarch64 always holds a `usize`.
fn to_u64(addr: usize) -> u64 {
    let Ok(addr) = u64::try_from(addr) else {
        unreachable!("aarch64 addresses are 64 bits");
    };
    addr
}

#[cfg(not(test))]
#[doc(hidden)]
pub use boot::boot_info;

/// Defines the kernel's entry point: `entry!(kmain, trap)` makes the arch
/// crate's `_start` call `kmain(thisos_hal::BootInfo<Aarch64>) -> !` on the
/// boot stack, with `trap`, a `thisos_hal::TrapHandler`, as the handler for
/// every trap. The kernel uses this macro so that it never names the boot
/// loader's types.
#[macro_export]
macro_rules! entry {
    ($main:path, $trap:path) => {
        // Inside an anonymous const, so the kernel can't name the entry
        // function and call it a second time.
        const _: () = {
            // _start calls it by this name.
            #[unsafe(no_mangle)]
            extern "C" fn __thisos_entry() -> ! {
                // SAFETY: _start calls this once, and nothing else can name
                // it.
                let boot = unsafe { $crate::boot_info($trap) };
                $main(boot)
            }
        };
    };
}
