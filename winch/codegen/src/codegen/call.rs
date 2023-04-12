//! Function call emission.  For more details around the ABI and
//! calling convention, see [ABI].
use super::CodeGenContext;
use crate::{
    abi::{align_to, calculate_frame_adjustment, ABIArg, ABIResult, ABISig, ABI},
    masm::{CalleeKind, MacroAssembler, OperandSize},
    reg::Reg,
    stack::Val,
};

/// All the information needed to emit a function call.
pub(crate) struct FnCall<'a> {
    /// The total stack space in bytes used by the function call.
    /// This amount includes the sum of:
    ///
    /// 1. The amount of stack space that needs to be explicitly
    ///    allocated at the callsite for callee arguments that
    ///    go in the stack, plus any alignment.
    /// 2. The amount of stack space created by saving any live
    ///    registers at the callsite.
    /// 3. The amount of space used by any memory entries in the value
    ///    stack present at the callsite, that will be used as
    ///    arguments for the function call. Any memory values in the
    ///    value stack that are needed as part of the function
    ///    arguments, will be consumed by the function call (either by
    ///    assigning those values to a register or by storing those
    ///    values to a memory location if the callee argument is on
    ///    the stack), so we track that stack space to reclaim it once
    ///    the function call has ended. This could also be done in
    ///    `assign_args` everytime a memory entry needs to be assigned
    ///    to a particular location, but doing so, will incur in more
    ///    instructions (e.g. a pop per argument that needs to be
    ///    assigned); it's more efficient to track the space needed by
    ///    those memory values and reclaim it at once.
    ///
    ///   The machine stack state that this amount is capturing, is the following:
    /// ┌──────────────────────────────────────────────────┐
    /// │                                                  │
    /// │                                                  │
    /// │  Stack space created by any previous spills      │
    /// │  from the value stack; and which memory values   │
    /// │  are used as function arguments.                 │
    /// │                                                  │
    /// ├──────────────────────────────────────────────────┤ ---> The Wasm value stack at this point in time would look like:
    /// │                                                  │      [ Reg | Reg | Mem(offset) | Mem(offset) ]
    /// │                                                  │
    /// │   Stack space created by saving                  │
    /// │   any live registers at the callsite.            │
    /// │                                                  │
    /// │                                                  │
    /// ├─────────────────────────────────────────────────┬┤ ---> The Wasm value stack at this point in time would look like:
    /// │                                                  │      [ Mem(offset) | Mem(offset) | Mem(offset) | Mem(offset) ]
    /// │                                                  │      Assuming that the callee takes 4 arguments, we calculate
    /// │                                                  │      2 spilled registers + 2 memory values; all of which will be used
    /// │   Stack space allocated for                      │      as arguments to the call via `assign_args`, thus the memory they represent is
    /// │   the callee function arguments in the stack;    │      is considered to be consumed by the call.
    /// │   represented by `arg_stack_space`               │
    /// │                                                  │
    /// │                                                  │
    /// │                                                  │
    /// └──────────────────────────────────────────────────┘ ------> Stack pointer when emitting the call
    ///
    total_stack_space: u32,
    /// The total stack space needed for the callee arguments on the
    /// stack, including any adjustments to the function's frame and
    /// aligned to to the required ABI alignment.
    arg_stack_space: u32,
    /// The ABI-specific signature of the callee.
    abi_sig: &'a ABISig,
    /// The stack pointer offset prior to preparing and emitting the
    /// call. This is tracked to assert the position of the stack
    /// pointer after the call has finished.
    sp_offset_at_callsite: u32,
}

impl<'a> FnCall<'a> {
    /// Allocate and setup a new function call.
    ///
    /// The setup process, will first save all the live registers in
    /// the value stack, tracking down those spilled for the function
    /// arguments(see comment below for more details) it will also
    /// track all the memory entries consumed by the function
    /// call. Then, it will calculate any adjustments needed to ensure
    /// the alignment of the caller's frame.  It's important to note
    /// that the order of operations in the setup is important, as we
    /// want to calculate any adjustments to the caller's frame, after
    /// having saved any live registers, so that we can account for
    /// any pushes generated by register spilling.
    pub fn new<A: ABI, M: MacroAssembler>(
        abi: &A,
        callee_sig: &'a ABISig,
        context: &mut CodeGenContext,
        masm: &mut M,
    ) -> Self {
        let stack = &context.stack;
        let arg_stack_space = callee_sig.stack_bytes;
        let callee_params = &callee_sig.params;
        let sp_offset_at_callsite = masm.sp_offset();

        let (spilled_regs, memory_values) = match callee_params.len() {
            0 => {
                let _ = context.spill_regs_and_count_memory_in(masm, ..);
                (0, 0)
            }
            _ => {
                // Here we perform a "spill" of the register entries
                // in the Wasm value stack, we also count any memory
                // values that will be used used as part of the callee
                // arguments.  Saving the live registers is done by
                // emitting push operations for every `Reg` entry in
                // the Wasm value stack. We do this to be compliant
                // with Winch's internal ABI, in which all registers
                // are treated as caller-saved. For more details, see
                // [ABI].
                //
                // The next few lines, partition the value stack into
                // two sections:
                // +------------------+--+--- (Stack top)
                // |                  |  |
                // |                  |  | 1. The top `n` elements, which are used for
                // |                  |  |    function arguments; for which we save any
                // |                  |  |    live registers, keeping track of the amount of registers
                // +------------------+  |    saved plus the amount of memory values consumed by the function call;
                // |                  |  |    with this information we can later reclaim the space used by the function call.
                // |                  |  |
                // +------------------+--+---
                // |                  |  | 2. The rest of the items in the stack, for which
                // |                  |  |    we only save any live registers.
                // |                  |  |
                // +------------------+  |
                assert!(stack.len() >= callee_params.len());
                let partition = stack.len() - callee_params.len();
                let _ = context.spill_regs_and_count_memory_in(masm, 0..partition);
                context.spill_regs_and_count_memory_in(masm, partition..)
            }
        };

        let delta = calculate_frame_adjustment(
            masm.sp_offset(),
            abi.arg_base_offset() as u32,
            abi.call_stack_align() as u32,
        );

        let arg_stack_space = align_to(arg_stack_space + delta, abi.call_stack_align() as u32);
        Self {
            abi_sig: &callee_sig,
            arg_stack_space,
            total_stack_space: (spilled_regs * <A as ABI>::word_bytes())
                + (memory_values * <A as ABI>::word_bytes())
                + arg_stack_space,
            sp_offset_at_callsite,
        }
    }

    /// Emit the function call.
    pub fn emit<M: MacroAssembler, A: ABI>(
        &self,
        masm: &mut M,
        context: &mut CodeGenContext,
        callee: u32,
    ) {
        masm.reserve_stack(self.arg_stack_space);
        self.assign_args(context, masm, <A as ABI>::scratch_reg());
        masm.call(CalleeKind::Direct(callee));
        masm.free_stack(self.total_stack_space);
        context.drop_last(self.abi_sig.params.len());
        // The stack pointer at the end of the function call
        // cannot be less than what it was when starting the
        // function call.
        assert!(self.sp_offset_at_callsite >= masm.sp_offset());
        self.handle_result(context, masm);
    }

    fn assign_args<M: MacroAssembler>(
        &self,
        context: &mut CodeGenContext,
        masm: &mut M,
        scratch: Reg,
    ) {
        let arg_count = self.abi_sig.params.len();
        let stack = &context.stack;
        let mut stack_values = stack.peekn(arg_count);
        for arg in &self.abi_sig.params {
            let val = stack_values
                .next()
                .unwrap_or_else(|| panic!("expected stack value for function argument"));
            match &arg {
                &ABIArg::Reg { ty, reg } => {
                    context.move_val_to_reg(&val, *reg, masm, (*ty).into());
                }
                &ABIArg::Stack { ty, offset } => {
                    let addr = masm.address_at_sp(*offset);
                    let size: OperandSize = (*ty).into();
                    context.move_val_to_reg(val, scratch, masm, size);
                    masm.store(scratch.into(), addr, size);
                }
            }
        }
    }

    fn handle_result<M: MacroAssembler>(&self, context: &mut CodeGenContext, masm: &mut M) {
        let result = &self.abi_sig.result;
        if result.is_void() {
            return;
        }

        match result {
            &ABIResult::Reg { ty: _, reg } => {
                assert!(context.regalloc.gpr_available(reg));
                let result_reg = Val::reg(context.gpr(reg, masm));
                context.stack.push(result_reg);
            }
        }
    }
}