//! The traits the ThisOS kernel needs from an architecture. Each
//! architecture crate implements them, and the kernel uses nothing else
//! from the hardware.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;
use core::ops::{Deref, Range};

use thisos_frames::Frame;

/// What the kernel learns from the boot loader, in a form that names no
/// boot protocol. Each architecture crate builds it from its loader's own
/// structure. Fields are added as the kernel comes to need them.
#[derive(Debug)]
pub struct BootInfo<P: Platform> {
    /// The serial port, or `None` when no device answers.
    pub serial: Option<P::Serial>,
    /// The screen, or `None` when the loader set up none in a format the
    /// kernel draws.
    pub framebuffer: Option<Framebuffer<P::Pixels>>,
    /// Whether the architecture made stores to the framebuffer
    /// write-combining. Without it each store is its own bus transaction
    /// where the firmware maps the framebuffer uncached.
    pub framebuffer_write_combining: bool,
    /// Physical memory, as the loader left it.
    pub memory: P::MemoryMap,
    /// The file the loader loaded as its ramdisk, mapped in the upper half
    /// in memory the loader's map marks Loader, or `None` without one.
    pub ramdisk: Option<&'static [u8]>,
}

/// A range of physical memory in the loader's memory map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// The physical address of the first byte.
    pub start: u64,
    /// The physical address after the last byte.
    pub end: u64,
    /// Who uses it.
    pub kind: MemoryKind,
}

/// Who uses a [`MemoryRegion`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// Free RAM the kernel may use.
    Usable,
    /// RAM the loader left in use: the kernel image, its stack, the page
    /// tables and the boot information, the memory map included.
    Loader,
    /// Memory the firmware reports, by the firmware's own type number, such
    /// as UEFI runtime services, ACPI tables or MMIO.
    Firmware(u32),
}

/// A linear framebuffer of 4-byte pixels, as the firmware set it up. Rows
/// are `stride` pixels apart, and `pixels` holds at least `height` of them.
#[derive(Debug)]
pub struct Framebuffer<M> {
    /// The pixels, row after row.
    pub pixels: M,
    /// Visible pixels per row.
    pub width: usize,
    /// Rows.
    pub height: usize,
    /// Pixels from the start of one row to the start of the next.
    pub stride: usize,
    /// The order of the color bytes in each pixel.
    pub format: PixelFormat,
}

/// The memory of a [`Framebuffer`], written one pixel or two at a time
/// and never read: firmware often maps it uncached, where every access is a
/// bus transaction. A pixel is 4 bytes, stored as a little-endian word, and
/// an index counts pixels from the start of the first row. Stores take
/// `&self`, so CPUs can draw disjoint parts through one shared handle
/// without a lock.
#[expect(
    clippy::len_without_is_empty,
    reason = "the console only needs the length to clear the screen"
)]
pub trait Pixels: Send + Sync {
    /// The number of pixels.
    fn len(&self) -> usize;

    /// Stores `pixel` at `index`. Panics if `index` is out of range.
    fn store(&self, index: usize, pixel: u32);

    /// Stores the low half of `pixels` at `index` and the high half after
    /// it, in one 8-byte store. Panics unless `index` is even and both
    /// pixels are in range.
    fn store_pair(&self, index: usize, pixels: u64);
}

/// The byte order of a framebuffer pixel. The fourth byte is unused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Red, green, blue.
    Rgb,
    /// Blue, green, red.
    Bgr,
}

/// Control of the processor the kernel runs on.
pub trait Cpu {
    /// Stops this processor for good: interrupts stay off and it never
    /// runs kernel code again.
    fn halt() -> !;

    /// Walks the frame pointers on the kernel stack and calls `f` with the
    /// address of each call on it, innermost first, starting in the caller.
    /// Addresses are link addresses, inside the call instruction: what
    /// `addr2line` expects for the kernel ELF. Stops after [`MAX_FRAMES`].
    fn backtrace(f: impl FnMut(u64)) -> BacktraceEnd;

    /// Traps with [`Trap::Breakpoint`], and returns once the trap handler
    /// does.
    fn breakpoint();

    /// Lets interrupts reach the trap handler. They are off until this
    /// runs.
    fn enable_interrupts();

    /// Turns interrupts on and waits for one, then returns once its
    /// handler has, with interrupts on.
    fn wait_for_interrupt();

    /// Runs `f` with interrupts off, then turns them back on if they were
    /// on before.
    fn without_interrupts<R>(f: impl FnOnce() -> R) -> R;

    /// Runs `probe`, which never returns by itself, and returns once a trap
    /// handler calls [`TrapFrame::abandon`]. Panics if a catch is running
    /// already. For tests.
    fn catch(probe: Probe);

    /// Whether the kernel is stopped from reading, writing and running user
    /// pages right now, as it is everywhere but in user copies. For tests.
    fn user_access_blocked() -> bool;

    /// Leaves the kernel for user code at `ip` in the address space the
    /// CPU uses, with stack pointer `sp`, `arg` as its first argument,
    /// every other general register zeroed, and interrupts on. The calling
    /// thread comes back to the kernel only through traps, which start at
    /// the top of its stack, so whatever it had there is gone. Panics
    /// unless `ip` is in [`AddressSpace::user_space`] and `sp` is in it or
    /// at its end.
    fn enter_user(ip: u64, sp: u64, arg: u64) -> !;
}

/// Code the architecture runs under [`Cpu::catch`] to make a trap. Each is
/// the architecture's own, written so that abandoning it skips nothing
/// with a destructor, which abandoning a Rust closure can't promise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Reads the first byte of [`Platform::guard_page`] again and again,
    /// which traps with [`Trap::PageFault`].
    ReadGuardPage,
    /// Pushes onto the kernel stack until it runs into the guard page, where
    /// the page fault's handler has no stack to run on, so it traps with
    /// [`Trap::DoubleFault`]. It runs with interrupts off, so that no
    /// interrupt handler runs on the stack it fills and overflows there
    /// itself.
    OverflowStack,
    /// Spins forever with interrupts as they were.
    Spin,
    /// Lets the kernel reach user pages, as a user copy does, then spins
    /// forever. Trap handlers must still find [`Cpu::user_access_blocked`],
    /// and the catch blocks user access again when it returns.
    SpinUserAccessOpen,
    /// Reads a read-only byte of the architecture's own and writes it back,
    /// again and again, which traps with [`Trap::PageFault`] on write at
    /// [`Platform::probe_target`] when the CPU enforces read-only pages.
    WriteReadOnly,
    /// Calls a `ret` of the architecture's own in a no-execute page, again
    /// and again, which traps with [`Trap::PageFault`] on execute at
    /// [`Platform::probe_target`] when the CPU enforces no-execute pages.
    ExecuteNoExecute,
    /// Reads a fixed user address, [`Platform::probe_target`], again and
    /// again, which traps with [`Trap::PageFault`] on read there: because
    /// nothing is mapped there, or because the kernel may not read user
    /// pages.
    ReadUser,
    /// Calls the same user address as [`Probe::ReadUser`] again and again,
    /// which traps with [`Trap::PageFault`] on execute there: because
    /// nothing is mapped there, or because the kernel may not run user
    /// code.
    ExecuteUser,
    /// x86_64 only: with interrupts off, makes user code's GS base the
    /// active one, as it is on a SYSCALL's first instruction, sends its own
    /// CPU an NMI and spins, which traps with [`Trap::Other`] on vector 2.
    /// The catch checks that the NMI's path gave user code's base back.
    NmiWithUserGs,
}

/// The ramdisk file that names which test that expects a panic a QEMU test
/// boot runs, by its place among them from 0, in decimal. xtask adds it to
/// every boot's ramdisk but the first's, and reads its name from here, as
/// it reads [`TestEnd::status`], so that the two can't drift apart.
pub const PANIC_TEST_FILE: &str = "panic-test";

/// How a QEMU test boot ends, which [`Platform::exit_qemu`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestEnd {
    /// Its tests passed.
    Passed,
    /// A test failed.
    Failed,
    /// Its tests passed, and another panic-expecting test follows the one
    /// it ran, for the next boot.
    PassedWithMore,
}

impl TestEnd {
    /// The status QEMU exits with: 33, 35 or 37. None is 0, because
    /// x86_64's `-no-reboot` turns a triple fault into status 0.
    #[must_use]
    pub const fn status(self) -> u8 {
        match self {
            TestEnd::Passed => 33,
            TestEnd::Failed => 35,
            TestEnd::PassedWithMore => 37,
        }
    }
}

/// User code the architecture writes with [`Platform::write_user_program`],
/// for tests that need code in user mode before anything loads programs.
/// Each fits in its first page and runs from anywhere. The architecture
/// writes each as it writes a [`Probe`], so kernel code can't make user
/// code of its own choosing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserProgram {
    /// Puts a value of its own in every general register a syscall keeps, on
    /// x86_64 sets the direction, alignment-check and nested-task flags, which
    /// the kernel must not run with, and on aarch64 the condition flags, makes
    /// a syscall with the number `u64::MAX`, which no syscall has, then checks
    /// those registers and flags. On aarch64 it first checks that `TPIDR_EL0`,
    /// a new thread's, and `TPIDRRO_EL0` read 0, so that no kernel value
    /// reaches user code through them. It exits with the error code the syscall
    /// returned if every check holds and the syscall's value is 0, and with
    /// `u64::MAX` otherwise. It leaves the trap flag clear, since set it would
    /// single-step the program itself.
    Syscalls,
    /// Reads the byte at its argument, then exits with status 0.
    Read,
    /// Runs an instruction only the kernel may run.
    Privileged,
    /// Runs an SSE instruction, or the architecture's equivalent.
    Simd,
    /// Spins for about a quarter second of a counter user code can read,
    /// through many timer ticks, then exits with status 0: 2^28 cycles of
    /// x86_64's TSC, or 2^24 of aarch64's 62.5 MHz virtual counter.
    Spin,
    /// Calls the write syscall with the buffer and the length in the two words
    /// at its argument, then exits with the error code it returned, or with
    /// `u64::MAX` if it succeeded but its value isn't the length.
    Write,
    /// Raises the debug exception that needs no debugger: ICEBP on x86_64,
    /// `brk #0` on aarch64.
    Debug,
    /// Gets a handle with read and duplicate rights at least as its
    /// argument, then checks, in order: (1) duplicating it with read and
    /// duplicate succeeds; (2) duplicating the copy with write fails with
    /// `BadRights`; (3) restricting the copy to read succeeds; (4)
    /// duplicating the copy then fails with `AccessDenied`; (5) closing
    /// the copy succeeds; (6) closing it again fails with `BadHandle`. It
    /// exits with status 0, or with the number of the first check that
    /// failed.
    Handles,
    /// Gets a process handle as its argument, then checks, in order: (1)
    /// creating a 4097-byte memory object fails with `OutOfRange`; (2)
    /// creating an 8 KiB one succeeds; (3) mapping all of it read-write at
    /// `0x40_0000` succeeds, and it writes 42 at its second page; (4) mapping
    /// that page read-only at `0x42_0000` succeeds and reads the 42 there;
    /// (5) mapping a page at `0x40_1000` fails with `AddressInUse`; (6)
    /// mapping it read-execute at `0x41_0000` fails with `Conflict`; (7)
    /// mapping the process handle fails with `WrongType`; (8) unmapping
    /// `0x40_0000` succeeds; (9) mapping the object read-execute at
    /// `0x41_0000` then succeeds. It exits with status 0, or with the number
    /// of the first check that failed.
    Memory,
    /// Gets a process handle with transfer rights as its argument, then
    /// checks, in order: (1) creating a channel succeeds; (2) receiving
    /// at its second endpoint fails with `ShouldWait`; (3) sending `abc`
    /// and the process handle on the first succeeds; (4) closing the
    /// process handle then fails with `BadHandle`, since it moved; (5)
    /// sending the second endpoint on the first fails with `Cycle`; (6)
    /// receiving into a 2-byte buffer fails with `BufferTooSmall`, with 3
    /// bytes and one handle as its value; (7)
    /// receiving then gets 3 bytes, `abc`, and one handle; (8) closing
    /// that handle succeeds; (9) closing the first endpoint succeeds; (10)
    /// sending on the second fails with `PeerClosed`; (11) so does
    /// receiving there. It exits with status 0, or with the number of the
    /// first check that failed.
    Channel,
    /// Gets a process handle as its argument, then checks, in order: (1)
    /// creating a channel succeeds; (2) waiting for its second endpoint to
    /// be readable, with a timeout of 0, fails with `TimedOut`; (3)
    /// waiting for a signal bit that no signal has fails with
    /// `BadSignals`; (4) waiting on a new memory object, which has no
    /// signals, fails with `WrongType`; (5) sending a byte on the first
    /// endpoint succeeds; (6)
    /// waiting for either endpoint to be readable, for good, then returns
    /// 1 and observes the second readable and writable and the first only
    /// writable; (7) closing the first endpoint succeeds; (8) waiting 10 ms
    /// for the second's peer to close returns at once, observing it
    /// readable with its peer closed; (9) waiting 10 ms on no items fails
    /// with `TimedOut`; (10) with a second channel and a 4 KiB memory
    /// object mapped read-write at `0x40_0000` and read-only at
    /// `0x41_0000`, an item for that channel's second endpoint to be
    /// readable, written through the first mapping and waited on for good
    /// through the second, fails with `BadAddress` at once. It exits with
    /// status 0, or with the number of the first check that failed.
    Wait,
    /// Makes a syscall with the number `u64::MAX` from the last
    /// instruction of its page, so the syscall returns to the address after
    /// the page. Mapped on the last page of user space, that address is
    /// user space's end, where nothing is mapped.
    LastSyscall,
    /// Masks interrupts, which user code may not do: `cli` on x86_64,
    /// `msr daifset, #2` on aarch64. Exits with status 0 if that runs.
    MaskInterrupts,
    /// Checks that the registers user code can write for its thread hold
    /// 0 (1), loads its argument into them, spins as `Spin` does, through
    /// many switches, then checks they still hold it (2). They are DS, ES,
    /// FS and GS on x86_64, where the argument has to be a selector ring 3
    /// can load that an interrupt's iretq keeps, the user data selector;
    /// and `TPIDR_EL0` on aarch64. It exits with status 0, or with the number
    /// of the first check that failed.
    ThreadRegisters,
    /// Reads where a kernel descriptor table is, which user code may not:
    /// SGDT on x86_64, where UMIP faults it, and `VBAR_EL1` on aarch64. Exits
    /// with status 0 if that runs, and on x86_64 with status 2 first if
    /// CPUID reports no UMIP.
    DescriptorTable,
}

/// The most frames [`Cpu::backtrace`] reports.
pub const MAX_FRAMES: usize = 32;

/// Why [`Cpu::backtrace`] stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BacktraceEnd {
    /// It reached the outermost frame.
    Done,
    /// It reported [`MAX_FRAMES`] frames.
    TooDeep,
    /// The next frame pointer is outside the kernel stack.
    OutsideStack(u64),
    /// The next frame pointer isn't 8-byte aligned.
    Misaligned(u64),
    /// The next frame pointer doesn't lie above the current one.
    NotIncreasing(u64),
}

/// The devices an architecture hands the kernel in [`BootInfo`].
pub trait Platform {
    /// The serial port the kernel logs to.
    type Serial: Serial;

    /// The memory of the framebuffer in [`BootInfo`].
    type Pixels: Pixels;

    /// The registers the CPU had when it trapped.
    type TrapFrame: TrapFrame;

    /// The memory map in [`BootInfo`]. A clone taken before iterating
    /// reads the whole map again.
    type MemoryMap: Iterator<Item = MemoryRegion> + Clone + Send;

    /// The kernel's own page tables.
    type PageTables: PageTables;

    /// The `e_machine` of the ELF files user programs come in.
    const ELF_MACHINE: u16;

    /// A suspended kernel thread: where it stopped and the stack it runs
    /// on. Resuming it consumes it, so no thread resumes twice, and
    /// dropping it leaves the thread suspended for good.
    type Context: Send;

    /// Builds page tables of the kernel's own, with frames from `alloc`,
    /// that map everything the loader mapped for the kernel at the same
    /// addresses, and nothing else but the registers of devices the
    /// architecture drives itself, and switches to them. Returns `None` if
    /// they were taken already. Panics if `alloc` runs out of frames.
    fn take_page_tables(alloc: &mut dyn FnMut() -> Option<Frame>) -> Option<Self::PageTables>;

    /// Makes the CPU use the kernel's own tables, as
    /// [`PageTables::activate`] does, without the [`PageTables`] value: the
    /// scheduler switches tables in the timer's handler, which can't take
    /// the lock the kernel keeps them behind. Does nothing if the CPU uses
    /// them already. Panics before [`Platform::take_page_tables`].
    fn activate_kernel_tables();

    /// Moves the stacks the CPU switches to for some traps, which start out
    /// with nothing unmapped below them, onto stacks from `stack`, which it
    /// calls with each one's size in bytes and keeps for good. Call it
    /// once, after [`Platform::take_page_tables`].
    fn move_trap_stacks(stack: &mut dyn FnMut(usize) -> Stack<Self>);

    /// The stacks the CPU switches to for some traps. For tests.
    fn trap_stacks() -> impl Iterator<Item = Range<u64>>;

    /// A kernel thread that, once [`Platform::switch`] resumes it, calls
    /// `entry` on `stack` with interrupts off, passing it the context of
    /// the thread that switched to it. The architecture writes the thread's
    /// first frame at the stack's top. The context holds the stack until
    /// [`Platform::into_stack`] gives it back.
    fn new_context(stack: Stack<Self>, entry: fn(Self::Context) -> !) -> Self::Context;

    /// The stack `context`'s thread ran on, which it never runs on again,
    /// for [`PageTables::unmap_stack`]; `None` for the boot thread's, which
    /// the loader mapped.
    fn into_stack(context: Self::Context) -> Option<Stack<Self>>;

    /// Suspends the calling thread and resumes `to`. Returns once another
    /// thread switches to the caller's context, with that thread's own
    /// context, which it left to do so. Panics if interrupts are on.
    fn switch(to: Self::Context) -> Self::Context;

    /// The address of the unmapped page right below the running thread's
    /// stack, which a stack overflow runs into.
    fn guard_page() -> u64;

    /// The address whose page fault ends `probe`: the guard page for
    /// [`Probe::ReadGuardPage`], and the probe's own byte for
    /// [`Probe::WriteReadOnly`] and [`Probe::ExecuteNoExecute`], and a page
    /// in [`AddressSpace::user_space`] for [`Probe::ReadUser`] and
    /// [`Probe::ExecuteUser`]. `None` for a probe that ends some other way.
    /// For tests.
    fn probe_target(probe: Probe) -> Option<u64>;

    /// A second handle to the serial port, for a panic that finds the first
    /// one locked. It returns `Some` at most once, and `None` when no device
    /// answers. Anything else using the port at the same time interleaves
    /// its bytes with this one's.
    fn panic_serial() -> Option<Self::Serial>;

    /// A second handle to the framebuffer in [`BootInfo`], for a panic that
    /// can't use the first: the first is locked, or not handed to the
    /// logger yet. It returns `Some` at most once, and `None` when there is
    /// no framebuffer. Anything still drawing through the first mixes its
    /// pixels with this one's.
    fn panic_framebuffer() -> Option<Framebuffer<Self::Pixels>>;

    /// A third handle to the framebuffer in [`BootInfo`], for the CPUs'
    /// status cells, which each CPU draws without a lock. It returns `Some`
    /// at most once, and `None` when there is no framebuffer.
    fn status_framebuffer() -> Option<Framebuffer<Self::Pixels>>;

    /// The kernel heap's counters. It takes the heap's lock, so trap
    /// handlers never call it.
    fn heap_stats() -> HeapStats;

    /// Starts the timer, which traps with [`Trap::Timer`] `hz` times a
    /// second once interrupts are on, and returns how fast it counts and
    /// how that was found. Call it once. Panics if the CPU lacks the timer
    /// or nothing gives its frequency.
    fn start_timer(hz: u32) -> TimerFrequency;

    /// Starts every other CPU the firmware lists as ready, one at a time,
    /// each on stacks from `stack`, which it calls with the size in bytes
    /// and keeps for good: one to run on, and any the CPU takes traps on.
    /// A started CPU sets up its own state, starts its timer at the rate
    /// [`Platform::start_timer`] set, checks in and calls `entry` on the
    /// stack it runs on, with interrupts off, while the caller goes on to
    /// the next; a trap there goes to the trap handler. Tells `report` what
    /// happens as it goes, so that the caller can log each step before the
    /// next wait. Returns how many CPUs run, the caller's included. Call it
    /// once, after [`Platform::start_timer`].
    fn start_cpus(
        stack: &mut dyn FnMut(usize) -> Stack<Self>,
        report: &mut dyn FnMut(CpuStart),
        entry: fn() -> !,
    ) -> usize;

    /// How many CPUs the firmware lists as ready, the caller's included:
    /// how many [`Platform::start_cpus`] tries to run. Panics if the tables
    /// that list them are malformed.
    fn cpus_listed() -> usize;

    /// Interrupts CPU `cpu`, which traps with the trap `ipi` names once
    /// its interrupts are on. Panics if `cpu` is the caller's own or isn't
    /// running.
    fn send_ipi(cpu: usize, ipi: Ipi);

    /// The timer's frequency from each source the architecture has: the
    /// one the CPU reports, and one measured again against another clock,
    /// with the timer stopped meanwhile. For tests.
    fn timer_frequencies() -> TimerFrequencies;

    /// Whether every line of the legacy interrupt controller is masked, or
    /// `true` where there is none. For tests.
    fn legacy_interrupts_masked() -> bool;

    /// Where `addr` is mapped in the page tables the CPU uses now, or
    /// `None` if it isn't. For tests.
    fn translate_active(addr: u64) -> Option<Translation>;

    /// Writes `bytes` into `frame` from `offset` and zeroes the rest of it.
    /// Panics unless `bytes` fits in the frame from `offset`.
    fn fill_frame(frame: &mut Frame, offset: usize, bytes: &[u8]);

    /// Copies `frame`'s bytes from `offset` into `buf`, reaching them
    /// through the kernel's own mapping. The frame may be shared, and
    /// mapped for user code: the kernel holds no reference into its bytes,
    /// and user code on another CPU, or another copy, may write them
    /// meanwhile, leaving `buf` some bytes from before the write and some
    /// from after. Panics unless `buf` fits in the frame from `offset`.
    fn read_frame(frame: &Frame, offset: usize, buf: &mut [u8]);

    /// Copies `bytes` into `frame` from `offset`, leaving its other bytes
    /// as they were, as [`read_frame`](Self::read_frame) copies the other
    /// way. Of bytes that user code or another copy writes meanwhile, each
    /// keeps one of the values written. Panics as `read_frame` does.
    fn write_frame(frame: &Frame, offset: usize, bytes: &[u8]);

    /// Writes code that only returns at the start of `frame`, for
    /// [`Probe::ExecuteUser`] to find if the CPU ever let it run. For
    /// tests.
    fn write_return(frame: &mut Frame);

    /// Raises the interrupt [`Trap::Spurious`] reports, and returns once
    /// the trap handler does. For tests.
    fn raise_spurious();

    /// Writes `program` at the start of `frame`. For tests.
    fn write_user_program(frame: &mut Frame, program: UserProgram);

    /// Ends a QEMU test boot, reporting how it ended: through QEMU's debug
    /// exit device on x86_64, which it touches only under a hypervisor, and
    /// through semihosting on aarch64, only on QEMU's virt machine. On real
    /// hardware it only halts.
    fn exit_qemu(end: TestEnd) -> !;
}

/// The kernel's own page tables: the upper half every address space
/// shares, with nothing mapped in the lower half. Pages are 4 KiB.
pub trait PageTables: Send {
    /// A user address space whose mappings each hold an `O`.
    type AddressSpace<O: Deref<Target = [Frame]> + Send + Sync>: AddressSpace<O>;

    /// A kernel stack that [`PageTables::map_stack`] mapped, which nothing
    /// else makes: neither `Clone` nor constructible outside the
    /// architecture, so no two threads run on one. Dropping it leaves its
    /// pages mapped for good.
    type Stack;

    /// The virtual addresses [`PageTables::map`] maps into. The loader
    /// mapped nothing there.
    fn kernel_space(&self) -> Range<u64>;

    /// Maps the page at `page` to `frame`, no-execute, and writable if
    /// `writable`. New tables on the way come from `alloc`. The tables hold
    /// the frame until [`PageTables::unmap`].
    fn map(
        &mut self,
        page: u64,
        frame: Frame,
        writable: bool,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError>;

    /// Unmaps the page at `page` in [`PageTables::kernel_space`], and
    /// returns the frame it mapped, or `None` if nothing was mapped there.
    /// Kernel-space addresses are never mapped again after this, so code
    /// that still uses the page faults and reaches no other memory.
    fn unmap(&mut self, page: u64) -> Option<Frame>;

    /// Maps a stack of `size` bytes, rounded up to whole pages, writable
    /// and no-execute, with frames from `alloc`, which also gives any new
    /// table on the way. It lies outside [`PageTables::kernel_space`], with
    /// an unmapped page below it that an overflow runs into, and its start
    /// is aligned to the power of two at or above twice its size, so that
    /// an architecture can tell an overflow by one bit of the stack
    /// pointer. An architecture that does may map one size only, and
    /// panic on any other. Stack addresses are never mapped again after
    /// [`PageTables::unmap_stack`], nor after a failed call. Returns
    /// `None` if `alloc` or the stacks' addresses run out, having handed
    /// the stack's frames it took to `free`; tables it added stay.
    fn map_stack(
        &mut self,
        size: usize,
        alloc: &mut dyn FnMut() -> Option<Frame>,
        free: &mut dyn FnMut(Frame),
    ) -> Option<Self::Stack>;

    /// Unmaps `stack` and hands each of its frames to `free`. The tables
    /// its pages needed stay.
    fn unmap_stack(&mut self, stack: Self::Stack, free: &mut dyn FnMut(Frame));

    /// Maps `frame` at the page after the heap's last one, writable and
    /// no-execute, and adds the page to the heap the global allocator
    /// allocates from. The heap's pages are outside
    /// [`PageTables::kernel_space`], and stay mapped for good. New tables
    /// on the way come from `alloc`. Panics if the heap's range is full.
    fn grow_heap(
        &mut self,
        frame: Frame,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError>;

    /// Where `addr` is mapped, or `None` if it isn't.
    fn translate(&self, addr: u64) -> Option<Translation>;

    /// Whether anything is mapped in the lower half. For tests.
    fn lower_half_mapped(&self) -> bool;

    /// A new address space with nothing in its lower half, sharing this
    /// upper half, with its first table from `alloc`. `None` if `alloc` has
    /// no frame.
    fn new_space<O: Deref<Target = [Frame]> + Send + Sync>(
        &self,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Option<Self::AddressSpace<O>>;

    /// Makes the CPU use these tables, with an empty lower half, in place
    /// of whichever address space it used.
    fn activate(&self);
}

/// A user address space: a lower half of its own, and the upper half of
/// the kernel's [`PageTables`]. Each mapping holds an `O`, whose frames it
/// maps, and hands it back only once no page maps them, so frames can't be
/// freed while user code reaches them. Mappings never overlap. Dropping a
/// space without [`AddressSpace::destroy`] leaks its tables and the `O`s,
/// as dropping a [`Frame`] does.
///
/// Its methods take `&self`, so that the threads sharing a space can
/// switch to it without a lock; map and unmap take a lock of the space's
/// own.
pub trait AddressSpace<O>: Send + Sync {
    /// The virtual addresses [`AddressSpace::map`] maps into: the lower
    /// half, apart from the first pages, which stay unmapped so that a
    /// null pointer plus a small offset always faults, and the last page,
    /// which SYSRET couldn't return past on x86_64 and aarch64 leaves out
    /// to match.
    fn user_space(&self) -> Range<u64>;

    /// Maps `pages` pages from `addr` for user code, with `protection`, to
    /// `object`'s frames from index `first` on. New tables on the way come
    /// from `alloc`. The space holds `object` until [`AddressSpace::unmap`]
    /// or [`AddressSpace::destroy`]. On failure nothing is mapped, and the
    /// error hands `object` back.
    fn map(
        &self,
        addr: u64,
        object: O,
        first: usize,
        pages: usize,
        protection: Protection,
        alloc: &mut dyn FnMut() -> Option<Frame>,
    ) -> Result<(), MapError<O>>;

    /// Unmaps the mapping that starts at `addr`, and returns its object,
    /// or `None` if no mapping starts there. Tables it leaves empty stay
    /// until [`AddressSpace::destroy`].
    fn unmap(&self, addr: u64) -> Option<O>;

    /// Where `addr` is mapped in this space, or `None` if it isn't.
    fn translate(&self, addr: u64) -> Option<Translation>;

    /// Copies the bytes at `addr` in this space into `buf`, reaching their
    /// frame through the kernel's own mapping, so the kernel never reaches
    /// user pages. The space need not be the one the CPU uses. The bytes
    /// must end inside `addr`'s page; a caller splits a longer copy.
    /// Returns `false`, copying nothing, unless the page is mapped for user
    /// code. Panics if the bytes cross a page.
    ///
    /// The page stays mapped until the copy is done: an
    /// [`unmap`](Self::unmap) on another CPU waits for it. User code there
    /// may write the bytes meanwhile, as
    /// [`Platform::read_frame`] describes.
    fn copy_from_user(&self, addr: u64, buf: &mut [u8]) -> bool;

    /// Copies `buf` to `addr` in this space, as
    /// [`copy_from_user`](Self::copy_from_user) copies the other way, and
    /// panics as it does. Returns `false`, copying nothing, unless the page
    /// is mapped writable for user code.
    fn copy_to_user(&self, addr: u64, buf: &[u8]) -> bool;

    /// Makes the CPU use this space.
    fn activate(&self);

    /// Ends the space: calls `free` with each frame of its tables, then
    /// `unmapped` with each mapping's object. If the CPU uses this space,
    /// it switches to the kernel's [`PageTables`] first.
    fn destroy(self, unmapped: &mut dyn FnMut(O), free: &mut dyn FnMut(Frame));
}

/// What user code may do with a page. No page is both writable and
/// executable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protection {
    /// Reads only.
    ReadOnly,
    /// Reads and writes.
    ReadWrite,
    /// Reads and instruction fetches.
    ReadExecute,
}

/// The kernel stacks of platform `P`.
pub type Stack<P> = <<P as Platform>::PageTables as PageTables>::Stack;

/// The kernel heap's counters, as [`Platform::heap_stats`] reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapStats {
    /// Bytes in live allocations, by the sizes their layouts ask for.
    pub allocated: usize,
    /// Bytes [`PageTables::grow_heap`] has given the heap, less what
    /// alignment cost.
    pub claimed: usize,
}

/// A step of [`Platform::start_cpus`]. A CPU's `id` is its hardware ID:
/// its APIC ID on x86_64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuStart {
    /// The firmware lists a CPU that is off but may be started, which is
    /// for hot-adding; it stays off.
    Skipped {
        /// The CPU's ID.
        id: u32,
    },
    /// The firmware lists `listed` CPUs ready to start besides the caller,
    /// and each gets up to `timeout_ms` to check in.
    Starting {
        /// How many there are.
        listed: usize,
        /// How long each may take, in milliseconds.
        timeout_ms: u32,
    },
    /// CPU number `cpu` checked in, and runs `entry`.
    Up {
        /// Its index among the running CPUs.
        cpu: usize,
        /// The ID the firmware lists.
        id: u32,
        /// The ID the CPU itself read.
        reported: u32,
        /// The index the CPU itself read from its own state.
        reported_cpu: usize,
        /// How long it took from the first signal, in microseconds.
        micros: u64,
    },
    /// The CPU didn't check in, and was stopped.
    Silent {
        /// Its ID.
        id: u32,
    },
}

/// What [`Platform::send_ipi`] asks of a CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipi {
    /// Trap with [`Trap::Wake`].
    Wake,
}

/// How fast the timer counts, as [`Platform::start_timer`] found it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerFrequency {
    /// Counts per second.
    pub hz: u64,
    /// Where the frequency came from, such as "CPUID leaf 0x15".
    pub source: &'static str,
}

/// The timer's frequency from each source, as
/// [`Platform::timer_frequencies`] finds it, or `None` from a source that
/// gave none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerFrequencies {
    /// What the CPU reports.
    pub reported: Option<u64>,
    /// What a measurement against another clock gives, and which clock.
    pub measured: Option<TimerFrequency>,
}

/// Why [`PageTables::map`] or [`AddressSpace::map`] failed. It hands back
/// the frame or object that wasn't mapped.
#[derive(Debug)]
pub struct MapError<T = Frame> {
    /// What was wrong.
    pub kind: MapErrorKind,
    /// The frame or object that wasn't mapped.
    pub given: T,
}

/// What was wrong with a [`PageTables::map`] or [`AddressSpace::map`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapErrorKind {
    /// The page isn't 4 KiB aligned.
    Unaligned,
    /// The page is outside [`PageTables::kernel_space`].
    OutsideKernelSpace,
    /// The page is outside [`AddressSpace::user_space`].
    OutsideUserSpace,
    /// The page is mapped already, or the range overlaps a mapping.
    Mapped,
    /// The range is empty, runs past the end of the object, or past the
    /// end of the address space.
    OutOfRange,
    /// `alloc` had no frame for a table.
    NoFrames,
    /// The space had no heap memory to record the mapping.
    NoMemory,
}

/// Where an address is mapped, as [`PageTables::translate`] finds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    /// The physical address it maps to.
    pub phys: u64,
    /// Whether writes are allowed.
    pub writable: bool,
    /// Whether instruction fetches are allowed.
    pub executable: bool,
    /// Whether user code may reach it.
    pub user: bool,
}

/// A serial port the kernel writes text to. Writing `\n` starts a new line
/// on the terminal at the other end.
pub trait Serial: fmt::Write + Send {}

/// The kernel's trap handler, which the architecture calls on every trap.
/// When it returns, the trapped code resumes, and a fault runs the faulting
/// instruction again; after a [`Trap::DoubleFault`] or a trap that can't
/// resume, the architecture halts instead, unless the handler called
/// [`TrapFrame::abandon`] after a double fault. The handler may switch
/// threads on a [`Trap::Timer`], which the architecture acknowledges before
/// calling it; the trapped thread resumes when a switch comes back to it.
/// After a trap [`TrapFrame::raised_by_user`], the handler may also never
/// return, ending the thread.
pub type TrapHandler<P> = fn(Trap, &mut <P as Platform>::TrapFrame);

/// Why the CPU trapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// A breakpoint instruction, as [`Cpu::breakpoint`] runs.
    Breakpoint,
    /// An access the page tables don't allow.
    PageFault {
        /// The address accessed.
        address: u64,
        /// What the access was.
        access: Access,
    },
    /// A fault while the CPU was starting the handler of another, such as
    /// a page fault on a full stack, or a trap taken with the stack pointer
    /// below the bottom of its stack.
    DoubleFault,
    /// A tick of the timer [`Platform::start_timer`] started.
    Timer,
    /// Another CPU's [`Ipi::Wake`].
    Wake,
    /// A syscall from user code. [`TrapFrame::syscall`] reads its number
    /// and arguments, and [`TrapFrame::set_syscall_result`] sets what it
    /// returns.
    Syscall,
    /// An interrupt that went away before the CPU took it, which the
    /// interrupt controller reports in its place: x86_64's spurious vector,
    /// or the GIC's interrupt ID 1023. It needs no end of interrupt.
    Spurious,
    /// An interrupt the kernel never asked for, by the architecture's
    /// number for it: a stray legacy line, or any other that isn't
    /// [`Trap::Spurious`].
    Stray {
        /// The vector number on x86_64, the interrupt ID on aarch64.
        vector: u32,
    },
    /// Any other exception, by the architecture's number for it and the
    /// error code it gave, or 0 when it gives none.
    Other {
        /// The vector number on x86_64, the exception class on aarch64.
        vector: u8,
        /// The error code on x86_64, the syndrome register on aarch64.
        error: u64,
    },
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trap::Breakpoint => write!(f, "breakpoint"),
            Trap::PageFault { address, access } => {
                write!(f, "page fault on {access} at {address:#x}")
            }
            Trap::DoubleFault => write!(f, "double fault"),
            Trap::Timer => write!(f, "timer interrupt"),
            Trap::Wake => write!(f, "wake interrupt"),
            Trap::Syscall => write!(f, "syscall"),
            Trap::Spurious => write!(f, "spurious interrupt"),
            Trap::Stray { vector } => write!(f, "stray interrupt on vector {vector:#x}"),
            Trap::Other { vector, error } => {
                write!(f, "exception {vector}, error code {error:#x}")
            }
        }
    }
}

/// What a faulting access was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// A data read.
    Read,
    /// A data write.
    Write,
    /// An instruction fetch.
    Execute,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Access::Read => "read",
            Access::Write => "write",
            Access::Execute => "execute",
        })
    }
}

/// The registers saved when the CPU trapped. Displaying it prints all of
/// them.
pub trait TrapFrame: fmt::Display {
    /// The trapped instruction's address, or for a breakpoint the address
    /// after it.
    fn ip(&self) -> u64;

    /// The stack pointer when the CPU trapped.
    fn sp(&self) -> u64;

    /// Whether user code raised the trap: a syscall, or an exception its
    /// instruction caused. Interrupts, NMIs, machine checks and double
    /// faults aren't the doing of the code they arrive in, so they are
    /// never user code's, even when they arrive in it.
    fn raised_by_user(&self) -> bool;

    /// The number and the six arguments of a [`Trap::Syscall`], in the
    /// registers ADR 0007 puts them in on x86_64 and ADR 0010 on aarch64.
    fn syscall(&self) -> (u64, [u64; 6]);

    /// Makes a [`Trap::Syscall`] return `error`, 0 for success, and
    /// `value`.
    fn set_syscall_result(&mut self, error: u64, value: u64);

    /// Calls `f` with the link address of the trapped instruction, then
    /// walks the trapped code's frame pointers as [`Cpu::backtrace`] does.
    fn backtrace(&self, f: impl FnMut(u64)) -> BacktraceEnd;

    /// Gives up the probe the running [`Cpu::catch`] runs: once the handler
    /// returns, the CPU carries on where that catch returns, even after a
    /// [`Trap::DoubleFault`]. Returns `false`, and changes nothing, when no
    /// catch is running. It can't tell a trap the probe caused from one in
    /// a handler that interrupted the probe, and gives up that handler too.
    fn abandon(&mut self) -> bool;
}
