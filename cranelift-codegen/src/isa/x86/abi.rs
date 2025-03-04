//! x86 ABI implementation.

use super::super::settings as shared_settings;
use super::registers::{FPR, GPR, RU};
use super::settings as isa_settings;
use super::unwind::UnwindInfo;
use crate::abi::{legalize_args, ArgAction, ArgAssigner, ValueConversion};
use crate::cursor::{Cursor, CursorPosition, EncCursor};
use crate::ir;
use crate::ir::immediates::Imm64;
use crate::ir::stackslot::{StackOffset, StackSize};
use crate::ir::{
    get_probestack_funcref, AbiParam, ArgumentExtension, ArgumentLoc, ArgumentPurpose, InstBuilder,
    ValueLoc,
};
use crate::isa::{CallConv, RegClass, RegUnit, TargetIsa};
use crate::regalloc::RegisterSet;
use crate::result::CodegenResult;
use crate::stack_layout::layout_stack;
use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::i32;
use target_lexicon::{PointerWidth, Triple};

/// Argument registers for x86-64
static ARG_GPRS: [RU; 6] = [RU::rdi, RU::rsi, RU::rdx, RU::rcx, RU::r8, RU::r9];

/// Return value registers.
static RET_GPRS: [RU; 3] = [RU::rax, RU::rdx, RU::rcx];

/// Argument registers for x86-64, when using windows fastcall
static ARG_GPRS_WIN_FASTCALL_X64: [RU; 4] = [RU::rcx, RU::rdx, RU::r8, RU::r9];

/// Return value registers for x86-64, when using windows fastcall
static RET_GPRS_WIN_FASTCALL_X64: [RU; 1] = [RU::rax];

struct Args {
    pointer_bytes: u8,
    pointer_bits: u8,
    pointer_type: ir::Type,
    gpr: &'static [RU],
    gpr_used: usize,
    fpr_limit: usize,
    fpr_used: usize,
    offset: u32,
    call_conv: CallConv,
    shared_flags: shared_settings::Flags,
    #[allow(dead_code)]
    isa_flags: isa_settings::Flags,
}

impl Args {
    fn new(
        bits: u8,
        gpr: &'static [RU],
        fpr_limit: usize,
        call_conv: CallConv,
        shared_flags: &shared_settings::Flags,
        isa_flags: &isa_settings::Flags,
    ) -> Self {
        let offset = if call_conv.extends_windows_fastcall() {
            // [1] "The caller is responsible for allocating space for parameters to the callee,
            // and must always allocate sufficient space to store four register parameters"
            32
        } else {
            0
        };

        Self {
            pointer_bytes: bits / 8,
            pointer_bits: bits,
            pointer_type: ir::Type::int(u16::from(bits)).unwrap(),
            gpr,
            gpr_used: 0,
            fpr_limit,
            fpr_used: 0,
            offset,
            call_conv,
            shared_flags: shared_flags.clone(),
            isa_flags: isa_flags.clone(),
        }
    }
}

impl ArgAssigner for Args {
    fn assign(&mut self, arg: &AbiParam) -> ArgAction {
        let ty = arg.value_type;

        // Vectors should stay in vector registers unless SIMD is not enabled--then they are split
        if ty.is_vector() {
            if self.shared_flags.enable_simd() {
                let reg = FPR.unit(self.fpr_used);
                self.fpr_used += 1;
                return ArgumentLoc::Reg(reg).into();
            }
            return ValueConversion::VectorSplit.into();
        }

        // Large integers and booleans are broken down to fit in a register.
        if !ty.is_float() && ty.bits() > u16::from(self.pointer_bits) {
            return ValueConversion::IntSplit.into();
        }

        // Small integers are extended to the size of a pointer register.
        if ty.is_int() && ty.bits() < u16::from(self.pointer_bits) {
            match arg.extension {
                ArgumentExtension::None => {}
                ArgumentExtension::Uext => return ValueConversion::Uext(self.pointer_type).into(),
                ArgumentExtension::Sext => return ValueConversion::Sext(self.pointer_type).into(),
            }
        }

        // Handle special-purpose arguments.
        if ty.is_int() && self.call_conv.extends_baldrdash() {
            match arg.purpose {
                // This is SpiderMonkey's `WasmTlsReg`.
                ArgumentPurpose::VMContext => {
                    return ArgumentLoc::Reg(if self.pointer_bits == 64 {
                        RU::r14
                    } else {
                        RU::rsi
                    } as RegUnit)
                    .into();
                }
                // This is SpiderMonkey's `WasmTableCallSigReg`.
                ArgumentPurpose::SignatureId => {
                    return ArgumentLoc::Reg(if self.pointer_bits == 64 {
                        RU::r10
                    } else {
                        RU::rcx
                    } as RegUnit)
                    .into()
                }
                _ => {}
            }
        }

        // Try to use a GPR.
        if !ty.is_float() && self.gpr_used < self.gpr.len() {
            let reg = self.gpr[self.gpr_used] as RegUnit;
            self.gpr_used += 1;
            return ArgumentLoc::Reg(reg).into();
        }

        // Try to use an FPR.
        let fpr_offset = if self.call_conv.extends_windows_fastcall() {
            // Float and general registers on windows share the same parameter index.
            // The used register depends entirely on the parameter index: Even if XMM0
            // is not used for the first parameter, it cannot be used for the second parameter.
            debug_assert_eq!(self.fpr_limit, self.gpr.len());
            &mut self.gpr_used
        } else {
            &mut self.fpr_used
        };

        if ty.is_float() && *fpr_offset < self.fpr_limit {
            let reg = FPR.unit(*fpr_offset);
            *fpr_offset += 1;
            return ArgumentLoc::Reg(reg).into();
        }

        // Assign a stack location.
        let loc = ArgumentLoc::Stack(self.offset as i32);
        self.offset += u32::from(self.pointer_bytes);
        debug_assert!(self.offset <= i32::MAX as u32);
        loc.into()
    }
}

/// Get the number of general-purpose and floating-point registers required to
/// hold the given `AbiParam` returns.
fn num_return_registers_required<'a>(
    word_bit_size: u8,
    call_conv: CallConv,
    shared_flags: &shared_settings::Flags,
    isa_flags: &isa_settings::Flags,
    return_params: impl IntoIterator<Item = &'a AbiParam>,
) -> (usize, usize) {
    // Pretend we have "infinite" registers to give out, since we aren't
    // actually assigning `AbiParam`s to registers yet, just seeing how many
    // registers we would need in order to fit all the `AbiParam`s in registers.
    let gprs = &[RU::rax; 128];
    let fpr_limit = std::usize::MAX;

    let mut assigner = Args::new(
        word_bit_size,
        gprs,
        fpr_limit,
        call_conv,
        shared_flags,
        isa_flags,
    );

    let mut gprs_required = 0;
    let mut fprs_required = 0;

    for param in return_params {
        match param.location {
            ArgumentLoc::Unassigned => {
                // Let this fall through so that we assign it a location and
                // account for how many registers it ends up requiring below...
            }
            ArgumentLoc::Reg(_) => {
                // This is already assigned to a register. Count it.
                if param.value_type.is_float() {
                    fprs_required += 1;
                } else {
                    gprs_required += 1;
                }
                continue;
            }
            _ => {
                // It is already assigned, but not to a register. Skip it.
                continue;
            }
        }

        // We're going to mutate the type as it gets converted, so make our own
        // copy that isn't visible to the outside world.
        let mut param = param.clone();

        let mut split_factor = 1;

        loop {
            match assigner.assign(&param) {
                ArgAction::Convert(ValueConversion::IntSplit) => {
                    split_factor *= 2;
                    param.value_type = param.value_type.half_width().unwrap();
                }
                ArgAction::Convert(ValueConversion::VectorSplit) => {
                    split_factor *= 2;
                    param.value_type = param.value_type.half_vector().unwrap();
                }
                ArgAction::Assign(ArgumentLoc::Reg(_))
                | ArgAction::Convert(ValueConversion::IntBits)
                | ArgAction::Convert(ValueConversion::Sext(_))
                | ArgAction::Convert(ValueConversion::Uext(_)) => {
                    // Ok! We can fit this (potentially split) value into a
                    // register! Add the number of params we split the parameter
                    // into to our current counts.
                    if param.value_type.is_float() {
                        fprs_required += split_factor;
                    } else {
                        gprs_required += split_factor;
                    }

                    // But we also have to call `assign` once for each split value, to
                    // update `assigner`'s internal state.
                    for _ in 1..split_factor {
                        match assigner.assign(&param) {
                            ArgAction::Assign(_)
                            | ArgAction::Convert(ValueConversion::IntBits)
                            | ArgAction::Convert(ValueConversion::Sext(_))
                            | ArgAction::Convert(ValueConversion::Uext(_)) => {
                                continue;
                            }
                            otherwise => panic!(
                                "unexpected action after first split succeeded: {:?}",
                                otherwise
                            ),
                        }
                    }

                    // Continue to the next param.
                    break;
                }
                ArgAction::Assign(loc) => panic!(
                    "unexpected location assignment, should have had enough registers: {:?}",
                    loc
                ),
            }
        }
    }

    (gprs_required, fprs_required)
}

/// Legalize `sig`.
pub fn legalize_signature(
    sig: &mut Cow<ir::Signature>,
    triple: &Triple,
    _current: bool,
    shared_flags: &shared_settings::Flags,
    isa_flags: &isa_settings::Flags,
) {
    let bits;
    let mut args;

    match triple.pointer_width().unwrap() {
        PointerWidth::U16 => panic!(),
        PointerWidth::U32 => {
            bits = 32;
            args = Args::new(bits, &[], 0, sig.call_conv, shared_flags, isa_flags);
        }
        PointerWidth::U64 => {
            bits = 64;
            args = if sig.call_conv.extends_windows_fastcall() {
                Args::new(
                    bits,
                    &ARG_GPRS_WIN_FASTCALL_X64[..],
                    4,
                    sig.call_conv,
                    shared_flags,
                    isa_flags,
                )
            } else {
                Args::new(
                    bits,
                    &ARG_GPRS[..],
                    8,
                    sig.call_conv,
                    shared_flags,
                    isa_flags,
                )
            };
        }
    }

    let (ret_regs, ret_fpr_limit) = if sig.call_conv.extends_windows_fastcall() {
        // windows-x64 calling convention only uses XMM0 or RAX for return values
        (&RET_GPRS_WIN_FASTCALL_X64[..], 1)
    } else {
        (&RET_GPRS[..], 2)
    };

    let mut rets = Args::new(
        bits,
        ret_regs,
        ret_fpr_limit,
        sig.call_conv,
        shared_flags,
        isa_flags,
    );

    if sig.is_multi_return() && {
        // Even if it is multi-return, see if the return values will fit into
        // our available return registers.
        let (gprs_required, fprs_required) = num_return_registers_required(
            bits,
            sig.call_conv,
            shared_flags,
            isa_flags,
            &sig.returns,
        );
        gprs_required > ret_regs.len() || fprs_required > ret_fpr_limit
    } {
        debug_assert!(!sig.uses_struct_return_param());

        // We're using the first register for the return pointer parameter.
        let mut ret_ptr_param = AbiParam {
            value_type: args.pointer_type,
            purpose: ArgumentPurpose::StructReturn,
            extension: ArgumentExtension::None,
            location: ArgumentLoc::Unassigned,
        };
        match args.assign(&ret_ptr_param) {
            ArgAction::Assign(ArgumentLoc::Reg(reg)) => {
                ret_ptr_param.location = ArgumentLoc::Reg(reg);
                sig.to_mut().params.push(ret_ptr_param);
            }
            _ => unreachable!("return pointer should always get a register assignment"),
        }

        // We're using the first return register for the return pointer (like
        // sys v does).
        let mut ret_ptr_return = AbiParam {
            value_type: args.pointer_type,
            purpose: ArgumentPurpose::StructReturn,
            extension: ArgumentExtension::None,
            location: ArgumentLoc::Unassigned,
        };
        match rets.assign(&ret_ptr_return) {
            ArgAction::Assign(ArgumentLoc::Reg(reg)) => {
                ret_ptr_return.location = ArgumentLoc::Reg(reg);
                sig.to_mut().returns.push(ret_ptr_return);
            }
            _ => unreachable!("return pointer should always get a register assignment"),
        }

        sig.to_mut().returns.retain(|ret| {
            // Either this is the return pointer, in which case we want to keep
            // it, or else assume that it is assigned for a reason and doesn't
            // conflict with our return pointering legalization.
            debug_assert_eq!(
                ret.location.is_assigned(),
                ret.purpose != ArgumentPurpose::Normal
            );
            ret.location.is_assigned()
        });
    }

    if let Some(new_params) = legalize_args(&sig.params, &mut args) {
        sig.to_mut().params = new_params;
    }

    if let Some(new_returns) = legalize_args(&sig.returns, &mut rets) {
        sig.to_mut().returns = new_returns;
    }
}

/// Get register class for a type appearing in a legalized signature.
pub fn regclass_for_abi_type(ty: ir::Type) -> RegClass {
    if ty.is_int() || ty.is_bool() {
        GPR
    } else {
        FPR
    }
}

/// Get the set of allocatable registers for `func`.
pub fn allocatable_registers(triple: &Triple, flags: &shared_settings::Flags) -> RegisterSet {
    let mut regs = RegisterSet::new();
    regs.take(GPR, RU::rsp as RegUnit);
    regs.take(GPR, RU::rbp as RegUnit);

    // 32-bit arch only has 8 registers.
    if triple.pointer_width().unwrap() != PointerWidth::U64 {
        for i in 8..16 {
            regs.take(GPR, GPR.unit(i));
            regs.take(FPR, FPR.unit(i));
        }
        if flags.enable_pinned_reg() {
            unimplemented!("Pinned register not implemented on x86-32.");
        }
    } else {
        // Choose r15 as the pinned register on 64-bits: it is non-volatile on native ABIs and
        // isn't the fixed output register of any instruction.
        if flags.enable_pinned_reg() {
            regs.take(GPR, RU::r15 as RegUnit);
        }
    }

    regs
}

/// Get the set of callee-saved registers.
fn callee_saved_gprs(isa: &dyn TargetIsa, call_conv: CallConv) -> &'static [RU] {
    match isa.triple().pointer_width().unwrap() {
        PointerWidth::U16 => panic!(),
        PointerWidth::U32 => &[RU::rbx, RU::rsi, RU::rdi],
        PointerWidth::U64 => {
            if call_conv.extends_windows_fastcall() {
                // "registers RBX, RBP, RDI, RSI, RSP, R12, R13, R14, R15 are considered nonvolatile
                //  and must be saved and restored by a function that uses them."
                // as per https://docs.microsoft.com/en-us/cpp/build/x64-calling-convention
                // RSP & RSB are not listed below, since they are restored automatically during
                // a function call. If that wasn't the case, function calls (RET) would not work.
                &[
                    RU::rbx,
                    RU::rdi,
                    RU::rsi,
                    RU::r12,
                    RU::r13,
                    RU::r14,
                    RU::r15,
                ]
            } else {
                &[RU::rbx, RU::r12, RU::r13, RU::r14, RU::r15]
            }
        }
    }
}

/// Get the set of callee-saved registers that are used.
fn callee_saved_gprs_used(isa: &dyn TargetIsa, func: &ir::Function) -> RegisterSet {
    let mut all_callee_saved = RegisterSet::empty();
    for reg in callee_saved_gprs(isa, func.signature.call_conv) {
        all_callee_saved.free(GPR, *reg as RegUnit);
    }

    let mut used = RegisterSet::empty();
    for value_loc in func.locations.values() {
        // Note that `value_loc` here contains only a single unit of a potentially multi-unit
        // register. We don't use registers that overlap each other in the x86 ISA, but in others
        // we do. So this should not be blindly reused.
        if let ValueLoc::Reg(ru) = *value_loc {
            if !used.is_avail(GPR, ru) {
                used.free(GPR, ru);
            }
        }
    }

    // regmove and regfill instructions may temporarily divert values into other registers,
    // and these are not reflected in `func.locations`. Scan the function for such instructions
    // and note which callee-saved registers they use.
    //
    // TODO: Consider re-evaluating how regmove/regfill/regspill work and whether it's possible
    // to avoid this step.
    for ebb in &func.layout {
        for inst in func.layout.ebb_insts(ebb) {
            match func.dfg[inst] {
                ir::instructions::InstructionData::RegMove { dst, .. }
                | ir::instructions::InstructionData::RegFill { dst, .. } => {
                    if !used.is_avail(GPR, dst) {
                        used.free(GPR, dst);
                    }
                }
                _ => (),
            }
        }
    }

    used.intersect(&all_callee_saved);
    used
}

pub fn prologue_epilogue(func: &mut ir::Function, isa: &dyn TargetIsa) -> CodegenResult<()> {
    match func.signature.call_conv {
        // For now, just translate fast and cold as system_v.
        CallConv::Fast | CallConv::Cold | CallConv::SystemV => {
            system_v_prologue_epilogue(func, isa)
        }
        CallConv::WindowsFastcall => fastcall_prologue_epilogue(func, isa),
        CallConv::BaldrdashSystemV | CallConv::BaldrdashWindows => {
            baldrdash_prologue_epilogue(func, isa)
        }
        CallConv::Probestack => unimplemented!("probestack calling convention"),
    }
}

fn baldrdash_prologue_epilogue(func: &mut ir::Function, isa: &dyn TargetIsa) -> CodegenResult<()> {
    debug_assert!(
        !isa.flags().probestack_enabled(),
        "baldrdash does not expect cranelift to emit stack probes"
    );

    // Baldrdash on 32-bit x86 always aligns its stack pointer to 16 bytes.
    let stack_align = 16;
    let word_size = StackSize::from(isa.pointer_bytes());
    let shadow_store_size = if func.signature.call_conv.extends_windows_fastcall() {
        32
    } else {
        0
    };

    let bytes =
        StackSize::from(isa.flags().baldrdash_prologue_words()) * word_size + shadow_store_size;

    let mut ss = ir::StackSlotData::new(ir::StackSlotKind::IncomingArg, bytes);
    ss.offset = Some(-(bytes as StackOffset));
    func.stack_slots.push(ss);

    layout_stack(&mut func.stack_slots, stack_align)?;
    Ok(())
}

/// Implementation of the fastcall-based Win64 calling convention described at [1]
/// [1] https://docs.microsoft.com/en-us/cpp/build/x64-calling-convention
fn fastcall_prologue_epilogue(func: &mut ir::Function, isa: &dyn TargetIsa) -> CodegenResult<()> {
    if isa.triple().pointer_width().unwrap() != PointerWidth::U64 {
        panic!("TODO: windows-fastcall: x86-32 not implemented yet");
    }

    // [1] "The primary exceptions are the stack pointer and malloc or alloca memory,
    // which are aligned to 16 bytes in order to aid performance"
    let stack_align = 16;

    let word_size = isa.pointer_bytes() as usize;
    let reg_type = isa.pointer_type();

    let csrs = callee_saved_gprs_used(isa, func);

    // [1] "Space is allocated on the call stack as a shadow store for callees to save"
    // This shadow store contains the parameters which are passed through registers (ARG_GPRS)
    // and is eventually used by the callee to save & restore the values of the arguments.
    //
    // [2] https://blogs.msdn.microsoft.com/oldnewthing/20110302-00/?p=11333
    // "Although the x64 calling convention reserves spill space for parameters,
    //  you don’t have to use them as such"
    //
    // The reserved stack area is composed of:
    //   return address + frame pointer + all callee-saved registers + shadow space
    //
    // Pushing the return address is an implicit function of the `call`
    // instruction. Each of the others we will then push explicitly. Then we
    // will adjust the stack pointer to make room for the rest of the required
    // space for this frame.
    const SHADOW_STORE_SIZE: i32 = 32;
    let csr_stack_size = ((csrs.iter(GPR).len() + 2) * word_size) as i32;

    // TODO: eventually use the 32 bytes (shadow store) as spill slot. This currently doesn't work
    //       since cranelift does not support spill slots before incoming args

    func.create_stack_slot(ir::StackSlotData {
        kind: ir::StackSlotKind::IncomingArg,
        size: csr_stack_size as u32,
        offset: Some(-(SHADOW_STORE_SIZE + csr_stack_size)),
    });

    let total_stack_size = layout_stack(&mut func.stack_slots, stack_align)? as i32;
    let local_stack_size = i64::from(total_stack_size - csr_stack_size);

    // Add CSRs to function signature
    let fp_arg = ir::AbiParam::special_reg(
        reg_type,
        ir::ArgumentPurpose::FramePointer,
        RU::rbp as RegUnit,
    );
    func.signature.params.push(fp_arg);
    func.signature.returns.push(fp_arg);

    for csr in csrs.iter(GPR) {
        let csr_arg = ir::AbiParam::special_reg(reg_type, ir::ArgumentPurpose::CalleeSaved, csr);
        func.signature.params.push(csr_arg);
        func.signature.returns.push(csr_arg);
    }

    // Set up the cursor and insert the prologue
    let entry_ebb = func.layout.entry_block().expect("missing entry block");
    let mut pos = EncCursor::new(func, isa).at_first_insertion_point(entry_ebb);
    insert_common_prologue(&mut pos, local_stack_size, reg_type, &csrs, isa);

    // Reset the cursor and insert the epilogue
    let mut pos = pos.at_position(CursorPosition::Nowhere);
    insert_common_epilogues(&mut pos, local_stack_size, reg_type, &csrs);

    Ok(())
}

/// Insert a System V-compatible prologue and epilogue.
fn system_v_prologue_epilogue(func: &mut ir::Function, isa: &dyn TargetIsa) -> CodegenResult<()> {
    // The original 32-bit x86 ELF ABI had a 4-byte aligned stack pointer, but
    // newer versions use a 16-byte aligned stack pointer.
    let stack_align = 16;
    let pointer_width = isa.triple().pointer_width().unwrap();
    let word_size = pointer_width.bytes() as usize;
    let reg_type = ir::Type::int(u16::from(pointer_width.bits())).unwrap();

    let csrs = callee_saved_gprs_used(isa, func);

    // The reserved stack area is composed of:
    //   return address + frame pointer + all callee-saved registers
    //
    // Pushing the return address is an implicit function of the `call`
    // instruction. Each of the others we will then push explicitly. Then we
    // will adjust the stack pointer to make room for the rest of the required
    // space for this frame.
    let csr_stack_size = ((csrs.iter(GPR).len() + 2) * word_size) as i32;
    func.create_stack_slot(ir::StackSlotData {
        kind: ir::StackSlotKind::IncomingArg,
        size: csr_stack_size as u32,
        offset: Some(-csr_stack_size),
    });

    let total_stack_size = layout_stack(&mut func.stack_slots, stack_align)? as i32;
    let local_stack_size = i64::from(total_stack_size - csr_stack_size);

    // Add CSRs to function signature
    let fp_arg = ir::AbiParam::special_reg(
        reg_type,
        ir::ArgumentPurpose::FramePointer,
        RU::rbp as RegUnit,
    );
    func.signature.params.push(fp_arg);
    func.signature.returns.push(fp_arg);

    for csr in csrs.iter(GPR) {
        let csr_arg = ir::AbiParam::special_reg(reg_type, ir::ArgumentPurpose::CalleeSaved, csr);
        func.signature.params.push(csr_arg);
        func.signature.returns.push(csr_arg);
    }

    // Set up the cursor and insert the prologue
    let entry_ebb = func.layout.entry_block().expect("missing entry block");
    let mut pos = EncCursor::new(func, isa).at_first_insertion_point(entry_ebb);
    insert_common_prologue(&mut pos, local_stack_size, reg_type, &csrs, isa);

    // Reset the cursor and insert the epilogue
    let mut pos = pos.at_position(CursorPosition::Nowhere);
    insert_common_epilogues(&mut pos, local_stack_size, reg_type, &csrs);

    Ok(())
}

/// Insert the prologue for a given function.
/// This is used by common calling conventions such as System V.
fn insert_common_prologue(
    pos: &mut EncCursor,
    stack_size: i64,
    reg_type: ir::types::Type,
    csrs: &RegisterSet,
    isa: &dyn TargetIsa,
) {
    if stack_size > 0 {
        // Check if there is a special stack limit parameter. If so insert stack check.
        if let Some(stack_limit_arg) = pos.func.special_param(ArgumentPurpose::StackLimit) {
            // Total stack size is the size of all stack area used by the function, including
            // pushed CSRs, frame pointer.
            // Also, the size of a return address, implicitly pushed by a x86 `call` instruction,
            // also should be accounted for.
            // TODO: Check if the function body actually contains a `call` instruction.
            let word_size = isa.pointer_bytes();
            let total_stack_size = (csrs.iter(GPR).len() + 1 + 1) as i64 * word_size as i64;

            insert_stack_check(pos, total_stack_size, stack_limit_arg);
        }
    }

    // Append param to entry EBB
    let ebb = pos.current_ebb().expect("missing ebb under cursor");
    let fp = pos.func.dfg.append_ebb_param(ebb, reg_type);
    pos.func.locations[fp] = ir::ValueLoc::Reg(RU::rbp as RegUnit);

    pos.ins().x86_push(fp);
    pos.ins()
        .copy_special(RU::rsp as RegUnit, RU::rbp as RegUnit);

    for reg in csrs.iter(GPR) {
        // Append param to entry EBB
        let csr_arg = pos.func.dfg.append_ebb_param(ebb, reg_type);

        // Assign it a location
        pos.func.locations[csr_arg] = ir::ValueLoc::Reg(reg);

        // Remember it so we can push it momentarily
        pos.ins().x86_push(csr_arg);
    }

    // Allocate stack frame storage.
    if stack_size > 0 {
        if isa.flags().probestack_enabled()
            && stack_size > (1 << isa.flags().probestack_size_log2())
        {
            // Emit a stack probe.
            let rax = RU::rax as RegUnit;
            let rax_val = ir::ValueLoc::Reg(rax);

            // The probestack function expects its input in %rax.
            let arg = pos.ins().iconst(reg_type, stack_size);
            pos.func.locations[arg] = rax_val;

            // Call the probestack function.
            let callee = get_probestack_funcref(pos.func, reg_type, rax, isa);

            // Make the call.
            let call = if !isa.flags().is_pic()
                && isa.triple().pointer_width().unwrap() == PointerWidth::U64
                && !pos.func.dfg.ext_funcs[callee].colocated
            {
                // 64-bit non-PIC non-colocated calls need to be legalized to call_indirect.
                // Use r11 as it may be clobbered under all supported calling conventions.
                let r11 = RU::r11 as RegUnit;
                let sig = pos.func.dfg.ext_funcs[callee].signature;
                let addr = pos.ins().func_addr(reg_type, callee);
                pos.func.locations[addr] = ir::ValueLoc::Reg(r11);
                pos.ins().call_indirect(sig, addr, &[arg])
            } else {
                // Otherwise just do a normal call.
                pos.ins().call(callee, &[arg])
            };

            // If the probestack function doesn't adjust sp, do it ourselves.
            if !isa.flags().probestack_func_adjusts_sp() {
                let result = pos.func.dfg.inst_results(call)[0];
                pos.func.locations[result] = rax_val;
                pos.func.prologue_end = Some(pos.ins().adjust_sp_down(result));
            }
        } else {
            // Simply decrement the stack pointer.
            pos.func.prologue_end = Some(pos.ins().adjust_sp_down_imm(Imm64::new(stack_size)));
        }
    }
}

/// Insert a check that generates a trap if the stack pointer goes
/// below a value in `stack_limit_arg`.
fn insert_stack_check(pos: &mut EncCursor, stack_size: i64, stack_limit_arg: ir::Value) {
    use crate::ir::condcodes::IntCC;

    // Copy `stack_limit_arg` into a %rax and use it for calculating
    // a SP threshold.
    let stack_limit_copy = pos.ins().copy(stack_limit_arg);
    pos.func.locations[stack_limit_copy] = ir::ValueLoc::Reg(RU::rax as RegUnit);
    let sp_threshold = pos.ins().iadd_imm(stack_limit_copy, stack_size);
    pos.func.locations[sp_threshold] = ir::ValueLoc::Reg(RU::rax as RegUnit);

    // If the stack pointer currently reaches the SP threshold or below it then after opening
    // the current stack frame, the current stack pointer will reach the limit.
    let cflags = pos.ins().ifcmp_sp(sp_threshold);
    pos.func.locations[cflags] = ir::ValueLoc::Reg(RU::rflags as RegUnit);
    pos.ins().trapif(
        IntCC::UnsignedGreaterThanOrEqual,
        cflags,
        ir::TrapCode::StackOverflow,
    );
}

/// Find all `return` instructions and insert epilogues before them.
fn insert_common_epilogues(
    pos: &mut EncCursor,
    stack_size: i64,
    reg_type: ir::types::Type,
    csrs: &RegisterSet,
) {
    while let Some(ebb) = pos.next_ebb() {
        pos.goto_last_inst(ebb);
        if let Some(inst) = pos.current_inst() {
            if pos.func.dfg[inst].opcode().is_return() {
                insert_common_epilogue(inst, stack_size, pos, reg_type, csrs);
            }
        }
    }
}

/// Insert an epilogue given a specific `return` instruction.
/// This is used by common calling conventions such as System V.
fn insert_common_epilogue(
    inst: ir::Inst,
    stack_size: i64,
    pos: &mut EncCursor,
    reg_type: ir::types::Type,
    csrs: &RegisterSet,
) {
    if stack_size > 0 {
        pos.ins().adjust_sp_up_imm(Imm64::new(stack_size));
    }

    // Pop all the callee-saved registers, stepping backward each time to
    // preserve the correct order.
    let fp_ret = pos.ins().x86_pop(reg_type);
    pos.prev_inst();

    pos.func.locations[fp_ret] = ir::ValueLoc::Reg(RU::rbp as RegUnit);
    pos.func.dfg.append_inst_arg(inst, fp_ret);

    for reg in csrs.iter(GPR) {
        let csr_ret = pos.ins().x86_pop(reg_type);
        pos.prev_inst();

        pos.func.locations[csr_ret] = ir::ValueLoc::Reg(reg);
        pos.func.dfg.append_inst_arg(inst, csr_ret);
    }
}

pub fn emit_unwind_info(func: &ir::Function, isa: &dyn TargetIsa, mem: &mut Vec<u8>) {
    // Assumption: RBP is being used as the frame pointer
    // In the future, Windows fastcall codegen should usually omit the frame pointer
    if let Some(info) = UnwindInfo::try_from_func(func, isa, Some(RU::rbp.into())) {
        info.emit(mem).expect("failed to emit unwind information");
    }
}
