//! The TSC, the CPU's time-stamp counter, and its frequency as CPUID leaf
//! 0x15 gives it, or as measured against the PIT where the leaf doesn't.

use crate::once::Once;
use crate::pit;

/// PIT ticks to measure the TSC over: 50 ms.
const PIT_TICKS: u16 = 59_659;

/// The TSC's frequency as [`measure`] found it.
static MEASURED: Once<Option<u64>> = Once::new();

/// The TSC's count.
pub fn read() -> u64 {
    // SAFETY: Every x86_64 CPU has the TSC, and CR4.TSD restricts reading it
    // only outside ring 0. Reading it changes nothing.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The TSC's frequency: the crystal frequency in ECX times the ratio EBX /
/// EAX, or `None` where the leaf doesn't give all three.
pub fn hz() -> Option<u64> {
    if core::arch::x86_64::__cpuid(0).eax < 0x15 {
        return None;
    }
    let leaf = core::arch::x86_64::__cpuid(0x15);
    if leaf.eax == 0 || leaf.ebx == 0 || leaf.ecx == 0 {
        return None;
    }
    Some(u64::from(leaf.ecx) * u64::from(leaf.ebx) / u64::from(leaf.eax))
}

/// Measures the TSC against the PIT where CPUID gives no frequency, for
/// [`rate`]. Call it once, with interrupts off.
pub fn measure() {
    let measured = if hz().is_some() {
        None
    } else {
        let mut started = 0;
        let ended = pit::count(PIT_TICKS, || started = read());
        let elapsed = read().wrapping_sub(started);
        ended.then(|| elapsed * pit::HZ / u64::from(PIT_TICKS))
    };
    MEASURED.init(measured);
}

/// The TSC's frequency from CPUID, or from [`measure`], or `None` before
/// it runs or where neither gives one.
pub fn rate() -> Option<u64> {
    hz().or_else(|| MEASURED.get().copied().flatten())
}
