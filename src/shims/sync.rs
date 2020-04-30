use std::time::{Duration, SystemTime};

use rustc_middle::ty::{layout::TyAndLayout, TyKind, TypeAndMut};
use rustc_target::abi::{LayoutOf, Size};

use crate::stacked_borrows::Tag;

use crate::*;

fn assert_ptr_target_min_size<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    operand: OpTy<'tcx, Tag>,
    min_size: u64,
) -> InterpResult<'tcx, ()> {
    let target_ty = match operand.layout.ty.kind {
        TyKind::RawPtr(TypeAndMut { ty, mutbl: _ }) => ty,
        _ => panic!("Argument to pthread function was not a raw pointer"),
    };
    let target_layout = ecx.layout_of(target_ty)?;
    assert!(target_layout.size.bytes() >= min_size);
    Ok(())
}

fn get_at_offset<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    op: OpTy<'tcx, Tag>,
    offset: u64,
    layout: TyAndLayout<'tcx>,
    min_size: u64,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    // Ensure that the following read at an offset to the attr pointer is within bounds
    assert_ptr_target_min_size(ecx, op, min_size)?;
    let op_place = ecx.deref_operand(op)?;
    let value_place = op_place.offset(Size::from_bytes(offset), MemPlaceMeta::None, layout, ecx)?;
    ecx.read_scalar(value_place.into())
}

fn set_at_offset<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    op: OpTy<'tcx, Tag>,
    offset: u64,
    value: impl Into<ScalarMaybeUndef<Tag>>,
    layout: TyAndLayout<'tcx>,
    min_size: u64,
) -> InterpResult<'tcx, ()> {
    // Ensure that the following write at an offset to the attr pointer is within bounds
    assert_ptr_target_min_size(ecx, op, min_size)?;
    let op_place = ecx.deref_operand(op)?;
    let value_place = op_place.offset(Size::from_bytes(offset), MemPlaceMeta::None, layout, ecx)?;
    ecx.write_scalar(value.into(), value_place.into())
}

// pthread_mutexattr_t is either 4 or 8 bytes, depending on the platform.

// Our chosen memory layout for emulation (does not have to match the platform layout!):
// store an i32 in the first four bytes equal to the corresponding libc mutex kind constant
// (e.g. PTHREAD_MUTEX_NORMAL).

const PTHREAD_MUTEXATTR_T_MIN_SIZE: u64 = 4;

fn mutexattr_get_kind<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    attr_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    get_at_offset(ecx, attr_op, 0, ecx.machine.layouts.i32, PTHREAD_MUTEXATTR_T_MIN_SIZE)
}

fn mutexattr_set_kind<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    attr_op: OpTy<'tcx, Tag>,
    kind: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    set_at_offset(ecx, attr_op, 0, kind, ecx.machine.layouts.i32, PTHREAD_MUTEXATTR_T_MIN_SIZE)
}

// pthread_mutex_t is between 24 and 48 bytes, depending on the platform.

// Our chosen memory layout for the emulated mutex (does not have to match the platform layout!):
// bytes 0-3: reserved for signature on macOS
// (need to avoid this because it is set by static initializer macros)
// bytes 4-7: mutex id as u32 or 0 if id is not assigned yet.
// bytes 12-15 or 16-19 (depending on platform): mutex kind, as an i32
// (the kind has to be at its offset for compatibility with static initializer macros)

const PTHREAD_MUTEX_T_MIN_SIZE: u64 = 24;

fn mutex_get_kind<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    mutex_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    let offset = if ecx.pointer_size().bytes() == 8 { 16 } else { 12 };
    get_at_offset(ecx, mutex_op, offset, ecx.machine.layouts.i32, PTHREAD_MUTEX_T_MIN_SIZE)
}

fn mutex_set_kind<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    mutex_op: OpTy<'tcx, Tag>,
    kind: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    let offset = if ecx.pointer_size().bytes() == 8 { 16 } else { 12 };
    set_at_offset(ecx, mutex_op, offset, kind, ecx.machine.layouts.i32, PTHREAD_MUTEX_T_MIN_SIZE)
}

fn mutex_get_id<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    mutex_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    get_at_offset(ecx, mutex_op, 4, ecx.machine.layouts.u32, PTHREAD_MUTEX_T_MIN_SIZE)
}

fn mutex_set_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    mutex_op: OpTy<'tcx, Tag>,
    id: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    set_at_offset(ecx, mutex_op, 4, id, ecx.machine.layouts.u32, PTHREAD_MUTEX_T_MIN_SIZE)
}

fn mutex_get_or_create_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    mutex_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, MutexId> {
    let id = mutex_get_id(ecx, mutex_op)?.to_u32()?;
    if id == 0 {
        // 0 is a default value and also not a valid mutex id. Need to allocate
        // a new mutex.
        let id = ecx.mutex_create();
        mutex_set_id(ecx, mutex_op, id.to_u32_scalar())?;
        Ok(id)
    } else {
        Ok(id.into())
    }
}

// pthread_rwlock_t is between 32 and 56 bytes, depending on the platform.

// Our chosen memory layout for the emulated rwlock (does not have to match the platform layout!):
// bytes 0-3: reserved for signature on macOS
// (need to avoid this because it is set by static initializer macros)
// bytes 4-7: rwlock id as u32 or 0 if id is not assigned yet.

const PTHREAD_RWLOCK_T_MIN_SIZE: u64 = 32;

fn rwlock_get_id<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    rwlock_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    get_at_offset(ecx, rwlock_op, 12, ecx.machine.layouts.u32, PTHREAD_RWLOCK_T_MIN_SIZE)
}

fn rwlock_set_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    rwlock_op: OpTy<'tcx, Tag>,
    id: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    set_at_offset(ecx, rwlock_op, 12, id, ecx.machine.layouts.u32, PTHREAD_RWLOCK_T_MIN_SIZE)
}

fn rwlock_get_or_create_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    rwlock_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, RwLockId> {
    let id = rwlock_get_id(ecx, rwlock_op)?.to_u32()?;
    if id == 0 {
        // 0 is a default value and also not a valid rwlock id. Need to allocate
        // a new read-write lock.
        let id = ecx.rwlock_create();
        rwlock_set_id(ecx, rwlock_op, id.to_u32_scalar())?;
        Ok(id)
    } else {
        Ok(id.into())
    }
}

// pthread_condattr_t

// Our chosen memory layout for emulation (does not have to match the platform layout!):
// store an i32 in the first four bytes equal to the corresponding libc clock id constant
// (e.g. CLOCK_REALTIME).

const PTHREAD_CONDATTR_T_MIN_SIZE: u64 = 4;

fn condattr_get_clock_id<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    attr_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    get_at_offset(ecx, attr_op, 0, ecx.machine.layouts.i32, PTHREAD_CONDATTR_T_MIN_SIZE)
}

fn condattr_set_clock_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    attr_op: OpTy<'tcx, Tag>,
    clock_id: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    set_at_offset(ecx, attr_op, 0, clock_id, ecx.machine.layouts.i32, PTHREAD_CONDATTR_T_MIN_SIZE)
}

// pthread_cond_t

// Our chosen memory layout for the emulated conditional variable (does not have
// to match the platform layout!):

// bytes 4-7: the conditional variable id as u32 or 0 if id is not assigned yet.
// bytes 8-11: the clock id constant as i32

const PTHREAD_COND_T_MIN_SIZE: u64 = 12;

fn cond_get_id<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    cond_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    get_at_offset(ecx, cond_op, 4, ecx.machine.layouts.u32, PTHREAD_COND_T_MIN_SIZE)
}

fn cond_set_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    cond_op: OpTy<'tcx, Tag>,
    id: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    set_at_offset(ecx, cond_op, 4, id, ecx.machine.layouts.u32, PTHREAD_COND_T_MIN_SIZE)
}

fn cond_get_or_create_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    cond_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, CondvarId> {
    let id = cond_get_id(ecx, cond_op)?.to_u32()?;
    if id == 0 {
        // 0 is a default value and also not a valid conditional variable id.
        // Need to allocate a new id.
        let id = ecx.condvar_create();
        cond_set_id(ecx, cond_op, id.to_u32_scalar())?;
        Ok(id)
    } else {
        Ok(id.into())
    }
}

fn cond_get_clock_id<'mir, 'tcx: 'mir>(
    ecx: &MiriEvalContext<'mir, 'tcx>,
    cond_op: OpTy<'tcx, Tag>,
) -> InterpResult<'tcx, ScalarMaybeUndef<Tag>> {
    get_at_offset(ecx, cond_op, 8, ecx.machine.layouts.i32, PTHREAD_COND_T_MIN_SIZE)
}

fn cond_set_clock_id<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    cond_op: OpTy<'tcx, Tag>,
    clock_id: impl Into<ScalarMaybeUndef<Tag>>,
) -> InterpResult<'tcx, ()> {
    set_at_offset(ecx, cond_op, 8, clock_id, ecx.machine.layouts.i32, PTHREAD_COND_T_MIN_SIZE)
}

/// Try to reacquire the mutex associated with the condition variable after we were signaled.
fn reacquire_cond_mutex<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    thread: ThreadId,
    mutex: MutexId,
) -> InterpResult<'tcx> {
    if ecx.mutex_is_locked(mutex) {
        ecx.mutex_enqueue(mutex, thread);
    } else {
        ecx.mutex_lock(mutex, thread);
        ecx.unblock_thread(thread)?;
    }
    Ok(())
}

/// Release the mutex associated with the condition variable because we are
/// entering the waiting state.
fn release_cond_mutex<'mir, 'tcx: 'mir>(
    ecx: &mut MiriEvalContext<'mir, 'tcx>,
    active_thread: ThreadId,
    mutex: MutexId,
) -> InterpResult<'tcx> {
    if let Some((owner_thread, current_locked_count)) = ecx.mutex_unlock(mutex) {
        if current_locked_count != 0 {
            throw_unsup_format!("awaiting on multiple times acquired lock is not supported");
        }
        if owner_thread != active_thread {
            throw_ub_format!("awaiting on a mutex owned by a different thread");
        }
        if let Some(thread) = ecx.mutex_dequeue(mutex) {
            // We have at least one thread waiting on this mutex. Transfer
            // ownership to it.
            ecx.mutex_lock(mutex, thread);
            ecx.unblock_thread(thread)?;
        }
    } else {
        throw_ub_format!("awaiting on unlocked mutex");
    }
    ecx.block_thread(active_thread)?;
    Ok(())
}

impl<'mir, 'tcx> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    fn pthread_mutexattr_init(&mut self, attr_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let default_kind = this.eval_libc("PTHREAD_MUTEX_DEFAULT")?;
        mutexattr_set_kind(this, attr_op, default_kind)?;

        Ok(0)
    }

    fn pthread_mutexattr_settype(
        &mut self,
        attr_op: OpTy<'tcx, Tag>,
        kind_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let kind = this.read_scalar(kind_op)?.not_undef()?;
        if kind == this.eval_libc("PTHREAD_MUTEX_NORMAL")?
            || kind == this.eval_libc("PTHREAD_MUTEX_ERRORCHECK")?
            || kind == this.eval_libc("PTHREAD_MUTEX_RECURSIVE")?
        {
            mutexattr_set_kind(this, attr_op, kind)?;
        } else {
            let einval = this.eval_libc_i32("EINVAL")?;
            return Ok(einval);
        }

        Ok(0)
    }

    fn pthread_mutexattr_destroy(&mut self, attr_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        mutexattr_set_kind(this, attr_op, ScalarMaybeUndef::Undef)?;

        Ok(0)
    }

    fn pthread_mutex_init(
        &mut self,
        mutex_op: OpTy<'tcx, Tag>,
        attr_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let attr = this.read_scalar(attr_op)?.not_undef()?;
        let kind = if this.is_null(attr)? {
            this.eval_libc("PTHREAD_MUTEX_DEFAULT")?
        } else {
            mutexattr_get_kind(this, attr_op)?.not_undef()?
        };

        let _ = mutex_get_or_create_id(this, mutex_op)?;
        mutex_set_kind(this, mutex_op, kind)?;

        Ok(0)
    }

    fn pthread_mutex_lock(&mut self, mutex_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let kind = mutex_get_kind(this, mutex_op)?.not_undef()?;
        let id = mutex_get_or_create_id(this, mutex_op)?;
        let active_thread = this.get_active_thread()?;

        if this.mutex_is_locked(id) {
            let owner_thread = this.mutex_get_owner(id);
            if owner_thread != active_thread {
                // Block the active thread.
                this.block_thread(active_thread)?;
                this.mutex_enqueue(id, active_thread);
                Ok(0)
            } else {
                // Trying to acquire the same mutex again.
                if kind == this.eval_libc("PTHREAD_MUTEX_NORMAL")? {
                    throw_machine_stop!(TerminationInfo::Deadlock);
                } else if kind == this.eval_libc("PTHREAD_MUTEX_ERRORCHECK")? {
                    this.eval_libc_i32("EDEADLK")
                } else if kind == this.eval_libc("PTHREAD_MUTEX_RECURSIVE")? {
                    this.mutex_lock(id, active_thread);
                    Ok(0)
                } else {
                    throw_ub_format!("called pthread_mutex_lock on an unsupported type of mutex");
                }
            }
        } else {
            // The mutex is unlocked. Let's lock it.
            this.mutex_lock(id, active_thread);
            Ok(0)
        }
    }

    fn pthread_mutex_trylock(&mut self, mutex_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let kind = mutex_get_kind(this, mutex_op)?.not_undef()?;
        let id = mutex_get_or_create_id(this, mutex_op)?;
        let active_thread = this.get_active_thread()?;

        if this.mutex_is_locked(id) {
            let owner_thread = this.mutex_get_owner(id);
            if owner_thread != active_thread {
                this.eval_libc_i32("EBUSY")
            } else {
                if kind == this.eval_libc("PTHREAD_MUTEX_NORMAL")?
                    || kind == this.eval_libc("PTHREAD_MUTEX_ERRORCHECK")?
                {
                    this.eval_libc_i32("EBUSY")
                } else if kind == this.eval_libc("PTHREAD_MUTEX_RECURSIVE")? {
                    this.mutex_lock(id, active_thread);
                    Ok(0)
                } else {
                    throw_ub_format!(
                        "called pthread_mutex_trylock on an unsupported type of mutex"
                    );
                }
            }
        } else {
            // The mutex is unlocked. Let's lock it.
            this.mutex_lock(id, active_thread);
            Ok(0)
        }
    }

    fn pthread_mutex_unlock(&mut self, mutex_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let kind = mutex_get_kind(this, mutex_op)?.not_undef()?;
        let id = mutex_get_or_create_id(this, mutex_op)?;

        if let Some((owner_thread, current_locked_count)) = this.mutex_unlock(id) {
            if owner_thread != this.get_active_thread()? {
                throw_ub_format!("called pthread_mutex_unlock on a mutex owned by another thread");
            }
            if current_locked_count == 0 {
                // The mutex is unlocked.
                if let Some(thread) = this.mutex_dequeue(id) {
                    // We have at least one thread waiting on this mutex. Transfer
                    // ownership to it.
                    this.mutex_lock(id, thread);
                    this.unblock_thread(thread)?;
                }
            }
            Ok(0)
        } else {
            if kind == this.eval_libc("PTHREAD_MUTEX_NORMAL")? {
                throw_ub_format!("unlocked a PTHREAD_MUTEX_NORMAL mutex that was not locked");
            } else if kind == this.eval_libc("PTHREAD_MUTEX_ERRORCHECK")? {
                this.eval_libc_i32("EPERM")
            } else if kind == this.eval_libc("PTHREAD_MUTEX_RECURSIVE")? {
                this.eval_libc_i32("EPERM")
            } else {
                throw_ub_format!("called pthread_mutex_unlock on an unsupported type of mutex");
            }
        }
    }

    fn pthread_mutex_destroy(&mut self, mutex_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = mutex_get_or_create_id(this, mutex_op)?;

        if this.mutex_is_locked(id) {
            throw_ub_format!("destroyed a locked mutex");
        }

        mutex_set_kind(this, mutex_op, ScalarMaybeUndef::Undef)?;
        mutex_set_id(this, mutex_op, ScalarMaybeUndef::Undef)?;

        Ok(0)
    }

    fn pthread_rwlock_rdlock(&mut self, rwlock_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = rwlock_get_or_create_id(this, rwlock_op)?;
        let active_thread = this.get_active_thread()?;

        if this.rwlock_is_write_locked(id) {
            this.rwlock_enqueue_reader(id, active_thread);
            this.block_thread(active_thread)?;
            Ok(0)
        } else {
            this.rwlock_reader_add(id, active_thread);
            Ok(0)
        }
    }

    fn pthread_rwlock_tryrdlock(&mut self, rwlock_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = rwlock_get_or_create_id(this, rwlock_op)?;
        let active_thread = this.get_active_thread()?;

        if this.rwlock_is_write_locked(id) {
            this.eval_libc_i32("EBUSY")
        } else {
            this.rwlock_reader_add(id, active_thread);
            Ok(0)
        }
    }

    fn pthread_rwlock_wrlock(&mut self, rwlock_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = rwlock_get_or_create_id(this, rwlock_op)?;
        let active_thread = this.get_active_thread()?;

        if this.rwlock_is_locked(id) {
            this.block_thread(active_thread)?;
            this.rwlock_enqueue_writer(id, active_thread);
        } else {
            this.rwlock_writer_set(id, active_thread);
        }

        Ok(0)
    }

    fn pthread_rwlock_trywrlock(&mut self, rwlock_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = rwlock_get_or_create_id(this, rwlock_op)?;
        let active_thread = this.get_active_thread()?;

        if this.rwlock_is_locked(id) {
            this.eval_libc_i32("EBUSY")
        } else {
            this.rwlock_writer_set(id, active_thread);
            Ok(0)
        }
    }

    fn pthread_rwlock_unlock(&mut self, rwlock_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = rwlock_get_or_create_id(this, rwlock_op)?;
        let active_thread = this.get_active_thread()?;

        if this.rwlock_reader_remove(id, active_thread) {
            // The thread was a reader.
            if this.rwlock_is_locked(id) {
                // No more readers owning the lock. Give it to a writer if there
                // is any.
                if let Some(writer) = this.rwlock_dequeue_writer(id) {
                    this.unblock_thread(writer)?;
                    this.rwlock_writer_set(id, writer);
                }
            }
            Ok(0)
        } else if Some(active_thread) == this.rwlock_writer_remove(id) {
            // The thread was a writer.
            //
            // We are prioritizing writers here against the readers. As a
            // result, not only readers can starve writers, but also writers can
            // starve readers.
            if let Some(writer) = this.rwlock_dequeue_writer(id) {
                // Give the lock to another writer.
                this.unblock_thread(writer)?;
                this.rwlock_writer_set(id, writer);
            } else {
                // Give the lock to all readers.
                while let Some(reader) = this.rwlock_dequeue_reader(id) {
                    this.unblock_thread(reader)?;
                    this.rwlock_reader_add(id, reader);
                }
            }
            Ok(0)
        } else {
            throw_ub_format!("unlocked an rwlock that was not locked by the active thread");
        }
    }

    fn pthread_rwlock_destroy(&mut self, rwlock_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = rwlock_get_or_create_id(this, rwlock_op)?;

        if this.rwlock_is_locked(id) {
            throw_ub_format!("destroyed a locked rwlock");
        }

        rwlock_set_id(this, rwlock_op, ScalarMaybeUndef::Undef)?;

        Ok(0)
    }

    fn pthread_condattr_init(&mut self, attr_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let default_clock_id = this.eval_libc("CLOCK_REALTIME")?;
        condattr_set_clock_id(this, attr_op, default_clock_id)?;

        Ok(0)
    }

    fn pthread_condattr_setclock(
        &mut self,
        attr_op: OpTy<'tcx, Tag>,
        clock_id_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let clock_id = this.read_scalar(clock_id_op)?.not_undef()?;
        if clock_id == this.eval_libc("CLOCK_REALTIME")?
            || clock_id == this.eval_libc("CLOCK_MONOTONIC")?
        {
            condattr_set_clock_id(this, attr_op, clock_id)?;
        } else {
            let einval = this.eval_libc_i32("EINVAL")?;
            return Ok(einval);
        }

        Ok(0)
    }

    fn pthread_condattr_getclock(
        &mut self,
        attr_op: OpTy<'tcx, Tag>,
        clk_id_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let clock_id = condattr_get_clock_id(this, attr_op)?;
        this.write_scalar(clock_id, this.deref_operand(clk_id_op)?.into())?;

        Ok(0)
    }

    fn pthread_condattr_destroy(&mut self, attr_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        condattr_set_clock_id(this, attr_op, ScalarMaybeUndef::Undef)?;

        Ok(0)
    }

    fn pthread_cond_init(
        &mut self,
        cond_op: OpTy<'tcx, Tag>,
        attr_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let attr = this.read_scalar(attr_op)?.not_undef()?;
        let clock_id = if this.is_null(attr)? {
            this.eval_libc("CLOCK_REALTIME")?
        } else {
            condattr_get_clock_id(this, attr_op)?.not_undef()?
        };

        let _ = cond_get_or_create_id(this, cond_op)?;
        cond_set_clock_id(this, cond_op, clock_id)?;

        Ok(0)
    }

    fn pthread_cond_signal(&mut self, cond_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();
        let id = cond_get_or_create_id(this, cond_op)?;
        if let Some((thread, mutex)) = this.condvar_signal(id) {
            reacquire_cond_mutex(this, thread, mutex)?;
            this.unregister_timeout_callback_if_exists(thread)?;
        }

        Ok(0)
    }

    fn pthread_cond_broadcast(&mut self, cond_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();
        let id = cond_get_or_create_id(this, cond_op)?;

        while let Some((thread, mutex)) = this.condvar_signal(id) {
            reacquire_cond_mutex(this, thread, mutex)?;
            this.unregister_timeout_callback_if_exists(thread)?;
        }

        Ok(0)
    }

    fn pthread_cond_wait(
        &mut self,
        cond_op: OpTy<'tcx, Tag>,
        mutex_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = cond_get_or_create_id(this, cond_op)?;
        let mutex_id = mutex_get_or_create_id(this, mutex_op)?;
        let active_thread = this.get_active_thread()?;

        release_cond_mutex(this, active_thread, mutex_id)?;
        this.condvar_wait(id, active_thread, mutex_id);

        Ok(0)
    }

    fn pthread_cond_timedwait(
        &mut self,
        cond_op: OpTy<'tcx, Tag>,
        mutex_op: OpTy<'tcx, Tag>,
        abstime_op: OpTy<'tcx, Tag>,
        dest: PlaceTy<'tcx, Tag>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();

        this.check_no_isolation("pthread_cond_timedwait")?;

        let id = cond_get_or_create_id(this, cond_op)?;
        let mutex_id = mutex_get_or_create_id(this, mutex_op)?;
        let active_thread = this.get_active_thread()?;

        release_cond_mutex(this, active_thread, mutex_id)?;
        this.condvar_wait(id, active_thread, mutex_id);

        // We return success for now and override it in the timeout callback.
        this.write_scalar(Scalar::from_i32(0), dest)?;

        // Extract the timeout.
        let clock_id = cond_get_clock_id(this, cond_op)?.to_i32()?;
        let duration = {
            let tp = this.deref_operand(abstime_op)?;
            let mut offset = Size::from_bytes(0);
            let layout = this.libc_ty_layout("time_t")?;
            let seconds_place = tp.offset(offset, MemPlaceMeta::None, layout, this)?;
            let seconds = this.read_scalar(seconds_place.into())?.to_u64()?;
            offset += layout.size;
            let layout = this.libc_ty_layout("c_long")?;
            let nanoseconds_place = tp.offset(offset, MemPlaceMeta::None, layout, this)?;
            let nanoseconds = this.read_scalar(nanoseconds_place.into())?.to_u64()?;
            Duration::new(seconds, nanoseconds as u32)
        };

        let timeout_time = if clock_id == this.eval_libc_i32("CLOCK_REALTIME")? {
            let time_anchor_since_epoch =
                this.machine.time_anchor_timestamp.duration_since(SystemTime::UNIX_EPOCH).unwrap();
            let duration_since_time_anchor = duration.checked_sub(time_anchor_since_epoch).unwrap();
            this.machine.time_anchor.checked_add(duration_since_time_anchor).unwrap()
        } else if clock_id == this.eval_libc_i32("CLOCK_MONOTONIC")? {
            this.machine.time_anchor.checked_add(duration).unwrap()
        } else {
            throw_ub_format!("Unsupported clock id.");
        };

        // Register the timeout callback.
        this.register_timeout_callback(
            active_thread,
            timeout_time,
            Box::new(move |ecx| {
                // Try to reacquire the mutex.
                reacquire_cond_mutex(ecx, active_thread, mutex_id)?;

                // Remove the thread from the conditional variable.
                ecx.condvar_remove_waiter(id, active_thread);

                // Set the timeout value.
                let timeout = ecx.eval_libc_i32("ETIMEDOUT")?;
                ecx.write_scalar(Scalar::from_i32(timeout), dest)?;

                Ok(())
            }),
        )?;

        Ok(())
    }

    fn pthread_cond_destroy(&mut self, cond_op: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let id = cond_get_or_create_id(this, cond_op)?;
        if this.condvar_is_awaited(id) {
            throw_ub_format!("destroyed an awaited conditional variable");
        }
        cond_set_id(this, cond_op, ScalarMaybeUndef::Undef)?;
        cond_set_clock_id(this, cond_op, ScalarMaybeUndef::Undef)?;

        Ok(0)
    }
}
