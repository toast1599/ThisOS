# 0008. Every user page belongs to a memory object, fixed in size and W^X across its mappings

- **Status:** Accepted
- **Date:** 2026-11-17

## Context

Milestone 4 gives processes shared memory, and with it the kernel's model of user memory. Before it, the kernel's loader mapped `init`'s pages as anonymous memory that only the address space owned, and nothing could share a page. The choices were these:

- Whether shared memory is a kind of object of its own beside anonymous pages, or the only kind of user memory there is.
- Whether an object takes its frames when it is made, or as pages are first touched, and whether it can grow.
- Whether the caller or the kernel chooses where a mapping goes, and whether an unmap may split a mapping.
- Whether W^X holds per mapping, which a second mapping of the same frames gets around, or per object.

## Decision

- **One kind of user memory.** Every user page belongs to a memory object, a handle-named kernel object, `init`'s segments and stack included. Address spaces never own frames; a mapping holds its object, and an object frees its frames once its last handle and mapping are gone.
- **Eager and fixed.** An object takes all its frames, zeroed, when it is made, and never changes size. Running out of frames fails `memory_create` with `Error::NoMemory`, never a later page fault.
- **Caller-chosen, whole mappings.** `map` puts a mapping at an address the caller chooses, page-aligned in user space, and fails if it overlaps another. `unmap(addr)` removes the whole mapping that starts at `addr`.
- **W^X per object.** An object mapped writable anywhere can't be mapped executable anywhere, and the reverse. A loader fills an object through a writable mapping, unmaps it, then maps it executable.

## Consequences

- A user fault on an unmapped page always ends the process; there are no demand-paged or copy-on-write pages, and a large object costs its frames up front.
- A program that wants more memory makes and maps another object. `thisos-rt`'s heap grows that way, and a growable object would need a new record.
- Programs manage their own address-space layout. Address-space randomization, if it comes, is user space's job or a new record.
- Code can't be written while any mapping of its object is executable, so a JIT compiler writes into an object, unmaps it and maps it executable, and nothing patches code in place.
- design.md's Memory objects section has the syscalls, their errors and how the kernel holds mappings.
