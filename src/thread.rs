//! Implements threads.

use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::convert::TryFrom;
use std::num::TryFromIntError;
use std::time::{Instant, SystemTime, Duration};

use log::trace;

use rustc_data_structures::fx::FxHashMap;
use rustc_hir::def_id::DefId;
use rustc_index::vec::{Idx, IndexVec};
use rustc_middle::{
    middle::codegen_fn_attrs::CodegenFnAttrFlags,
    mir,
    ty::{self, Instance},
};

use crate::sync::SynchronizationState;
use crate::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedulingAction {
    /// Execute step on the active thread.
    ExecuteStep,
    /// Execute a timeout callback.
    ExecuteTimeoutCallback,
    /// Execute destructors of the active thread.
    ExecuteDtors,
    /// Stop the program.
    Stop,
}

/// Timeout timeout_callbacks can be created by synchronization primitives to tell the
/// scheduler that they should be called once some period of time passes.
type TimeoutCallback<'mir, 'tcx> =
    Box<dyn FnOnce(&mut InterpCx<'mir, 'tcx, Evaluator<'mir, 'tcx>>) -> InterpResult<'tcx> + 'tcx>;

/// A thread identifier.
#[derive(Clone, Copy, Debug, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct ThreadId(u32);

/// The main thread. When it terminates, the whole application terminates.
const MAIN_THREAD: ThreadId = ThreadId(0);

impl ThreadId {
    pub fn to_u32(self) -> u32 {
        self.0
    }
}

impl Idx for ThreadId {
    fn new(idx: usize) -> Self {
        ThreadId(u32::try_from(idx).unwrap())
    }

    fn index(self) -> usize {
        usize::try_from(self.0).unwrap()
    }
}

impl TryFrom<u64> for ThreadId {
    type Error = TryFromIntError;
    fn try_from(id: u64) -> Result<Self, Self::Error> {
        u32::try_from(id).map(|id_u32| Self(id_u32))
    }
}

impl From<u32> for ThreadId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl ThreadId {
    pub fn to_u32_scalar<'tcx>(&self) -> Scalar<Tag> {
        Scalar::from_u32(u32::try_from(self.0).unwrap())
    }
}

/// The state of a thread.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ThreadState {
    /// The thread is enabled and can be executed.
    Enabled,
    /// The thread tried to join the specified thread and is blocked until that
    /// thread terminates.
    BlockedOnJoin(ThreadId),
    /// The thread is blocked on some synchronization primitive. It is the
    /// responsibility of the synchronization primitives to track threads that
    /// are blocked by them.
    BlockedOnSync,
    /// The thread has terminated its execution (we do not delete terminated
    /// threads).
    Terminated,
}

/// The join status of a thread.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ThreadJoinStatus {
    /// The thread can be joined.
    Joinable,
    /// A thread is detached if its join handle was destroyed and no other
    /// thread can join it.
    Detached,
    /// The thread was already joined by some thread and cannot be joined again.
    Joined,
}

/// A thread.
pub struct Thread<'mir, 'tcx> {
    state: ThreadState,
    /// Name of the thread.
    thread_name: Option<Vec<u8>>,
    /// The virtual call stack.
    stack: Vec<Frame<'mir, 'tcx, Tag, FrameData<'tcx>>>,
    /// The join status.
    join_status: ThreadJoinStatus,
}

impl<'mir, 'tcx> Thread<'mir, 'tcx> {
    /// Check if the thread is done executing (no more stack frames). If yes,
    /// change the state to terminated and return `true`.
    fn check_terminated(&mut self) -> bool {
        if self.state == ThreadState::Enabled {
            if self.stack.is_empty() {
                self.state = ThreadState::Terminated;
                return true;
            }
        }
        false
    }
}

impl<'mir, 'tcx> std::fmt::Debug for Thread<'mir, 'tcx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ref name) = self.thread_name {
            write!(f, "{}", String::from_utf8_lossy(name))?;
        } else {
            write!(f, "<unnamed>")?;
        }
        write!(f, "({:?}, {:?})", self.state, self.join_status)
    }
}

impl<'mir, 'tcx> Default for Thread<'mir, 'tcx> {
    fn default() -> Self {
        Self {
            state: ThreadState::Enabled,
            thread_name: None,
            stack: Vec::new(),
            join_status: ThreadJoinStatus::Joinable,
        }
    }
}

#[derive(Debug)]
pub enum Time {
    Monotonic(Instant),
    RealTime(SystemTime),
}

/// Callbacks are used to implement timeouts. For example, waiting on a
/// conditional variable with a timeout creates a callback that is called after
/// the specified time and unblocks the thread. If another thread signals on the
/// conditional variable, the signal handler deletes the callback.
struct TimeoutCallbackInfo<'mir, 'tcx> {
    /// The callback should be called no earlier than this time.
    call_time: Time,
    /// The called function.
    callback: TimeoutCallback<'mir, 'tcx>,
}

impl<'mir, 'tcx> std::fmt::Debug for TimeoutCallbackInfo<'mir, 'tcx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CallBack({:?})", self.call_time)
    }
}

/// A set of threads.
#[derive(Debug)]
pub struct ThreadManager<'mir, 'tcx> {
    /// Identifier of the currently active thread.
    active_thread: ThreadId,
    /// Threads used in the program.
    ///
    /// Note that this vector also contains terminated threads.
    threads: IndexVec<ThreadId, Thread<'mir, 'tcx>>,
    /// This field is pub(crate) because the synchronization primitives
    /// (`crate::sync`) need a way to access it.
    pub(crate) sync: SynchronizationState,
    /// A mapping from a thread-local static to an allocation id of a thread
    /// specific allocation.
    thread_local_alloc_ids: RefCell<FxHashMap<(DefId, ThreadId), AllocId>>,
    /// A flag that indicates that we should change the active thread.
    yield_active_thread: bool,
    /// Callbacks that are called once the specified time passes.
    timeout_callbacks: FxHashMap<ThreadId, TimeoutCallbackInfo<'mir, 'tcx>>,
}

impl<'mir, 'tcx> Default for ThreadManager<'mir, 'tcx> {
    fn default() -> Self {
        let mut threads = IndexVec::new();
        // Create the main thread and add it to the list of threads.
        let mut main_thread = Thread::default();
        // The main thread can *not* be joined on.
        main_thread.join_status = ThreadJoinStatus::Detached;
        threads.push(main_thread);
        Self {
            active_thread: ThreadId::new(0),
            threads: threads,
            sync: SynchronizationState::default(),
            thread_local_alloc_ids: Default::default(),
            yield_active_thread: false,
            timeout_callbacks: FxHashMap::default(),
        }
    }
}

impl<'mir, 'tcx: 'mir> ThreadManager<'mir, 'tcx> {
    /// Check if we have an allocation for the given thread local static for the
    /// active thread.
    fn get_thread_local_alloc_id(&self, def_id: DefId) -> Option<AllocId> {
        self.thread_local_alloc_ids.borrow().get(&(def_id, self.active_thread)).cloned()
    }

    /// Set the allocation id as the allocation id of the given thread local
    /// static for the active thread.
    ///
    /// Panics if a thread local is initialized twice for the same thread.
    fn set_thread_local_alloc_id(&self, def_id: DefId, new_alloc_id: AllocId) {
        self.thread_local_alloc_ids
            .borrow_mut()
            .insert((def_id, self.active_thread), new_alloc_id)
            .unwrap_none();
    }

    /// Borrow the stack of the active thread.
    fn active_thread_stack(&self) -> &[Frame<'mir, 'tcx, Tag, FrameData<'tcx>>] {
        &self.threads[self.active_thread].stack
    }

    /// Mutably borrow the stack of the active thread.
    fn active_thread_stack_mut(&mut self) -> &mut Vec<Frame<'mir, 'tcx, Tag, FrameData<'tcx>>> {
        &mut self.threads[self.active_thread].stack
    }

    /// Create a new thread and returns its id.
    fn create_thread(&mut self) -> ThreadId {
        let new_thread_id = ThreadId::new(self.threads.len());
        self.threads.push(Default::default());
        new_thread_id
    }

    /// Set an active thread and return the id of the thread that was active before.
    fn set_active_thread_id(&mut self, id: ThreadId) -> ThreadId {
        let active_thread_id = self.active_thread;
        self.active_thread = id;
        assert!(self.active_thread.index() < self.threads.len());
        active_thread_id
    }

    /// Get the id of the currently active thread.
    fn get_active_thread_id(&self) -> ThreadId {
        self.active_thread
    }

    /// Get the total number of threads that were ever spawn by this program.
    fn get_total_thread_count(&self) -> usize {
        self.threads.len()
    }

    /// Has the given thread terminated?
    fn has_terminated(&self, thread_id: ThreadId) -> bool {
        self.threads[thread_id].state == ThreadState::Terminated
    }

    /// Enable the thread for execution. The thread must be terminated.
    fn enable_thread(&mut self, thread_id: ThreadId) {
        assert!(self.has_terminated(thread_id));
        self.threads[thread_id].state = ThreadState::Enabled;
    }

    /// Get a mutable borrow of the currently active thread.
    fn active_thread_mut(&mut self) -> &mut Thread<'mir, 'tcx> {
        &mut self.threads[self.active_thread]
    }

    /// Get a shared borrow of the currently active thread.
    fn active_thread_ref(&self) -> &Thread<'mir, 'tcx> {
        &self.threads[self.active_thread]
    }

    /// Mark the thread as detached, which means that no other thread will try
    /// to join it and the thread is responsible for cleaning up.
    fn detach_thread(&mut self, id: ThreadId) -> InterpResult<'tcx> {
        if self.threads[id].join_status != ThreadJoinStatus::Joinable {
            throw_ub_format!("trying to detach thread that was already detached or joined");
        }
        self.threads[id].join_status = ThreadJoinStatus::Detached;
        Ok(())
    }

    /// Mark that the active thread tries to join the thread with `joined_thread_id`.
    fn join_thread(&mut self, joined_thread_id: ThreadId) -> InterpResult<'tcx> {
        if self.threads[joined_thread_id].join_status != ThreadJoinStatus::Joinable {
            throw_ub_format!("trying to join a detached or already joined thread");
        }
        if joined_thread_id == self.active_thread {
            throw_ub_format!("trying to join itself");
        }
        assert!(
            self.threads
                .iter()
                .all(|thread| thread.state != ThreadState::BlockedOnJoin(joined_thread_id)),
            "a joinable thread already has threads waiting for its termination"
        );
        // Mark the joined thread as being joined so that we detect if other
        // threads try to join it.
        self.threads[joined_thread_id].join_status = ThreadJoinStatus::Joined;
        if self.threads[joined_thread_id].state != ThreadState::Terminated {
            // The joined thread is still running, we need to wait for it.
            self.active_thread_mut().state = ThreadState::BlockedOnJoin(joined_thread_id);
            trace!(
                "{:?} blocked on {:?} when trying to join",
                self.active_thread,
                joined_thread_id
            );
        }
        Ok(())
    }

    /// Set the name of the active thread.
    fn set_thread_name(&mut self, new_thread_name: Vec<u8>) {
        self.active_thread_mut().thread_name = Some(new_thread_name);
    }

    /// Get the name of the active thread.
    fn get_thread_name(&self) -> &[u8] {
        if let Some(ref thread_name) = self.active_thread_ref().thread_name {
            thread_name
        } else {
            b"<unnamed>"
        }
    }

    /// Put the thread into the blocked state.
    fn block_thread(&mut self, thread: ThreadId) {
        let state = &mut self.threads[thread].state;
        assert_eq!(*state, ThreadState::Enabled);
        *state = ThreadState::BlockedOnSync;
    }

    /// Put the blocked thread into the enabled state.
    fn unblock_thread(&mut self, thread: ThreadId) {
        let state = &mut self.threads[thread].state;
        assert_eq!(*state, ThreadState::BlockedOnSync);
        *state = ThreadState::Enabled;
    }

    /// Change the active thread to some enabled thread.
    fn yield_active_thread(&mut self) {
        self.yield_active_thread = true;
    }

    /// Register the given `callback` to be called once the `call_time` passes.
    fn register_timeout_callback(
        &mut self,
        thread: ThreadId,
        call_time: Time,
        callback: TimeoutCallback<'mir, 'tcx>,
    ) {
        self.timeout_callbacks
            .insert(thread, TimeoutCallbackInfo { call_time, callback })
            .unwrap_none();
    }

    /// Unregister the callback for the `thread`.
    fn unregister_timeout_callback_if_exists(&mut self, thread: ThreadId) {
        self.timeout_callbacks.remove(&thread);
    }

    /// Get a callback that is ready to be called.
    fn get_ready_callback(&mut self) -> Option<(ThreadId, TimeoutCallback<'mir, 'tcx>)> {
        let current_monotonic_time = Instant::now();
        let current_real_time = SystemTime::now();
        // We use a for loop here to make the scheduler more deterministic.
        for thread in self.threads.indices() {
            match self.timeout_callbacks.entry(thread) {
                Entry::Occupied(entry) =>
                    match entry.get().call_time {
                        Time::Monotonic(call_time) if current_monotonic_time >= call_time => {
                            return Some((thread, entry.remove().callback));
                        }
                        Time::RealTime(call_time) if current_real_time >= call_time => {
                            return Some((thread, entry.remove().callback));
                        }
                        _ => {}
                    },
                Entry::Vacant(_) => {}
            }
        }
        None
    }

    /// Get the time how long we need to wait until the next callback will be
    /// triggered. Returns `None`, if there are no callbacks registered.
    fn get_next_callback_wait_time(&self) -> Option<Duration> {
        let iter = self.timeout_callbacks.values();
        if let Some(callback) = iter.next() {
            let duration = callback.get_wait_time();
            Some(duration)
        } else {
            None
        }
    }

    /// Decide which action to take next and on which thread.
    ///
    /// The currently implemented scheduling policy is the one that is commonly
    /// used in stateless model checkers such as Loom: run the active thread as
    /// long as we can and switch only when we have to (the active thread was
    /// blocked, terminated, or has explicitly asked to be preempted).
    fn schedule(&mut self) -> InterpResult<'tcx, SchedulingAction> {
        // Check whether the thread has **just** terminated (`check_terminated`
        // checks whether the thread has popped all its stack and if yes, sets
        // the thread state to terminated).
        if self.threads[self.active_thread].check_terminated() {
            // Check if we need to unblock any threads.
            for (i, thread) in self.threads.iter_enumerated_mut() {
                if thread.state == ThreadState::BlockedOnJoin(self.active_thread) {
                    trace!("unblocking {:?} because {:?} terminated", i, self.active_thread);
                    thread.state = ThreadState::Enabled;
                }
            }
            return Ok(SchedulingAction::ExecuteDtors);
        }
        if self.threads[MAIN_THREAD].state == ThreadState::Terminated {
            // The main thread terminated; stop the program.
            if self.threads.iter().any(|thread| thread.state != ThreadState::Terminated) {
                // FIXME: This check should be either configurable or just emit
                // a warning. For example, it seems normal for a program to
                // terminate without waiting for its detached threads to
                // terminate. However, this case is not trivial to support
                // because we also probably do not want to consider the memory
                // owned by these threads as leaked.
                throw_unsup_format!("the main thread terminated without waiting for other threads");
            }
            return Ok(SchedulingAction::Stop);
        }
        if self.threads[self.active_thread].state == ThreadState::Enabled
            && !self.yield_active_thread
        {
            // The currently active thread is still enabled, just continue with it.
            return Ok(SchedulingAction::ExecuteStep);
        }
        // We need to pick a new thread for execution.
        for (id, thread) in self.threads.iter_enumerated() {
            if thread.state == ThreadState::Enabled {
                if !self.yield_active_thread || id != self.active_thread {
                    self.active_thread = id;
                    break;
                }
            }
        }
        self.yield_active_thread = false;
        if self.threads[self.active_thread].state == ThreadState::Enabled {
            return Ok(SchedulingAction::ExecuteStep);
        }
        // We have not found a thread to execute.
        if self.threads.iter().all(|thread| thread.state == ThreadState::Terminated) {
            unreachable!();
        } else {
            for timeout_callback in self.timeout_callbacks.values() {

                match self.timeout_callbacks.entry(thread) {
                    Entry::Occupied(entry) =>
                        match entry.get().call_time {
                            Time::Monotonic(call_time) if current_monotonic_time >= call_time => {
                                return Some((thread, entry.remove().callback));
                            }
                            Time::RealTime(call_time) if current_real_time >= call_time => {
                                return Some((thread, entry.remove().callback));
                            }
                            _ => {}
                        },
                    Entry::Vacant(_) => {}
                }
            }
        }
        
        if let Some(next_call_time) =
            self.timeout_callbacks.values().min_by_key(|info| info.call_time)
        {
            // All threads are currently blocked, but we have unexecuted
            // timeout_callbacks, which may unblock some of the threads. Hence,
            // sleep until the first callback.
            if let Some(sleep_time) =
                next_call_time.call_time.checked_duration_since(Instant::now())
            {
                std::thread::sleep(sleep_time);
            }
            Ok(SchedulingAction::ExecuteTimeoutCallback)
        } else {
            throw_machine_stop!(TerminationInfo::Deadlock);
        }
    }
}

// Public interface to thread management.
impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    /// A workaround for thread-local statics until
    /// https://github.com/rust-lang/rust/issues/70685 is fixed: change the
    /// thread-local allocation id with a freshly generated allocation id for
    /// the currently active thread.
    fn remap_thread_local_alloc_ids(
        &self,
        val: &mut mir::interpret::ConstValue<'tcx>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_ref();
        match *val {
            mir::interpret::ConstValue::Scalar(Scalar::Ptr(ref mut ptr)) => {
                let alloc_id = ptr.alloc_id;
                let alloc = this.tcx.alloc_map.lock().get(alloc_id);
                let tcx = this.tcx;
                let is_thread_local = |def_id| {
                    tcx.codegen_fn_attrs(def_id).flags.contains(CodegenFnAttrFlags::THREAD_LOCAL)
                };
                match alloc {
                    Some(GlobalAlloc::Static(def_id)) if is_thread_local(def_id) => {
                        ptr.alloc_id = this.get_or_create_thread_local_alloc_id(def_id)?;
                    }
                    _ => {}
                }
            }
            _ => {
                // FIXME: Handling only `Scalar` seems to work for now, but at
                // least in principle thread-locals could be in any constant, so
                // we should also consider other cases. However, once
                // https://github.com/rust-lang/rust/issues/70685 gets fixed,
                // this code will have to be rewritten anyway.
            }
        }
        Ok(())
    }

    /// Get a thread-specific allocation id for the given thread-local static.
    /// If needed, allocate a new one.
    ///
    /// FIXME: This method should be replaced as soon as
    /// https://github.com/rust-lang/rust/issues/70685 gets fixed.
    fn get_or_create_thread_local_alloc_id(&self, def_id: DefId) -> InterpResult<'tcx, AllocId> {
        let this = self.eval_context_ref();
        let tcx = this.tcx;
        if let Some(new_alloc_id) = this.machine.threads.get_thread_local_alloc_id(def_id) {
            // We already have a thread-specific allocation id for this
            // thread-local static.
            Ok(new_alloc_id)
        } else {
            // We need to allocate a thread-specific allocation id for this
            // thread-local static.
            //
            // At first, we invoke the `const_eval_raw` query and extract the
            // allocation from it. Unfortunately, we have to duplicate the code
            // from `Memory::get_global_alloc` that does this.
            //
            // Then we store the retrieved allocation back into the `alloc_map`
            // to get a fresh allocation id, which we can use as a
            // thread-specific allocation id for the thread-local static.
            if tcx.is_foreign_item(def_id) {
                throw_unsup_format!("foreign thread-local statics are not supported");
            }
            // Invoke the `const_eval_raw` query.
            let instance = Instance::mono(tcx.tcx, def_id);
            let gid = GlobalId { instance, promoted: None };
            let raw_const =
                tcx.const_eval_raw(ty::ParamEnv::reveal_all().and(gid)).map_err(|err| {
                    // no need to report anything, the const_eval call takes care of that
                    // for statics
                    assert!(tcx.is_static(def_id));
                    err
                })?;
            let id = raw_const.alloc_id;
            // Extract the allocation from the query result.
            let mut alloc_map = tcx.alloc_map.lock();
            let allocation = alloc_map.unwrap_memory(id);
            // Create a new allocation id for the same allocation in this hacky
            // way. Internally, `alloc_map` deduplicates allocations, but this
            // is fine because Miri will make a copy before a first mutable
            // access.
            let new_alloc_id = alloc_map.create_memory_alloc(allocation);
            this.machine.threads.set_thread_local_alloc_id(def_id, new_alloc_id);
            Ok(new_alloc_id)
        }
    }

    #[inline]
    fn create_thread(&mut self) -> InterpResult<'tcx, ThreadId> {
        let this = self.eval_context_mut();
        Ok(this.machine.threads.create_thread())
    }

    #[inline]
    fn detach_thread(&mut self, thread_id: ThreadId) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.machine.threads.detach_thread(thread_id)
    }

    #[inline]
    fn join_thread(&mut self, joined_thread_id: ThreadId) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.machine.threads.join_thread(joined_thread_id)
    }

    #[inline]
    fn set_active_thread(&mut self, thread_id: ThreadId) -> InterpResult<'tcx, ThreadId> {
        let this = self.eval_context_mut();
        Ok(this.machine.threads.set_active_thread_id(thread_id))
    }

    #[inline]
    fn get_active_thread(&self) -> InterpResult<'tcx, ThreadId> {
        let this = self.eval_context_ref();
        Ok(this.machine.threads.get_active_thread_id())
    }

    #[inline]
    fn get_total_thread_count(&self) -> InterpResult<'tcx, usize> {
        let this = self.eval_context_ref();
        Ok(this.machine.threads.get_total_thread_count())
    }

    #[inline]
    fn has_terminated(&self, thread_id: ThreadId) -> InterpResult<'tcx, bool> {
        let this = self.eval_context_ref();
        Ok(this.machine.threads.has_terminated(thread_id))
    }

    #[inline]
    fn enable_thread(&mut self, thread_id: ThreadId) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.machine.threads.enable_thread(thread_id);
        Ok(())
    }

    #[inline]
    fn active_thread_stack(&self) -> &[Frame<'mir, 'tcx, Tag, FrameData<'tcx>>] {
        let this = self.eval_context_ref();
        this.machine.threads.active_thread_stack()
    }

    #[inline]
    fn active_thread_stack_mut(&mut self) -> &mut Vec<Frame<'mir, 'tcx, Tag, FrameData<'tcx>>> {
        let this = self.eval_context_mut();
        this.machine.threads.active_thread_stack_mut()
    }

    #[inline]
    fn set_active_thread_name(&mut self, new_thread_name: Vec<u8>) -> InterpResult<'tcx, ()> {
        let this = self.eval_context_mut();
        Ok(this.machine.threads.set_thread_name(new_thread_name))
    }

    #[inline]
    fn get_active_thread_name<'c>(&'c self) -> InterpResult<'tcx, &'c [u8]>
    where
        'mir: 'c,
    {
        let this = self.eval_context_ref();
        Ok(this.machine.threads.get_thread_name())
    }

    #[inline]
    fn block_thread(&mut self, thread: ThreadId) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        Ok(this.machine.threads.block_thread(thread))
    }

    #[inline]
    fn unblock_thread(&mut self, thread: ThreadId) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        Ok(this.machine.threads.unblock_thread(thread))
    }

    #[inline]
    fn yield_active_thread(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.machine.threads.yield_active_thread();
        Ok(())
    }

    #[inline]
    fn register_timeout_callback(
        &mut self,
        thread: ThreadId,
        call_time: Instant,
        clock: Clock,
        callback: TimeoutCallback<'mir, 'tcx>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.machine.threads.register_timeout_callback(thread, call_time, clock, callback);
        Ok(())
    }

    #[inline]
    fn unregister_timeout_callback_if_exists(&mut self, thread: ThreadId) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        this.machine.threads.unregister_timeout_callback_if_exists(thread);
        Ok(())
    }

    /// Execute a timeout callback on the callback's thread.
    #[inline]
    fn run_timeout_callback(&mut self) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let (thread, callback) = this.machine.threads.get_ready_callback().expect("no callback found");
        let old_thread = this.set_active_thread(thread)?;
        callback(this)?;
        this.set_active_thread(old_thread)?;
        Ok(())
    }

    /// Decide which action to take next and on which thread.
    #[inline]
    fn schedule(&mut self) -> InterpResult<'tcx, SchedulingAction> {
        let this = self.eval_context_mut();
        this.machine.threads.schedule()
    }
}
