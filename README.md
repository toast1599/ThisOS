# ThisOS

ThisOS is a general-purpose operating system written in Rust, built on a microkernel. The kernel manages memory, threads, interrupts, handles and IPC. Drivers, filesystems and other services are to run as isolated user-space programs that reach only what they were handed. It runs on 64-bit hardware, starting with x86_64, and programs will target Rust's standard library. It has no libc and is not Unix-like.

## Status

The design for the first milestones is settled. Milestone 1 (booting the kernel) is done: the kernel boots under UEFI in QEMU and on the test laptop, logs to serial and a framebuffer console, prints a backtrace when it panics, and runs its tests in QEMU, in CI and on the laptop. Milestone 2 (the kernel core) is done: the kernel loads its own GDT, TSS and IDT and reports any trap with its registers and a backtrace, allocates physical frames from the memory map, runs on page tables of its own with a heap, and ticks a LAPIC timer whose watchdog stops a hung test. One laptop flash runs every kernel test.

Milestone 3 (user mode) is done: kernel threads with guarded stacks, preempted round robin on every tick; user address spaces; and syscall entry through SYSCALL. The kernel loads `init` from the ramdisk as an ELF file and runs it in ring 3, where it prints through a write syscall and exits.

Milestone 4 (handles and IPC) is in progress. Done: blocking in the scheduler; a handle table per process, with rights, duplicate and restrict; memory objects that user code maps, with W^X held across every mapping of an object; channels carrying bytes and handles; a wait call on several objects; the page-table code's raw-pointer walks under Miri on the host; syscalls that build a process from user space, with a ramdisk of several programs; and a log that a kernel thread draws from a ring once the kernel is up, so that code that logs doesn't wait for the serial port with interrupts off. `init` loads `hello` and `fault` from the ramdisk, sends `hello` a line, and reports how each ended. Left: two programs talking, with a measurement of their round trip.

aarch64 runs in QEMU only, behind x86_64 ([ADR 0006](docs/adr/0006-aarch64-behind-x86_64.md)). It has milestone 2's parity and boots as far as creating the idle thread.

- [docs/design.md](docs/design.md) describes the system and its milestones.
- [docs/adr/](docs/adr/) holds the decisions that are hard to reverse.
- [CONTRIBUTING.md](CONTRIBUTING.md) says how code is written, tested and decided.

## License

ThisOS is licensed under either of the [MIT license](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in ThisOS by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
