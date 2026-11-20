//! What ThisOS's kernel and user code agree on: syscall numbers, errors,
//! rights and signals. How they pass through the registers is ADR 0007's.

#![no_std]
#![forbid(unsafe_code)]

use core::ops::BitOr;

/// The most bytes one [`Syscall::Write`] takes.
pub const MAX_WRITE: usize = 1024;

/// The most bytes a channel message holds.
pub const MAX_MESSAGE_BYTES: usize = 64 << 10;

/// The most handles a channel message holds.
pub const MAX_MESSAGE_HANDLES: usize = 64;

/// The most messages a channel queues toward one endpoint.
pub const MAX_QUEUED: usize = 64;

/// The most items one [`Syscall::Wait`] takes.
pub const MAX_WAIT_ITEMS: usize = 64;

/// The bytes of one [`Syscall::Wait`] item: three little-endian `u32`s,
/// the handle, the wanted [`Signals`] and the observed ones.
pub const WAIT_ITEM_SIZE: usize = 12;

/// The end of the addresses user code may map, where a loader puts a new
/// program's stack: below the last page of x86_64's lower half, since
/// SYSRET can't return past it (ADR 0007). aarch64's user space ends there
/// too.
pub const USER_SPACE_END: u64 = 0x7fff_ffff_f000;

/// A syscall, by its number in RAX on x86_64 and x8 on aarch64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syscall {
    /// Ends the calling process, with its first argument as the process's
    /// exit status. It never returns.
    Exit,
    /// Logs the UTF-8 text at its first argument, as long as its second,
    /// at most [`MAX_WRITE`] bytes, in the kernel's log, one record per
    /// line. A newline at its end is dropped, and other control characters
    /// show as escapes such as `\u{1b}`. A zero-length write reads nothing,
    /// checks no address and logs one empty line. It waits while the log
    /// has no room for all its lines. Its value is the length. It fails,
    /// writing nothing, with [`Error::TooLong`], [`Error::BadAddress`] or
    /// [`Error::NotUtf8`], or with [`Error::NoMemory`] when the kernel has
    /// no room to note that it waits.
    Write,
    /// Closes the handle in its first argument. Fails with
    /// [`Error::BadHandle`].
    Close,
    /// Makes a new handle to the object of the handle in its first
    /// argument, with the [`Rights`] in its second, which must be ones the
    /// handle has. Its value is the new handle. Fails with
    /// [`Error::BadHandle`], with [`Error::AccessDenied`] if the handle
    /// lacks [`Rights::DUPLICATE`], with [`Error::BadRights`],
    /// [`Error::NoHandles`] or [`Error::NoMemory`].
    Duplicate,
    /// Leaves the handle in its first argument with only the [`Rights`] in
    /// its second, which must be ones it has. Fails with
    /// [`Error::BadHandle`] or [`Error::BadRights`], and then changes
    /// nothing.
    Restrict,
    /// Makes a memory object of as many zeroed bytes as its first
    /// argument, a multiple of 4096. Its value is a handle to it with
    /// [`Rights::ALL`]. Fails with [`Error::OutOfRange`] for a size that
    /// is 0 or not a multiple, with [`Error::NoMemory`], or with
    /// [`Error::NoHandles`].
    MemoryCreate,
    /// Maps the memory object of the handle in its first argument into the
    /// calling process: as many bytes as its third argument, from the
    /// object's byte offset in its second, at the address in its fourth,
    /// with the [`Rights`] in its fifth: [`Rights::READ`], with
    /// [`Rights::WRITE`] or [`Rights::EXECUTE`] or neither. The handle
    /// must have those rights. The length and offset are multiples of
    /// 4096, and so is the address. The mapping holds the object until it
    /// is unmapped, closing the handle or not. No object is mapped writable
    /// and executable at once. Fails with [`Error::BadHandle`],
    /// [`Error::WrongType`], [`Error::BadRights`],
    /// [`Error::AccessDenied`], with [`Error::BadAddress`] for an offset or
    /// address not a multiple of 4096 or a range outside user space, with
    /// [`Error::OutOfRange`] for a length that is 0 or not a multiple, or a
    /// range past the object's end, with [`Error::AddressInUse`],
    /// [`Error::Conflict`] or [`Error::NoMemory`], and then maps nothing.
    Map,
    /// Unmaps the mapping that starts at the address in its first
    /// argument, all of it. Fails with [`Error::BadAddress`] if no mapping
    /// starts there.
    Unmap,
    /// Makes a channel: two endpoints, each with [`Rights::READ`],
    /// [`Rights::WRITE`] and [`Rights::TRANSFER`], where what is sent on
    /// one is received on the other. Its value holds one endpoint's handle
    /// in its low 32 bits and the other's in its high 32. Fails with
    /// [`Error::NoMemory`] or [`Error::NoHandles`].
    ChannelCreate,
    /// Sends a message on the channel endpoint of the handle in its first
    /// argument, which needs [`Rights::WRITE`]: as many bytes as its third
    /// argument, at most [`MAX_MESSAGE_BYTES`], from the address in its
    /// second, and as many handles as its fifth, at most
    /// [`MAX_MESSAGE_HANDLES`], from the array of `u32`s at its fourth.
    /// Each handle needs [`Rights::TRANSFER`] and moves out of the
    /// caller's table, keeping its rights. Fails with
    /// [`Error::BadHandle`], also for a handle named twice,
    /// [`Error::WrongType`], [`Error::AccessDenied`], [`Error::TooLong`],
    /// [`Error::BadAddress`], with [`Error::Cycle`] if the message carries
    /// either endpoint of the channel, with [`Error::PeerClosed`], with
    /// [`Error::ShouldWait`] if [`MAX_QUEUED`] messages wait at the other
    /// endpoint already, or with [`Error::NoMemory`], and then sends
    /// nothing and moves no handle.
    Send,
    /// Receives the oldest message waiting at the channel endpoint of the
    /// handle in its first argument, which needs [`Rights::READ`]: writes
    /// its bytes at the address in its second argument, which has room for
    /// as many as its third, and its handles, as `u32`s, at the address in
    /// its fourth, which has room for as many as its fifth. Its value holds
    /// the number of bytes in its low 32 bits and the number of handles in
    /// its high 32. Fails with [`Error::BadHandle`], [`Error::WrongType`],
    /// [`Error::AccessDenied`], with [`Error::ShouldWait`] if no message
    /// waits, with [`Error::PeerClosed`] if none waits and the other
    /// endpoint is closed, with [`Error::BufferTooSmall`],
    /// [`Error::BadAddress`], [`Error::NoHandles`] or [`Error::NoMemory`],
    /// and then leaves the message waiting.
    Receive,
    /// Waits until an object of one of the items at the address in its
    /// first argument, as many as its second, at most [`MAX_WAIT_ITEMS`],
    /// has one of the item's wanted [`Signals`], or until as many
    /// nanoseconds as its third have passed, rounded up to the scheduler's
    /// ticks: 0 only checks, and `u64::MAX` waits for good. Each item is
    /// [`WAIT_ITEM_SIZE`] bytes: a handle, which needs [`Rights::READ`],
    /// the signals it waits for, and a field the call writes the object's
    /// signals into when it succeeds, for every item. Its value is the
    /// number of items with a wanted signal. A handle may be in more than
    /// one item. With no items it waits out the time. Fails with
    /// [`Error::TooLong`], with [`Error::BadAddress`] before it waits for
    /// items it can't read and write, [`Error::BadHandle`],
    /// [`Error::WrongType`] for an object that has no signals,
    /// [`Error::AccessDenied`], [`Error::BadSignals`], [`Error::NoMemory`],
    /// or with [`Error::TimedOut`] once the time has passed with no wanted
    /// signal.
    Wait,
    /// Makes a process with an empty address space and no thread. Its
    /// value is a handle to it with [`Rights::READ`], [`Rights::WRITE`],
    /// [`Rights::DUPLICATE`] and [`Rights::TRANSFER`]. Fails with
    /// [`Error::NoMemory`] or [`Error::NoHandles`].
    ProcessCreate,
    /// Maps a memory object into the space of the process of the handle in
    /// its first argument, which needs [`Rights::WRITE`], before that
    /// process starts: the rest of its arguments are [`Syscall::Map`]'s,
    /// the memory object's handle, offset, length, address and rights.
    /// Fails as map does, and with [`Error::Started`] once the process has
    /// started.
    MapInto,
    /// Starts the process of the handle in its first argument, which needs
    /// [`Rights::WRITE`], with one thread at the address in its second
    /// argument and the stack pointer in its third, both in its user
    /// space; the stack pointer may be at its end. The handle in its fourth
    /// argument, which needs [`Rights::TRANSFER`], moves into the new
    /// process's table with its rights, and the thread gets that handle in
    /// its first argument register. Fails with [`Error::BadHandle`],
    /// [`Error::WrongType`] if the first isn't a process,
    /// [`Error::AccessDenied`], [`Error::BadAddress`], [`Error::Started`]
    /// if the process has started already, [`Error::NoHandles`] or
    /// [`Error::NoMemory`], and then starts nothing and moves no handle.
    ProcessStart,
    /// The exit status of the process of the handle in its first argument,
    /// which needs [`Rights::READ`], once it has [`Signals::ENDED`]. Fails
    /// with [`Error::BadHandle`], [`Error::WrongType`],
    /// [`Error::AccessDenied`], with [`Error::ShouldWait`] if it hasn't
    /// ended, or with [`Error::Killed`] if a trap ended it.
    ProcessEnd,
    /// Copies as many bytes as its fourth argument from the memory object
    /// of the handle in its first argument, which needs [`Rights::READ`],
    /// from the byte offset in its second, to the address in its third.
    /// Offset and length may be any bytes in the object. Fails with
    /// [`Error::BadHandle`], [`Error::WrongType`], [`Error::AccessDenied`],
    /// with [`Error::OutOfRange`] for a range past the object's end, or
    /// with [`Error::BadAddress`], having written the pages before the bad
    /// one.
    MemoryRead,
    /// Copies as many bytes as its fourth argument from the address in its
    /// third into the memory object of the handle in its first argument,
    /// which needs [`Rights::WRITE`], from the byte offset in its second.
    /// Offset and length may be any bytes in the object. Fails with
    /// [`Error::BadHandle`], [`Error::WrongType`], [`Error::AccessDenied`],
    /// with [`Error::OutOfRange`] for a range past the object's end, with
    /// [`Error::Conflict`] while the object is mapped executable anywhere,
    /// so that writing can't get around W^X, or with
    /// [`Error::BadAddress`], having written the object's bytes before the
    /// bad page.
    MemoryWrite,
}

impl Syscall {
    /// The syscall's number.
    pub const fn number(self) -> u64 {
        match self {
            Syscall::Exit => 0,
            Syscall::Write => 1,
            Syscall::Close => 2,
            Syscall::Duplicate => 3,
            Syscall::Restrict => 4,
            Syscall::MemoryCreate => 5,
            Syscall::Map => 6,
            Syscall::Unmap => 7,
            Syscall::ChannelCreate => 8,
            Syscall::Send => 9,
            Syscall::Receive => 10,
            Syscall::Wait => 11,
            Syscall::ProcessCreate => 12,
            Syscall::MapInto => 13,
            Syscall::ProcessStart => 14,
            Syscall::ProcessEnd => 15,
            Syscall::MemoryRead => 16,
            Syscall::MemoryWrite => 17,
        }
    }
}

impl TryFrom<u64> for Syscall {
    type Error = Error;

    fn try_from(number: u64) -> Result<Syscall, Error> {
        match number {
            0 => Ok(Syscall::Exit),
            1 => Ok(Syscall::Write),
            2 => Ok(Syscall::Close),
            3 => Ok(Syscall::Duplicate),
            4 => Ok(Syscall::Restrict),
            5 => Ok(Syscall::MemoryCreate),
            6 => Ok(Syscall::Map),
            7 => Ok(Syscall::Unmap),
            8 => Ok(Syscall::ChannelCreate),
            9 => Ok(Syscall::Send),
            10 => Ok(Syscall::Receive),
            11 => Ok(Syscall::Wait),
            12 => Ok(Syscall::ProcessCreate),
            13 => Ok(Syscall::MapInto),
            14 => Ok(Syscall::ProcessStart),
            15 => Ok(Syscall::ProcessEnd),
            16 => Ok(Syscall::MemoryRead),
            17 => Ok(Syscall::MemoryWrite),
            _ => Err(Error::UnknownSyscall),
        }
    }
}

/// Why a syscall failed, by its code in RAX on x86_64 and x0 on aarch64. A syscall that succeeds
/// returns 0 there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No syscall has the number given.
    UnknownSyscall,
    /// A buffer isn't all in pages mapped for user code.
    BadAddress,
    /// Text isn't UTF-8.
    NotUtf8,
    /// A buffer is longer than the syscall takes, or a message has more
    /// handles than one holds.
    TooLong,
    /// A handle doesn't name an object in the calling process's table:
    /// it was never made, or was closed.
    BadHandle,
    /// The calling process's handle table is full.
    NoHandles,
    /// Rights aren't a subset of the handle's, or have bits that no right
    /// has.
    BadRights,
    /// A handle lacks a right the syscall needs.
    AccessDenied,
    /// No free memory is left for the request.
    NoMemory,
    /// An address range overlaps a mapping.
    AddressInUse,
    /// A size or length is 0 or not a multiple of 4096, or a range runs
    /// past the end of its object.
    OutOfRange,
    /// The object's other mappings forbid this one: no object is mapped
    /// writable and executable at once.
    Conflict,
    /// A handle names an object of another type than the syscall takes.
    WrongType,
    /// The call would have to wait: a channel is full, or has no message.
    ShouldWait,
    /// The other endpoint of a channel is closed.
    PeerClosed,
    /// A buffer is too small for what the syscall would write there.
    BufferTooSmall,
    /// A message carries an endpoint of the channel it is sent on.
    Cycle,
    /// A wait's time passed with no wanted signal.
    TimedOut,
    /// Signals have bits that no signal has.
    BadSignals,
    /// The process has started, and the call works only before that.
    Started,
    /// The process was ended by a trap its code raised, so it has no exit
    /// status.
    Killed,
}

impl Error {
    /// The error's code.
    pub const fn code(self) -> u64 {
        match self {
            Error::UnknownSyscall => 1,
            Error::BadAddress => 2,
            Error::NotUtf8 => 3,
            Error::TooLong => 4,
            Error::BadHandle => 5,
            Error::NoHandles => 6,
            Error::BadRights => 7,
            Error::AccessDenied => 8,
            Error::NoMemory => 9,
            Error::AddressInUse => 10,
            Error::OutOfRange => 11,
            Error::Conflict => 12,
            Error::WrongType => 13,
            Error::ShouldWait => 14,
            Error::PeerClosed => 15,
            Error::BufferTooSmall => 16,
            Error::Cycle => 17,
            Error::TimedOut => 18,
            Error::BadSignals => 19,
            Error::Started => 20,
            Error::Killed => 21,
        }
    }

    /// The error with `code`, or `None` if no error has it.
    pub const fn from_code(code: u64) -> Option<Error> {
        match code {
            1 => Some(Error::UnknownSyscall),
            2 => Some(Error::BadAddress),
            3 => Some(Error::NotUtf8),
            4 => Some(Error::TooLong),
            5 => Some(Error::BadHandle),
            6 => Some(Error::NoHandles),
            7 => Some(Error::BadRights),
            8 => Some(Error::AccessDenied),
            9 => Some(Error::NoMemory),
            10 => Some(Error::AddressInUse),
            11 => Some(Error::OutOfRange),
            12 => Some(Error::Conflict),
            13 => Some(Error::WrongType),
            14 => Some(Error::ShouldWait),
            15 => Some(Error::PeerClosed),
            16 => Some(Error::BufferTooSmall),
            17 => Some(Error::Cycle),
            18 => Some(Error::TimedOut),
            19 => Some(Error::BadSignals),
            20 => Some(Error::Started),
            21 => Some(Error::Killed),
            _ => None,
        }
    }
}

/// What a handle lets its process do with its object. Rights can be
/// removed from a handle and never added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rights(u32);

impl Rights {
    /// No rights.
    pub const NONE: Rights = Rights(0);
    /// Reading the object's state.
    pub const READ: Rights = Rights(1 << 0);
    /// Changing the object's state.
    pub const WRITE: Rights = Rights(1 << 1);
    /// Mapping a memory object executable.
    pub const EXECUTE: Rights = Rights(1 << 2);
    /// Making another handle to the object.
    pub const DUPLICATE: Rights = Rights(1 << 3);
    /// Sending the handle over a channel.
    pub const TRANSFER: Rights = Rights(1 << 4);
    /// Every right.
    pub const ALL: Rights = Rights((1 << 5) - 1);

    /// The rights with these bits, or `None` if a bit has no right.
    pub const fn from_bits(bits: u32) -> Option<Rights> {
        if bits & !Rights::ALL.0 == 0 {
            Some(Rights(bits))
        } else {
            None
        }
    }

    /// The rights' bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// These rights and `other`'s, for constants, where `|` can't run.
    #[must_use]
    pub const fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }

    /// Whether these rights include every one of `other`.
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Rights {
    type Output = Rights;

    fn bitor(self, other: Rights) -> Rights {
        self.union(other)
    }
}

/// What [`Syscall::Wait`] waits for: states of an object that can come and
/// go. Channel endpoints and processes have signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signals(u32);

impl Signals {
    /// No signals.
    pub const NONE: Signals = Signals(0);
    /// A message waits at the endpoint.
    pub const READABLE: Signals = Signals(1 << 0);
    /// The other endpoint is open and has room for a message.
    pub const WRITABLE: Signals = Signals(1 << 1);
    /// The other endpoint is closed.
    pub const PEER_CLOSED: Signals = Signals(1 << 2);
    /// The process has ended, and its handles are closed and its memory
    /// freed. It stays set.
    pub const ENDED: Signals = Signals(1 << 3);
    /// Every signal.
    pub const ALL: Signals = Signals((1 << 4) - 1);

    /// The signals with these bits, or `None` if a bit has no signal.
    pub const fn from_bits(bits: u32) -> Option<Signals> {
        if bits & !Signals::ALL.0 == 0 {
            Some(Signals(bits))
        } else {
            None
        }
    }

    /// The signals' bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// These signals and `other`'s, for constants, where `|` can't run.
    #[must_use]
    pub const fn union(self, other: Signals) -> Signals {
        Signals(self.0 | other.0)
    }

    /// Whether these signals include every one of `other`.
    pub const fn contains(self, other: Signals) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether these signals and `other` share one.
    pub const fn intersects(self, other: Signals) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for Signals {
    type Output = Signals;

    fn bitor(self, other: Signals) -> Signals {
        self.union(other)
    }
}
