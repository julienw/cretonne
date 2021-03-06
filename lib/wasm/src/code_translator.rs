//! This module contains the bulk of the interesting code performing the translation between
//! WebAssembly and Cretonne IL.
//!
//! The translation is done in one pass, opcode by opcode. Two main data structures are used during
//! code translations: the value stack and the control stack. The value stack mimics the execution
//! of the WebAssembly stack machine: each instruction result is pushed onto the stack and
//! instruction arguments are popped off the stack. Similarly, when encountering a control flow
//! block, it is pushed onto the control stack and popped off when encountering the corresponding
//! `End`.
//!
//! Another data structure, the translation state, records information concerning unreachable code
//! status and about if inserting a return at the end of the function is necessary.
//!
//! Some of the WebAssembly instructions need information about the environment for which they
//! are being translated:
//!
//! - the loads and stores need the memory base address;
//! - the `get_global` et `set_global` instructions depends on how the globals are implemented;
//! - `current_memory` and `grow_memory` are runtime functions;
//! - `call_indirect` has to translate the function index into the address of where this
//!    is;
//!
//! That is why `translate_function_body` takes an object having the `WasmRuntime` trait as
//! argument.
use cretonne::ir::{self, InstBuilder, Ebb, MemFlags, JumpTableData};
use cretonne::ir::types::*;
use cretonne::ir::condcodes::{IntCC, FloatCC};
use cton_frontend::FunctionBuilder;
use wasmparser::{Operator, MemoryImmediate};
use translation_utils::{f32_translation, f64_translation, type_to_type, num_return_values, Local};
use translation_utils::{TableIndex, SignatureIndex, FunctionIndex, MemoryIndex};
use state::{TranslationState, ControlStackFrame};
use std::collections::HashMap;
use environ::{FuncEnvironment, GlobalValue};
use std::u32;

/// Translates wasm operators into Cretonne IL instructions. Returns `true` if it inserted
/// a return.
pub fn translate_operator<FE: FuncEnvironment + ?Sized>(
    op: &Operator,
    builder: &mut FunctionBuilder<Local>,
    state: &mut TranslationState,
    environ: &mut FE,
) {
    if state.in_unreachable_code() {
        return translate_unreachable_operator(op, builder, state);
    }

    // This big match treats all Wasm code operators.
    match *op {
        /********************************** Locals ****************************************
         *  `get_local` and `set_local` are treated as non-SSA variables and will completely
         *  diseappear in the Cretonne Code
         ***********************************************************************************/
        Operator::GetLocal { local_index } => state.push1(builder.use_var(Local(local_index))),
        Operator::SetLocal { local_index } => {
            let val = state.pop1();
            builder.def_var(Local(local_index), val);
        }
        Operator::TeeLocal { local_index } => {
            let val = state.peek1();
            builder.def_var(Local(local_index), val);
        }
        /********************************** Globals ****************************************
         *  `get_global` and `set_global` are handled by the environment.
         ***********************************************************************************/
        Operator::GetGlobal { global_index } => {
            let val = match state.get_global(builder.func, global_index, environ) {
                GlobalValue::Const(val) => val,
                GlobalValue::Memory { gv, ty } => {
                    let addr = builder.ins().global_addr(environ.native_pointer(), gv);
                    // TODO: It is likely safe to set `aligned notrap` flags on a global load.
                    let flags = ir::MemFlags::new();
                    builder.ins().load(ty, flags, addr, 0)
                }
            };
            state.push1(val);
        }
        Operator::SetGlobal { global_index } => {
            match state.get_global(builder.func, global_index, environ) {
                GlobalValue::Const(_) => panic!("global #{} is a constant", global_index),
                GlobalValue::Memory { gv, .. } => {
                    let addr = builder.ins().global_addr(environ.native_pointer(), gv);
                    // TODO: It is likely safe to set `aligned notrap` flags on a global store.
                    let flags = ir::MemFlags::new();
                    let val = state.pop1();
                    builder.ins().store(flags, val, addr, 0);
                }
            }
        }
        /********************************* Stack misc ***************************************
         *  `drop`, `nop`,  `unreachable` and `select`.
         ***********************************************************************************/
        Operator::Drop => {
            state.pop1();
        }
        Operator::Select => {
            let (arg1, arg2, cond) = state.pop3();
            state.push1(builder.ins().select(cond, arg1, arg2));
        }
        Operator::Nop => {
            // We do nothing
        }
        Operator::Unreachable => {
            // We use `trap user0` to indicate a user-generated trap.
            // We could make the trap code configurable if need be.
            builder.ins().trap(ir::TrapCode::User(0));
            state.real_unreachable_stack_depth = 1;
        }
        /***************************** Control flow blocks **********************************
         *  When starting a control flow block, we create a new `Ebb` that will hold the code
         *  after the block, and we push a frame on the control stack. Depending on the type
         *  of block, we create a new `Ebb` for the body of the block with an associated
         *  jump instruction.
         *
         *  The `End` instruction pops the last control frame from the control stack, seals
         *  the destination block (since `br` instructions targeting it only appear inside the
         *  block and have already been translated) and modify the value stack to use the
         *  possible `Ebb`'s arguments values.
         ***********************************************************************************/
        Operator::Block { ty } => {
            let next = builder.create_ebb();
            if let Ok(ty_cre) = type_to_type(&ty) {
                builder.append_ebb_param(next, ty_cre);
            }
            state.push_block(next, num_return_values(ty));
        }
        Operator::Loop { ty } => {
            let loop_body = builder.create_ebb();
            let next = builder.create_ebb();
            if let Ok(ty_cre) = type_to_type(&ty) {
                builder.append_ebb_param(next, ty_cre);
            }
            builder.ins().jump(loop_body, &[]);
            state.push_loop(loop_body, next, num_return_values(ty));
            builder.switch_to_block(loop_body, &[]);
        }
        Operator::If { ty } => {
            let val = state.pop1();
            let if_not = builder.create_ebb();
            let jump_inst = builder.ins().brz(val, if_not, &[]);
            // Here we append an argument to an Ebb targeted by an argumentless jump instruction
            // But in fact there are two cases:
            // - either the If does not have a Else clause, in that case ty = EmptyBlock
            //   and we add nothing;
            // - either the If have an Else clause, in that case the destination of this jump
            //   instruction will be changed later when we translate the Else operator.
            if let Ok(ty_cre) = type_to_type(&ty) {
                builder.append_ebb_param(if_not, ty_cre);
            }
            state.push_if(jump_inst, if_not, num_return_values(ty));
        }
        Operator::Else => {
            // We take the control frame pushed by the if, use its ebb as the else body
            // and push a new control frame with a new ebb for the code after the if/then/else
            // At the end of the then clause we jump to the destination
            let i = state.control_stack.len() - 1;
            let (destination, return_count, branch_inst) = match state.control_stack[i] {
                ControlStackFrame::If {
                    destination,
                    num_return_values,
                    branch_inst,
                    ..
                } => (destination, num_return_values, branch_inst),
                _ => panic!("should not happen"),
            };
            builder.ins().jump(destination, state.peekn(return_count));
            state.popn(return_count);
            // We change the target of the branch instruction
            let else_ebb = builder.create_ebb();
            builder.change_jump_destination(branch_inst, else_ebb);
            builder.seal_block(else_ebb);
            builder.switch_to_block(else_ebb, &[]);
        }
        Operator::End => {
            let frame = state.control_stack.pop().unwrap();
            let return_count = frame.num_return_values();
            if !builder.is_unreachable() || !builder.is_pristine() {
                builder.ins().jump(
                    frame.following_code(),
                    state.peekn(return_count),
                );
            }
            builder.switch_to_block(frame.following_code(), state.peekn(return_count));
            builder.seal_block(frame.following_code());
            // If it is a loop we also have to seal the body loop block
            match frame {
                ControlStackFrame::Loop { header, .. } => builder.seal_block(header),
                _ => {}
            }
            state.stack.truncate(frame.original_stack_size());
            state.stack.extend_from_slice(
                builder.ebb_params(frame.following_code()),
            );
        }
        /**************************** Branch instructions *********************************
         * The branch instructions all have as arguments a target nesting level, which
         * corresponds to how many control stack frames do we have to pop to get the
         * destination `Ebb`.
         *
         * Once the destination `Ebb` is found, we sometimes have to declare a certain depth
         * of the stack unreachable, because some branch instructions are terminator.
         *
         * The `br_table` case is much more complicated because Cretonne's `br_table` instruction
         * does not support jump arguments like all the other branch instructions. That is why, in
         * the case where we would use jump arguments for every other branch instructions, we
         * need to split the critical edges leaving the `br_tables` by creating one `Ebb` per
         * table destination; the `br_table` will point to these newly created `Ebbs` and these
         * `Ebb`s contain only a jump instruction pointing to the final destination, this time with
         * jump arguments.
         *
         * This system is also implemented in Cretonne's SSA construction algorithm, because
         * `use_var` located in a destination `Ebb` of a `br_table` might trigger the addition
         * of jump arguments in each predecessor branch instruction, one of which might be a
         * `br_table`.
         ***********************************************************************************/
        Operator::Br { relative_depth } => {
            let i = state.control_stack.len() - 1 - (relative_depth as usize);
            let (return_count, br_destination) = {
                let frame = &mut state.control_stack[i];
                // We signal that all the code that follows until the next End is unreachable
                frame.set_reachable();
                let return_count = if frame.is_loop() {
                    0
                } else {
                    frame.num_return_values()
                };
                (return_count, frame.br_destination())
            };
            builder.ins().jump(
                br_destination,
                state.peekn(return_count),
            );
            state.popn(return_count);
            state.real_unreachable_stack_depth = 1 + relative_depth as usize;
        }
        Operator::BrIf { relative_depth } => {
            let val = state.pop1();
            let i = state.control_stack.len() - 1 - (relative_depth as usize);
            let (return_count, br_destination) = {
                let frame = &mut state.control_stack[i];
                // The values returned by the branch are still available for the reachable
                // code that comes after it
                frame.set_reachable();
                let return_count = if frame.is_loop() {
                    0
                } else {
                    frame.num_return_values()
                };
                (return_count, frame.br_destination())
            };
            builder.ins().brnz(
                val,
                br_destination,
                state.peekn(return_count),
            );
        }
        Operator::BrTable { ref table } => {
            let (depths, default) = table.read_table();
            let mut min_depth = default;
            for depth in &depths {
                if *depth < min_depth {
                    min_depth = *depth;
                }
            }
            let jump_args_count = {
                let i = state.control_stack.len() - 1 - (min_depth as usize);
                let min_depth_frame = &state.control_stack[i];
                if min_depth_frame.is_loop() {
                    0
                } else {
                    min_depth_frame.num_return_values()
                }
            };
            if jump_args_count == 0 {
                // No jump arguments
                let val = state.pop1();
                let mut data = JumpTableData::with_capacity(depths.len());
                for depth in depths {
                    let i = state.control_stack.len() - 1 - (depth as usize);
                    let frame = &mut state.control_stack[i];
                    let ebb = frame.br_destination();
                    data.push_entry(ebb);
                    frame.set_reachable();
                }
                let jt = builder.create_jump_table(data);
                builder.ins().br_table(val, jt);
                let i = state.control_stack.len() - 1 - (default as usize);
                let frame = &mut state.control_stack[i];
                let ebb = frame.br_destination();
                builder.ins().jump(ebb, &[]);
                state.real_unreachable_stack_depth = 1 + min_depth as usize;
                frame.set_reachable();
            } else {
                // Here we have jump arguments, but Cretonne's br_table doesn't support them
                // We then proceed to split the edges going out of the br_table
                let val = state.pop1();
                let return_count = jump_args_count;
                let mut data = JumpTableData::with_capacity(depths.len());
                let dest_ebbs: HashMap<usize, Ebb> = depths.iter().fold(HashMap::new(), |mut acc,
                 &depth| {
                    if acc.get(&(depth as usize)).is_none() {
                        let branch_ebb = builder.create_ebb();
                        data.push_entry(branch_ebb);
                        acc.insert(depth as usize, branch_ebb);
                        return acc;
                    };
                    let branch_ebb = acc[&(depth as usize)];
                    data.push_entry(branch_ebb);
                    acc
                });
                let jt = builder.create_jump_table(data);
                builder.ins().br_table(val, jt);
                let default_ebb = state.control_stack[state.control_stack.len() - 1 -
                                                          (default as usize)]
                    .br_destination();
                builder.ins().jump(default_ebb, state.peekn(return_count));
                for (depth, dest_ebb) in dest_ebbs {
                    builder.switch_to_block(dest_ebb, &[]);
                    builder.seal_block(dest_ebb);
                    let i = state.control_stack.len() - 1 - depth;
                    let real_dest_ebb = {
                        let frame = &mut state.control_stack[i];
                        frame.set_reachable();
                        frame.br_destination()
                    };
                    builder.ins().jump(real_dest_ebb, state.peekn(return_count));
                }
                state.popn(return_count);
                state.real_unreachable_stack_depth = 1 + min_depth as usize;
            }
        }
        Operator::Return => {
            let (return_count, br_destination) = {
                let frame = &mut state.control_stack[0];
                frame.set_reachable();
                let return_count = frame.num_return_values();
                (return_count, frame.br_destination())
            };
            {
                let args = state.peekn(return_count);
                if environ.flags().return_at_end() {
                    builder.ins().jump(br_destination, args);
                } else {
                    builder.ins().return_(args);
                }
            }
            state.popn(return_count);
            state.real_unreachable_stack_depth = 1;
        }
        /************************************ Calls ****************************************
         * The call instructions pop off their arguments from the stack and append their
         * return values to it. `call_indirect` needs environment support because there is an
         * argument referring to an index in the external functions table of the module.
         ************************************************************************************/
        Operator::Call { function_index } => {
            let (fref, num_args) = state.get_direct_func(builder.func, function_index, environ);
            let call = environ.translate_call(
                builder.cursor(),
                function_index as FunctionIndex,
                fref,
                state.peekn(num_args),
            );
            state.popn(num_args);
            state.pushn(builder.func.dfg.inst_results(call));
        }
        Operator::CallIndirect { index, table_index } => {
            // `index` is the index of the function's signature and `table_index` is the index of
            // the table to search the function in.
            let (sigref, num_args) = state.get_indirect_sig(builder.func, index, environ);
            let callee = state.pop1();
            let call = environ.translate_call_indirect(
                builder.cursor(),
                table_index as TableIndex,
                index as SignatureIndex,
                sigref,
                callee,
                state.peekn(num_args),
            );
            state.popn(num_args);
            state.pushn(builder.func.dfg.inst_results(call));
        }
        /******************************* Memory management ***********************************
         * Memory management is handled by environment. It is usually translated into calls to
         * special functions.
         ************************************************************************************/
        Operator::GrowMemory { reserved } => {
            // The WebAssembly MVP only supports one linear memory, but we expect the reserved
            // argument to be a memory index.
            let heap_index = reserved as MemoryIndex;
            let heap = state.get_heap(builder.func, reserved, environ);
            let val = state.pop1();
            state.push1(environ.translate_grow_memory(
                builder.cursor(),
                heap_index,
                heap,
                val,
            ))
        }
        Operator::CurrentMemory { reserved } => {
            let heap_index = reserved as MemoryIndex;
            let heap = state.get_heap(builder.func, reserved, environ);
            state.push1(environ.translate_current_memory(
                builder.cursor(),
                heap_index,
                heap,
            ));
        }
        /******************************* Load instructions ***********************************
         * Wasm specifies an integer alignment flag but we drop it in Cretonne.
         * The memory base address is provided by the environment.
         * TODO: differentiate between 32 bit and 64 bit architecture, to put the uextend or not
         ************************************************************************************/
        Operator::I32Load8U { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Uload8, I32, builder, state, environ);
        }
        Operator::I32Load16U { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Uload16, I32, builder, state, environ);
        }
        Operator::I32Load8S { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Sload8, I32, builder, state, environ);
        }
        Operator::I32Load16S { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Sload16, I32, builder, state, environ);
        }
        Operator::I64Load8U { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Uload8, I64, builder, state, environ);
        }
        Operator::I64Load16U { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Uload16, I64, builder, state, environ);
        }
        Operator::I64Load8S { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Sload8, I64, builder, state, environ);
        }
        Operator::I64Load16S { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Sload16, I64, builder, state, environ);
        }
        Operator::I64Load32S { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Sload32, I64, builder, state, environ);
        }
        Operator::I64Load32U { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Uload32, I64, builder, state, environ);
        }
        Operator::I32Load { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Load, I32, builder, state, environ);
        }
        Operator::F32Load { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Load, F32, builder, state, environ);
        }
        Operator::I64Load { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Load, I64, builder, state, environ);
        }
        Operator::F64Load { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_load(offset, ir::Opcode::Load, F64, builder, state, environ);
        }
        /****************************** Store instructions ***********************************
         * Wasm specifies an integer alignment flag but we drop it in Cretonne.
         * The memory base address is provided by the environment.
         * TODO: differentiate between 32 bit and 64 bit architecture, to put the uextend or not
         ************************************************************************************/
        Operator::I32Store { memarg: MemoryImmediate { flags: _, offset } } |
        Operator::I64Store { memarg: MemoryImmediate { flags: _, offset } } |
        Operator::F32Store { memarg: MemoryImmediate { flags: _, offset } } |
        Operator::F64Store { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_store(offset, ir::Opcode::Store, builder, state, environ);
        }
        Operator::I32Store8 { memarg: MemoryImmediate { flags: _, offset } } |
        Operator::I64Store8 { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_store(offset, ir::Opcode::Istore8, builder, state, environ);
        }
        Operator::I32Store16 { memarg: MemoryImmediate { flags: _, offset } } |
        Operator::I64Store16 { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_store(offset, ir::Opcode::Istore16, builder, state, environ);
        }
        Operator::I64Store32 { memarg: MemoryImmediate { flags: _, offset } } => {
            translate_store(offset, ir::Opcode::Istore32, builder, state, environ);
        }
        /****************************** Nullary Operators ************************************/
        Operator::I32Const { value } => state.push1(builder.ins().iconst(I32, value as i64)),
        Operator::I64Const { value } => state.push1(builder.ins().iconst(I64, value)),
        Operator::F32Const { value } => {
            state.push1(builder.ins().f32const(f32_translation(value)));
        }
        Operator::F64Const { value } => {
            state.push1(builder.ins().f64const(f64_translation(value)));
        }
        /******************************* Unary Operators *************************************/
        Operator::I32Clz => {
            let arg = state.pop1();
            state.push1(builder.ins().clz(arg));
        }
        Operator::I64Clz => {
            let arg = state.pop1();
            state.push1(builder.ins().clz(arg));
        }
        Operator::I32Ctz => {
            let arg = state.pop1();
            state.push1(builder.ins().ctz(arg));
        }
        Operator::I64Ctz => {
            let arg = state.pop1();
            state.push1(builder.ins().ctz(arg));
        }
        Operator::I32Popcnt => {
            let arg = state.pop1();
            state.push1(builder.ins().popcnt(arg));
        }
        Operator::I64Popcnt => {
            let arg = state.pop1();
            state.push1(builder.ins().popcnt(arg));
        }
        Operator::I64ExtendSI32 => {
            let val = state.pop1();
            state.push1(builder.ins().sextend(I64, val));
        }
        Operator::I64ExtendUI32 => {
            let val = state.pop1();
            state.push1(builder.ins().uextend(I64, val));
        }
        Operator::I32WrapI64 => {
            let val = state.pop1();
            state.push1(builder.ins().ireduce(I32, val));
        }
        Operator::F32Sqrt |
        Operator::F64Sqrt => {
            let arg = state.pop1();
            state.push1(builder.ins().sqrt(arg));
        }
        Operator::F32Ceil |
        Operator::F64Ceil => {
            let arg = state.pop1();
            state.push1(builder.ins().ceil(arg));
        }
        Operator::F32Floor |
        Operator::F64Floor => {
            let arg = state.pop1();
            state.push1(builder.ins().floor(arg));
        }
        Operator::F32Trunc |
        Operator::F64Trunc => {
            let arg = state.pop1();
            state.push1(builder.ins().trunc(arg));
        }
        Operator::F32Nearest |
        Operator::F64Nearest => {
            let arg = state.pop1();
            state.push1(builder.ins().nearest(arg));
        }
        Operator::F32Abs | Operator::F64Abs => {
            let val = state.pop1();
            state.push1(builder.ins().fabs(val));
        }
        Operator::F32Neg | Operator::F64Neg => {
            let arg = state.pop1();
            state.push1(builder.ins().fneg(arg));
        }
        Operator::F64ConvertUI64 |
        Operator::F64ConvertUI32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_from_uint(F64, val));
        }
        Operator::F64ConvertSI64 |
        Operator::F64ConvertSI32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_from_sint(F64, val));
        }
        Operator::F32ConvertSI64 |
        Operator::F32ConvertSI32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_from_sint(F32, val));
        }
        Operator::F32ConvertUI64 |
        Operator::F32ConvertUI32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_from_uint(F32, val));
        }
        Operator::F64PromoteF32 => {
            let val = state.pop1();
            state.push1(builder.ins().fpromote(F64, val));
        }
        Operator::F32DemoteF64 => {
            let val = state.pop1();
            state.push1(builder.ins().fdemote(F32, val));
        }
        Operator::I64TruncSF64 |
        Operator::I64TruncSF32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_to_sint(I64, val));
        }
        Operator::I32TruncSF64 |
        Operator::I32TruncSF32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_to_sint(I32, val));
        }
        Operator::I64TruncUF64 |
        Operator::I64TruncUF32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_to_uint(I64, val));
        }
        Operator::I32TruncUF64 |
        Operator::I32TruncUF32 => {
            let val = state.pop1();
            state.push1(builder.ins().fcvt_to_uint(I32, val));
        }
        Operator::I64TruncSSatF64 |
        Operator::I64TruncSSatF32 |
        Operator::I32TruncSSatF64 |
        Operator::I32TruncSSatF32 |
        Operator::I64TruncUSatF64 |
        Operator::I64TruncUSatF32 |
        Operator::I32TruncUSatF64 |
        Operator::I32TruncUSatF32 => {
            panic!("proposed saturating conversion operators not yet supported");
        }
        Operator::F32ReinterpretI32 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(F32, val));
        }
        Operator::F64ReinterpretI64 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(F64, val));
        }
        Operator::I32ReinterpretF32 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(I32, val));
        }
        Operator::I64ReinterpretF64 => {
            let val = state.pop1();
            state.push1(builder.ins().bitcast(I64, val));
        }
        /****************************** Binary Operators ************************************/
        Operator::I32Add | Operator::I64Add => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().iadd(arg1, arg2));
        }
        Operator::I32And | Operator::I64And => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().band(arg1, arg2));
        }
        Operator::I32Or | Operator::I64Or => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().bor(arg1, arg2));
        }
        Operator::I32Xor | Operator::I64Xor => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().bxor(arg1, arg2));
        }
        Operator::I32Shl | Operator::I64Shl => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().ishl(arg1, arg2));
        }
        Operator::I32ShrS |
        Operator::I64ShrS => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().sshr(arg1, arg2));
        }
        Operator::I32ShrU |
        Operator::I64ShrU => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().ushr(arg1, arg2));
        }
        Operator::I32Rotl |
        Operator::I64Rotl => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().rotl(arg1, arg2));
        }
        Operator::I32Rotr |
        Operator::I64Rotr => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().rotr(arg1, arg2));
        }
        Operator::F32Add | Operator::F64Add => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fadd(arg1, arg2));
        }
        Operator::I32Sub | Operator::I64Sub => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().isub(arg1, arg2));
        }
        Operator::F32Sub | Operator::F64Sub => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fsub(arg1, arg2));
        }
        Operator::I32Mul | Operator::I64Mul => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().imul(arg1, arg2));
        }
        Operator::F32Mul | Operator::F64Mul => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fmul(arg1, arg2));
        }
        Operator::F32Div | Operator::F64Div => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fdiv(arg1, arg2));
        }
        Operator::I32DivS |
        Operator::I64DivS => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().sdiv(arg1, arg2));
        }
        Operator::I32DivU |
        Operator::I64DivU => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().udiv(arg1, arg2));
        }
        Operator::I32RemS |
        Operator::I64RemS => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().srem(arg1, arg2));
        }
        Operator::I32RemU |
        Operator::I64RemU => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().urem(arg1, arg2));
        }
        Operator::F32Min | Operator::F64Min => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fmin(arg1, arg2));
        }
        Operator::F32Max | Operator::F64Max => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fmax(arg1, arg2));
        }
        Operator::F32Copysign |
        Operator::F64Copysign => {
            let (arg1, arg2) = state.pop2();
            state.push1(builder.ins().fcopysign(arg1, arg2));
        }
        /**************************** Comparison Operators **********************************/
        Operator::I32LtS | Operator::I64LtS => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::SignedLessThan, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32LtU | Operator::I64LtU => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::UnsignedLessThan, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32LeS | Operator::I64LeS => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::SignedLessThanOrEqual, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32LeU | Operator::I64LeU => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(
                IntCC::UnsignedLessThanOrEqual,
                arg1,
                arg2,
            );
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32GtS | Operator::I64GtS => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::SignedGreaterThan, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32GtU | Operator::I64GtU => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::UnsignedGreaterThan, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32GeS | Operator::I64GeS => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(
                IntCC::SignedGreaterThanOrEqual,
                arg1,
                arg2,
            );
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32GeU | Operator::I64GeU => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(
                IntCC::UnsignedGreaterThanOrEqual,
                arg1,
                arg2,
            );
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32Eqz | Operator::I64Eqz => {
            let arg = state.pop1();
            let val = builder.ins().icmp_imm(IntCC::Equal, arg, 0);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32Eq | Operator::I64Eq => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::Equal, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::F32Eq | Operator::F64Eq => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().fcmp(FloatCC::Equal, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::I32Ne | Operator::I64Ne => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().icmp(IntCC::NotEqual, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::F32Ne | Operator::F64Ne => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().fcmp(FloatCC::NotEqual, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::F32Gt | Operator::F64Gt => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().fcmp(FloatCC::GreaterThan, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::F32Ge | Operator::F64Ge => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().fcmp(FloatCC::GreaterThanOrEqual, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::F32Lt | Operator::F64Lt => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().fcmp(FloatCC::LessThan, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
        Operator::F32Le | Operator::F64Le => {
            let (arg1, arg2) = state.pop2();
            let val = builder.ins().fcmp(FloatCC::LessThanOrEqual, arg1, arg2);
            state.push1(builder.ins().bint(I32, val));
        }
    }
}

/// Deals with a Wasm instruction located in an unreachable portion of the code. Most of them
/// are dropped but special ones like `End` or `Else` signal the potential end of the unreachable
/// portion so the translation state muts be updated accordingly.
fn translate_unreachable_operator(
    op: &Operator,
    builder: &mut FunctionBuilder<Local>,
    state: &mut TranslationState,
) {
    let stack = &mut state.stack;
    let control_stack = &mut state.control_stack;

    // We don't translate because the code is unreachable
    // Nevertheless we have to record a phantom stack for this code
    // to know when the unreachable code ends
    match *op {
        Operator::If { ty: _ } |
        Operator::Loop { ty: _ } |
        Operator::Block { ty: _ } => {
            state.phantom_unreachable_stack_depth += 1;
        }
        Operator::End => {
            if state.phantom_unreachable_stack_depth > 0 {
                state.phantom_unreachable_stack_depth -= 1;
            } else {
                // This End corresponds to a real control stack frame
                // We switch to the destination block but we don't insert
                // a jump instruction since the code is still unreachable
                let frame = control_stack.pop().unwrap();

                builder.switch_to_block(frame.following_code(), &[]);
                builder.seal_block(frame.following_code());
                match frame {
                    // If it is a loop we also have to seal the body loop block
                    ControlStackFrame::Loop { header, .. } => builder.seal_block(header),
                    // If it is an if then the code after is reachable again
                    ControlStackFrame::If { .. } => {
                        state.real_unreachable_stack_depth = 1;
                    }
                    _ => {}
                }
                if frame.is_reachable() {
                    state.real_unreachable_stack_depth = 1;
                }
                // Now we have to split off the stack the values not used
                // by unreachable code that hasn't been translated
                stack.truncate(frame.original_stack_size());
                // And add the return values of the block but only if the next block is reachble
                // (which corresponds to testing if the stack depth is 1)
                if state.real_unreachable_stack_depth == 1 {
                    stack.extend_from_slice(builder.ebb_params(frame.following_code()));
                }
                state.real_unreachable_stack_depth -= 1;
            }
        }
        Operator::Else => {
            if state.phantom_unreachable_stack_depth > 0 {
                // This is part of a phantom if-then-else, we do nothing
            } else {
                // Encountering an real else means that the code in the else
                // clause is reachable again
                let (branch_inst, original_stack_size) = match control_stack[control_stack.len() -
                                                                                   1] {
                    ControlStackFrame::If {
                        branch_inst,
                        original_stack_size,
                        ..
                    } => (branch_inst, original_stack_size),
                    _ => panic!("should not happen"),
                };
                // We change the target of the branch instruction
                let else_ebb = builder.create_ebb();
                builder.change_jump_destination(branch_inst, else_ebb);
                builder.seal_block(else_ebb);
                builder.switch_to_block(else_ebb, &[]);
                // Now we have to split off the stack the values not used
                // by unreachable code that hasn't been translated
                stack.truncate(original_stack_size);
                state.real_unreachable_stack_depth = 0;
            }
        }
        _ => {
            // We don't translate because this is unreachable code
        }
    }
}

// Get the address+offset to use for a heap access.
fn get_heap_addr(
    heap: ir::Heap,
    addr32: ir::Value,
    offset: u32,
    addr_ty: ir::Type,
    builder: &mut FunctionBuilder<Local>,
) -> (ir::Value, i32) {
    use std::cmp::min;

    let guard_size: i64 = builder.func.heaps[heap].guard_size.into();
    assert!(guard_size > 0, "Heap guard pages currently required");

    // Generate `heap_addr` instructions that are friendly to CSE by checking offsets that are
    // multiples of the guard size. Add one to make sure that we check the pointer itself is in
    // bounds.
    //
    // For accesses on the outer skirts of the guard pages, we expect that we get a trap
    // even if the access goes beyond the guard pages. This is because the first byte pointed to is
    // inside the guard pages.
    let check_size = min(
        u32::max_value() as i64,
        1 + (offset as i64 / guard_size) * guard_size,
    ) as u32;
    let base = builder.ins().heap_addr(addr_ty, heap, addr32, check_size);

    // Native load/store instructions take a signed `Offset32` immediate, so adjust the base
    // pointer if necessary.
    if offset > i32::max_value() as u32 {
        // Offset doesn't fit in the load/store instruction.
        let adj = builder.ins().iadd_imm(base, i32::max_value() as i64 + 1);
        (adj, (offset - (i32::max_value() as u32 + 1)) as i32)
    } else {
        (base, offset as i32)
    }
}

// Translate a load instruction.
fn translate_load<FE: FuncEnvironment + ?Sized>(
    offset: u32,
    opcode: ir::Opcode,
    result_ty: ir::Type,
    builder: &mut FunctionBuilder<Local>,
    state: &mut TranslationState,
    environ: &mut FE,
) {
    let addr32 = state.pop1();
    // We don't yet support multiple linear memories.
    let heap = state.get_heap(builder.func, 0, environ);
    let (base, offset) = get_heap_addr(heap, addr32, offset, environ.native_pointer(), builder);
    let flags = MemFlags::new();
    let (load, dfg) = builder.ins().Load(
        opcode,
        result_ty,
        flags,
        offset.into(),
        base,
    );
    state.push1(dfg.first_result(load));
}

// Translate a store instruction.
fn translate_store<FE: FuncEnvironment + ?Sized>(
    offset: u32,
    opcode: ir::Opcode,
    builder: &mut FunctionBuilder<Local>,
    state: &mut TranslationState,
    environ: &mut FE,
) {
    let (addr32, val) = state.pop2();
    let val_ty = builder.func.dfg.value_type(val);

    // We don't yet support multiple linear memories.
    let heap = state.get_heap(builder.func, 0, environ);
    let (base, offset) = get_heap_addr(heap, addr32, offset, environ.native_pointer(), builder);
    let flags = MemFlags::new();
    builder.ins().Store(
        opcode,
        val_ty,
        flags,
        offset.into(),
        val,
        base,
    );
}
