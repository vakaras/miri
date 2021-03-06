use std::time::{Duration, SystemTime, Instant};
use std::convert::TryFrom;

use crate::stacked_borrows::Tag;
use crate::*;
use helpers::immty_from_int_checked;

/// Returns the time elapsed between the provided time and the unix epoch as a `Duration`.
pub fn system_time_to_duration<'tcx>(time: &SystemTime) -> InterpResult<'tcx, Duration> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| err_unsup_format!("times before the Unix epoch are not supported").into())
}

impl<'mir, 'tcx> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    fn clock_gettime(
        &mut self,
        clk_id_op: OpTy<'tcx, Tag>,
        tp_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        this.assert_target_os("linux", "clock_gettime");
        this.check_no_isolation("clock_gettime")?;

        let clk_id = this.read_scalar(clk_id_op)?.to_i32()?;
        let tp = this.deref_operand(tp_op)?;

        let duration = if clk_id == this.eval_libc_i32("CLOCK_REALTIME")? {
            system_time_to_duration(&SystemTime::now())?
        } else if clk_id == this.eval_libc_i32("CLOCK_MONOTONIC")? {
            // Absolute time does not matter, only relative time does, so we can just
            // use our own time anchor here.
            Instant::now().duration_since(this.machine.time_anchor)
        } else {
            let einval = this.eval_libc("EINVAL")?;
            this.set_last_error(einval)?;
            return Ok(-1);
        };

        let tv_sec = duration.as_secs();
        let tv_nsec = duration.subsec_nanos();

        let imms = [
            immty_from_int_checked(tv_sec, this.libc_ty_layout("time_t")?)?,
            immty_from_int_checked(tv_nsec, this.libc_ty_layout("c_long")?)?,
        ];

        this.write_packed_immediates(tp, &imms)?;

        Ok(0)
    }

    fn gettimeofday(
        &mut self,
        tv_op: OpTy<'tcx, Tag>,
        tz_op: OpTy<'tcx, Tag>,
    ) -> InterpResult<'tcx, i32> {
        let this = self.eval_context_mut();

        this.assert_target_os("macos", "gettimeofday");
        this.check_no_isolation("gettimeofday")?;

        // Using tz is obsolete and should always be null
        let tz = this.read_scalar(tz_op)?.not_undef()?;
        if !this.is_null(tz)? {
            let einval = this.eval_libc("EINVAL")?;
            this.set_last_error(einval)?;
            return Ok(-1);
        }

        let tv = this.deref_operand(tv_op)?;

        let duration = system_time_to_duration(&SystemTime::now())?;
        let tv_sec = duration.as_secs();
        let tv_usec = duration.subsec_micros();

        let imms = [
            immty_from_int_checked(tv_sec, this.libc_ty_layout("time_t")?)?,
            immty_from_int_checked(tv_usec, this.libc_ty_layout("suseconds_t")?)?,
        ];

        this.write_packed_immediates(tv, &imms)?;

        Ok(0)
    }

    fn mach_absolute_time(&self) -> InterpResult<'tcx, u64> {
        let this = self.eval_context_ref();

        this.assert_target_os("macos", "mach_absolute_time");
        this.check_no_isolation("mach_absolute_time")?;

        // This returns a u64, with time units determined dynamically by `mach_timebase_info`.
        // We return plain nanoseconds.
        let duration = Instant::now().duration_since(this.machine.time_anchor);
        u64::try_from(duration.as_nanos())
            .map_err(|_| err_unsup_format!("programs running longer than 2^64 nanoseconds are not supported").into())
    }
}
