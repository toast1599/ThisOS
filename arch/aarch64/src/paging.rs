//! Page tables, reached through Limine's mapping of all physical memory.
//! The kernel's tables are TTBR1's, the upper half every address space
//! shares; a user space is a level 0 table of its own in TTBR0. Nothing
//! here makes a reference to a table: each access is one atomic operation
//! on one entry, as on x86_64. The walker writes no entries today, since
//! `TCR_EL1.HA` and HD stay off, but it would if they were ever turned on.

use core::marker::PhantomData;
use core::ops::{Deref, Range};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aarch64_cpu::registers::{ID_AA64MMFR1_EL1, TCR_EL1};
use spin::Mutex;
use thisos_frames::Frame;
use thisos_hal::{MapError, MapErrorKind, Protection, Translation};
use thisos_regions::{InsertError, Regions};

use crate::heap::{self, Heap};
use crate::{ptr_at, to_u64};

/// The CPU state the tables depend on: the translation registers, PAN, the
/// TLBs, the caches and the barriers that order the tables' stores for the
/// walker. Host tests swap in `tests::cpu`, a fake per test thread.
#[cfg(not(test))]
mod cpu {
    use aarch64_cpu::asm::barrier::{self, ISH, ISHST};
    use aarch64_cpu::registers::{
        ID_AA64MMFR1_EL1, MAIR_EL1, Readable, TCR_EL1, TTBR0_EL1, TTBR1_EL1,
    };

    pub(super) use crate::sysreg::{
        dc_civac, enable_pan, read_ctr, sync_icache, tlbi_aside1is, tlbi_vaae1is, tlbi_vae1is,
        tlbi_vmalle1is, write_mair, write_tcr, write_ttbr0, write_ttbr1,
    };

    pub fn mmfr1() -> u64 {
        ID_AA64MMFR1_EL1.get()
    }

    pub fn tcr() -> u64 {
        TCR_EL1.get()
    }

    pub fn mair() -> u64 {
        MAIR_EL1.get()
    }

    /// The level 0 table TTBR0 points at.
    pub fn ttbr0() -> u64 {
        TTBR0_EL1.get_baddr()
    }

    /// The level 0 table TTBR1 points at.
    pub fn ttbr1() -> u64 {
        TTBR1_EL1.get_baddr()
    }

    pub fn dsb_ishst() {
        barrier::dsb(ISHST);
    }

    pub fn dsb_ish() {
        barrier::dsb(ISH);
    }

    pub fn isb() {
        barrier::isb(barrier::SY);
    }
}

#[cfg(test)]
use tests::cpu;

// Descriptor bits, for the 4 KiB granule.
const VALID: u64 = 1 << 0;
/// In a level 0 to 2 entry, a table rather than a block; in a level 3
/// entry it must be set.
const TABLE_OR_PAGE: u64 = 1 << 1;
const ATTR_INDEX: u64 = 7 << 2;
/// Attribute index 2, which Limine leaves unused and the kernel gives the
/// framebuffer.
const ATTR_NON_CACHEABLE: u64 = 2 << 2;
/// Attribute index 3, which Limine leaves unused and the kernel gives
/// device registers.
const ATTR_DEVICE: u64 = 3 << 2;
/// AP[1]: EL0 may reach the page.
const EL0: u64 = 1 << 6;
/// AP[2]: nothing may write the page.
const READ_ONLY: u64 = 1 << 7;
const INNER_SHAREABLE: u64 = 3 << 8;
/// The Access flag. Without it the first access faults, since the walker
/// doesn't set it with `TCR_EL1.HA` off.
const ACCESSED: u64 = 1 << 10;
const NOT_GLOBAL: u64 = 1 << 11;
/// EL1 may not execute the page.
const PXN: u64 = 1 << 53;
/// EL0 may not execute the page.
const UXN: u64 = 1 << 54;
const ADDRESS: u64 = 0x0000_ffff_ffff_f000;

/// The bits every leaf the kernel makes has: valid, normal write-back
/// memory (attribute index 0), inner shareable, accessed.
const LEAF: u64 = VALID | TABLE_OR_PAGE | INNER_SHAREABLE | ACCESSED;
const TABLE: u64 = VALID | TABLE_OR_PAGE;

/// The first address TTBR1 translates: T1SZ is 16, as Limine sets it.
const UPPER_HALF: u64 = 0xffff_0000_0000_0000;

/// Where user mappings may go: TTBR0's 256 TiB, above the first 64 KiB.
pub const USER_SPACE: Range<u64> = 0x1_0000..1 << 48;

/// `MAIR_EL1`'s attribute 2 as normal memory, non-cacheable inner and outer,
/// so that the CPU gathers stores to it as x86_64's write-combining does.
const MAIR_NON_CACHEABLE: u64 = 0x44;
/// `MAIR_EL1`'s attribute 3 as Device-nGnRE: no gathering or reordering,
/// with early write acknowledgement, as Linux maps device registers.
const MAIR_DEVICE: u64 = 0x04;

/// The page tables at `level_0`. Whoever makes one is the only user of the
/// tables while it lives, apart from the CPU's page walker.
#[derive(Debug)]
pub struct PageTables {
    phys_offset: u64,
    level_0: u64,
}

/// The table indices of `addr` at levels 0 to 3.
fn indices(addr: u64) -> [u64; 4] {
    [39, 30, 21, 12].map(|shift| (addr >> shift) & 511)
}

/// The bytes one entry of a level `level` table maps.
fn entry_size(level: u8) -> u64 {
    1 << (39 - 9 * u32::from(level))
}

impl PageTables {
    /// Entry `index` of the table at physical address `table`.
    fn entry(&self, table: u64, index: u64) -> Entry<'_> {
        // The table's address comes from a TTBR or a table entry, and
        // reaches memory through the physical-memory mapping.
        Entry(ptr_at(self.phys_offset + table + 8 * index), PhantomData)
    }

    /// The level 3 entry that maps `page`, when it is mapped as a 4 KiB
    /// page.
    fn leaf(&self, page: u64) -> Option<Entry<'_>> {
        let [i0, i1, i2, i3] = indices(page);
        let mut table = self.level_0;
        for index in [i0, i1, i2] {
            let value = self.entry(table, index).load();
            if value & VALID == 0 || value & TABLE_OR_PAGE == 0 {
                return None;
            }
            table = value & ADDRESS;
        }
        let leaf = self.entry(table, i3);
        (leaf.load() & VALID != 0).then_some(leaf)
    }

    /// Makes `frame` an empty table, and returns its physical address. The
    /// frame belongs to the tables from here on.
    fn new_table(&self, frame: Frame) -> u64 {
        let start = frame.into_start();
        // A physical address, reached through the physical-memory mapping.
        let table = ptr_at::<u64>(self.phys_offset + start);
        // SAFETY: The allocator handed over the frame, so nothing else
        // reads or writes it, and it isn't in any table yet, so the CPU's
        // walker doesn't either. Its 512 entries are its 4 KiB, which the
        // physical-memory mapping maps.
        unsafe { ptr::write_bytes(table, 0, 512) };
        start
    }

    /// A new empty table for the kernel's tables, from `alloc`. Panics if
    /// it has no frames left, since the kernel can't run without them.
    fn kernel_table(&self, alloc: &mut dyn FnMut() -> Option<Frame>) -> u64 {
        let Some(frame) = alloc() else {
            panic!("no frames left for the kernel's page tables");
        };
        self.new_table(frame)
    }

    /// Copies the valid entries of the level `level` table at `from`, which
    /// maps from `va`, into a new table, and returns the new table's
    /// address. Each entry becomes what `copy_entry` makes of it.
    fn copy(
        &self,
        from: u64,
        level: u8,
        va: u64,
        leaf: &dyn Fn(u64, u64, u64) -> u64,
        split: &dyn Fn(u64, u64) -> bool,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> u64 {
        let to = self.kernel_table(alloc);
        for index in 0..512 {
            let value = self.entry(from, index).load();
            if value & VALID == 0 {
                continue;
            }
            let address = va + index * entry_size(level);
            let value = self.copy_entry(value, level, address, leaf, split, alloc);
            // SAFETY: The new table isn't in use yet: no TTBR points at it.
            unsafe { self.entry(to, index).store(value) };
        }
        to
    }

    /// What the valid entry `value` of a level `level` table, which maps
    /// from `va`, becomes in a copy. A table is copied in turn. A block for
    /// which `split` gives true, given its address and size, becomes a
    /// table of smaller entries that map the same memory. Any other leaf
    /// becomes what `leaf` makes of it, given its address, size and value.
    fn copy_entry(
        &self,
        value: u64,
        level: u8,
        va: u64,
        leaf: &dyn Fn(u64, u64, u64) -> u64,
        split: &dyn Fn(u64, u64) -> bool,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> u64 {
        let size = entry_size(level);
        if !is_leaf(level, value) {
            let table = self.copy(value & ADDRESS, level + 1, va, leaf, split, alloc);
            return (value & !ADDRESS) | table;
        }
        if level == 3 || !split(va, size) {
            return leaf(va, size, value);
        }
        let to = self.kernel_table(alloc);
        let child = entry_size(level + 1);
        let page = if level + 1 == 3 { TABLE_OR_PAGE } else { 0 };
        for index in 0..512 {
            let offset = index * child;
            let value = (value & !ADDRESS & !TABLE_OR_PAGE) | ((value & ADDRESS) + offset) | page;
            let value = self.copy_entry(value, level + 1, va + offset, leaf, split, alloc);
            // SAFETY: The new table isn't in use yet: no TTBR points at it.
            unsafe { self.entry(to, index).store(value) };
        }
        to | TABLE
    }

    /// Maps `page` to `frame` with leaf bits `flags`, and links any new
    /// table on the way, from `alloc`.
    fn map_page(
        &self,
        page: u64,
        frame: Frame,
        flags: u64,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError> {
        // SAFETY: The frame is the caller's to give, and on success the
        // leaf holds it until unmap reclaims it.
        match unsafe { self.map_address(page, frame.start(), flags, alloc) } {
            Ok(()) => {
                let _ = frame.into_start();
                Ok(())
            }
            Err(kind) => Err(MapError { kind, given: frame }),
        }
    }

    /// Maps `page` to the physical address `phys` with leaf bits `flags`,
    /// and links any new table on the way, from `alloc`.
    ///
    /// # Safety
    ///
    /// `phys` must be a frame the caller hands to these tables, a frame
    /// that stays allocated while the page maps it and that the kernel
    /// holds no reference into, or device registers.
    unsafe fn map_address(
        &self,
        page: u64,
        phys: u64,
        flags: u64,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapErrorKind> {
        let [i0, i1, i2, i3] = indices(page);
        let mut table = self.level_0;
        for index in [i0, i1, i2] {
            let entry = self.entry(table, index);
            let mut value = entry.load();
            if value & VALID == 0 {
                let Some(new) = alloc() else {
                    return Err(MapErrorKind::NoFrames);
                };
                value = self.new_table(new) | TABLE;
                // SAFETY: The entry isn't valid, so nothing is reached
                // through it yet.
                unsafe { entry.store(value) };
            }
            // Only Limine's mappings have blocks, and none is in kernel
            // space, the heap's range, the lower half or device registers,
            // so this is a kernel bug.
            assert!(
                value & TABLE_OR_PAGE != 0,
                "a block on the way to {page:#x}"
            );
            table = value & ADDRESS;
        }
        let leaf = self.entry(table, i3);
        if leaf.load() & VALID != 0 {
            return Err(MapErrorKind::Mapped);
        }
        // SAFETY: The entry isn't valid, so nothing is reached through it
        // yet, and the caller vouches for phys.
        unsafe { leaf.store(phys | flags) };
        // An invalid entry is never in a TLB, so the new one needs no
        // flush, only to reach the walker before the page is used.
        cpu::dsb_ishst();
        cpu::isb();
        Ok(())
    }

    /// Unmaps `page` and returns its frame, or `None` if it isn't mapped as
    /// a 4 KiB page. Then drops it from the TLB with `flush`.
    ///
    /// # Safety
    ///
    /// The page's frame must have come from `map_page`, nothing may reach
    /// other memory through the page once it is gone, and `flush` must drop
    /// every TLB entry for the page.
    unsafe fn unmap_page(&self, page: u64, flush: impl FnOnce(u64)) -> Option<Frame> {
        // SAFETY: As for this call.
        let phys = unsafe { self.clear_leaf(page, flush) }?;
        // SAFETY: map_page stored into_start of the frame there, and the
        // entry, now clear, was its only holder.
        Some(unsafe { Frame::reclaim(phys) })
    }

    /// Unmaps `page` and returns the address it mapped, or `None` if it
    /// isn't mapped as a 4 KiB page. Then drops it from the TLB with
    /// `flush`.
    ///
    /// # Safety
    ///
    /// Nothing may reach other memory through the page once it is gone,
    /// and `flush` must drop every TLB entry for the page.
    unsafe fn clear_leaf(&self, page: u64, flush: impl FnOnce(u64)) -> Option<u64> {
        let leaf = self.leaf(page)?;
        // SAFETY: The caller guarantees nothing reaches other memory through
        // the page once it is gone.
        let value = unsafe { leaf.clear() };
        flush(page);
        Some(value & ADDRESS)
    }

    /// Where `addr` is mapped in these tables, which map the half of the
    /// address space that holds `addr`, or `None` if it isn't.
    fn translate(&self, addr: u64) -> Option<Translation> {
        let mut table = self.level_0;
        for (level, index) in (0..4).zip(indices(addr)) {
            let value = self.entry(table, index).load();
            if value & VALID == 0 {
                return None;
            }
            if is_leaf(level, value) {
                let size = entry_size(level);
                let user = value & EL0 != 0;
                return Some(Translation {
                    phys: (value & ADDRESS & !(size - 1)) | (addr & (size - 1)),
                    writable: value & READ_ONLY == 0,
                    executable: value & if user { UXN } else { PXN } == 0,
                    user,
                });
            }
            table = value & ADDRESS;
        }
        None
    }

    /// Calls `free` with each table below the level `level` table at
    /// `table`. The frames that level 3 entries map are left alone: they
    /// are a user space's, whose mappings' objects hold them.
    ///
    /// # Safety
    ///
    /// Nothing may use the tables or the pages they map any more, and every
    /// table in them must have come from `new_table`.
    unsafe fn release(&self, table: u64, level: u8, free: &mut dyn FnMut(Frame)) {
        if level == 3 {
            return;
        }
        for index in 0..512 {
            let value = self.entry(table, index).load();
            if value & VALID == 0 {
                continue;
            }
            let frame = value & ADDRESS;
            // SAFETY: As for this call.
            unsafe { self.release(frame, level + 1, free) };
            // SAFETY: new_table took the frame with into_start, and nothing
            // uses the table any more.
            free(unsafe { Frame::reclaim(frame) });
        }
    }
}

/// Whether `value`, a valid entry in a level `level` table, maps memory
/// rather than pointing at a table.
fn is_leaf(level: u8, value: u64) -> bool {
    level == 3 || (level > 0 && value & TABLE_OR_PAGE == 0)
}

/// A page-table entry. Only `PageTables::entry` makes one, from a table it
/// reached through the physical-memory mapping, so the pointer is 8-byte
/// aligned and mapped, and the CPU's page walker, which only reads entries
/// while `TCR_EL1.HA` and HD are off, is its only other user while the
/// `PageTables` lives.
#[derive(Clone, Copy)]
struct Entry<'t>(*mut u64, PhantomData<&'t PageTables>);

impl Entry<'_> {
    fn load(self) -> u64 {
        // SAFETY: As the type says: aligned, mapped, and accessed only
        // atomically. The reference lasts this one load.
        unsafe { AtomicU64::from_ptr(self.0) }.load(Ordering::Relaxed)
    }

    /// Replaces the entry with `value`. The store is Release, so that what
    /// was written before it, such as the zeroes of a table it links, can't
    /// be moved after it.
    ///
    /// # Safety
    ///
    /// The entry must not be valid, or its table must not be in use, so
    /// that nothing the kernel reaches changes underneath it.
    unsafe fn store(self, value: u64) {
        // SAFETY: As in load.
        unsafe { AtomicU64::from_ptr(self.0) }.store(value, Ordering::Release);
    }

    /// Clears the entry, and returns what it held.
    ///
    /// # Safety
    ///
    /// Nothing may reach other memory through the entry's page once it is
    /// gone.
    unsafe fn clear(self) -> u64 {
        // SAFETY: As in load.
        unsafe { AtomicU64::from_ptr(self.0) }.swap(0, Ordering::Relaxed)
    }
}

/// Drops the global entry for kernel page `page` from every TLB.
fn flush_kernel_page(page: u64) {
    cpu::tlbi_vaae1is(page);
}

/// Drops every TLB entry of ASID 0, which every user space uses, and
/// keeps the kernel's global ones. The leading isb puts the caller's TTBR0
/// and TCR writes in effect first, so that no walk after the TLBI uses the
/// old values.
fn flush_user() {
    cpu::isb();
    cpu::tlbi_aside1is();
}

/// Drops the entry for user page `page`, in ASID 0, from every TLB.
fn flush_user_page(page: u64) {
    cpu::tlbi_vae1is(page);
}

/// Turns TTBR0's walks off, so that the lower half is empty, and drops
/// what the TLB held of it.
fn empty_lower_half() {
    // SAFETY: With TTBR0's walks off every lower-half access faults, which
    // reaches no memory.
    unsafe { set_ttbr0_walks(false) };
    flush_user();
}

/// Whether TTBR0's walks are on, so that the lower half is mapped through
/// TTBR0's table.
fn ttbr0_walks() -> bool {
    TCR_EL1::EPD0::EnableTTBR0Walks.matches_all(cpu::tcr())
}

/// The page tables the kernel built and runs on. Only `take` makes one,
/// once.
#[derive(Debug)]
pub struct KernelPageTables {
    tables: PageTables,
    /// The level 0 entry the heap, the stacks and `map` map into.
    space: u64,
    heap: Heap,
    /// The lowest address in the stacks' window that no stack has taken,
    /// which `map_stack` moves up.
    stacks: u64,
}

/// The size of every stack EL1 runs on, and the only one `map_stack`
/// takes. Each one's base is aligned to twice this, so that the exception
/// vectors can tell an overflow by one bit of SP.
pub(crate) const STACK_SIZE: usize = 64 << 10;

/// A kernel stack: whole pages of the stacks' window, mapped writable for
/// one owner, with an unmapped page below. Only `map_stack` makes one.
#[derive(Debug)]
pub struct Stack {
    range: Range<u64>,
}

impl Stack {
    pub(crate) fn range(&self) -> Range<u64> {
        self.range.clone()
    }
}

/// The stacks' share of the reserved level 0 entry, right after the heap.
/// A 64 KiB stack takes 128 KiB of it, for its alignment, and stack
/// addresses are never reused, so it holds about 3.1 million.
const STACKS_SIZE: u64 = 384 << 30;

/// Where `map_stack` maps in level 0 entry `space`: right above the heap.
fn stack_window(space: u64) -> Range<u64> {
    let start = space_start(space) + heap::SIZE;
    start..start + STACKS_SIZE
}

/// Whether `range` and the `size` bytes from `va` share an address.
fn overlaps(range: &Range<u64>, va: u64, size: u64) -> bool {
    !range.is_empty() && va < range.end && range.start.saturating_sub(va) < size
}

/// Whether the `size` bytes from `va` all lie in `range`.
fn inside(range: &Range<u64>, va: u64, size: u64) -> bool {
    range.contains(&va) && size <= range.end - va
}

/// The 4 KiB pages that hold `range`.
fn pages(range: &Range<u64>) -> Range<u64> {
    range.start & !0xfff..range.end.next_multiple_of(4096)
}

/// What the kernel's copy of Limine's leaf `value`, which maps `size`
/// bytes from `va`, is: global, out of EL0's reach, executable by EL1 only
/// if it is read-only and inside the kernel's `text_pages`, and normal
/// non-cacheable memory if it holds any of the framebuffer's `fb_pages`.
fn kernel_leaf(
    va: u64,
    size: u64,
    value: u64,
    text_pages: &Range<u64>,
    fb_pages: &Range<u64>,
) -> u64 {
    let code = value & READ_ONLY != 0 && inside(text_pages, va, size);
    let pxn = if code { 0 } else { PXN };
    let attr = if overlaps(fb_pages, va, size) {
        ATTR_NON_CACHEABLE
    } else {
        value & ATTR_INDEX
    };
    (value & !NOT_GLOBAL & !PXN & !ATTR_INDEX & !EL0) | UXN | pxn | attr
}

/// Copies Limine's upper half into new tables made from `alloc`'s frames,
/// letting EL1 execute only the kernel's `text` and making the framebuffer
/// at `fb` normal non-cacheable memory, and maps the physical `devices` as
/// device memory in the physical-memory mapping. Then switches to them,
/// empties the lower half and turns on PAN. Returns `None` after the first
/// call. Panics if the CPU lacks PAN, if Limine's `TCR_EL1` isn't what this
/// module assumes, if Limine mapped the boot stack's guard page at `guard`,
/// or if it mapped any of `devices`.
///
/// # Safety
///
/// `phys_offset` must be where Limine mapped all physical memory, `text`
/// must hold all of the kernel's code, `fb` must be the framebuffer or
/// empty, `devices` must be device registers and no memory, and nothing
/// may use the lower half.
pub unsafe fn take(
    phys_offset: u64,
    guard: u64,
    text: Range<u64>,
    fb: Range<u64>,
    devices: &[Range<u64>],
    alloc: &mut dyn FnMut() -> Option<Frame>,
) -> Option<KernelPageTables> {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    if TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    assert!(
        ID_AA64MMFR1_EL1::PAN.read(cpu::mmfr1()) != 0,
        "the CPU lacks PAN"
    );
    let tcr = cpu::tcr();
    let t1sz = TCR_EL1::T1SZ.read(tcr);
    assert!(t1sz == 16, "Limine set TCR_EL1.T1SZ to {t1sz}, not 16");
    let tg1 = TCR_EL1::TG1.read(tcr);
    assert!(
        TCR_EL1::TG1::KiB_4.matches_all(tcr),
        "Limine set TCR_EL1.TG1 to {tg1:#b}, not 4 KiB"
    );
    // USER_SPACE and every user space's walk assume the same for TTBR0.
    let t0sz = TCR_EL1::T0SZ.read(tcr);
    assert!(t0sz == 16, "Limine set TCR_EL1.T0SZ to {t0sz}, not 16");
    let tg0 = TCR_EL1::TG0.read(tcr);
    assert!(
        TCR_EL1::TG0::KiB_4.matches_all(tcr),
        "Limine set TCR_EL1.TG0 to {tg0:#b}, not 4 KiB"
    );
    // Either would let the walker write entries, which the software Access
    // flag and Entry, whose walker only reads, assume it never does.
    assert!(
        TCR_EL1::HA.read(tcr) == 0 && TCR_EL1::HD.read(tcr) == 0,
        "Limine turned on TCR_EL1.HA or HD"
    );
    let loader = PageTables {
        phys_offset,
        level_0: cpu::ttbr1(),
    };
    assert!(
        loader.translate(guard).is_none(),
        "Limine mapped the boot stack's guard page at {guard:#x}"
    );
    // linker.ld starts the next segment on a new page, so the text's last
    // page holds nothing else.
    let text_pages = pages(&text);
    let fb_pages = pages(&fb);
    let level_0 = loader.copy(
        loader.level_0,
        0,
        UPPER_HALF,
        &|va, size, value| kernel_leaf(va, size, value, &text_pages, &fb_pages),
        // A block only partly framebuffer would make its other memory
        // non-cacheable, which is too slow for RAM.
        &|va, size| overlaps(&fb_pages, va, size) && !inside(&fb_pages, va, size),
        alloc,
    );
    let Some(space) = (0..512).find(|&i| loader.entry(loader.level_0, i).load() & VALID == 0)
    else {
        panic!("Limine left no level 0 entry free in the upper half");
    };
    let tables = PageTables {
        phys_offset,
        level_0,
    };
    let level_1 = tables.kernel_table(alloc);
    // SAFETY: The new tables aren't in use yet.
    unsafe { tables.entry(level_0, space).store(level_1 | TABLE) };
    for page in devices.iter().flat_map(|range| pages(range).step_by(4096)) {
        let flags = VALID | TABLE_OR_PAGE | ACCESSED | ATTR_DEVICE | PXN | UXN;
        // SAFETY: The caller guarantees the page holds device registers.
        if let Err(kind) = unsafe { tables.map_address(phys_offset + page, page, flags, alloc) } {
            panic!("mapping the device page {page:#x} failed: {kind:?}");
        }
    }
    let mair = cpu::mair() & !(0xffff << 16);
    // SAFETY: Limine's tables use attributes 0 and 1 alone, so changing 2
    // and 3 changes no live mapping.
    unsafe { cpu::write_mair(mair | (MAIR_NON_CACHEABLE << 16) | (MAIR_DEVICE << 24)) };
    // SAFETY: The new tables map every upper-half address to the same frame
    // as Limine's, with the same attributes apart from the global bit,
    // EL0 access, execute permissions and the framebuffer's cacheability,
    // and the kernel's code, stack, statics, Limine's answers, the
    // framebuffer and the physical-memory mapping are all in the upper
    // half. The kernel executes only its code, which the caller guarantees
    // is in text and stays read-only and executable. load_kernel's isb puts
    // the MAIR write in effect too.
    unsafe { load_kernel(level_0) };
    if !fb_pages.is_empty() {
        // SAFETY: fb_pages is the framebuffer, mapped in the new tables.
        unsafe { clean(fb_pages) };
    }
    // The caller guarantees nothing uses the lower half.
    empty_lower_half();
    // SAFETY: PAN only stops EL1 reaching EL0 pages, of which there are
    // none yet; the kernel never reaches one but through user copies, which
    // will clear PSTATE.PAN for it.
    unsafe { cpu::enable_pan() };
    Some(KernelPageTables {
        tables,
        space,
        // SAFETY: Limine mapped nothing in the space entry, TAKEN makes
        // this the only Heap, and only grow_heap maps in its range, since
        // map_stack and map stay inside the stacks' window and
        // kernel_space, which start above it.
        heap: unsafe { Heap::new(space_start(space)) },
        stacks: stack_window(space).start,
    })
}

/// Makes TTBR1 point at the level 0 table at `level_0`, and drops every
/// TLB entry Limine's tables left.
///
/// # Safety
///
/// The table must map everything the kernel reaches as the tables TTBR1
/// points at now do, executable where the kernel executes.
unsafe fn load_kernel(level_0: u64) {
    // The Release stores that built the tables order them, but the walker
    // is only certain to see them after a dsb.
    cpu::dsb_ishst();
    // SAFETY: The caller guarantees the table maps what the kernel reaches.
    unsafe { cpu::write_ttbr1(level_0) };
    cpu::isb();
    // QEMU models no TLB conflicts; hardware that does could raise one
    // between the switch and this flush for an address whose old and new
    // entries differ in the global bit, which needs the switch made from
    // tables that map the switching code alone.
    cpu::tlbi_vmalle1is();
}

/// Turns TTBR0's walks on or off with `TCR_EL1.EPD0` (bit 7).
///
/// # Safety
///
/// Turning them on needs TTBR0 to hold a table that `cpu::write_ttbr0`'s
/// contract covers.
unsafe fn set_ttbr0_walks(on: bool) {
    let tcr = cpu::tcr();
    let tcr = if on { tcr & !(1 << 7) } else { tcr | (1 << 7) };
    // SAFETY: Only EPD0 changes, and the caller guarantees TTBR0's table
    // when it turns the walks on.
    unsafe { cpu::write_tcr(tcr) };
}

/// The first address that level 0 entry `index` of the upper half maps.
fn space_start(index: u64) -> u64 {
    UPPER_HALF + index * entry_size(0)
}

impl thisos_hal::PageTables for KernelPageTables {
    type AddressSpace<O: Deref<Target = [Frame]> + Send + Sync> = UserSpace<O>;
    type Stack = Stack;

    fn kernel_space(&self) -> Range<u64> {
        let start = space_start(self.space);
        start + heap::SIZE + STACKS_SIZE..start + entry_size(0)
    }

    fn map(
        &mut self,
        page: u64,
        frame: Frame,
        writable: bool,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError> {
        let fail = |kind, given| Err(MapError { kind, given });
        if page % 4096 != 0 {
            return fail(MapErrorKind::Unaligned, frame);
        }
        if !self.kernel_space().contains(&page) {
            return fail(MapErrorKind::OutsideKernelSpace, frame);
        }
        let write = if writable { 0 } else { READ_ONLY };
        self.tables
            .map_page(page, frame, LEAF | PXN | UXN | write, alloc)
    }

    fn unmap(&mut self, page: u64) -> Option<Frame> {
        if page % 4096 != 0 || !self.kernel_space().contains(&page) {
            return None;
        }
        // SAFETY: Only map maps in kernel space, through map_page, and
        // kernel space's addresses are handed out once, so code still using
        // the page faults. The page is global, which flush_kernel_page
        // drops whatever the ASID.
        unsafe { self.tables.unmap_page(page, flush_kernel_page) }
    }

    fn map_stack(
        &mut self,
        size: usize,
        alloc: &mut dyn FnMut() -> Option<Frame>,
        free: &mut dyn FnMut(Frame),
    ) -> Option<Stack> {
        assert_eq!(
            size, STACK_SIZE,
            "a stack of {size} bytes; aarch64 maps only {STACK_SIZE}"
        );
        let size = to_u64(STACK_SIZE);
        let align = 2 * size;
        let window = stack_window(self.space);
        // The page below start stays unmapped: it is at or above the
        // cursor, where nothing is mapped yet.
        let start = (self.stacks + 4096).next_multiple_of(align);
        let range = start..start + size;
        if range.end > window.end {
            return None;
        }
        // A failed stack's addresses aren't mapped again either.
        self.stacks = range.end;
        for page in range.clone().step_by(4096) {
            let Some(frame) = alloc() else {
                self.unmap_stack(Stack { range: start..page }, free);
                return None;
            };
            if let Err(e) = self.tables.map_page(page, frame, LEAF | PXN | UXN, alloc) {
                assert_eq!(
                    e.kind,
                    MapErrorKind::NoFrames,
                    "mapping a stack page at {page:#x} failed"
                );
                free(e.given);
                self.unmap_stack(Stack { range: start..page }, free);
                return None;
            }
        }
        Some(Stack { range })
    }

    fn unmap_stack(&mut self, stack: Stack, free: &mut dyn FnMut(Frame)) {
        for page in stack.range.step_by(4096) {
            // SAFETY: Only map_stack maps in the stacks' window, through
            // map_page, and never at an address it mapped before, so code
            // still using the page faults. The Stack was the page's one
            // holder. The page is global, which flush_kernel_page drops
            // whatever the ASID.
            let frame = unsafe { self.tables.unmap_page(page, flush_kernel_page) };
            let Some(frame) = frame else {
                panic!("stack page {page:#x} wasn't mapped");
            };
            free(frame);
        }
    }

    fn grow_heap(
        &mut self,
        frame: Frame,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError> {
        let next = self.heap.next();
        assert!(
            next < space_start(self.space) + heap::SIZE,
            "the heap's {} GiB are full",
            heap::SIZE >> 30
        );
        self.tables.map_page(next, frame, LEAF | PXN | UXN, alloc)?;
        // SAFETY: map_page mapped the page writable to the frame, and
        // nothing unmaps in the heap's range, maps there or reaches the
        // frame otherwise.
        unsafe { self.heap.grow() };
        Ok(())
    }

    fn translate(&self, addr: u64) -> Option<Translation> {
        if addr < UPPER_HALF {
            return None;
        }
        self.tables.translate(addr)
    }

    // These tables are TTBR1's and can't map the lower half, so this says
    // whether the CPU maps any of it now: whether TTBR0's walks are on with
    // a table that maps anything. The kernel's activate turns them off.
    fn lower_half_mapped(&self) -> bool {
        if !ttbr0_walks() {
            return false;
        }
        let ttbr0 = PageTables {
            phys_offset: self.tables.phys_offset,
            level_0: cpu::ttbr0(),
        };
        (0..512).any(|i| ttbr0.entry(ttbr0.level_0, i).load() & VALID != 0)
    }

    fn new_space<O: Deref<Target = [Frame]> + Send + Sync>(
        &self,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Option<UserSpace<O>> {
        let level_0 = self.tables.new_table(alloc()?);
        Some(UserSpace {
            tables: PageTables {
                phys_offset: self.tables.phys_offset,
                level_0,
            },
            kernel: self.tables.level_0,
            mappings: Mutex::new(Regions::new()),
        })
    }

    fn activate(&self) {
        empty_lower_half();
    }
}

/// A user address space: a level 0 table for TTBR0. Only
/// `KernelPageTables::new_space` makes one.
pub struct UserSpace<O> {
    tables: PageTables,
    /// The kernel's level 0 table, which maps the upper half for every
    /// space.
    kernel: u64,
    /// Every page mapped in the lower half is in one of these, and maps a
    /// frame of its object. Map and unmap change the tables only while
    /// they hold this lock.
    mappings: Mutex<Regions<O>>,
}

impl<O> UserSpace<O> {
    /// Whether the CPU translates the lower half through this space.
    fn active(&self) -> bool {
        ttbr0_walks() && cpu::ttbr0() == self.tables.level_0
    }
}

impl<O: Deref<Target = [Frame]> + Send + Sync> thisos_hal::AddressSpace<O> for UserSpace<O> {
    fn user_space(&self) -> Range<u64> {
        USER_SPACE
    }

    fn map(
        &self,
        addr: u64,
        object: O,
        first: usize,
        pages: usize,
        protection: Protection,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError<O>> {
        let fail = |kind, given| Err(MapError { kind, given });
        if addr % 4096 != 0 {
            return fail(MapErrorKind::Unaligned, object);
        }
        if !USER_SPACE.contains(&addr) {
            return fail(MapErrorKind::OutsideUserSpace, object);
        }
        let end = u64::try_from(pages)
            .ok()
            .and_then(|pages| pages.checked_mul(4096))
            .and_then(|len| addr.checked_add(len))
            .filter(|&end| end <= USER_SPACE.end);
        let in_object = first
            .checked_add(pages)
            .is_some_and(|last| last <= object.len());
        let Some(end) = end.filter(|_| pages > 0 && in_object) else {
            return fail(MapErrorKind::OutOfRange, object);
        };
        let flags = match protection {
            Protection::ReadOnly => READ_ONLY | UXN,
            Protection::ReadWrite => UXN,
            Protection::ReadExecute => READ_ONLY,
        };
        let active = self.active();
        let flush = |page| {
            if active {
                flush_user_page(page);
            }
        };
        let mut mappings = self.mappings.lock();
        let object = match mappings.insert(addr..end, object) {
            Ok(object) => object,
            Err((InsertError::Overlap, object)) => return fail(MapErrorKind::Mapped, object),
            Err((InsertError::NoMemory, object)) => return fail(MapErrorKind::NoMemory, object),
        };
        let mut done = addr;
        let mapped = object[first..first + pages].iter().try_for_each(|frame| {
            // SAFETY: The mapping now holds the object, which holds the
            // frame, until unmap or destroy clears the page. The kernel
            // never reaches user pages but through user copies.
            unsafe {
                self.tables.map_address(
                    done,
                    frame.start(),
                    LEAF | EL0 | NOT_GLOBAL | PXN | flags,
                    alloc,
                )
            }?;
            done += 4096;
            Ok(())
        });
        let Err(kind) = mapped else {
            return Ok(());
        };
        for page in (addr..done).step_by(4096) {
            // SAFETY: This call mapped the page a moment ago, and returns no
            // address in it, so nothing reaches it.
            unsafe { self.tables.clear_leaf(page, flush) };
        }
        let Some((_, object)) = mappings.remove(addr) else {
            panic!("the mapping at {addr:#x} went missing");
        };
        fail(kind, object)
    }

    fn unmap(&self, addr: u64) -> Option<O> {
        let mut mappings = self.mappings.lock();
        let (range, object) = mappings.remove(addr)?;
        let active = self.active();
        for page in range.step_by(4096) {
            // SAFETY: Only map maps in the lower half, every page of the
            // mapping, and the frames stay the object's, so nothing but
            // user code reaches memory through them, and the object goes
            // back only after the pages are gone. Every space uses ASID 0,
            // and switching spaces drops ASID 0's entries, so the TLB holds
            // the page only while this space is active.
            let cleared = unsafe {
                self.tables.clear_leaf(page, |page| {
                    if active {
                        flush_user_page(page);
                    }
                })
            };
            assert!(
                cleared.is_some(),
                "mapped page {page:#x} wasn't in the tables"
            );
        }
        Some(object)
    }

    fn translate(&self, addr: u64) -> Option<Translation> {
        let level_0 = if addr >= UPPER_HALF {
            self.kernel
        } else if addr < 1 << 48 {
            self.tables.level_0
        } else {
            return None;
        };
        PageTables {
            phys_offset: self.tables.phys_offset,
            level_0,
        }
        .translate(addr)
    }

    fn activate(&self) {
        // SAFETY: Only map maps in the space, with frames its mappings'
        // objects hold until the pages are gone.
        unsafe { cpu::write_ttbr0(self.tables.level_0) };
        // Without it the TCR write could take effect first, and walks
        // would start from the TTBR0 a destroyed space left.
        cpu::isb();
        // SAFETY: TTBR0 holds this space's table, as above.
        unsafe { set_ttbr0_walks(true) };
        flush_user();
    }

    fn destroy(self, unmapped: &mut dyn FnMut(O), free: &mut dyn FnMut(Frame)) {
        if self.active() {
            empty_lower_half();
        }
        // SAFETY: self is gone after this, TTBR0's walks don't use its
        // tables, and every table in them came from new_table.
        unsafe { self.tables.release(self.tables.level_0, 0, free) };
        // SAFETY: new_space took the level 0 frame with into_start, and
        // nothing uses the table any more.
        free(unsafe { Frame::reclaim(self.tables.level_0) });
        // No table maps the objects' frames any more.
        for (_, object) in self.mappings.into_inner().take_all() {
            unmapped(object);
        }
    }
}

/// Where `addr` is mapped in the tables the CPU uses now.
#[cfg_attr(
    test,
    expect(
        dead_code,
        reason = "only Platform::translate_active calls it, which host tests don't build"
    )
)]
pub fn translate_active(phys_offset: u64, addr: u64) -> Option<Translation> {
    // Only reads entries, one atomic load each, so it can share the tables
    // with their owner.
    let level_0 = if addr >= UPPER_HALF {
        cpu::ttbr1()
    } else if addr < 1 << 48 && ttbr0_walks() {
        cpu::ttbr0()
    } else {
        return None;
    };
    PageTables {
        phys_offset,
        level_0,
    }
    .translate(addr)
}

/// Writes `ret` at the start of `frame`, and makes instruction fetches see
/// it.
#[cfg_attr(
    test,
    expect(
        dead_code,
        reason = "only Platform::write_return calls it, which host tests don't build"
    )
)]
pub fn write_return(phys_offset: u64, frame: &mut Frame) {
    // A physical address, reached through the physical-memory mapping.
    let word = ptr_at::<u32>(phys_offset + frame.start());
    // SAFETY: The caller holds the frame mutably. Kernel page tables hold
    // the frames they map, and a user space holds an object that shares
    // the frames it maps, so neither maps this one: nothing maps it but the
    // physical-memory mapping, and no Rust reference to its memory exists.
    unsafe { word.write_volatile(0xd65f_03c0) };
    cpu::sync_icache(to_u64(word.addr()));
}

/// Writes back and drops every data cache line that holds part of the
/// virtual addresses `range`. Lines the console's stores left while Limine
/// mapped the framebuffer write-back go out to memory this way; no new
/// ones come once it is non-cacheable.
///
/// # Safety
///
/// `range` must be mapped.
unsafe fn clean(range: Range<u64>) {
    // DminLine, bits 19:16, is the smallest data cache line in words.
    let line = 4 << ((cpu::read_ctr() >> 16) & 0xf);
    let mut addr = range.start & !(line - 1);
    while addr < range.end {
        // SAFETY: The caller guarantees range is mapped, and the line
        // holding addr overlaps it, so lies in the same pages.
        unsafe { cpu::dc_civac(addr) };
        addr += line;
    }
    cpu::dsb_ish();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::vec::Vec;
    use std::{hint, thread, vec};

    use thisos_frames::{FLOOR, Frames};
    use thisos_hal::{AddressSpace as _, PageTables as _};

    use self::cpu::Flush;
    use super::*;

    /// The CPU as the tables see it, faked per test thread: the registers
    /// start as Limine leaves them, but with TTBR0 and TTBR1 at 0, where no
    /// table is; TLB maintenance and cache cleaning are recorded, and
    /// barriers do nothing.
    pub mod cpu {
        use core::cell::RefCell;
        use std::vec::Vec;

        use aarch64_cpu::registers::{ID_AA64MMFR1_EL1, TCR_EL1};

        /// A TLB maintenance operation.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Flush {
            /// `tlbi vaae1is`: a page in every ASID, global entries too.
            KernelPage(u64),
            /// `tlbi vae1is`: a page in ASID 0.
            UserPage(u64),
            /// `tlbi aside1is`: all of ASID 0.
            User,
            /// `tlbi vmalle1is`: everything.
            All,
        }

        /// `MAIR_EL1` before `take`: attribute 0 normal write-back and 1
        /// device, as Limine uses them, and bytes in 2 and 3 that `take`
        /// replaces.
        pub const MAIR: u64 = 0x1122_3344_5566_04ff;

        pub struct Fake {
            pub tcr: u64,
            pub mair: u64,
            pub ttbr0: u64,
            pub ttbr1: u64,
            pub pan: bool,
            pub flushes: Vec<Flush>,
            /// The addresses `dc civac` was given.
            pub cleaned: Vec<u64>,
        }

        std::thread_local! {
            static FAKE: RefCell<Fake> = RefCell::new(Fake {
                // 48-bit halves of 4 KiB pages, TTBR0's walks on.
                tcr: (TCR_EL1::T0SZ.val(16)
                    + TCR_EL1::T1SZ.val(16)
                    + TCR_EL1::TG0::KiB_4
                    + TCR_EL1::TG1::KiB_4)
                    .value,
                mair: MAIR,
                ttbr0: 0,
                ttbr1: 0,
                pan: false,
                flushes: Vec::new(),
                cleaned: Vec::new(),
            });
        }

        pub fn with<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
            FAKE.with_borrow_mut(f)
        }

        /// The flushes since the last call.
        pub fn flushes() -> Vec<Flush> {
            with(|cpu| core::mem::take(&mut cpu.flushes))
        }

        fn flush(op: Flush) {
            with(|cpu| cpu.flushes.push(op));
        }

        pub fn mmfr1() -> u64 {
            ID_AA64MMFR1_EL1::PAN.val(1).value
        }

        pub fn tcr() -> u64 {
            with(|cpu| cpu.tcr)
        }

        pub fn mair() -> u64 {
            with(|cpu| cpu.mair)
        }

        pub fn ttbr0() -> u64 {
            with(|cpu| cpu.ttbr0)
        }

        pub fn ttbr1() -> u64 {
            with(|cpu| cpu.ttbr1)
        }

        /// # Safety
        ///
        /// None; it is unsafe as the real one is.
        pub unsafe fn write_ttbr0(table: u64) {
            with(|cpu| cpu.ttbr0 = table);
        }

        /// # Safety
        ///
        /// None; it is unsafe as the real one is.
        pub unsafe fn write_ttbr1(table: u64) {
            with(|cpu| cpu.ttbr1 = table);
        }

        /// # Safety
        ///
        /// None; it is unsafe as the real one is.
        pub unsafe fn write_tcr(value: u64) {
            with(|cpu| cpu.tcr = value);
        }

        /// # Safety
        ///
        /// None; it is unsafe as the real one is.
        pub unsafe fn write_mair(value: u64) {
            with(|cpu| cpu.mair = value);
        }

        /// # Safety
        ///
        /// None; it is unsafe as the real one is.
        pub unsafe fn enable_pan() {
            with(|cpu| cpu.pan = true);
        }

        /// 64-byte data cache lines: `CTR_EL0.DminLine`, in words, is 16.
        pub fn read_ctr() -> u64 {
            4 << 16
        }

        /// # Safety
        ///
        /// None; it is unsafe as the real one is.
        pub unsafe fn dc_civac(addr: u64) {
            with(|cpu| cpu.cleaned.push(addr));
        }

        pub fn sync_icache(_: u64) {}

        pub fn tlbi_vaae1is(page: u64) {
            flush(Flush::KernelPage(page));
        }

        pub fn tlbi_vae1is(page: u64) {
            flush(Flush::UserPage(page));
        }

        pub fn tlbi_aside1is() {
            flush(Flush::User);
        }

        pub fn tlbi_vmalle1is() {
            flush(Flush::All);
        }

        pub fn dsb_ishst() {}

        pub fn dsb_ish() {}

        pub fn isb() {}
    }

    /// Physical memory ends here; the frames from `FLOOR` up are free.
    const END: u64 = 0x14_0000;
    const PAGES: usize = 0x140;
    const _: () = assert!(END == 4096 * 0x140);

    #[derive(Clone)]
    #[repr(C, align(4096))]
    struct Phys([u8; 4096]);

    /// A buffer standing in for physical memory from 0 to `END`, and the
    /// frame allocator over it.
    struct Memory {
        frames: Frames<5>,
        phys_offset: u64,
        _buffer: Vec<Phys>,
    }

    impl Memory {
        fn new() -> Self {
            let mut buffer = vec![Phys([0; 4096]); PAGES];
            // The tables reach the buffer only through this address, as the
            // kernel reaches RAM through the physical-memory mapping, so the
            // buffer's provenance has to be exposed.
            let start = buffer.as_mut_ptr().expose_provenance();
            let Ok(start) = u64::try_from(start) else {
                panic!("a host address wider than 64 bits");
            };
            let mut frames = Frames::new();
            assert_eq!(frames.add(FLOOR..END), 0);
            Self {
                frames,
                phys_offset: start,
                _buffer: buffer,
            }
        }

        fn frame(&mut self) -> Frame {
            let Some(frame) = self.frames.alloc() else {
                panic!("the test's physical memory is used up");
            };
            frame
        }

        fn object(&mut self, pages: usize) -> Object {
            Object(Arc::new((0..pages).map(|_| self.frame()).collect()))
        }

        fn free_object(&mut self, object: Object) {
            let Some(frames) = Arc::into_inner(object.0) else {
                panic!("an object is still shared");
            };
            for frame in frames {
                self.frames.free(frame);
            }
        }
    }

    /// A memory object as the kernel's `Mapping` is: frames that mappings
    /// share.
    #[derive(Clone)]
    struct Object(Arc<Vec<Frame>>);

    impl Deref for Object {
        type Target = [Frame];

        fn deref(&self) -> &[Frame] {
            &self.0
        }
    }

    /// Kernel page tables as `take` leaves them, without Limine's upper
    /// half: the reserved level 0 entry and its level 1 table.
    fn kernel(memory: &mut Memory) -> KernelPageTables {
        let space = 300;
        let mut tables = PageTables {
            phys_offset: memory.phys_offset,
            level_0: 0,
        };
        tables.level_0 = tables.new_table(memory.frame());
        let level_1 = tables.new_table(memory.frame());
        // SAFETY: The tables aren't in use.
        unsafe { tables.entry(tables.level_0, space).store(level_1 | TABLE) };
        KernelPageTables {
            tables,
            space,
            // SAFETY: Nothing is mapped in the space entry, and the test
            // never grows the heap.
            heap: unsafe { Heap::new(space_start(space)) },
            stacks: stack_window(space).start,
        }
    }

    fn new_space(kernel: &KernelPageTables, memory: &mut Memory) -> UserSpace<Object> {
        let Some(space) = kernel.new_space(&mut || memory.frames.alloc()) else {
            panic!("no frame for a level 0 table");
        };
        space
    }

    /// Destroys `space`, frees every frame it held, and checks that the
    /// frame count is back to `free`.
    fn destroy(space: UserSpace<Object>, memory: &mut Memory, free: usize) {
        let mut objects = Vec::new();
        space.destroy(&mut |object| objects.push(object), &mut |frame| {
            memory.frames.free(frame);
        });
        for object in objects {
            memory.free_object(object);
        }
        assert_eq!(memory.frames.free_frames(), free);
    }

    /// The level and value of the block or page descriptor that maps
    /// `addr`, or `None` if nothing does.
    fn descriptor(tables: &PageTables, addr: u64) -> Option<(u8, u64)> {
        let mut table = tables.level_0;
        for (level, index) in (0..4).zip(indices(addr)) {
            let value = tables.entry(table, index).load();
            if value & VALID == 0 {
                return None;
            }
            if is_leaf(level, value) {
                return Some((level, value));
            }
            table = value & ADDRESS;
        }
        None
    }

    #[test]
    fn user_mappings_translate_and_unmap() {
        let mut memory = Memory::new();
        let kernel = kernel(&mut memory);
        let free = memory.frames.free_frames();
        let space = new_space(&kernel, &mut memory);
        let data = memory.object(3);
        let code = memory.object(1);
        let (data_1, code_0) = (data[1].start(), code[0].start());
        let mut alloc = || memory.frames.alloc();
        let rw = space.map(
            0x40_0000,
            data.clone(),
            0,
            3,
            Protection::ReadWrite,
            &mut alloc,
        );
        let rx = space.map(0x40_3000, code, 0, 1, Protection::ReadExecute, &mut alloc);
        let ro = space.map(0x80_0000, data, 1, 2, Protection::ReadOnly, &mut alloc);
        assert!(rw.is_ok() && rx.is_ok() && ro.is_ok());
        let translation = |phys, writable, executable| {
            Some(Translation {
                phys,
                writable,
                executable,
                user: true,
            })
        };
        assert_eq!(
            space.translate(0x40_1234),
            translation(data_1 + 0x234, true, false)
        );
        assert_eq!(
            space.translate(0x40_3008),
            translation(code_0 + 8, false, true)
        );
        assert_eq!(
            space.translate(0x80_0010),
            translation(data_1 + 0x10, false, false)
        );
        assert_eq!(space.translate(0x40_4000), None);
        // EL1 may execute no user page.
        let Some((_, leaf)) = descriptor(&space.tables, 0x40_3000) else {
            panic!("the code page isn't mapped");
        };
        assert_eq!(leaf & (PXN | NOT_GLOBAL | EL0), PXN | NOT_GLOBAL | EL0);

        let other = memory.object(1);
        let Err(error) = space.map(0x40_2000, other, 0, 1, Protection::ReadOnly, &mut || {
            memory.frames.alloc()
        }) else {
            panic!("an overlapping mapping was made");
        };
        assert_eq!(error.kind, MapErrorKind::Mapped);
        memory.free_object(error.given);

        let Some(data) = space.unmap(0x40_0000) else {
            panic!("the mapping at 0x40_0000 is gone");
        };
        assert_eq!(space.translate(0x40_1234), None);
        assert!(space.unmap(0x40_0000).is_none());
        // The read-only mapping still shares the frames.
        assert_eq!(
            space.translate(0x80_0010),
            translation(data_1 + 0x10, false, false)
        );
        drop(data);
        assert_eq!(
            cpu::flushes(),
            Vec::<Flush>::new(),
            "flushed an inactive space's pages"
        );
        destroy(space, &mut memory, free);
    }

    #[test]
    fn failed_map_takes_its_pages_back() {
        let mut memory = Memory::new();
        let kernel = kernel(&mut memory);
        let free = memory.frames.free_frames();
        let space = new_space(&kernel, &mut memory);
        let object = memory.object(3);
        // The third page needs a second level 3 table, and the frames run
        // out after the first one.
        let mut budget = 3;
        let mut alloc = || {
            budget -= 1;
            (budget >= 0).then(|| memory.frames.alloc()).flatten()
        };
        let Err(error) = space.map(0x1f_e000, object, 0, 3, Protection::ReadWrite, &mut alloc)
        else {
            panic!("mapped with too few frames");
        };
        assert_eq!(error.kind, MapErrorKind::NoFrames);
        assert_eq!(space.translate(0x1f_e000), None);
        assert_eq!(space.translate(0x1f_f000), None);
        // The range is free again.
        let mapped = space.map(
            0x1f_e000,
            error.given,
            0,
            3,
            Protection::ReadWrite,
            &mut || memory.frames.alloc(),
        );
        assert!(mapped.is_ok());
        destroy(space, &mut memory, free);
    }

    #[test]
    fn only_the_active_space_flushes() {
        let mut memory = Memory::new();
        let kernel = kernel(&mut memory);
        let free = memory.frames.free_frames();
        let space = new_space(&kernel, &mut memory);
        let object = memory.object(2);
        space.activate();
        assert_eq!(cpu::flushes(), [Flush::User]);
        assert!(!kernel.lower_half_mapped());
        let mapped = space.map(0x40_0000, object, 0, 2, Protection::ReadOnly, &mut || {
            memory.frames.alloc()
        });
        assert!(mapped.is_ok());
        // An invalid entry is never in the TLB.
        assert_eq!(cpu::flushes(), Vec::<Flush>::new());
        assert!(kernel.lower_half_mapped());
        let Some(object) = space.unmap(0x40_0000) else {
            panic!("the mapping is gone");
        };
        assert_eq!(
            cpu::flushes(),
            [Flush::UserPage(0x40_0000), Flush::UserPage(0x40_1000)]
        );

        kernel.activate();
        assert_eq!(cpu::flushes(), [Flush::User]);
        let mapped = space.map(0x40_0000, object, 0, 2, Protection::ReadOnly, &mut || {
            memory.frames.alloc()
        });
        assert!(mapped.is_ok());
        assert!(!kernel.lower_half_mapped());
        assert!(space.unmap(0x40_0000).is_some_and(|object| {
            memory.free_object(object);
            true
        }));
        assert_eq!(cpu::flushes(), Vec::<Flush>::new());

        // Destroying the active space turns TTBR0's walks off.
        space.activate();
        assert_eq!(cpu::flushes(), [Flush::User]);
        destroy(space, &mut memory, free);
        assert_eq!(cpu::flushes(), [Flush::User]);
        assert!(!ttbr0_walks());
    }

    #[test]
    fn kernel_space_maps_in_every_space() {
        let mut memory = Memory::new();
        let mut kernel = kernel(&mut memory);
        let space = new_space(&kernel, &mut memory);
        let start = kernel.kernel_space().start;
        for (page, kind) in [
            (start + 8, MapErrorKind::Unaligned),
            (start - 4096, MapErrorKind::OutsideKernelSpace),
        ] {
            let frame = memory.frame();
            let Err(error) = kernel.map(page, frame, true, &mut || memory.frames.alloc()) else {
                panic!("mapped {page:#x}");
            };
            assert_eq!(error.kind, kind);
            memory.frames.free(error.given);
        }

        let frame = memory.frame();
        let phys = frame.start();
        let mapped = kernel.map(start, frame, true, &mut || memory.frames.alloc());
        assert!(mapped.is_ok());
        assert_eq!(cpu::flushes(), Vec::<Flush>::new());
        let expected = Some(Translation {
            phys: phys + 0x10,
            writable: true,
            executable: false,
            user: false,
        });
        assert_eq!(kernel.translate(start + 0x10), expected);
        // The space was made before the page was mapped, and shares the
        // level 0 table's reserved entry.
        assert_eq!(space.translate(start + 0x10), expected);
        let Some((_, leaf)) = descriptor(&kernel.tables, start) else {
            panic!("the page at {start:#x} isn't mapped");
        };
        assert_eq!(leaf & (NOT_GLOBAL | EL0 | UXN | PXN), UXN | PXN);
        assert!(!kernel.lower_half_mapped());

        let Some(frame) = kernel.unmap(start) else {
            panic!("the page at {start:#x} is gone");
        };
        assert_eq!(frame.start(), phys);
        assert_eq!(cpu::flushes(), [Flush::KernelPage(start)]);
        assert!(kernel.unmap(start).is_none());
        assert_eq!(space.translate(start + 0x10), None);
        memory.frames.free(frame);
    }

    // A syscall can start a thread, so running out of frames for its stack
    // fails the call rather than the kernel.
    #[test]
    fn a_stack_without_frames_gives_back_what_it_took() {
        let mut memory = Memory::new();
        let mut kernel = kernel(&mut memory);
        let mut freed = Vec::new();
        // The first stack adds the tables the window's first stacks share.
        let Some(first) =
            kernel.map_stack(STACK_SIZE, &mut || memory.frames.alloc(), &mut |frame| {
                freed.push(frame);
            })
        else {
            panic!("no first stack");
        };
        let free = memory.frames.free_frames();
        let mut left = 2;
        let failed = kernel.map_stack(
            STACK_SIZE,
            &mut || {
                left -= 1;
                if left < 0 {
                    None
                } else {
                    memory.frames.alloc()
                }
            },
            &mut |frame| freed.push(frame),
        );
        assert!(failed.is_none());
        assert_eq!(freed.len(), 2);
        for frame in freed.drain(..) {
            memory.frames.free(frame);
        }
        assert_eq!(memory.frames.free_frames(), free);
        let Some(next) =
            kernel.map_stack(STACK_SIZE, &mut || memory.frames.alloc(), &mut |frame| {
                freed.push(frame);
            })
        else {
            panic!("no stack after the failed one");
        };
        // The failed stack's pages stay unmapped, below the next stack.
        let failed = first.range().end + 4096..next.range().start - 4096;
        assert!(!failed.is_empty());
        assert!(
            failed
                .step_by(4096)
                .all(|page| kernel.translate(page).is_none())
        );
    }

    #[test]
    fn stacks_are_guarded_aligned_and_apart_from_kernel_space() {
        let mut memory = Memory::new();
        let mut kernel = kernel(&mut memory);
        let window = stack_window(kernel.space);
        let free = memory.frames.free_frames();
        let mut taken = 0_usize;
        let mut alloc = || {
            taken += 1;
            memory.frames.alloc()
        };
        let stacks: Vec<Stack> = (0..3)
            .map(|_| {
                let Some(stack) = kernel.map_stack(STACK_SIZE, &mut alloc, &mut |_| {
                    panic!("a stack freed a frame");
                }) else {
                    panic!("no stack");
                };
                stack
            })
            .collect();
        let ranges: Vec<Range<u64>> = stacks.iter().map(Stack::range).collect();
        for range in &ranges {
            assert_eq!(usize::try_from(range.end - range.start), Ok(STACK_SIZE));
            // The trap vectors' overflow check needs this.
            assert_eq!(range.start % (2 * to_u64(STACK_SIZE)), 0, "{range:#x?}");
            assert!(window.start <= range.start && range.end <= window.end);
            assert_eq!(kernel.translate(range.start - 4096), None, "guard page");
            for page in range.clone().step_by(4096) {
                let Some(t) = kernel.translate(page) else {
                    panic!("stack page {page:#x} isn't mapped");
                };
                assert!(t.writable && !t.executable && !t.user, "{page:#x}: {t:?}");
            }
        }

        // map and unmap stay out of the window, guard pages included.
        assert!(!kernel.kernel_space().contains(&(window.end - 1)));
        for page in [ranges[0].start - 4096, ranges[1].start] {
            let frame = memory.frame();
            let Err(error) = kernel.map(page, frame, true, &mut || memory.frames.alloc()) else {
                panic!("mapped {page:#x}");
            };
            assert_eq!(error.kind, MapErrorKind::OutsideKernelSpace);
            memory.frames.free(error.given);
            assert!(kernel.unmap(page).is_none(), "unmapped {page:#x}");
        }

        cpu::flushes();
        let tables = taken - 3 * (STACK_SIZE / 4096);
        for stack in stacks {
            kernel.unmap_stack(stack, &mut |frame| memory.frames.free(frame));
        }
        assert_eq!(memory.frames.free_frames(), free - tables);
        let unmapped: Vec<u64> = ranges
            .iter()
            .flat_map(|range| range.clone().step_by(4096))
            .collect();
        let flushed: Vec<Flush> = unmapped
            .iter()
            .map(|&page| Flush::KernelPage(page))
            .collect();
        assert_eq!(cpu::flushes(), flushed);
        assert!(
            unmapped
                .iter()
                .all(|&page| kernel.translate(page).is_none())
        );
    }

    #[test]
    #[should_panic(expected = "aarch64 maps only 65536")]
    fn stacks_have_one_size() {
        let mut memory = Memory::new();
        let mut kernel = kernel(&mut memory);
        kernel.map_stack(STACK_SIZE / 2, &mut || memory.frames.alloc(), &mut |_| {});
    }

    /// Where the fake Limine maps physical memory and the kernel's image,
    /// level 0 entries 256 and 511, as Limine does.
    const HHDM: u64 = 0xffff_8000_0000_0000;
    const IMAGE: u64 = 0xffff_ffff_8000_0000;

    // take runs once a process, so this is the only test that calls it.
    #[test]
    fn take_copies_limines_upper_half() {
        let mut memory = Memory::new();
        let phys_offset = memory.phys_offset;
        let mut loader = PageTables {
            phys_offset,
            level_0: 0,
        };
        loader.level_0 = loader.new_table(memory.frame());
        let [hhdm_1, hhdm_2, image_1, image_2, image_3] =
            core::array::from_fn(|_| loader.new_table(memory.frame()));
        let normal = VALID | INNER_SHAREABLE | ACCESSED;
        let page = normal | TABLE_OR_PAGE;
        let mut entries = vec![
            (loader.level_0, 256, hhdm_1 | TABLE),
            (hhdm_1, 0, hhdm_2 | TABLE),
            (loader.level_0, 511, image_1 | TABLE),
            (image_1, 510, image_2 | TABLE),
            (image_2, 0, image_3 | TABLE),
            // .text, the second page with bits the copy drops.
            (image_3, 0, 0x10_0000 | page | READ_ONLY),
            (image_3, 1, 0x10_1000 | page | READ_ONLY | EL0 | NOT_GLOBAL),
            // .rodata and .data.
            (image_3, 2, 0x10_2000 | page | READ_ONLY),
            (image_3, 3, 0x10_3000 | page),
        ];
        // Physical memory, as 2 MiB blocks.
        entries.extend((0..4).map(|i| (hhdm_2, i, (i << 21) | normal | NOT_GLOBAL)));
        for (table, index, value) in entries {
            // SAFETY: The tables aren't in use.
            unsafe { loader.entry(table, index).store(value) };
        }
        cpu::with(|cpu| cpu.ttbr1 = loader.level_0);
        // The framebuffer is inside the second block, and the GIC's
        // registers are in none.
        let fb = HHDM + 0x20_3800..HHDM + 0x20_5100;
        let gic = 0x800_0000..0x800_1800;
        let guard = IMAGE - 4096;
        // SAFETY: Nothing reaches memory through the tables, the fake CPU
        // included, so what they map needs no vouching for.
        let kernel = unsafe {
            take(
                phys_offset,
                guard,
                IMAGE..IMAGE + 0x1800,
                fb,
                &[gic],
                &mut || memory.frames.alloc(),
            )
        };
        let Some(kernel) = kernel else {
            panic!("take refused its first call");
        };
        // SAFETY: As above.
        let again = unsafe { take(phys_offset, guard, 0..0, 0..0, &[], &mut || None) };
        assert!(again.is_none(), "take ran twice");

        cpu::with(|cpu| {
            assert_eq!(cpu.ttbr1, kernel.tables.level_0);
            assert_eq!(cpu.mair, 0x1122_3344_0444_04ff);
            assert!(cpu.pan);
            let lines: Vec<u64> = (HHDM + 0x20_3000..HHDM + 0x20_6000).step_by(64).collect();
            assert_eq!(cpu.cleaned, lines);
        });
        assert_eq!(cpu::flushes(), [Flush::All, Flush::User]);
        assert!(!ttbr0_walks());

        let at = |addr| descriptor(&kernel.tables, addr);
        // Blocks stay blocks, global and executable by neither EL.
        let block = normal | UXN | PXN;
        assert_eq!(at(HHDM + 0x1234), Some((2, block)));
        assert_eq!(at(HHDM + 0x40_0000), Some((2, 0x40_0000 | block)));
        // The framebuffer's block becomes pages, non-cacheable where the
        // framebuffer is.
        let split = page | UXN | PXN;
        for phys in [0x20_2000, 0x20_6000] {
            assert_eq!(at(HHDM + phys), Some((3, phys | split)));
        }
        for phys in [0x20_3000, 0x20_4000, 0x20_5000] {
            assert_eq!(
                at(HHDM + phys),
                Some((3, phys | split | ATTR_NON_CACHEABLE))
            );
        }
        // EL1 executes only .text's read-only pages, and EL0 none.
        let code = page | READ_ONLY | UXN;
        assert_eq!(at(IMAGE), Some((3, 0x10_0000 | code)));
        assert_eq!(at(IMAGE + 0x1000), Some((3, 0x10_1000 | code)));
        assert_eq!(at(IMAGE + 0x2000), Some((3, 0x10_2000 | code | PXN)));
        assert_eq!(at(IMAGE + 0x3000), Some((3, 0x10_3000 | page | UXN | PXN)));
        assert_eq!(at(guard), None);
        // The GIC's pages, in the physical-memory mapping.
        let device = VALID | TABLE_OR_PAGE | ACCESSED | ATTR_DEVICE | PXN | UXN;
        for phys in [0x800_0000, 0x800_1000] {
            assert_eq!(at(phys_offset + phys), Some((3, phys | device)));
        }
        // The copy has tables of its own, and the kernel's entry is one
        // Limine left free.
        assert_ne!(
            kernel.tables.entry(kernel.tables.level_0, 256).load(),
            loader.entry(loader.level_0, 256).load()
        );
        assert_eq!(loader.entry(loader.level_0, kernel.space).load(), 0);
    }

    // The walker reads entries while the space changes, so that Miri's race
    // detector sees any non-atomic access to them. Both mappings share one
    // level 3 table, linked before the walker starts, so meanwhile the space
    // only stores and clears entries.
    #[test]
    fn the_walker_reads_entries_as_they_change() {
        let mut memory = Memory::new();
        let kernel = kernel(&mut memory);
        let free = memory.frames.free_frames();
        let space = new_space(&kernel, &mut memory);
        for addr in [0x40_0000, 0x40_4000] {
            let object = memory.object(4);
            let mapped = space.map(addr, object, 0, 4, Protection::ReadWrite, &mut || {
                memory.frames.alloc()
            });
            assert!(mapped.is_ok());
        }
        space.activate();
        let tables = PageTables {
            phys_offset: space.tables.phys_offset,
            level_0: space.tables.level_0,
        };
        let walker = thread::spawn(move || {
            for addr in (0x40_0000..0x40_8000).step_by(4096) {
                hint::black_box(tables.translate(addr));
            }
        });
        let Some(object) = space.unmap(0x40_4000) else {
            panic!("the mapping at 0x40_4000 is gone");
        };
        let phys = object[1].start();
        let mapped = space.map(0x40_4000, object, 1, 2, Protection::ReadOnly, &mut || {
            memory.frames.alloc()
        });
        assert!(mapped.is_ok());
        let Some(object) = space.unmap(0x40_0000) else {
            panic!("the mapping at 0x40_0000 is gone");
        };
        assert!(walker.join().is_ok());
        memory.free_object(object);

        assert_eq!(space.translate(0x40_0000), None);
        assert_eq!(space.translate(0x40_4000).map(|t| t.phys), Some(phys));
        assert_eq!(space.translate(0x40_6000), None);
        destroy(space, &mut memory, free);
    }
}
