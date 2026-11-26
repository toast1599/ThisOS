//! Each CPU's own state, in a block of [`BLOCKS`] that GS's base points at
//! while the CPU runs kernel code. User code's GS base waits in
//! `IA32_KERNEL_GS_BASE` meanwhile: the trap and SYSCALL stubs swap the two
//! with SWAPGS on the way in from ring 3 and on the way back. The stubs for
//! the vectors with IST stacks can arrive in ring 0 before the SYSCALL
//! stub's SWAPGS, so they decide from `IA32_GS_BASE` itself: user code's base
//! is never an upper-half address, since FSGSBASE stays off and no syscall
//! sets one, so only a selector load can change it, to a descriptor's base
//! of 0.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::offset_of;
use core::sync::atomic::{AtomicU64, Ordering};

use thisos_locks::MAX_CPUS;
use x86_64::VirtAddr;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
use x86_64::registers::model_specific::{Efer, EferFlags, GsBase, KernelGsBase, Msr};
use x86_64::structures::gdt::GlobalDescriptorTable;
use x86_64::structures::tss::TaskStateSegment;

use crate::once::Once;
use crate::{apic, pat, trap};

/// A CPU's state. The stubs reach its fields at GS-relative offsets.
#[repr(C)]
pub struct Block {
    /// The CPU's index among the running CPUs, 0 for the boot CPU. First,
    /// so that a read through a base of 0 faults.
    index: AtomicU64,
    /// The user stack pointer the SYSCALL stub saves until it has pushed it.
    user_rsp: AtomicU64,
    /// The stack a SYSCALL starts on, the TSS's RSP0 again.
    kernel_rsp: AtomicU64,
    /// The running thread's stack, which backtraces stay inside.
    pub stack_start: AtomicU64,
    pub stack_end: AtomicU64,
    /// The stack pointer `thisos_catch` saved the registers at while a
    /// catch runs, and 0 otherwise.
    pub saved_rsp: AtomicU64,
    pub tss: Tss,
    pub gdt: Once<GlobalDescriptorTable>,
}

/// The MSR that holds the active GS base.
pub const IA32_GS_BASE: u32 = 0xc000_0101;

/// Offsets for the stubs' GS-relative operands.
pub const USER_RSP: usize = offset_of!(Block, user_rsp);
pub const KERNEL_RSP: usize = offset_of!(Block, kernel_rsp);
pub const SAVED_RSP: usize = offset_of!(Block, saved_rsp);

impl Block {
    const fn new() -> Self {
        Self {
            index: AtomicU64::new(0),
            user_rsp: AtomicU64::new(0),
            kernel_rsp: AtomicU64::new(0),
            stack_start: AtomicU64::new(0),
            stack_end: AtomicU64::new(0),
            saved_rsp: AtomicU64::new(0),
            tss: Tss {
                _align: 0,
                tss: UnsafeCell::new(TaskStateSegment::new()),
            },
            gdt: Once::new(),
        }
    }

    /// Makes `top` the stack a trap from user code starts on, through the
    /// IDT or SYSCALL. A trap reads it on the block's own CPU, which sees
    /// its own stores in order, or on a CPU that `prepare` wrote it for
    /// before starting it.
    pub fn set_kernel_stack(&self, top: u64) {
        self.tss.word(RSP0).store(top, Ordering::Relaxed);
        self.kernel_rsp.store(top, Ordering::Relaxed);
    }
}

static BLOCKS: [Block; MAX_CPUS] = [const { Block::new() }; MAX_CPUS];

/// The TSS, 4 bytes into an 8-aligned block. The x86_64 crate's TSS is
/// packed, with RSP0 and the IST slots 4 bytes past an 8-byte boundary, so
/// here each of them is 8-aligned, and one atomic store writes it whole:
/// an NMI never finds half of a stack address in a slot.
#[repr(C, align(8))]
pub struct Tss {
    _align: u32,
    tss: UnsafeCell<TaskStateSegment>,
}

// SAFETY: Rust code reaches the TSS's fields only through `word`, whose
// atomics make every read and write whole. The CPU reads the TSS when it
// takes a trap, and never writes a 64-bit field.
unsafe impl Sync for Tss {}

/// Where RSP0 and the first IST slot are in the TSS.
const RSP0: usize = offset_of!(TaskStateSegment, privilege_stack_table);
const IST: usize = offset_of!(TaskStateSegment, interrupt_stack_table);

const _: () =
    assert!((offset_of!(Tss, tss) + RSP0) % 8 == 0 && (offset_of!(Tss, tss) + IST) % 8 == 0);
const _: () = assert!(offset_of!(Block, tss) % 8 == 0);

impl Tss {
    /// IST slot `index`, as an atomic.
    pub fn slot(&self, index: u16) -> &AtomicU64 {
        self.word(IST + 8 * usize::from(index))
    }

    /// The TSS itself, for its descriptor.
    pub fn get(&self) -> *mut TaskStateSegment {
        self.tss.get()
    }

    /// The stack pointer slot at `offset` into the TSS, RSP0 or an IST
    /// slot, as an atomic.
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "Tss's layout puts RSP0 and every IST slot on an 8-byte boundary, which the assert above checks"
    )]
    fn word(&self, offset: usize) -> &AtomicU64 {
        let word = self
            .tss
            .get()
            .cast::<u8>()
            .wrapping_add(offset)
            .cast::<u64>();
        // SAFETY: Its callers pass RSP0's offset or an IST slot's, inside
        // this TSS, so word lives as long as the reference returned. It is
        // 8-aligned, as Tss's layout makes those slots. Rust code reaches it
        // only through atomics, since Tss hands out no other reference.
        unsafe { AtomicU64::from_ptr(word) }
    }
}

/// The running CPU's index.
pub fn index() -> usize {
    let index: u64;
    // SAFETY: From set_gs on, GS's base is this CPU's block in kernel code,
    // and its first word is the index; boot_info runs set_gs first. The
    // read changes nothing.
    unsafe {
        asm!(
            "mov {}, qword ptr gs:[0]",
            out(reg) index,
            options(nostack, preserves_flags, readonly),
        );
    }
    let Ok(index) = usize::try_from(index) else {
        unreachable!("x86_64's usize is 64 bits");
    };
    index
}

/// The running CPU's block.
pub fn this() -> &'static Block {
    &BLOCKS[index()]
}

/// Points GS's base at CPU `index`'s block, and user code's at 0.
///
/// # Safety
///
/// Only the CPU that is to be CPU `index` may call it, and no other CPU may
/// run as `index`.
pub unsafe fn set_gs(index: usize) {
    let block = &BLOCKS[index];
    let Ok(word) = u64::try_from(index) else {
        unreachable!("x86_64's usize is 64 bits");
    };
    block.index.store(word, Ordering::Relaxed);
    GsBase::write(VirtAddr::from_ptr(block));
    KernelGsBase::write(VirtAddr::zero());
}

/// Gives CPU `index` its trap stacks, `ist` by IST slot, and `kernel` as
/// the stack a trap from user code starts on, before that CPU runs
/// [`init_cpu`].
pub fn prepare(index: usize, ist: [u64; 4], kernel: u64) {
    let block = &BLOCKS[index];
    for (slot, top) in (0..).zip(ist) {
        block.tss.slot(slot).store(top, Ordering::Relaxed);
    }
    block.set_kernel_stack(kernel);
}

/// Sets up the CPU it runs on as CPU `index`: its descriptor tables and
/// GS base, CR0, CR4 and EFER, and the SYSCALL MSRs. An AP also takes the
/// PAT the boot CPU uses, global pages and its x2APIC, which the boot CPU
/// sets up later, in `pat`, `paging` and `apic`. Panics if the CPU lacks
/// SMEP or SMAP.
///
/// # Safety
///
/// Each CPU calls it once, with its own index, after [`prepare`] for that
/// index, with interrupts off, before anything on it uses the segment
/// registers or the descriptor tables, and before it traps. On an AP, the
/// boot CPU must have run it first.
pub unsafe fn init_cpu(index: usize) {
    // CPUID leaf 7 reports SMEP in EBX bit 7, SMAP in bit 20, and UMIP in
    // ECX bit 2.
    let leaf = core::arch::x86_64::__cpuid_count(7, 0);
    let both = (1 << 7) | (1 << 20);
    assert!(leaf.ebx & both == both, "the CPU lacks SMEP or SMAP");
    let block = &BLOCKS[index];
    // SAFETY: The caller runs this before anything uses the segment
    // registers or descriptor tables, with the block prepare filled.
    unsafe {
        trap::load_tables(block);
    }
    // After the tables: loading a GS selector clears the base on Intel.
    // SAFETY: The caller passes this CPU's own index.
    unsafe {
        set_gs(index);
    }
    // Written whole, so an AP's CD and NW from INIT are cleared and no bit
    // the firmware set on the boot CPU leaks through. The kernel saves no
    // x87, SSE or AVX state, so user code may use none of them: with EM
    // set x87 instructions trap, and with OSFXSR and OSXSAVE clear so do
    // SSE and AVX instructions, and XSETBV. AM stays clear, so RFLAGS.AC
    // means user access to SMAP alone and never an alignment check.
    // SAFETY: The kernel is built soft-float and runs none of them either.
    // PE and PG are on already, and WP only stops writes to read-only pages.
    unsafe {
        Cr0::write(
            Cr0Flags::PROTECTED_MODE_ENABLE
                | Cr0Flags::MONITOR_COPROCESSOR
                | Cr0Flags::EMULATE_COPROCESSOR
                | Cr0Flags::EXTENSION_TYPE
                | Cr0Flags::NUMERIC_ERROR
                | Cr0Flags::WRITE_PROTECT
                | Cr0Flags::PAGING,
        );
    }
    let mut cr4 = Cr4Flags::PHYSICAL_ADDRESS_EXTENSION
        | Cr4Flags::MACHINE_CHECK_EXCEPTION
        | Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION
        | Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION;
    if leaf.ecx & (1 << 2) != 0 {
        cr4 |= Cr4Flags::USER_MODE_INSTRUCTION_PREVENTION;
    }
    // The boot CPU turns global pages on in paging::take, whose toggle also
    // flushes the loader's, and keeps them as they are until then.
    if index != 0 || Cr4::read().contains(Cr4Flags::PAGE_GLOBAL) {
        cr4 |= Cr4Flags::PAGE_GLOBAL;
    }
    // SAFETY: PAE is on already, in long mode. Global pages only change
    // what the TLB keeps, and the upper half, the only one with global
    // leaves, is the same in every space. SMEP and SMAP only stop the
    // kernel reaching user pages, which it reaches only through user
    // copies, with AC set; the loader maps nothing the kernel uses for user
    // code. MCE delivers machine checks to their handler instead of
    // shutting the CPU down. UMIP only stops ring 3 reading descriptor
    // table addresses.
    unsafe {
        Cr4::write(cr4);
    }
    // SAFETY: Long mode and no-execute are on already. SYSCALL enters the
    // stub load_syscall below names, which runs on the stack the block
    // holds, and nothing reaches ring 3 to run it before a switch has set
    // that stack.
    unsafe {
        Efer::write(
            EferFlags::LONG_MODE_ENABLE
                | EferFlags::LONG_MODE_ACTIVE
                | EferFlags::NO_EXECUTE_ENABLE
                | EferFlags::SYSTEM_CALL_EXTENSIONS,
        );
    }
    trap::load_syscall();
    if index != 0 {
        if let Some(value) = pat::written() {
            // SAFETY: The boot CPU wrote this PAT, so the CPU has one, and
            // its mappings, which this CPU shares, already rely on it.
            unsafe { Msr::new(pat::IA32_PAT).write(value) };
        }
        apic::enable_on_ap();
    }
}
