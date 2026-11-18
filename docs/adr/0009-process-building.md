# 0009. A process owns its space, is built before it starts, and starts with one handle

- **Status:** Accepted
- **Date:** 2026-11-18

## Context

ADR 0004 has the kernel load only `init`, and user space build every other process through general syscalls: create an address space, map memory objects into it, create a thread in it, and start that thread with the handles it should hold. Milestone 4 needs those syscalls, and with them answers to these questions:

- Whether address spaces and threads are kernel objects with handles of their own, or parts of a process.
- Whether a creator can change a process's memory after it starts.
- How many handles a new process starts with, and how it learns what they are.
- How a creator learns that a process ended, and how.
- How a loader without unsafe code fills a memory object with a program's bytes.

## Decision

- **A process owns its space.** `process_create()` makes a process with an empty address space and no thread. There are no space or thread handles: a process has one thread until a later record adds more.
- **Built, then started.** `map_into(process, memory, offset, len, addr, rights)` maps as `map` does, into the process's space, and works only until `process_start(process, ip, sp, handle)` makes the process's thread and starts it. After that both fail with `Error::Started`, and only the process's own threads change its space. Both need write on the process handle.
- **One start handle.** `process_start` moves one handle, which needs transfer, out of the caller's table into the new process's, and the thread gets its value as its argument. By convention it is a channel endpoint over which the creator sends everything else, the new process's own handle included. The kernel starts `init` the same way, with one message queued on such a channel carrying `init`'s process handle and a read-only handle to the ramdisk.
- **ENDED and an exit status.** A process handle has `Signals::ENDED` once the reaper has freed the process: its thread, its handles and its space. `process_end(h)` then gives the exit status, or fails with `Error::Killed` if a trap ended it.
- **Copies into memory objects.** `memory_write` and `memory_read` copy between the caller and a memory object at any byte offset. `memory_write` fails with `Error::Conflict` while the object is mapped executable anywhere, which keeps 0008's W^X.

We considered space and thread objects, as 0004's list reads, and an array of start handles. Neither has a use while a process has one thread, and both add ABI. A creator that maps into a running process would need a second error for a process that ended, and has no user yet.

## Consequences

- Starting a process takes `process_create`, one `memory_create`, `memory_write` and `map_into` per segment and for the stack, a channel and `process_start`. Milestone 4's measurement covers IPC, not this; if starting proves too slow, it gets a new record, as 0004 says.
- A program's entry gets a channel endpoint. `thisos-rt`'s `entry!` main is `fn(Handle<Channel>) -> u64`.
- Shared memory between running processes goes over channels as memory handles; nothing maps into a process from outside once it runs.
- More threads per process, or debugging another process's memory, needs a new record.
- design.md's Processes section has the syscalls, their errors and how the kernel holds a process's space.
