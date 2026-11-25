//! The warm-reset experiment's hardware access (issue #107, spike branch
//! only): physical words in the sampled blocks, the memory-map holes that
//! keep the frame allocator out of them, and the 83DA's TCO, PMC and CF9.

use x86_64::instructions::port::Port;
use x86_64::registers::model_specific::Msr;

use crate::phys_offset;

/// Each sampled block's size.
pub const BLOCK: u64 = 64 << 10;
/// Blocks start at multiples of this.
pub const STRIDE: u64 = 64 << 20;
/// Stride indices the holes bitmap covers: 64 GiB, the frame allocator's
/// limit.
pub const INDICES: u64 = 1024;
/// Words in a holes bitmap.
pub const BITMAP: usize = 16;
/// The Firmware type the memory map gives a hole.
pub const HOLE_KIND: u32 = 0x5741_524d;

/// Whether stride index `i` is set in `bitmap`.
pub fn bit(bitmap: &[u64; BITMAP], i: u64) -> bool {
    let word = usize::try_from(i / 64).ok().and_then(|w| bitmap.get(w));
    word.is_some_and(|word| word & (1 << (i % 64)) != 0)
}

/// Sets stride index `i` in `bitmap`.
pub fn set(bitmap: &mut [u64; BITMAP], i: u64) {
    if let Some(word) = usize::try_from(i / 64).ok().and_then(|w| bitmap.get_mut(w)) {
        *word |= 1 << (i % 64);
    }
}

/// The blocks the memory map reports as holes.
static HOLES: crate::once::Once<[u64; BITMAP]> = crate::once::Once::new();

/// Makes the memory map report the blocks in `bitmap` as Firmware regions.
/// Call it once, before the frame allocator reads the memory map.
pub fn reserve(bitmap: [u64; BITMAP]) {
    HOLES.init(bitmap);
}

/// The first reserved block that overlaps `start..end`.
pub(crate) fn first_hole(start: u64, end: u64) -> Option<u64> {
    let holes = HOLES.get()?;
    (start / STRIDE..INDICES)
        .take_while(|i| i * STRIDE < end)
        .find(|&i| bit(holes, i) && i * STRIDE + BLOCK > start)
        .map(|i| i * STRIDE)
}

/// Reads the word at physical address `phys`.
pub fn read(phys: u64) -> u64 {
    let ptr = (phys_offset() + phys).as_ptr::<u64>();
    // SAFETY: The loader maps all physical memory at phys_offset, and the
    // experiment reads only RAM regions of its map, 8-byte aligned.
    unsafe { ptr.read_volatile() }
}

/// Writes `value` to the word at physical address `phys`.
pub fn write(phys: u64, value: u64) {
    let ptr = (phys_offset() + phys).as_mut_ptr::<u64>();
    // SAFETY: As in read; the experiment writes only blocks it reserved
    // from the frame allocator, or headers it found there before the
    // allocator started.
    unsafe { ptr.write_volatile(value) }
}

fn rdmsr(msr: u32) -> u64 {
    // SAFETY: Callers read only the MTRR MSRs, which every x86_64 CPU has;
    // reading them changes nothing.
    unsafe { Msr::new(msr).read() }
}

fn inw(port: u16) -> u16 {
    // SAFETY: Callers read only the 83DA's TCO registers, after on_83da.
    unsafe { Port::<u16>::new(port).read() }
}

fn outw(port: u16, value: u16) {
    // SAFETY: Callers write only the 83DA's TCO registers, after on_83da.
    unsafe { Port::<u16>::new(port).write(value) }
}

/// Reads dword `offset` of PCI function bus 0 `dev.func`'s config space.
fn pci_read(dev: u32, func: u32, offset: u32) -> u32 {
    let mut address = Port::<u32>::new(0xcf8);
    // SAFETY: Configuration mechanism 1: nothing else uses 0xcf8 and 0xcfc
    // while the kernel boots on one CPU.
    unsafe { address.write(0x8000_0000 | dev << 11 | func << 8 | (offset & 0xfc)) };
    // SAFETY: As above; a config read changes nothing.
    unsafe { Port::<u32>::new(0xcfc).read() }
}

/// Whether the MTRRs make `phys` uncacheable, ignoring the fixed ranges
/// below 1 MiB.
fn uncacheable(phys: u64) -> bool {
    let mut found = None;
    for i in 0..u32::from(rdmsr(0xfe).to_le_bytes()[0]) {
        let (base, mask) = (rdmsr(0x200 + 2 * i), rdmsr(0x201 + 2 * i));
        let valid = mask & (1 << 11) != 0;
        let mask = mask & !0xfff;
        if valid && phys & mask == base & mask {
            let ty = base.to_le_bytes()[0];
            // Uncacheable wins over every overlapping type.
            if ty == 0 {
                return true;
            }
            found = Some(ty);
        }
    }
    found.unwrap_or(rdmsr(0x2ff).to_le_bytes()[0]) == 0
}

/// The PMC's MMIO base on the 83DA, where `ACPI_TMR_CTL` and the reset
/// causes are.
const PWRMBASE: u64 = 0xfe00_0000;
/// The TCO's I/O base on the 83DA.
const TCOBASE: u16 = 0x400;

/// Whether this is the 83DA's chipset with the PMC uncacheable: the `SMBus`
/// controller at 0:1f.4 reads 8086:7e22, and the MTRRs make `PWRMBASE`
/// uncacheable.
pub fn on_83da() -> bool {
    pci_read(0x1f, 4, 0) == 0x7e22_8086 && uncacheable(PWRMBASE)
}

fn pmc_read(offset: u64) -> u32 {
    let ptr = (phys_offset() + PWRMBASE + offset).as_ptr::<u32>();
    // SAFETY: Callers check on_83da first: the PMC's registers are there,
    // uncacheable, and these are 4-byte aligned registers reads don't change.
    unsafe { ptr.read_volatile() }
}

fn pmc_write(offset: u64, value: u32) {
    let ptr = (phys_offset() + PWRMBASE + offset).as_mut_ptr::<u32>();
    // SAFETY: As in pmc_read; the experiment writes only ACPI_TMR_CTL.
    unsafe { ptr.write_volatile(value) }
}

/// The PMC's `HPR_CAUSE0` and `GEN_PMCON_A`, which say how the last reset
/// happened. Call only where [`on_83da`] holds.
pub fn reset_causes() -> (u32, u32) {
    (pmc_read(0x192c), pmc_read(0x1020))
}

/// Clears `ACPI_TIM_DIS` so the TCO counts, then starts the TCO from
/// `ticks` of 0.6 s. Nothing feeds it afterwards. Call only where
/// [`on_83da`] holds.
pub fn arm_tco(ticks: u16) {
    const ACPI_TMR_CTL: u64 = 0x18fc;
    const RLD: u16 = TCOBASE;
    const STS1: u16 = TCOBASE + 4;
    const STS2: u16 = TCOBASE + 6;
    const CNT1: u16 = TCOBASE + 8;
    const TMR: u16 = TCOBASE + 0x12;
    pmc_write(ACPI_TMR_CTL, pmc_read(ACPI_TMR_CTL) & !(1 << 1));
    outw(TMR, inw(TMR) & 0xfc00 | ticks & 0x3ff);
    // TIMEOUT and SECOND_TO_STS are write-1-to-clear.
    outw(STS1, 1 << 3);
    outw(STS2, 1 << 1);
    // Clear TCO_TMR_HALT. Bit 8, NMI_NOW, inverts when written back as 1,
    // so it is written as 0.
    outw(CNT1, inw(CNT1) & !(1 << 11) & !(1 << 8));
    outw(RLD, 1);
}

/// Writes back and invalidates the caches, then resets through the reset
/// control register: hard reset off, then a CPU and system reset. Without
/// the write-back, lines still dirty in the caches never reach RAM.
pub fn reset_cf9() -> ! {
    // SAFETY: WBINVD only writes dirty lines back and empties the caches;
    // it is privileged, and the kernel runs in ring 0.
    unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
    let mut cf9 = Port::<u8>::new(0xcf9);
    // SAFETY: 0xcf9 is the PCH's reset control register; resetting is the
    // point.
    unsafe { cf9.write(0x02) };
    // SAFETY: As above.
    unsafe { cf9.write(0x06) };
    hang()
}

/// Spins with interrupts off, as a kernel stuck on a lock would.
pub fn hang() -> ! {
    x86_64::instructions::interrupts::disable();
    loop {
        core::hint::spin_loop();
    }
}

/// A seed for a run's patterns: the TSC, which differs every boot.
pub fn seed() -> u64 {
    crate::tsc::read()
}
