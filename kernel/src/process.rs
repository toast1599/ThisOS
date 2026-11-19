//! Processes: the handle table of the objects a process can reach, its
//! address space, and how the process ended. Its threads hold a process,
//! and so do the handles that name it. A process starts once, with one
//! thread and one handle, and ending it closes its handles, which also
//! breaks the cycle that a handle to itself makes, and lets go of its
//! space.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem;

use spin::Mutex;
use thisos_abi::{Error, Rights, Signals};
use thisos_hal::{AddressSpace as _, Cpu, Trap};
use thisos_handles::Table;

use crate::channel::Endpoint;
use crate::memory::MemoryObject;
use crate::paging::Space;
use crate::{Arch, thread};

/// The rights of a handle to a new process.
pub const PROCESS_RIGHTS: Rights = Rights::READ
    .union(Rights::WRITE)
    .union(Rights::DUPLICATE)
    .union(Rights::TRANSFER);

/// A kernel object, as a handle names it.
#[derive(Clone)]
pub enum Object {
    Process(Arc<Process>),
    Memory(Arc<MemoryObject>),
    Channel(Arc<Endpoint>),
}

fn signals(life: &Life) -> Signals {
    if life.ended {
        Signals::ENDED
    } else {
        Signals::NONE
    }
}

/// How a process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Through the exit syscall, with this status.
    Exited(u64),
    /// By a trap its code raised at `ip`.
    Killed { trap: Trap, ip: u64 },
}

pub struct Process {
    /// Taken with interrupts on, and never by a trap handler, since making
    /// a handle can allocate.
    handles: Mutex<Table<Object>>,
    /// Taken only with interrupts off, since the trap handler that kills a
    /// thread sets it.
    end: Mutex<Option<End>>,
    /// Taken with interrupts on, and never by a trap handler: starting the
    /// process and mapping into it allocate. Taken before the process's
    /// and its creator's handle-table locks.
    life: Mutex<Life>,
}

/// Where a process is in its life, and who waits for its end.
struct Life {
    /// Its address space, until it ends.
    space: Option<Arc<Space>>,
    started: bool,
    /// Whether it ended and the reaper freed it, which
    /// [`Signals::ENDED`] reports.
    ended: bool,
    /// The ids of the threads waiting for it to end.
    waiters: Vec<u64>,
}

impl Process {
    /// A process with `space` and no thread. Fails with
    /// [`Error::NoMemory`].
    pub fn new(space: Arc<Space>) -> Result<Arc<Self>, Error> {
        Arc::try_new(Self {
            handles: Mutex::new(Table::new()),
            end: Mutex::new(None),
            life: Mutex::new(Life {
                space: Some(space),
                started: false,
                ended: false,
                waiters: Vec::new(),
            }),
        })
        .map_err(|_| Error::NoMemory)
    }

    /// Runs `map` on the process's space, if the process hasn't started:
    /// once it has, only its own threads change its space. Fails with
    /// [`Error::Started`] otherwise.
    pub fn map_into<R>(&self, map: impl FnOnce(&Space) -> Result<R, Error>) -> Result<R, Error> {
        let life = self.life.lock();
        if life.started {
            return Err(Error::Started);
        }
        let Some(space) = &life.space else {
            unreachable!("a process that hasn't started hasn't ended");
        };
        map(space)
    }

    /// Starts the process with one thread at `ip`, with stack pointer `sp`,
    /// and moves the handle `take` hands over into its table, whose value
    /// the thread gets as its argument. `take` runs last, once nothing else
    /// can fail, so a handle it takes from another table never has to go
    /// back. Fails with [`Error::Started`] if the process has started
    /// already, [`Error::BadAddress`] unless `ip` is in user space and `sp`
    /// in it or at its end, [`Error::NoMemory`], or with what `take` fails
    /// with.
    pub fn start(
        self: &Arc<Self>,
        ip: u64,
        sp: u64,
        take: impl FnOnce() -> Result<(Object, Rights), Error>,
    ) -> Result<(), Error> {
        let mut life = self.life.lock();
        let thread = self.new_thread(&life, ip, sp)?;
        // Only start puts handles in the table of a process that hasn't
        // started, and the life lock keeps out another start, so the room
        // stays reserved, and take's lock on its table needn't nest in this
        // one.
        self.handles
            .lock()
            .try_reserve(1)
            .map_err(|_| Error::NoMemory)?;
        let (object, rights) = take()?;
        let Ok(handle) = self.handles.lock().insert(object, rights) else {
            unreachable!("the empty table had memory reserved");
        };
        life.started = true;
        thread.start(u64::from(handle));
        Ok(())
    }

    /// Starts the process with one thread at `ip`, with stack pointer `sp`
    /// and `arg` as its argument, whatever that is. Panics if the thread
    /// can't start.
    #[cfg(test)]
    pub fn start_with(self: &Arc<Self>, ip: u64, sp: u64, arg: u64) {
        let mut life = self.life.lock();
        match self.new_thread(&life, ip, sp) {
            Ok(thread) => thread.start(arg),
            Err(e) => panic!("starting the process failed: {e:?}"),
        }
        life.started = true;
    }

    /// The thread [`start`](Self::start) starts.
    fn new_thread(
        self: &Arc<Self>,
        life: &Life,
        ip: u64,
        sp: u64,
    ) -> Result<thread::Unstarted, Error> {
        if life.started {
            return Err(Error::Started);
        }
        let Some(space) = &life.space else {
            unreachable!("a process that hasn't started hasn't ended");
        };
        let user = space.user_space();
        if !user.contains(&ip) || !(user.start..=user.end).contains(&sp) {
            return Err(Error::BadAddress);
        }
        thread::new_user(Arc::clone(self), Arc::clone(space), ip, sp)
    }

    /// The exit status of the process, once it has [`Signals::ENDED`].
    /// Fails with [`Error::ShouldWait`] before that, and with
    /// [`Error::Killed`] if a trap ended it.
    pub fn exit_status(&self) -> Result<u64, Error> {
        if !self.life.lock().ended {
            return Err(Error::ShouldWait);
        }
        match Arch::without_interrupts(|| *self.end.lock()) {
            Some(End::Exited(status)) => Ok(status),
            Some(End::Killed { .. }) => Err(Error::Killed),
            None => unreachable!("a user process ends through exit or a trap"),
        }
    }

    /// Adds `id` to the threads waiting for the process to end, and returns
    /// its signals. Fails with [`Error::NoMemory`].
    pub fn add_waiter(&self, id: u64) -> Result<Signals, Error> {
        let mut life = self.life.lock();
        life.waiters.try_reserve(1).map_err(|_| Error::NoMemory)?;
        life.waiters.push(id);
        Ok(signals(&life))
    }

    /// Takes `id` off the threads waiting for the process to end, if it is
    /// there, and returns its signals.
    pub fn remove_waiter(&self, id: u64) -> Signals {
        let mut life = self.life.lock();
        if let Some(i) = life.waiters.iter().position(|&waiter| waiter == id) {
            life.waiters.swap_remove(i);
        }
        signals(&life)
    }

    /// Lets go of the process's space and wakes the threads waiting for it
    /// to end. The reaper calls this once it has closed the handles of the
    /// process's last thread, with interrupts on.
    pub fn reaped(&self) {
        let space = self.life.lock().space.take();
        // The reaper dropped the thread's hold first, so this destroys the
        // space, before ENDED says the process's memory is free.
        drop(space);
        let waiters = {
            let mut life = self.life.lock();
            life.ended = true;
            mem::take(&mut life.waiters)
        };
        for id in waiters {
            thread::unpark(id);
        }
    }

    /// Puts a handle to this process with `rights` in its own table, and
    /// returns it.
    #[cfg(test)]
    pub fn insert_self(self: &Arc<Self>, rights: Rights) -> Result<u32, Error> {
        self.insert(Object::Process(Arc::clone(self)), rights)
    }

    /// Puts `object` in the table with `rights`, and returns its handle.
    pub fn insert(&self, object: Object, rights: Rights) -> Result<u32, Error> {
        let inserted = self.handles.lock().insert(object, rights);
        // An object the table handed back drops here, after the lock is
        // released: a memory object's drop frees its frames.
        inserted.map_err(|(e, _)| e)
    }

    /// The object `handle` names, and the handle's rights.
    pub fn get(&self, handle: u32) -> Result<(Object, Rights), Error> {
        let handles = self.handles.lock();
        let (object, rights) = handles.get(handle)?;
        Ok((object.clone(), rights))
    }

    pub fn close(&self, handle: u32) -> Result<(), Error> {
        // The object drops after the lock is released.
        let _closed = self.handles.lock().remove(handle)?;
        Ok(())
    }

    /// Closes every handle in `handles` and returns their objects and
    /// rights, if each names an object `check` accepts; see
    /// [`Table::remove_all`]. `check` runs under the table's lock.
    pub fn take(
        &self,
        handles: &[u32],
        check: impl FnMut(&Object, Rights) -> Result<(), Error>,
    ) -> Result<Vec<(Object, Rights)>, Error> {
        self.handles.lock().remove_all(handles, check)
    }

    /// Puts every one of `objects` in the table, then calls `then` with
    /// their handles, in order, all under the table's lock. If there isn't
    /// room or memory for them all, or `then` fails, it fails with that
    /// error and hands `objects` back, leaving the table as it was.
    pub fn insert_all<R>(
        &self,
        objects: Vec<(Object, Rights)>,
        then: impl FnOnce(&[u32]) -> Result<R, Error>,
    ) -> Result<R, (Error, Vec<(Object, Rights)>)> {
        let mut table = self.handles.lock();
        if table.room() < objects.len() {
            return Err((Error::NoHandles, objects));
        }
        // All reserved first, so that no insert fails and undoing
        // allocates nothing.
        let (mut handles, mut undone) = (Vec::new(), Vec::new());
        if table.try_reserve(objects.len()).is_err()
            || handles.try_reserve_exact(objects.len()).is_err()
            || undone.try_reserve_exact(objects.len()).is_err()
        {
            return Err((Error::NoMemory, objects));
        }
        for (object, rights) in objects {
            let Ok(handle) = table.insert(object, rights) else {
                unreachable!("the table had room and memory");
            };
            handles.push(handle);
        }
        then(&handles).map_err(|e| {
            for &handle in &handles {
                let Ok(entry) = table.remove(handle) else {
                    unreachable!("{handle:#x} was made under the same lock");
                };
                undone.push(entry);
            }
            (e, undone)
        })
    }

    pub fn duplicate(&self, handle: u32, rights: Rights) -> Result<u32, Error> {
        self.handles.lock().duplicate(handle, rights)
    }

    pub fn restrict(&self, handle: u32, rights: Rights) -> Result<(), Error> {
        self.handles.lock().restrict(handle, rights)
    }

    /// Records how the process ended, unless it ended already.
    pub fn set_end(&self, end: End) {
        Arch::without_interrupts(|| {
            self.end.lock().get_or_insert(end);
        });
    }

    #[cfg(test)]
    pub fn end(&self) -> Option<End> {
        Arch::without_interrupts(|| *self.end.lock())
    }

    /// Closes every handle in the table. The scheduler calls this when it
    /// frees the process's last thread, with interrupts on.
    pub fn close_handles(&self) {
        let _closed = mem::take(&mut *self.handles.lock());
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use thisos_hal::Platform;

    use super::*;
    use crate::testing::fill_heap;
    use crate::{frames, paging};

    fn memory() -> Object {
        let Ok(object) = MemoryObject::new(1) else {
            panic!("no memory for a page");
        };
        Object::Memory(Arc::new(object))
    }

    #[test_case]
    fn a_full_heap_makes_no_handle_and_frees_the_object() {
        let allocated = Arch::heap_stats().allocated;
        let before = frames::free_frames();
        let Some(space) = paging::new_space() else {
            panic!("no free frame for a space");
        };
        let Ok(process) = Process::new(Arc::new(space)) else {
            panic!("no memory for a process");
        };
        let free = frames::free_frames();
        let (one, all) = (memory(), vec![(memory(), Rights::ALL)]);
        let filler = fill_heap();
        // The table is empty, so its first handle needs memory.
        assert_eq!(process.insert(one, Rights::ALL), Err(Error::NoMemory));
        assert_eq!(frames::free_frames(), free - 1, "insert's object is gone");
        let Err((e, all)) = process.insert_all(all, |_| Ok(())) else {
            panic!("the handles had no memory");
        };
        assert_eq!(e, Error::NoMemory);
        assert_eq!(all.len(), 1);
        drop(filler);
        assert_eq!(process.insert_self(Rights::READ), Ok(1 << 16));
        process.close_handles();
        drop((all, process));
        assert_eq!(frames::free_frames(), before);
        assert_eq!(Arch::heap_stats().allocated, allocated);
    }
}
