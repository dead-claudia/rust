pub mod dlsym;
pub mod env;
pub mod foreign_items;
pub mod fs;
pub mod intrinsics;
pub mod os_str;
pub mod panic;
pub mod time;
pub mod tls;

use std::convert::TryFrom;

use log::trace;

use rustc_middle::{mir, ty};

use crate::*;

impl<'mir, 'tcx> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    fn find_mir_or_eval_fn(
        &mut self,
        instance: ty::Instance<'tcx>,
        args: &[OpTy<'tcx, Tag>],
        ret: Option<(PlaceTy<'tcx, Tag>, mir::BasicBlock)>,
        unwind: Option<mir::BasicBlock>,
    ) -> InterpResult<'tcx, Option<&'mir mir::Body<'tcx>>> {
        let this = self.eval_context_mut();
        trace!("eval_fn_call: {:#?}, {:?}", instance, ret.map(|p| *p.0));

        // There are some more lang items we want to hook that CTFE does not hook (yet).
        if this.tcx.lang_items().align_offset_fn() == Some(instance.def.def_id()) {
            this.align_offset(args[0], args[1], ret, unwind)?;
            return Ok(None);
        }

        // Try to see if we can do something about foreign items.
        if this.tcx.is_foreign_item(instance.def_id()) {
            // An external function call that does not have a MIR body. We either find MIR elsewhere
            // or emulate its effect.
            // This will be Ok(None) if we're emulating the intrinsic entirely within Miri (no need
            // to run extra MIR), and Ok(Some(body)) if we found MIR to run for the
            // foreign function
            // Any needed call to `goto_block` will be performed by `emulate_foreign_item`.
            return this.emulate_foreign_item(instance.def_id(), args, ret, unwind);
        }

        // Better error message for panics on Windows.
        let def_id = instance.def_id();
        if Some(def_id) == this.tcx.lang_items().begin_panic_fn() ||
            Some(def_id) == this.tcx.lang_items().panic_impl()
        {
            this.check_panic_supported()?;
        }

        // Otherwise, load the MIR.
        Ok(Some(&*this.load_mir(instance.def, None)?))
    }

    fn align_offset(
        &mut self,
        ptr_op: OpTy<'tcx, Tag>,
        align_op: OpTy<'tcx, Tag>,
        ret: Option<(PlaceTy<'tcx, Tag>, mir::BasicBlock)>,
        unwind: Option<mir::BasicBlock>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let (dest, ret) = ret.unwrap();

        let req_align = this
            .force_bits(this.read_scalar(align_op)?.not_undef()?, this.pointer_size())?;

        // Stop if the alignment is not a power of two.
        if !req_align.is_power_of_two() {
            return this.start_panic("align_offset: align is not a power-of-two", unwind);
        }

        let ptr_scalar = this.read_scalar(ptr_op)?.not_undef()?;

        // Default: no result.
        let mut result = this.machine_usize_max();
        if let Ok(ptr) = this.force_ptr(ptr_scalar) {
            // Only do anything if we can identify the allocation this goes to.
            let cur_align =
                this.memory.get_size_and_align(ptr.alloc_id, AllocCheck::MaybeDead)?.1.bytes();
            if u128::from(cur_align) >= req_align {
                // If the allocation alignment is at least the required alignment we use the
                // libcore implementation.
                // FIXME: is this correct in case of truncation?
                result = u64::try_from(
                    (this.force_bits(ptr_scalar, this.pointer_size())? as *const i8)
                        .align_offset(usize::try_from(req_align).unwrap())
                ).unwrap();
            }
        }

        // Return result, and jump to caller.
        this.write_scalar(Scalar::from_machine_usize(result, this), dest)?;
        this.go_to_block(ret);
        Ok(())
    }
}
