//! Traps: each CPU's GDT and TSS, which give it stacks to take some traps
//! on, the IDT they share, and the stubs that save the registers in a
//! [`TrapFrame`] and call the kernel's handler.

use core::arch::{asm, global_asm};
use core::cell::UnsafeCell;
use core::fmt;
use core::ops::Range;
use core::sync::atomic::Ordering;

use thisos_hal::{Access, BacktraceEnd, Cpu, Trap, TrapHandler};
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::control::Cr2;
use x86_64::registers::model_specific::{LStar, SFMask, Star};
use x86_64::registers::rflags::{self, RFlags};
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::idt::{Entry, EntryOptions, InterruptDescriptorTable};

use crate::once::Once;
use crate::paging::USER_SPACE;
use crate::{X86_64, apic, backtrace, catch, paging, percpu};

// Each stub sits 16 bytes after the one before it, so that the IDT can find
// stub n at thisos_trap_stubs + 16n. The .org fails to assemble if a stub
// outgrows its 16 bytes. A stub pushes a zero in place of the error code
// for the vectors whose error code the CPU doesn't push, so that every
// trap leaves the same frame, then pushes the vector. The common path
// pushes the general registers to complete the TrapFrame, calls trap_entry
// with it, and undoes all of that. The CPU aligns the stack to 16 bytes
// before it pushes its five words, so after the stub's two and the fifteen
// registers it is aligned again for the call, as the C ABI wants. The
// vector is pushed as hand-encoded push imm32, because Intel syntax reads a
// bare symbol such as .Lvector as a memory operand.
//
// The common path swaps GS's base in from ring 3 and back, by the CS the
// CPU saved (see percpu). The vectors on IST stacks, #DB, NMI, #DF and #MC,
// can arrive in ring 0 before the SYSCALL stub's SWAPGS, so their path
// swaps by IA32_GS_BASE instead, and keeps whether it did in rbx, which
// trap_entry saves as the C ABI says.
global_asm!(
    ".pushsection .text.thisos_trap, \"ax\", @progbits",
    ".p2align 4",
    ".globl thisos_trap_stubs",
    ".hidden thisos_trap_stubs",
    "thisos_trap_stubs:",
    ".set .Lvector, 0",
    ".rept 256",
    ".if !(.Lvector == 8 || (.Lvector >= 10 && .Lvector <= 14) || .Lvector == 17 || .Lvector == 21 || .Lvector == 29 || .Lvector == 30)",
    "push 0",
    ".endif",
    ".byte 0x68",
    ".long .Lvector",
    ".if .Lvector == 1 || .Lvector == 2 || .Lvector == 8 || .Lvector == 18",
    "jmp .Ltrap_ist",
    ".else",
    "jmp .Ltrap_common",
    ".endif",
    ".org thisos_trap_stubs + 16 * (.Lvector + 1), 0xcc",
    ".set .Lvector, .Lvector + 1",
    ".endr",
    ".Ltrap_common:",
    "test byte ptr [rsp + 24], 3",
    "jz 2f",
    "swapgs",
    "2:",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rdi, rsp",
    // The C ABI wants the direction flag clear, and a trap can come while
    // it is set. A gate leaves AC as the trapped code had it, and with AC
    // set SMAP stops nothing, so the stub closes user access itself;
    // iretq gives the trapped code its AC back.
    "cld",
    "clac",
    "call {entry}",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    "add rsp, 16",
    "test byte ptr [rsp + 8], 3",
    "jz 3f",
    "swapgs",
    "3:",
    "iretq",
    ".Ltrap_ist:",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov ecx, {gs_base}",
    "rdmsr",
    "xor ebx, ebx",
    "test edx, edx",
    "js 4f",
    "swapgs",
    "mov ebx, 1",
    "4:",
    "mov rdi, rsp",
    "cld",
    "clac",
    "call {entry}",
    "test ebx, ebx",
    "jz 5f",
    "swapgs",
    "5:",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    "add rsp, 16",
    "iretq",
    ".popsection",
    entry = sym trap_entry,
    gs_base = const percpu::IA32_GS_BASE,
);

// SYSCALL leaves RSP as user code had it, so the stub swaps GS's base in
// and parks it in the CPU's block, then loads the stack top the switch
// keeps there, a copy of TSS.RSP0. Then it pushes what an IDT stub's frame
// holds, with RCX as RIP, R11 as RFLAGS and the syscall's own vector, and
// calls trap_entry as the IDT stubs do. SFMASK clears IF, DF and AC, so
// nothing but an NMI, #MC or #DB interrupts it before the stack is the
// kernel's, and it needs no cld or clac. The handler may turn interrupts
// on, so the stub turns them off before the pops: from pop rsp to sysretq
// the stack is the user's, and an interrupt there would push its frame
// onto it in ring 0. trap_entry checks that RIP is no higher than user
// space's end, since SYSRET needs it canonical.
global_asm!(
    ".pushsection .text.thisos_syscall, \"ax\", @progbits",
    ".globl thisos_syscall",
    ".hidden thisos_syscall",
    "thisos_syscall:",
    "swapgs",
    "mov qword ptr gs:[{user_rsp}], rsp",
    "mov rsp, qword ptr gs:[{kernel_rsp}]",
    "push {user_data}",
    "push qword ptr gs:[{user_rsp}]",
    "push r11",
    "push {user_code}",
    "push rcx",
    "push 0",
    "push {vector}",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rdi, rsp",
    "call {entry}",
    "cli",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    "add rsp, 16",
    "pop rcx",
    "add rsp, 8",
    "pop r11",
    "pop rsp",
    "swapgs",
    "sysretq",
    ".popsection",
    user_rsp = const percpu::USER_RSP,
    kernel_rsp = const percpu::KERNEL_RSP,
    user_data = const USER_DATA,
    user_code = const USER_CODE,
    vector = const SYSCALL_VECTOR,
    entry = sym trap_entry,
);

unsafe extern "C" {
    static thisos_trap_stubs: [u8; 256 * 16];
    static thisos_syscall: u8;
}

/// The GDT's selectors, in the order `init` appends the descriptors.
const KERNEL_CODE: u16 = 0x08;
const KERNEL_DATA: u16 = 0x10;
/// With RPL 3, as SYSRET loads them.
pub const USER_DATA: u16 = 0x1b;
pub const USER_CODE: u16 = 0x23;

/// The vector in the frame of a syscall, outside the IDT's 256.
const SYSCALL_VECTOR: u64 = 0x100;

/// The registers when the CPU trapped, as the stubs and the CPU push them.
#[repr(C)]
#[derive(Debug)]
pub struct TrapFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

impl thisos_hal::TrapFrame for TrapFrame {
    fn ip(&self) -> u64 {
        self.rip
    }

    fn sp(&self) -> u64 {
        self.rsp
    }

    fn raised_by_user(&self) -> bool {
        self.cs & 3 == 3
            && matches!(self.vector, 0..32 | SYSCALL_VECTOR)
            && !matches!(self.vector, NMI | DOUBLE_FAULT | MACHINE_CHECK)
    }

    fn syscall(&self) -> (u64, [u64; 6]) {
        (
            self.rax,
            [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9],
        )
    }

    fn set_syscall_result(&mut self, error: u64, value: u64) {
        self.rax = error;
        self.rdx = value;
    }

    fn backtrace(&self, mut f: impl FnMut(u64)) -> BacktraceEnd {
        f(backtrace::link_address(self.rip));
        backtrace::walk(self.rbp, f)
    }

    fn abandon(&mut self) -> bool {
        catch::abandon(self)
    }
}

impl TrapFrame {
    /// Makes `iretq` continue at `rip` with stack pointer `rsp`, in place of
    /// the trapped code, in the kernel's code segment. The CS a double fault
    /// saves is undefined, as its RIP is, and the trap gate loaded the
    /// kernel's code selector into CS for the handler.
    pub(crate) fn resume_at(&mut self, rip: u64, rsp: u64) {
        self.rip = rip;
        self.rsp = rsp;
        self.cs = u64::from(CS::get_reg().0);
    }
}

/// The exceptions' mnemonics, by vector, with "" for the reserved ones.
const MNEMONICS: [&str; 32] = [
    "#DE", "#DB", "NMI", "#BP", "#OF", "#BR", "#UD", "#NM", "#DF", "", "#TS", "#NP", "#SS", "#GP",
    "#PF", "", "#MF", "#AC", "#MC", "#XM", "#VE", "#CP", "", "", "", "", "", "", "#HV", "#VC",
    "#SX", "",
];

impl fmt::Display for TrapFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mnemonic = if self.vector == SYSCALL_VECTOR {
            "SYSCALL"
        } else {
            usize::try_from(self.vector)
                .ok()
                .and_then(|vector| MNEMONICS.get(vector))
                .unwrap_or(&"")
        };
        writeln!(
            f,
            "registers at vector {} {mnemonic}, error code {:#x}:",
            self.vector, self.error
        )?;
        let registers = [
            ("rip", self.rip),
            ("rsp", self.rsp),
            ("rflags", self.rflags),
            ("cs", self.cs),
            ("ss", self.ss),
            ("rax", self.rax),
            ("rbx", self.rbx),
            ("rcx", self.rcx),
            ("rdx", self.rdx),
            ("rsi", self.rsi),
            ("rdi", self.rdi),
            ("rbp", self.rbp),
            ("r8", self.r8),
            ("r9", self.r9),
            ("r10", self.r10),
            ("r11", self.r11),
            ("r12", self.r12),
            ("r13", self.r13),
            ("r14", self.r14),
            ("r15", self.r15),
        ];
        for row in registers.chunks(4) {
            write!(f, " ")?;
            for (name, value) in row {
                write!(f, " {name:>6} {value:#018x}")?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

const NMI: u64 = 2;
/// The vectors that can't resume after their handler returns.
const DOUBLE_FAULT: u64 = 8;
const MACHINE_CHECK: u64 = 18;

extern "C" fn trap_entry(frame: &mut TrapFrame) {
    // User code can set DF, AC, NT and TF, and SFMASK must clear them: with
    // DF set the kernel's copies run backwards, with AC set SMAP is open.
    // The list repeats the mask's rather than sharing it, so a bit dropped
    // from the mask isn't dropped from the check.
    if frame.vector == SYSCALL_VECTOR {
        let leaked = rflags::read()
            & (RFlags::INTERRUPT_FLAG
                | RFlags::DIRECTION_FLAG
                | RFlags::TRAP_FLAG
                | RFlags::ALIGNMENT_CHECK
                | RFlags::NESTED_TASK);
        if !leaked.is_empty() {
            // The panic handler's copies would run backwards with DF set,
            // and the report would never reach the screen.
            // SAFETY: cld only clears DF, which the Rust ABI expects clear,
            // and touches no memory or stack.
            unsafe { asm!("cld", options(nomem, nostack)) };
            panic!("a syscall entered with {leaked:?} set");
        }
    }
    let trap = match frame.vector {
        SYSCALL_VECTOR => Trap::Syscall,
        3 => Trap::Breakpoint,
        DOUBLE_FAULT => Trap::DoubleFault,
        14 => {
            let (write, fetch) = (1 << 1, 1 << 4);
            let access = if frame.error & fetch != 0 {
                Access::Execute
            } else if frame.error & write != 0 {
                Access::Write
            } else {
                Access::Read
            };
            Trap::PageFault {
                address: Cr2::read_raw(),
                access,
            }
        }
        // The stubs push only vectors below 256, so the low byte is all of
        // it.
        vector @ 0..32 => Trap::Other {
            vector: vector.to_le_bytes()[0],
            error: frame.error,
        },
        apic::TIMER_VECTOR => Trap::Timer,
        apic::SPURIOUS_VECTOR => Trap::Spurious,
        vector => Trap::Stray {
            vector: u32::from(vector.to_le_bytes()[0]),
        },
    };
    // init stores the handler before it loads the IDT, the only way here.
    let Some(handler) = HANDLER.get() else {
        X86_64::halt();
    };
    // Before the handler, which may switch to another thread that runs for
    // many ticks before this one leaves it. Interrupts stay off in the
    // handler, so the next tick waits for iretq or for another thread.
    if frame.vector == apic::TIMER_VECTOR {
        apic::eoi();
    }
    handler(trap, frame);
    match frame.vector {
        // A double fault the handler abandoned resumes in the catch, from
        // registers it saved itself, not from the state the fault left.
        DOUBLE_FAULT if !catch::abandoned(frame) => X86_64::halt(),
        MACHINE_CHECK => X86_64::halt(),
        _ => {}
    }
    // SYSRET to a non-canonical RIP faults in ring 0 on the user's stack.
    // User space ends a page below the canonical boundary, so a syscall
    // in its last bytes returns to its end at most, and only a kernel bug
    // sets RIP past that. A frame that returns through iretq needs no
    // check: its RIP is one the CPU saved, wherever user code jumped.
    if frame.vector == SYSCALL_VECTOR {
        assert!(
            frame.rip <= USER_SPACE.end,
            "returning from a syscall to {:#x}, past user space",
            frame.rip
        );
    }
}

static HANDLER: Once<TrapHandler<X86_64>> = Once::new();

/// The IST slots, counted from 0 as the x86_64 crate does, each the index
/// of its stack in `IST_STACKS` and in what `start_cpus` gives an AP.
const DOUBLE_FAULT_IST: u16 = 0;
const NMI_IST: u16 = 1;
const MACHINE_CHECK_IST: u16 = 2;
const DEBUG_IST: u16 = 3;
const IST_SLOTS: [u16; 4] = [DOUBLE_FAULT_IST, NMI_IST, MACHINE_CHECK_IST, DEBUG_IST];

/// The size of each IST stack, the boot stacks' and the guarded ones'.
pub const IST_STACK_SIZE: usize = 20 * 1024;

/// A stack the boot CPU switches to for one vector until `move_stacks`
/// replaces it, with nothing unmapped below it. Rust code never reads or
/// writes its bytes; the CPU does, through the TSS.
#[repr(align(16))]
struct Stack(UnsafeCell<[u8; IST_STACK_SIZE]>);

// SAFETY: Rust code only takes the stack's address, which is the same on
// every thread. The CPU uses the bytes for the one vector whose IST slot
// holds them, in the boot CPU's TSS alone, so two traps never share one.
unsafe impl Sync for Stack {}

impl Stack {
    /// The address just past the stack, where a stack that grows down
    /// starts.
    fn top(&self) -> u64 {
        VirtAddr::from_ptr(self.0.get().wrapping_add(1)).as_u64()
    }
}

static IST_STACKS: [Stack; 4] = [const { Stack(UnsafeCell::new([0; IST_STACK_SIZE])) }; 4];

/// The IDT, which every CPU loads.
static IDT: Once<InterruptDescriptorTable> = Once::new();

/// Sets up the boot CPU's descriptor tables and registers with `handler`
/// as the kernel's trap handler; see `percpu::init_cpu`, which panics if
/// the CPU lacks SMEP or SMAP: the stubs run `clac`, which faults without
/// SMAP.
///
/// # Safety
///
/// Call this once, on the boot CPU, before anything else uses the segment
/// registers or the descriptor tables, and before anything traps.
pub unsafe fn init(handler: TrapHandler<X86_64>) {
    HANDLER.init(handler);
    // The boot thread's stack, which backtrace::init recorded, until the
    // first switch.
    percpu::prepare(
        0,
        IST_SLOTS.map(|slot| IST_STACKS[usize::from(slot)].top()),
        backtrace::stack().end,
    );
    // SAFETY: The caller runs this once, first, on the boot CPU, after
    // prepare for index 0.
    unsafe {
        percpu::init_cpu(0);
    }
}

/// Loads `block`'s GDT, built the first time, with its TSS, the kernel's
/// segments and the IDT, built by the first CPU here.
///
/// # Safety
///
/// Only `percpu::init_cpu` calls it, for its own CPU's block, before
/// anything there uses the segment registers or descriptor tables.
pub unsafe fn load_tables(block: &'static percpu::Block) {
    // SYSCALL loads SS from the selector after the kernel's code, and
    // SYSRET loads SS and CS from the two after the kernel's data, in that
    // order. There is no 32-bit user code, so no slot for it.
    let mut gdt = GlobalDescriptorTable::new();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let data = gdt.append(Descriptor::kernel_data_segment());
    let user_data = gdt.append(Descriptor::user_data_segment());
    let user_code = gdt.append(Descriptor::user_code_segment());
    assert_eq!(
        [code.0, data.0, user_data.0, user_code.0],
        [KERNEL_CODE, KERNEL_DATA, USER_DATA, USER_CODE]
    );
    // SAFETY: The TSS is in a static block, so the descriptor's address
    // stays valid for as long as the GDT, in the same block, is loaded. The
    // safe tss_segment would need a &'static TaskStateSegment, which the
    // atomic writes through Tss::slot would alias.
    let tss_selector = gdt.append(unsafe { Descriptor::tss_segment_unchecked(block.tss.get()) });
    // The CPU sets the TSS descriptor's busy bit when load_tss runs, but the
    // x86_64 crate keeps each entry in an atomic, so that write doesn't
    // break the shared reference's rules.
    let gdt = block.gdt.init(gdt);
    // SAFETY: The caller runs this before anything uses the segment
    // registers.
    unsafe {
        load_gdt(gdt, code);
    }
    // SAFETY: tss_selector selects the TSS descriptor of the GDT just
    // loaded, for a TSS that lives for the rest of the kernel's life, and
    // used by this CPU alone.
    unsafe {
        load_tss(tss_selector);
    }
    // After the CS reload: set_handler_addr gives each entry the current CS
    // as its code selector, and the loader's would index this GDT. Every
    // GDT has the kernel's code at the same selector, so one IDT serves
    // every CPU.
    match IDT.get() {
        Some(idt) => idt.load(),
        None => IDT.init(idt()).load(),
    }
}

/// Points SYSCALL at its stub, with the GDT's selectors and the flags it
/// clears.
pub fn load_syscall() {
    let selectors = Star::write(
        SegmentSelector(USER_CODE),
        SegmentSelector(USER_DATA),
        SegmentSelector(KERNEL_CODE),
        SegmentSelector(KERNEL_DATA),
    );
    if let Err(e) = selectors {
        panic!("the GDT's selectors don't suit SYSCALL: {e:?}");
    }
    LStar::write(VirtAddr::from_ptr(&raw const thisos_syscall));
    // Linux's mask: user code can't make the stub run with interrupts or
    // single-stepping on, with the direction flag set or with user access
    // open.
    SFMask::write(
        RFlags::INTERRUPT_FLAG
            | RFlags::DIRECTION_FLAG
            | RFlags::TRAP_FLAG
            | RFlags::ALIGNMENT_CHECK
            | RFlags::NESTED_TASK
            | RFlags::IOPL_LOW
            | RFlags::IOPL_HIGH,
    );
}

fn idt() -> InterruptDescriptorTable {
    let mut idt = InterruptDescriptorTable::new();
    install(&mut idt.divide_error, 0);
    // On a stack of its own, as Linux has it: a data breakpoint on a MOV SS
    // right before a SYSCALL raises its #DB on the stub's first
    // instruction, while RSP is still the user's. User code can't set
    // breakpoints yet. A SYSCALL with TF set traps only after SYSRET, in
    // user code, as a test under QEMU showed.
    install_on(&mut idt.debug, 1, DEBUG_IST);
    install_on(&mut idt.non_maskable_interrupt, 2, NMI_IST);
    install(&mut idt.breakpoint, 3);
    install(&mut idt.overflow, 4);
    install(&mut idt.bound_range_exceeded, 5);
    install(&mut idt.invalid_opcode, 6);
    install(&mut idt.device_not_available, 7);
    install_on(&mut idt.double_fault, 8, DOUBLE_FAULT_IST);
    install(&mut idt.invalid_tss, 10);
    install(&mut idt.segment_not_present, 11);
    install(&mut idt.stack_segment_fault, 12);
    install(&mut idt.general_protection_fault, 13);
    install(&mut idt.page_fault, 14);
    install(&mut idt.x87_floating_point, 16);
    install(&mut idt.alignment_check, 17);
    install_on(&mut idt.machine_check, 18, MACHINE_CHECK_IST);
    install(&mut idt.simd_floating_point, 19);
    install(&mut idt.virtualization, 20);
    install(&mut idt.cp_protection_exception, 21);
    install(&mut idt.hv_injection_exception, 28);
    install(&mut idt.vmm_communication_exception, 29);
    install(&mut idt.security_exception, 30);
    for vector in 32..=u8::MAX {
        install(&mut idt[vector], vector);
    }
    idt
}

/// Loads `gdt` and the segment registers, with `code` in CS.
///
/// # Safety
///
/// `code` must select the 64-bit kernel code segment of `gdt`, and nothing
/// may rely on the segment registers' old values, GS's base among them,
/// which a selector load clears on Intel.
unsafe fn load_gdt(gdt: &'static GlobalDescriptorTable, code: SegmentSelector) {
    gdt.load();
    // SAFETY: The caller guarantees code, which is what the kernel already
    // runs as, maybe in another GDT.
    unsafe {
        CS::set_reg(code);
    }
    // SAFETY: Ring 0 may run with a null SS in 64-bit mode. The loader's
    // selector would index this GDT, where it may name the TSS, and iretq
    // reloads SS from the trap frame.
    unsafe {
        SS::set_reg(SegmentSelector::NULL);
    }
    // Old selectors in these index this GDT too, and name whatever its
    // layout puts there.
    // SAFETY: 64-bit mode ignores DS and ES, the kernel addresses nothing
    // through FS, and the caller sets GS's base after this.
    unsafe {
        asm!(
            "mov ds, {0:e}",
            "mov es, {0:e}",
            "mov fs, {0:e}",
            "mov gs, {0:e}",
            in(reg) 0u32,
            options(nostack, preserves_flags),
        );
    }
}

/// Points `entry`, the IDT entry for `vector`, at that vector's stub.
fn install<F>(entry: &mut Entry<F>, vector: u8) -> &mut EntryOptions {
    let stub = VirtAddr::from_ptr(&raw const thisos_trap_stubs) + 16 * u64::from(vector);
    // SAFETY: stub is the stub for vector, which saves and restores every
    // register it changes and returns with iretq after popping the error
    // code the CPU pushes for vector, or the zero it pushed in place of
    // one. The caller passes the vector its entry is for.
    unsafe { entry.set_handler_addr(stub) }
}

/// As `install`, with the CPU switching to stack `ist` before the stub runs.
fn install_on<F>(entry: &mut Entry<F>, vector: u8, ist: u16) {
    let options = install(entry, vector);
    // SAFETY: ist is one of the slots percpu::prepare fills with a stack of
    // its own for every CPU, which move_stacks replaces with another of its
    // own on the boot CPU, and idt passes each to one vector only, so no
    // two traps share one. The stub for each of these vectors is one that
    // swaps GS's base by IA32_GS_BASE.
    unsafe {
        options.set_stack_index(ist);
    }
}

/// The stacks `move_stacks` put in the boot CPU's IST slots, which keep
/// them for good.
static MOVED_STACKS: Once<[paging::Stack; 4]> = Once::new();

/// Moves each of the boot CPU's IST slots to a stack from `stack`, which it
/// calls with the stack's size; see `Platform::move_trap_stacks`.
pub fn move_stacks(stack: &mut dyn FnMut(usize) -> paging::Stack) {
    assert!(
        MOVED_STACKS.get().is_none(),
        "the trap stacks moved already"
    );
    let tss = &percpu::this().tss;
    let moved = IST_SLOTS.map(|slot| {
        let moved = stack(IST_STACK_SIZE);
        // The CPU reads the slot on this core, which sees its own stores in
        // order, and the boot stack it held stays valid, so a trap meanwhile
        // takes either stack.
        tss.slot(slot).store(moved.range().end, Ordering::Relaxed);
        moved
    });
    MOVED_STACKS.init(moved);
}

/// Makes `top` the stack a trap from user code starts on, through IDT or
/// SYSCALL: the top of the stack of the thread about to run.
pub fn set_kernel_stack(top: u64) {
    percpu::this().set_kernel_stack(top);
}

/// The stacks the running CPU's IST slots hold.
pub fn stacks() -> impl Iterator<Item = Range<u64>> {
    let Ok(size) = u64::try_from(IST_STACK_SIZE) else {
        panic!("{IST_STACK_SIZE:#x} doesn't fit a u64");
    };
    let tss = &percpu::this().tss;
    IST_SLOTS.into_iter().map(move |slot| {
        let top = tss.slot(slot).load(Ordering::Relaxed);
        top - size..top
    })
}
