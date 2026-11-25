//! The ThisOS kernel. It is generic: it reaches the hardware only through
//! `thisos-hal`'s traits, and never names an architecture's types. The
//! kernel binary and the library's test binary are its entry points.

#![no_std]
#![cfg_attr(test, no_main)]
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![feature(allocator_ext)]
#![feature(custom_test_frameworks)]
#![test_runner(crate::testing::run)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

mod channel;
mod cpu;
mod frames;
mod heap;
mod logger;
mod memory;
mod paging;
mod panic;
mod process;
mod status;
#[cfg(test)]
mod testing;
mod thread;
mod timer;
mod trap;
#[cfg_attr(
    all(target_arch = "x86_64", not(test)),
    expect(dead_code, reason = "the warm-reset spike runs in place of init")
)]
mod user;
mod wait;
#[cfg(all(target_arch = "x86_64", not(test)))]
mod warm;

use thisos_hal::BootInfo;
#[cfg(test)]
use thisos_hal::Cpu;

pub use trap::trap;

/// The architecture the kernel is built for.
#[cfg(target_arch = "x86_64")]
pub type Arch = thisos_arch_x86_64::X86_64;

#[cfg(target_arch = "x86_64")]
pub use thisos_arch_x86_64::entry;

/// The architecture the kernel is built for.
#[cfg(target_arch = "aarch64")]
pub type Arch = thisos_arch_aarch64::Aarch64;

#[cfg(target_arch = "aarch64")]
pub use thisos_arch_aarch64::entry;

/// The kernel's lock: ranked, and limited in how long anyone spins for it
/// (`thisos-locks`).
type Lock<T> = thisos_locks::Lock<T, Arch>;

#[cfg(test)]
entry!(test_kmain, trap);

/// The commit, profile and build time, which `cargo xtask build` passes in.
const BUILD: &str = match option_env!("THISOS_BUILD") {
    Some(build) => build,
    None => "unknown build",
};

/// Starts logging to the devices in `boot`, and logs the build ID first.
/// Ends by starting the timer, with interrupts on, then the logger's
/// drainer. Every entry point calls this once, first.
fn init(boot: BootInfo<Arch>) {
    let has_framebuffer = boot.framebuffer.is_some();
    logger::init(boot.serial, boot.framebuffer);
    log::info!("ThisOS {} {BUILD}", env!("CARGO_PKG_VERSION"));
    if !has_framebuffer {
        log::warn!("no framebuffer in a format the console draws; logging to serial only");
    } else if boot.framebuffer_write_combining {
        log::info!("framebuffer is write-combining");
    } else {
        log::warn!("framebuffer isn't write-combining, so the console may be slow");
    }
    match <Arch as thisos_locks::Platform>::counter_hz() {
        Some(hz) => log::info!("lock spins panic after 1 s of a {hz} Hz counter"),
        None => log::warn!("the counter's frequency is unknown, so lock spins have no limit"),
    }
    // Before anything can take the experiment's blocks.
    #[cfg(all(target_arch = "x86_64", not(test)))]
    warm::check(boot.memory.clone());
    frames::init(boot.memory);
    paging::init();
    trap::init();
    heap::init();
    // Copying the ramdisk takes frames and the heap.
    user::init(boot.ramdisk);
    thread::init();
    timer::init();
    logger::start();
}

/// The kernel binary's main function.
pub fn kmain(boot: BootInfo<Arch>) -> ! {
    init(boot);
    #[cfg(all(target_arch = "x86_64", not(test)))]
    warm::act();
    #[cfg(not(all(target_arch = "x86_64", not(test))))]
    {
        user::start_init();
        thread::exit()
    }
}

#[cfg(test)]
fn test_kmain(boot: BootInfo<Arch>) -> ! {
    init(boot);
    test_main();
    Arch::halt()
}
