//! Starting the APs, the CPUs besides the one the firmware booted. The MADT
//! lists them. Each starts in real mode at a trampoline in a frame below
//! 1 MiB, which takes it through protected mode into long mode on low page
//! tables that hold the kernel's upper half, and on into the kernel. Until
//! per-CPU state exists, an AP takes no lock and logs nothing: it stores its
//! APIC ID in its slot, where the BSP finds it, and parks.

use core::arch::global_asm;
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use bootloader_api::info::{MemoryRegion, MemoryRegionKind};
use thisos_acpi::{HEADER_SIZE, Madt, RSDP_SIZE, State, Xsdt};
use thisos_hal::{Cpu as _, CpuStart};
use thisos_locks::MAX_CPUS;
use x86_64::VirtAddr;
use x86_64::registers::control::{Cr0, Cr4};
use x86_64::registers::model_specific::{Efer, Msr};
use x86_64::structures::idt::{Entry, InterruptDescriptorTable};

use crate::once::Once;
use crate::{X86_64, apic, paging, phys_offset, trap, tsc};

/// Each AP's stack, the size of a kernel thread's.
const STACK_SIZE: usize = 64 * 1024;
/// How long an AP gets to check in after its last startup IPI.
const TIMEOUT_MS: u32 = 250;
/// The SDM's waits after INIT and after the first startup IPI.
const INIT_WAIT_US: u64 = 10_000;
const STARTUP_WAIT_US: u64 = 200;
/// Where conventional memory ends and the legacy video area starts.
const LOW_END: u64 = 0xa_0000;
/// Where [`Data`] sits in the trampoline's page, after the code.
const DATA: usize = 0x800;
/// A slot's value once its AP checked in, with the APIC ID below it.
const CHECKED_IN: u64 = 1 << 32;
const IA32_PAT: u32 = 0x277;

// The trampoline's GDT: flat 32-bit code and data, and 64-bit code.
const CODE32: u16 = 0x08;
const DATA32: u16 = 0x10;
const CODE64: u16 = 0x18;
const GDT: [u64; 4] = [
    0,
    0x00cf_9a00_0000_ffff,
    0x00cf_9200_0000_ffff,
    0x00af_9a00_0000_ffff,
];

/// What the trampoline reads, at [`DATA`] in its page. The BSP writes it
/// before each AP's INIT.
#[repr(C)]
struct Data {
    gdt: [u64; 4],
    /// LGDT's operand: the limit in the low 16 bits, the base above.
    gdtr: u64,
    /// The far jumps' operands into 32-bit and 64-bit code: the offset in
    /// the low 32 bits, the selector above.
    far32: u64,
    far64: u64,
    /// The low tables, which the AP turns paging on with.
    low_cr3: u64,
    /// What the AP takes from the BSP once it runs in the upper half.
    kernel_cr3: u64,
    cr0: u64,
    cr4: u64,
    efer: u64,
    pat: u64,
    stack: u64,
    /// Where the AP jumps into the upper half: `thisos_ap_high`.
    high: u64,
    /// The AP's slot.
    index: u64,
}

const _: () = assert!(DATA + size_of::<Data>() <= 4096);

// The trampoline, which the BSP copies to the start of a frame below 1 MiB.
// A startup IPI starts the AP at its first byte, in real mode, with CS
// holding the frame's address / 16. It finds its own address from CS and
// keeps it in ESI, since everything it reads is at a fixed offset from it,
// and the BSP writes each far pointer and the GDT's base as an address.
// Interrupts stay off from here on. In 32-bit mode it turns on PAE, the low
// tables, long mode and no-execute, which the kernel's upper half uses, then
// paging; in 64-bit mode, still in the low 2 MiB, it jumps to
// thisos_ap_high. AT&T syntax, which spells operand sizes in 16-bit code.
global_asm!(
    ".pushsection .rodata.thisos_ap_trampoline, \"a\", @progbits",
    ".globl thisos_ap_trampoline",
    ".hidden thisos_ap_trampoline",
    ".globl thisos_ap_32",
    ".hidden thisos_ap_32",
    ".globl thisos_ap_64",
    ".hidden thisos_ap_64",
    ".globl thisos_ap_trampoline_end",
    ".hidden thisos_ap_trampoline_end",
    ".code16",
    "thisos_ap_trampoline:",
    "cli",
    "cld",
    "xorl %esi, %esi",
    "movw %cs, %si",
    "shll $4, %esi",
    "movw %cs, %ax",
    "movw %ax, %ds",
    "lgdtl {data}+{gdtr}",
    "movl %cr0, %eax",
    "orl $1, %eax",
    "movl %eax, %cr0",
    "ljmpl *{data}+{far32}",
    ".code32",
    "thisos_ap_32:",
    "movw ${data32}, %ax",
    "movw %ax, %ds",
    "movw %ax, %es",
    "movw %ax, %ss",
    "movl %cr4, %eax",
    "orl $0x20, %eax",
    "movl %eax, %cr4",
    "movl {data}+{low_cr3}(%esi), %eax",
    "movl %eax, %cr3",
    "movl $0xc0000080, %ecx",
    "rdmsr",
    "orl $0x900, %eax",
    "wrmsr",
    "movl %cr0, %eax",
    "orl $0x80000000, %eax",
    "movl %eax, %cr0",
    "ljmpl *{data}+{far64}(%esi)",
    ".code64",
    "thisos_ap_64:",
    "movl %esi, %esi",
    "movq {data}+{high}(%rsi), %rax",
    "jmpq *%rax",
    "thisos_ap_trampoline_end:",
    ".popsection",
    data = const DATA,
    gdtr = const offset_of!(Data, gdtr),
    far32 = const offset_of!(Data, far32),
    far64 = const offset_of!(Data, far64),
    low_cr3 = const offset_of!(Data, low_cr3),
    high = const offset_of!(Data, high),
    data32 = const DATA32,
    options(att_syntax),
);

// The AP's first code in the upper half, in the kernel's image. It reads
// everything it needs from the trampoline's page, then takes the BSP's
// PAT, EFER, CR4 and CR0, and last the kernel's tables, which don't map
// the trampoline's page. The stack's top is 16-byte aligned, as the call
// wants.
global_asm!(
    ".pushsection .text.thisos_ap_high, \"ax\", @progbits",
    ".globl thisos_ap_high",
    ".hidden thisos_ap_high",
    "thisos_ap_high:",
    "mov rsp, [rsi + {data} + {stack}]",
    "mov rdi, [rsi + {data} + {index}]",
    "mov ecx, {ia32_pat}",
    "mov eax, [rsi + {data} + {pat}]",
    "mov edx, [rsi + {data} + {pat} + 4]",
    "wrmsr",
    "mov ecx, 0xc0000080",
    "mov eax, [rsi + {data} + {efer}]",
    "mov edx, [rsi + {data} + {efer} + 4]",
    "wrmsr",
    "mov rax, [rsi + {data} + {cr4}]",
    "mov cr4, rax",
    "mov rax, [rsi + {data} + {cr0}]",
    "mov cr0, rax",
    "mov rax, [rsi + {data} + {kernel_cr3}]",
    "mov cr3, rax",
    "xor ebp, ebp",
    "call {main}",
    "ud2",
    ".popsection",
    data = const DATA,
    stack = const offset_of!(Data, stack),
    index = const offset_of!(Data, index),
    ia32_pat = const IA32_PAT,
    pat = const offset_of!(Data, pat),
    efer = const offset_of!(Data, efer),
    cr4 = const offset_of!(Data, cr4),
    cr0 = const offset_of!(Data, cr0),
    kernel_cr3 = const offset_of!(Data, kernel_cr3),
    main = sym ap_main,
);

// The park IDT's stubs, 16 bytes apart as the kernel's are, one for each
// exception vector. A stub stores the vector + 1 in AP_TRAP and halts with
// interrupts off, which the gate already turned off. mov eax, imm32 is
// hand-encoded for the reason trap.rs gives for its push.
global_asm!(
    ".pushsection .text.thisos_park, \"ax\", @progbits",
    ".p2align 4",
    ".globl thisos_park_stubs",
    ".hidden thisos_park_stubs",
    "thisos_park_stubs:",
    ".set .Lpark_vector, 0",
    ".rept 32",
    ".byte 0xb8",
    ".long .Lpark_vector + 1",
    "jmp .Lpark_common",
    ".org thisos_park_stubs + 16 * (.Lpark_vector + 1), 0xcc",
    ".set .Lpark_vector, .Lpark_vector + 1",
    ".endr",
    ".Lpark_common:",
    "mov qword ptr [rip + {trap}], rax",
    "2:",
    "cli",
    "hlt",
    "jmp 2b",
    ".popsection",
    trap = sym AP_TRAP,
);

unsafe extern "C" {
    static thisos_ap_trampoline: u8;
    static thisos_ap_32: u8;
    static thisos_ap_64: u8;
    static thisos_ap_trampoline_end: u8;
    static thisos_ap_high: u8;
    static thisos_park_stubs: [u8; 32 * 16];
}

/// Each AP's slot, by its index among the running CPUs: 0 until it checks
/// in, then [`CHECKED_IN`] with its APIC ID.
static SLOTS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The vector + 1 an AP trapped on, which the park stubs store, or 0.
static AP_TRAP: AtomicU64 = AtomicU64::new(0);

/// The IDT the APs load, whose exception vectors go to the park stubs.
static PARK_IDT: Once<InterruptDescriptorTable> = Once::new();

/// The running APs' stacks, kept for good.
static STACKS: Once<[Option<paging::Stack>; MAX_CPUS]> = Once::new();

/// The loader's memory map, and the RSDP's address if it found one.
static REGIONS: Once<&'static [MemoryRegion]> = Once::new();
static RSDP: Once<Option<u64>> = Once::new();

/// Keeps the loader's memory map and the RSDP's address for [`start`].
pub fn record(regions: &'static [MemoryRegion], rsdp: Option<u64>) {
    REGIONS.init(regions);
    RSDP.init(rsdp);
}

/// Starts every AP the MADT lists as enabled; see
/// `Platform::start_cpus`. Panics if the ACPI tables are malformed, if the
/// TSC's rate is unknown, or if fewer than four frames below 640 KiB are
/// free for the trampoline.
pub fn start(
    stack: &mut dyn FnMut(usize) -> paging::Stack,
    report: &mut dyn FnMut(CpuStart),
) -> usize {
    let Some(madt) = madt() else {
        return 1;
    };
    let bsp = apic::id();
    for cpu in madt.cpus().filter(|c| c.state == State::OnlineCapable) {
        report(CpuStart::Skipped { id: cpu.apic_id });
    }
    let aps = || {
        madt.cpus()
            .filter(move |c| c.state == State::Enabled && c.apic_id != bsp)
    };
    let listed = aps().count();
    if listed == 0 {
        return 1;
    }
    assert!(
        listed < MAX_CPUS,
        "the MADT lists {} CPUs, more than {MAX_CPUS}",
        listed + 1
    );
    let Some(hz) = tsc::rate() else {
        panic!("the TSC's rate is unknown, so the startup IPIs can't be timed");
    };
    let Some(&regions) = REGIONS.get() else {
        panic!("boot_info didn't record the memory map");
    };
    let [page, level_4, level_3, level_2] = low_frames(regions);
    // SAFETY: The loader mapped all physical memory at phys_offset.
    // low_frames gives Usable frames below 1 MiB, which neither the loader
    // nor the frame allocator hands out, and start runs once, so the
    // frames are its own; no CPU uses them yet.
    unsafe {
        paging::write_ap_tables(phys_offset(), level_4, level_3, level_2);
    }
    write_code(page);
    let Ok(vector) = u8::try_from(page >> 12) else {
        unreachable!("the trampoline is below 1 MiB");
    };
    PARK_IDT.init(park_idt());
    let data = |stack: u64, index: usize| Data {
        gdt: GDT,
        gdtr: (to_u64(size_of_val(&GDT)) - 1) | (page + to_u64(DATA + offset_of!(Data, gdt))) << 16,
        far32: (page + offset(&raw const thisos_ap_32)) | u64::from(CODE32) << 32,
        far64: (page + offset(&raw const thisos_ap_64)) | u64::from(CODE64) << 32,
        low_cr3: level_4,
        kernel_cr3: paging::kernel_level_4(),
        cr0: Cr0::read_raw(),
        cr4: Cr4::read_raw(),
        efer: Efer::read_raw(),
        // SAFETY: Every CPU the kernel runs on has the PAT, which
        // pat::write_combine checks before it uses it, and reading it
        // changes nothing.
        pat: unsafe { Msr::new(IA32_PAT).read() },
        stack,
        high: VirtAddr::from_ptr(&raw const thisos_ap_high).as_u64(),
        index: to_u64(index),
    };
    report(CpuStart::Starting {
        listed,
        timeout_ms: TIMEOUT_MS,
    });
    let mut stacks = [const { None }; MAX_CPUS];
    let mut running = 1;
    for cpu in aps() {
        let ap_stack = stack(STACK_SIZE);
        SLOTS[running].store(0, Ordering::Relaxed);
        write_data(page, data(ap_stack.range().end, running));
        let started = tsc::read();
        let slot = signal(cpu.apic_id, vector, hz, &SLOTS[running]);
        let micros = tsc::read().wrapping_sub(started) * 1_000_000 / hz;
        let trapped = AP_TRAP.swap(0, Ordering::Relaxed);
        if trapped != 0 {
            let Ok(vector) = u8::try_from(trapped - 1) else {
                unreachable!("the park stubs cover vectors 0 to 31");
            };
            report(CpuStart::Trapped {
                id: cpu.apic_id,
                vector,
            });
        }
        if slot == 0 {
            // Back to waiting for a startup IPI, so that it can't run the
            // trampoline with the next AP's data.
            apic::send_init(cpu.apic_id);
            report(CpuStart::Silent { id: cpu.apic_id });
            continue;
        }
        let Ok(reported) = u32::try_from(slot & (CHECKED_IN - 1)) else {
            unreachable!("an APIC ID is 32 bits");
        };
        report(CpuStart::Up {
            cpu: running,
            id: cpu.apic_id,
            reported,
            micros,
        });
        stacks[running] = Some(ap_stack);
        running += 1;
    }
    STACKS.init(stacks);
    running
}

/// Sends the AP with APIC ID `id` INIT and startup IPIs for the
/// trampoline at page `vector`, with the SDM's waits timed on a TSC that
/// counts at `hz`, and returns `slot` once the AP checks in there or the
/// timeout passes.
fn signal(id: u32, vector: u8, hz: u64, slot: &AtomicU64) -> u64 {
    let checked_in = || slot.load(Ordering::Acquire);
    apic::send_init(id);
    wait(hz, INIT_WAIT_US, || false);
    apic::send_startup(id, vector);
    wait(hz, STARTUP_WAIT_US, || checked_in() != 0);
    if checked_in() == 0 {
        apic::send_startup(id, vector);
    }
    wait(hz, u64::from(TIMEOUT_MS) * 1000, || checked_in() != 0);
    checked_in()
}

/// The AP's code once it runs in the kernel's upper half and on its own
/// stack, with interrupts off, on the kernel's tables: it loads the GDT and
/// the park IDT, puts its APIC in x2APIC mode, checks in with its APIC ID
/// and parks.
extern "sysv64" fn ap_main(index: usize) -> ! {
    // SAFETY: Nothing on this AP has used the segment registers since the
    // trampoline, whose GDT the kernel's tables no longer map.
    unsafe {
        trap::load_gdt_on_ap();
    }
    let Some(idt) = PARK_IDT.get() else {
        unreachable!("start builds the park IDT before any AP starts");
    };
    idt.load();
    let id = apic::enable_on_ap();
    if let Some(slot) = SLOTS.get(index) {
        slot.store(CHECKED_IN | u64::from(id), Ordering::Release);
    }
    X86_64::halt()
}

/// The MADT, or `None` if the loader found no RSDP or the XSDT lists no
/// MADT. Panics if a table on the way is malformed.
fn madt() -> Option<Madt<'static>> {
    let rsdp = (*RSDP.get()?)?;
    let xsdt = match thisos_acpi::xsdt_address(firmware_bytes(rsdp, RSDP_SIZE)) {
        Ok(addr) => addr,
        Err(e) => panic!("the RSDP at {rsdp:#x} is malformed: {e:?}"),
    };
    let table = |addr: u64| match thisos_acpi::table_size(firmware_bytes(addr, HEADER_SIZE)) {
        Ok(size) => firmware_bytes(addr, size),
        Err(e) => panic!("the ACPI table at {addr:#x} is malformed: {e:?}"),
    };
    let xsdt = match Xsdt::new(table(xsdt)) {
        Ok(xsdt) => xsdt,
        Err(e) => panic!("the XSDT at {xsdt:#x} is malformed: {e:?}"),
    };
    let addr = xsdt.tables().find(|&addr| {
        thisos_acpi::signature(firmware_bytes(addr, HEADER_SIZE)) == Some(*b"APIC")
    })?;
    match Madt::new(table(addr)) {
        Ok(madt) => Some(madt),
        Err(e) => panic!("the MADT at {addr:#x} is malformed: {e:?}"),
    }
}

/// `len` bytes of the firmware's ACPI tables at physical address `addr`.
fn firmware_bytes(addr: u64, len: usize) -> &'static [u8] {
    // A physical address from the RSDP or a table, reached through the
    // physical-memory mapping.
    let start = (phys_offset() + addr).as_ptr::<u8>();
    // SAFETY: The loader maps all physical memory at phys_offset. The ACPI
    // tables are in regions the memory map marks as firmware's, which the
    // frame allocator never hands out, and nothing writes them.
    unsafe { core::slice::from_raw_parts(start, len) }
}

/// The four highest Usable frames below [`LOW_END`], highest first.
fn low_frames(regions: &[MemoryRegion]) -> [u64; 4] {
    let usable = |page: u64| {
        regions
            .iter()
            .any(|r| r.kind == MemoryRegionKind::Usable && r.start <= page && page + 4096 <= r.end)
    };
    let mut frames = [0; 4];
    let mut found = 0;
    for page in (0..LOW_END / 4096)
        .rev()
        .map(|i| i * 4096)
        .filter(|&p| usable(p))
    {
        frames[found] = page;
        found += 1;
        if found == frames.len() {
            return frames;
        }
    }
    panic!("only {found} free frames below {LOW_END:#x}, and the APs' trampoline needs 4");
}

/// The trampoline's address in the kernel's image, from its symbols.
fn trampoline() -> &'static [u8] {
    let start = &raw const thisos_ap_trampoline;
    let len = (&raw const thisos_ap_trampoline_end).addr() - start.addr();
    // SAFETY: The two symbols bound the trampoline's bytes, in a read-only
    // section of the kernel's image, which is mapped for the kernel's life.
    unsafe { core::slice::from_raw_parts(start, len) }
}

/// `symbol`'s offset into the trampoline.
fn offset(symbol: *const u8) -> u64 {
    to_u64(symbol.addr() - (&raw const thisos_ap_trampoline).addr())
}

fn to_u64(n: usize) -> u64 {
    let Ok(n) = u64::try_from(n) else {
        unreachable!("x86_64's usize is 64 bits");
    };
    n
}

/// Copies the trampoline to the start of the frame at `page`.
fn write_code(page: u64) {
    let code = trampoline();
    assert!(code.len() <= DATA, "the trampoline runs into its data");
    // SAFETY: page is one of low_frames', which start alone holds, mapped
    // through the physical-memory mapping, and no AP runs from it yet.
    unsafe {
        ptr::copy_nonoverlapping(
            code.as_ptr(),
            (phys_offset() + page).as_mut_ptr::<u8>(),
            code.len(),
        );
    }
}

/// Writes the trampoline's data in the frame at `page`.
fn write_data(page: u64, data: Data) {
    let at = (phys_offset() + page + to_u64(DATA)).as_mut_ptr::<Data>();
    // SAFETY: As in write_code. DATA is 8-byte aligned, which Data needs.
    // No AP reads it meanwhile: the last one checked in after it read
    // everything, or was sent INIT.
    unsafe { ptr::write(at, data) };
}

/// Spins until `done` or until `micros` pass on a TSC that counts at `hz`.
fn wait(hz: u64, micros: u64, done: impl Fn() -> bool) {
    let cycles = micros * hz / 1_000_000;
    let started = tsc::read();
    while !done() && tsc::read().wrapping_sub(started) < cycles {
        core::hint::spin_loop();
    }
}

/// An IDT whose exception vectors go to the park stubs, for the APs.
fn park_idt() -> InterruptDescriptorTable {
    let mut idt = InterruptDescriptorTable::new();
    park(&mut idt.divide_error, 0);
    park(&mut idt.debug, 1);
    park(&mut idt.non_maskable_interrupt, 2);
    park(&mut idt.breakpoint, 3);
    park(&mut idt.overflow, 4);
    park(&mut idt.bound_range_exceeded, 5);
    park(&mut idt.invalid_opcode, 6);
    park(&mut idt.device_not_available, 7);
    park(&mut idt.double_fault, 8);
    park(&mut idt.invalid_tss, 10);
    park(&mut idt.segment_not_present, 11);
    park(&mut idt.stack_segment_fault, 12);
    park(&mut idt.general_protection_fault, 13);
    park(&mut idt.page_fault, 14);
    park(&mut idt.x87_floating_point, 16);
    park(&mut idt.alignment_check, 17);
    park(&mut idt.machine_check, 18);
    park(&mut idt.simd_floating_point, 19);
    park(&mut idt.virtualization, 20);
    park(&mut idt.cp_protection_exception, 21);
    park(&mut idt.hv_injection_exception, 28);
    park(&mut idt.vmm_communication_exception, 29);
    park(&mut idt.security_exception, 30);
    idt
}

/// Points `entry`, the IDT entry for `vector`, at that vector's park stub.
fn park<F>(entry: &mut Entry<F>, vector: u64) {
    let stub = VirtAddr::from_ptr(&raw const thisos_park_stubs) + 16 * vector;
    // SAFETY: stub never returns, so the frame the CPU pushes, with or
    // without an error code, doesn't matter. The entry takes the current
    // CS, the kernel's code segment, which the APs load before the IDT.
    unsafe {
        entry.set_handler_addr(stub);
    }
}
