# 0003. IPC through kernel-buffered async channels, with shared memory for bulk data

- **Status:** Accepted, confirmed by the milestone 4 measurement in Consequences
- **Date:** 2026-11-04, accepted 2026-11-19

## Context

The main alternatives are:

- **L4/seL4-style synchronous call and reply.** Small messages pass in registers, the kernel buffers nothing, and it switches straight to the server. It is the fastest per call, but every server needs threads or extra notifications to handle concurrency.
- **Zircon-style channels.** The kernel queues messages. Services are easier to write, at the cost of a copy and kernel memory per queued message.

The kernel has a heap ([0001](0001-kernel-scope.md)), so buffering messages is cheap to implement.

## Decision

- **Channels.** A channel is a bounded, two-way pair of endpoints whose messages carry bytes and handles ([0002](0002-handles.md)). Sending a handle moves it.
- **Waiting.** A thread can wait on several objects at once.
- **Bulk data.** Large data goes through shared memory objects mapped by both sides, not through channel messages.

## Consequences

Milestone 4's `ping` and `pong` (design.md, Processes) time round trips over one channel: `ping` sends, waits for the channel to be readable, and receives; `pong` does the same in the other order. A round trip is six syscalls and two switches between address spaces. `ping` reads the TSC before the send and after the receive, and runs each case 100 times uncounted, then 1000 times counted. The null syscall is a close of handle 0, which fails at the handle lookup.

Lenovo 83DA (Core Ultra 5 125H, TSC at 2995.2 MHz from CPUID leaf 0x15), release main image `671fcda-dirty release 2026-11-19T11:37Z`, which differs from 671fcda only by this measurement's code. Min / median / max:

| Case | Cycles | ns |
|---|---|---|
| null syscall | 216 / 222 / 258 | 72 / 74 / 86 |
| round trip, 16 B | 5056 / 5098 / 18994 | 1688 / 1702 / 6341 |
| round trip, 1 KiB | 5136 / 5190 / 5314 | 1714 / 1732 / 1774 |
| round trip, 4 KiB | 5784 / 5844 / 19606 | 1931 / 1951 / 6545 |
| round trip, 64 KiB | 25684 / 25992 / 27176 | 8575 / 8677 / 9073 |
| round trip, 16 B and 1 handle | 6450 / 6516 / 20638 | 2153 / 2175 / 6890 |

A small round trip costs about 23 null syscalls. Copying is cheap next to that: 4 KiB adds 0.25 µs over 16 bytes, and 64 KiB, copied four times in a round trip, adds 7 µs. Moving one handle each way adds 0.47 µs. Maxima near 6 µs are most likely runs that a timer tick interrupted. Nothing measured a synchronous call for comparison, and no case had more than one message queued.

- The measurement confirms this record. The bounds stay as milestone 4 set them: 64 KiB and 64 handles a message, 64 messages queued toward an endpoint. The heap is a fixed 4 MiB and there are no quotas, so one channel's full queue of 64 KiB messages fills it, and other processes' syscalls fail with `Error::NoMemory` until it drains, as with any other way to fill the heap while quotas are a non-goal.
- A full channel fails the send with `Error::ShouldWait` and changes nothing; a sender that wants to block waits for `Signals::WRITABLE`.
- Interrupts reach user space as something a thread can wait on, alongside channels. Milestone 5 does that.
