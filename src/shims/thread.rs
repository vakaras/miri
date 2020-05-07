use std::convert::TryInto;
use std::time::{Duration, Instant};

use crate::*;
use rustc_target::abi::{LayoutOf, Size};

impl<'mir, 'tcx> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    /// Helper function that converts `timespec` argument to duration.
    fn posix_timespec_to_duration(
        &mut self,
        timespec: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, Duration> {
        let this = self.eval_context_mut();

        let tp = this.deref_operand(timespec)?;
        let mut offset = Size::from_bytes(0);
        let layout = this.libc_ty_layout("time_t")?;
        let seconds_place = tp.offset(offset, MemPlaceMeta::None, layout, this)?;
        let seconds = this.read_scalar(seconds_place.into())?;
        offset += layout.size;
        let layout = this.libc_ty_layout("c_long")?;
        let nanoseconds_place = tp.offset(offset, MemPlaceMeta::None, layout, this)?;
        let nanoseconds = this.read_scalar(nanoseconds_place.into())?;
        let (seconds, nanoseconds) = if this.pointer_size().bytes() == 8 {
            let nanoseconds = nanoseconds.to_u64()?;
            if nanoseconds > 999999999 {
                throw_ub_format!(
                    "the provided value for nanoseconds is {}, but should be less than a billion",
                    nanoseconds
                );
            }
            (seconds.to_u64()?, nanoseconds.try_into().unwrap())
        } else {
            let nanoseconds = nanoseconds.to_u32()?;
            if nanoseconds > 999999999 {
                throw_ub_format!(
                    "the provided value for nanoseconds is {}, but should be less than a billion",
                    nanoseconds
                );
            }
            (seconds.to_u32()?.into(), nanoseconds)
        };
        Ok(Duration::new(seconds, nanoseconds))
    }

    fn pthread_create(
        &mut self,
        thread: OpTy<'tcx, Tag>,
        _attr: OpTy<'tcx, Tag>,
        start_routine: OpTy<'tcx, Tag>,
        arg: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        this.tcx.sess.warn(
            "thread support is experimental. \
             For example, Miri does not detect data races yet.",
        );

        let new_thread_id = this.create_thread()?;
        // Also switch to new thread so that we can push the first stackframe.
        let old_thread_id = this.set_active_thread(new_thread_id)?;

        let thread_info_place = this.deref_operand(thread)?;
        this.write_scalar(
            Scalar::from_uint(new_thread_id.to_u32(), thread_info_place.layout.size),
            thread_info_place.into(),
        )?;

        let fn_ptr = this.read_scalar(start_routine)?.not_undef()?;
        let instance = this.memory.get_fn(fn_ptr)?.as_instance()?;

        let func_arg = this.read_immediate(arg)?;

        // Note: the returned value is currently ignored (see the FIXME in
        // pthread_join below) because the Rust standard library does not use
        // it.
        let ret_place =
            this.allocate(this.layout_of(this.tcx.types.usize)?, MiriMemoryKind::Machine.into());

        this.call_function(
            instance,
            &[*func_arg],
            Some(ret_place.into()),
            StackPopCleanup::None { cleanup: true },
        )?;

        this.set_active_thread(old_thread_id)?;

        Ok(0)
    }

    fn pthread_join(
        &mut self,
        thread: OpTy<'tcx, Tag>,
        retval: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        if !this.is_null(this.read_scalar(retval)?.not_undef()?)? {
            // FIXME: implement reading the thread function's return place.
            throw_unsup_format!("Miri supports pthread_join only with retval==NULL");
        }

        let thread_id = this.read_scalar(thread)?.to_machine_usize(this)?;
        this.join_thread(thread_id.try_into().expect("thread ID should fit in u32"))?;

        Ok(0)
    }

    fn pthread_detach(&mut self, thread: OpTy<'tcx, Tag>) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let thread_id = this.read_scalar(thread)?.to_machine_usize(this)?;
        this.detach_thread(thread_id.try_into().expect("thread ID should fit in u32"))?;

        Ok(0)
    }

    fn pthread_self(&mut self, dest: PlaceTy<'tcx, Tag>) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();

        let thread_id = this.get_active_thread()?;
        this.write_scalar(Scalar::from_uint(thread_id.to_u32(), dest.layout.size), dest)
    }

    fn prctl(
        &mut self,
        option: OpTy<'tcx, Tag>,
        arg2: OpTy<'tcx, Tag>,
        _arg3: OpTy<'tcx, Tag>,
        _arg4: OpTy<'tcx, Tag>,
        _arg5: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let option = this.read_scalar(option)?.to_i32()?;
        if option == this.eval_libc_i32("PR_SET_NAME")? {
            let address = this.read_scalar(arg2)?.not_undef()?;
            let mut name = this.memory.read_c_str(address)?.to_owned();
            // The name should be no more than 16 bytes, including the null
            // byte. Since `read_c_str` returns the string without the null
            // byte, we need to truncate to 15.
            name.truncate(15);
            this.set_active_thread_name(name)?;
        } else if option == this.eval_libc_i32("PR_GET_NAME")? {
            let address = this.read_scalar(arg2)?.not_undef()?;
            let mut name = this.get_active_thread_name()?.to_vec();
            name.push(0u8);
            assert!(name.len() <= 16);
            this.memory.write_bytes(address, name)?;
        } else {
            throw_unsup_format!("unsupported prctl option {}", option);
        }

        Ok(0)
    }

    fn sched_yield(&mut self) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        this.yield_active_thread()?;

        Ok(0)
    }

    fn nanosleep(
        &mut self,
        req: OpTy<'tcx, Tag>,
        _rem: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        let active_thread = this.get_active_thread()?;

        let duration = this.posix_timespec_to_duration(req)?;
        let timeout = Instant::now().checked_add(duration).unwrap();

        this.block_thread(active_thread)?;

        this.register_timeout_callback(
            active_thread,
            timeout,
            Box::new(move |ecx| ecx.unblock_thread(active_thread)),
        )?;

        Ok(0)
    }
}
