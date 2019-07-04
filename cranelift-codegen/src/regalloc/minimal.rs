//! Minimal register allocator.
//!
//! The `minimal` register allocator assigns every Value in the incoming program to a unique stack
//! slot, then moves values into registers only as required by each instruction, and finally moves
//! any values defined by the instruction out of registers directly after the instruction.
//!
//! The values that are in registers are new Value slots, and the instructions are updated to take
//! these new Values as arguments and produce them as results.  Value movement is through fill and
//! spill instructions.
//!
//! The allocator must handle the function ABI and two-address operations (tied registers) and must
//! obey all instruction constraints (eg fixed registers and register classes), but is otherwise the
//! simplest register allocator imaginable for our given IR structure.

use std::vec::Vec;

use crate::cursor::{Cursor, EncCursor};
use crate::dominator_tree::DominatorTree;
use crate::flowgraph::ControlFlowGraph;
use crate::ir::{
    ArgumentLoc, Ebb, Function, Inst, InstBuilder, InstructionData, Opcode, 
    Value, ValueLoc,
};
use crate::isa::registers::{RegClass, RegUnit};
use crate::isa::{ConstraintKind, EncInfo, TargetIsa};
use crate::regalloc::live_value_tracker::LiveValueTracker;
use crate::regalloc::liveness::Liveness;
use crate::regalloc::register_set::RegisterSet;
use crate::topo_order::TopoOrder;

/// Register allocator state.
pub struct Minimal {}

impl Minimal {
    /// Create a new register allocator state.
    pub fn new() -> Self {
        Self {}
    }

    /// Clear the state of the allocator.
    pub fn clear(&mut self) {}

    /// Run register allocation.
    pub fn run(
        &mut self,
        isa: &TargetIsa,
        func: &mut Function,
        cfg: &mut ControlFlowGraph,
        domtree: &mut DominatorTree,
        _liveness: &mut Liveness,
        topo: &mut TopoOrder,
        _tracker: &mut LiveValueTracker,
    ) {
        let mut ctx = Context {
            new_blocks: false,
            usable_regs: isa.allocatable_registers(func),
            cur: EncCursor::new(func, isa),
            encinfo: isa.encoding_info(),
            domtree,
            topo,
            cfg,
        };
        ctx.run()
    }
}

struct Regs {
    registers: RegisterSet,
}

impl Regs {
    fn new(registers: RegisterSet) -> Self {
        Self { registers }
    }

    fn take_specific(&mut self, rc: RegClass, r: RegUnit) {
        self.registers.take(rc, r);
    }

    fn take(&mut self, rc: RegClass) -> Option<RegUnit> {
        // FIXME: This is probably quite slow.
        let mut i = self.registers.iter(rc);
        let r = i.next();
        if r.is_some() {
            self.registers.take(rc, r.unwrap());
        }
        r
    }

    fn free(&mut self, rc: RegClass, r: RegUnit) {
        self.registers.free(rc, r);
    }
}

struct Context<'a> {
    // True if new blocks were inserted
    new_blocks: bool,
        
    // Set of registers that the allocator can use.
    usable_regs: RegisterSet,

    // Current instruction as well as reference to function and ISA.
    cur: EncCursor<'a>,

    // Cached ISA information.
    // We save it here to avoid frequent virtual function calls on the `TargetIsa` trait object.
    encinfo: EncInfo,

    // References to contextual data structures we need.
    domtree: &'a mut DominatorTree,
    topo: &'a mut TopoOrder,
    cfg: &'a mut ControlFlowGraph,
}

impl<'a> Context<'a> {
    fn run(&mut self) {
        dbg!(&self.cur.func);

        // For the entry block, spill register parameters to the stack while retaining their names.
        self.visit_entry_block(self.cur.func.layout.entry_block().unwrap());

        // For all blocks other than the entry block, assign stack slots to all block parameters so
        // that we can later process control transfer instructions.
        self.visit_other_blocks();

        // Process all instructions in domtree order so that we'll always know the location of a
        // definition when we see its use.  Fill any register args before the instruction and spill
        // any definitions after.
        let mut regs = Regs::new(self.usable_regs.clone());
        self.topo.reset(self.cur.func.layout.ebbs());
        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            self.cur.goto_top(ebb);
            while let Some(inst) = self.cur.next_inst() {
                if !self.cur.func.dfg[inst].opcode().is_ghost() {
                    self.visit_inst(inst, &mut regs);
                }
            }
        }

        // If blocks were added the cfg and domtree are inconsistent and must be recomputed.
        if self.new_blocks {
            self.cfg.compute(&self.cur.func);
            self.domtree.compute(&self.cur.func, self.cfg);
        }

        dbg!(&self.cur.func);
        dbg!(&self.cur.func.locations);
    }

    fn visit_entry_block(&mut self, entry: Ebb) {
        let signature_info: Vec<_> = self
            .cur
            .func
            .dfg
            .ebb_params(entry)
            .iter()
            .zip(&self.cur.func.signature.params)
            .map(|(param, abi)| (*param, *abi))
            .collect();

        self.cur.goto_first_inst(entry);
        for (param, abi) in signature_info {
            match abi.location {
                ArgumentLoc::Reg(reg) => {
                    let new_param = self.cur.func.dfg.replace_ebb_param(param, abi.value_type);
                    self.cur.func.locations[new_param] = ValueLoc::Reg(reg);
                    self.cur.ins().with_result(param).spill(new_param);

                    let ss = self.cur.func.stack_slots.make_spill_slot(abi.value_type);
                    self.cur.func.locations[param] = ValueLoc::Stack(ss);
                }
                ArgumentLoc::Stack(_offset) => {
                    // Incoming stack arguments have sensible pre-initialized locations.
                    debug_assert!(
                        if let ValueLoc::Stack(_ss) = self.cur.func.locations[param] {
                            true
                        } else {
                            false
                        }
                    );
                }
                ArgumentLoc::Unassigned => {
                    panic!("Should not happen");
                }
            }
        }
    }

    fn visit_other_blocks(&mut self) {
        let entry = self.cur.func.layout.entry_block().unwrap();
        self.topo.reset(self.cur.func.layout.ebbs());

        // Skip the entry block.
        let first = self.topo.next(&self.cur.func.layout, self.domtree).unwrap();
        debug_assert!(first == entry);

        while let Some(ebb) = self.topo.next(&self.cur.func.layout, self.domtree) {
            for param in self.cur.func.dfg.ebb_params(ebb) {
                let ss = self
                    .cur
                    .func
                    .stack_slots
                    .make_spill_slot(self.cur.func.dfg.value_type(*param));
                self.cur.func.locations[*param] = ValueLoc::Stack(ss);
            }
        }
    }

    fn visit_inst(&mut self, inst: Inst, regs: &mut Regs) {
        let opcode = self.cur.func.dfg[inst].opcode();

        // TODO: Fallthrough will make our ebb-insertion mechanism in visit_branch() not work.  We
        // really should not have plain Fallthrough in the code at this point, though I don't think
        // anything stops them from being there.  The "right" fix is to pick a block B that the new
        // block is to be inserted after, and if B ends with a fallthrough rewrite it as a jump.
        // This is not hard.
        debug_assert!(opcode != Opcode::Fallthrough);

        if opcode == Opcode::Copy {
            self.visit_copy(inst, regs, opcode);
        } else if opcode.is_branch() {
            self.visit_branch(inst, regs, opcode);
        } else if opcode.is_terminator() {
            self.visit_terminator(inst, regs, opcode);
        } else if opcode.is_call() {
            self.visit_call(inst, regs, opcode);
        } else if opcode == Opcode::Spill || opcode == Opcode::Fill {
            // Inserted by the register allocator; ignore them.
        } else {
            // Some opcodes should not be encountered here.
            debug_assert!(opcode != Opcode::Regmove && opcode != Opcode::Regfill && opcode != Opcode::Regspill && opcode != Opcode::CopySpecial);
            self.visit_plain_inst(inst, regs, opcode);
        }
    }

    fn visit_copy(&mut self, inst: Inst, _regs: &mut Regs, _opcode: Opcode) {
        // As the stack slots are immutable, a copy is simply a sharing of location.
        let arg = *self.cur.func.dfg.inst_args(inst).get(0).unwrap();
        let dest = *self.cur.func.dfg.inst_results(inst).get(0).unwrap();
        self.cur.func.locations[dest] = self.cur.func.locations[arg];
    }

    fn visit_branch(&mut self, inst: Inst, regs: &mut Regs, opcode: Opcode) {
        let (target_info, has_argument) = self.classify_branch(inst, opcode);
        if let Some((target, side_exit)) = target_info {
            // Insert the fill/spill along the taken edge only.  May have to create a new block to
            // hold the fill/spill instructions.

            let mut inst = inst;
            let mut orig_inst = inst;

            let new_block = side_exit && self.cur.func.dfg.ebb_params(target).len() > 0;
            if new_block {
                // Remember the arguments to the side exit.
                let jump_args: Vec<Value> = self.cur.func.dfg.inst_variable_args(inst).iter().map(|x| *x).collect();

                // Create the block the side exit will jump to.
                let new_ebb = self.make_empty_ebb();

                // Remove the arguments from the side exit and make it jump to the new block.
                self.rewrite_side_exit(inst, opcode, new_ebb);

                if has_argument {
                    self.visit_plain_inst(inst, regs, opcode);
                    orig_inst = self.cur.current_inst().unwrap();
                }

                // Insert a jump to the original target with the original arguments into the new
                // block.
                self.cur.goto_first_insertion_point(new_ebb);
                self.cur.ins().jump(target, jump_args.as_slice());

                // Make the fill/spill code below target the jump instruction in the new block,
                // otherwise it won't be visited, as it is not in the current topo order.
                self.cur.goto_first_inst(new_ebb);
                inst = self.cur.current_inst().unwrap();
            } else if has_argument {
                self.visit_plain_inst(inst, regs, opcode);
            }

            let arginfo: Vec<_> = self
                .cur
                .func
                .dfg
                .ebb_params(target)
                .iter()
                .zip(self.cur.func.dfg.inst_args(inst).iter())
                .map(|(a, b)| (*b, *a))
                .enumerate()
                .collect();

            for (k, (arg, target_arg)) in arginfo {
                let temp = self.cur.ins().fill(arg);
                let dest = self.cur.ins().spill(temp);
                let spill = self.cur.built_inst();
                let enc = self.cur.func.encodings[spill];
                let constraints = self.encinfo.operand_constraints(enc).unwrap();
                let rc = constraints.ins[0].regclass;
                let reg = regs.take(rc).unwrap();
                self.cur.func.locations[temp] = ValueLoc::Reg(reg);
                self.cur.func.locations[dest] = self.cur.func.locations[target_arg];
                self.cur.func.dfg.inst_args_mut(inst)[k] = dest;
                regs.free(rc, reg);
            }

            // Restore the point, so that the iteration will work correctly.
            if new_block {
                self.cur.goto_inst(orig_inst);
            }
        }
    }

    fn visit_terminator(&mut self, inst: Inst, _regs: &mut Regs, opcode: Opcode) {
        // Some terminators are handled as branches and should not be seen here; others are illegal.
        match opcode {
            Opcode::Return | Opcode::FallthroughReturn => {
                let return_info: Vec<_> = self
                    .cur
                    .func
                    .dfg
                    .inst_args(inst)
                    .iter()
                    .zip(&self.cur.func.signature.returns)
                    .map(|(val, abi)| (*val, *abi))
                    .enumerate()
                    .collect();

                for (k, (val, abi)) in return_info {
                    let temp = self.cur.ins().fill(val);
                    match abi.location {
                        ArgumentLoc::Reg(r) => {
                            self.cur.func.locations[temp] = ValueLoc::Reg(r);
                            self.cur.func.dfg.inst_args_mut(inst)[k] = temp;
                        }
                        _ => panic!("Only register returns"),
                    }
                }
            }
            Opcode::Trap => {}
            _ => unreachable!(),
        }
    }

    fn visit_call(&mut self, _inst: Inst, _regs: &mut Regs, _opcode: Opcode) {
        // TODO: Implement this
        // Have to set up outgoing parameters according to ABI
        panic!("Calls not yet implemented");
    }

    fn visit_plain_inst(&mut self, inst: Inst, regs: &mut Regs, _opcode: Opcode) {
        let constraints = self.encinfo.operand_constraints(self.cur.func.encodings[inst]);

        // Reserve any fixed input registers.
        if let Some(constraints) = constraints {
            if constraints.fixed_ins {
                for constraint in constraints.ins {
                    match constraint.kind {
                        ConstraintKind::FixedReg(r) => regs.take_specific(constraint.regclass, r),
                        ConstraintKind::FixedTied(r) => regs.take_specific(constraint.regclass, r),
                        _ => {}
                    }
                }
            }
        }

        // Assign all input registers.
        let mut reg_args = vec![];
        for (k, arg) in self.cur.func.dfg.inst_args(inst).iter().enumerate() {
            debug_assert!(
                if let ValueLoc::Stack(_ss) = self.cur.func.locations[*arg] {
                    true
                } else {
                    self.cur.func.dfg.value_type(*arg).is_flags()
                }
            );
            let constraint = &constraints.unwrap().ins[k];
            if constraint.kind == ConstraintKind::Stack {
                continue;
            }
            let rc = constraint.regclass;
            let (reg, is_tied) = match constraint.kind {
                ConstraintKind::FixedReg(r) => (r, false),
                ConstraintKind::FixedTied(r) => (r, true),
                ConstraintKind::Tied(_) => (regs.take(rc).unwrap(), true),
                ConstraintKind::Reg => (regs.take(rc).unwrap(), false),
                ConstraintKind::Stack => unreachable!(),
            };
            reg_args.push((k, *arg, rc, reg, is_tied));
        }

        // Insert fills, assign locations, update the instruction, free registers.
        for (k, arg, rc, reg, is_tied) in &reg_args {
            let value_type = self.cur.func.dfg.value_type(*arg);
            if value_type.is_flags() {
                self.cur.func.locations[*arg] = ValueLoc::Reg(*reg);
            } else {
                let temp = self.cur.ins().fill(*arg);
                self.cur.func.locations[temp] = ValueLoc::Reg(*reg);
                self.cur.func.dfg.inst_args_mut(inst)[*k] = temp;
            }
            if !*is_tied {
                regs.free(*rc, *reg);
            }
        }

        // Reserve any fixed output registers that are not also tied.
        if let Some(constraints) = constraints {
            if constraints.fixed_outs {
                for constraint in constraints.outs {
                    match constraint.kind {
                        ConstraintKind::FixedReg(r) => regs.take_specific(constraint.regclass, r),
                        _ => {}
                    }
                }
            }
        }

        // Assign the output registers.
        let mut reg_results = vec![];
        for (k, result) in self.cur.func.dfg.inst_results(inst).iter().enumerate() {
            let constraint = &constraints.unwrap().outs[k];
            debug_assert!(constraint.kind != ConstraintKind::Stack);
            let (rc, reg) = match constraint.kind {
                ConstraintKind::FixedTied(r) => (constraint.regclass, r),
                ConstraintKind::FixedReg(r) => (constraint.regclass, r),
                ConstraintKind::Tied(input) => {
                    let hit = *reg_args
                        .iter()
                        .filter(|(input_k, ..)| *input_k == input as usize)
                        .next()
                        .unwrap();
                    debug_assert!(hit.4);
                    (hit.2, hit.3)
                }
                ConstraintKind::Reg => {
                    (constraint.regclass, regs.take(constraint.regclass).unwrap())
                }
                ConstraintKind::Stack => unreachable!(),
            };
            reg_results.push((k, *result, rc, reg));
        }

        // Insert spills, assign locations, update the instruction, free registers.
        let mut last = inst;
        self.cur.goto_after_inst(inst);
        for (_k, result, rc, reg) in reg_results {
            let value_type = self.cur.func.dfg.value_type(result);
            if value_type.is_flags() {
                self.cur.func.locations[result] = ValueLoc::Reg(reg);
            } else {
                let new_result = self.cur.func.dfg.replace_result(result, value_type);
                self.cur.func.locations[new_result] = ValueLoc::Reg(reg);

                self.cur.ins().with_result(result).spill(new_result);
                let spill = self.cur.built_inst();
                let ss = self.cur.func.stack_slots.make_spill_slot(value_type);
                self.cur.func.locations[result] = ValueLoc::Stack(ss);

                last = spill;
            }

            regs.free(rc, reg);
        }
        self.cur.goto_inst(last);
    }

    // Returns (Option<(target_ebb, side_exit)>, has_argument)
    fn classify_branch(&self, inst:Inst, opcode:Opcode) -> (Option<(Ebb, bool)>, bool) {
        match self.cur.func.dfg[inst] {
            InstructionData::IndirectJump { .. } => {
                debug_assert!(opcode == Opcode::IndirectJumpTableBr);
                (None, true) 
            }
            InstructionData::Jump { destination, .. } => {
                debug_assert!(opcode == Opcode::Jump || opcode == Opcode::Fallthrough);
                (Some((destination, false)), false)
            }
            InstructionData::Branch { destination, .. } => {
                debug_assert!(opcode == Opcode::Brz || opcode == Opcode::Brnz);
                (Some((destination, true)), true)
            }
            InstructionData::BranchIcmp { destination, .. } => {
                debug_assert!(opcode == Opcode::BrIcmp);
                (Some((destination, true)), true)
            }
            InstructionData::BranchInt { destination, .. } => {
                debug_assert!(opcode == Opcode::Brif);
                (Some((destination, true)), true)
            }
            InstructionData::BranchFloat { destination, .. } => {
                debug_assert!(opcode == Opcode::Brff);
                (Some((destination, true)), true)
            }
            _ => {
                panic!("Unexpected instruction in classify_branch");
            }
        }
    }

    // Make `inst`, which must be a side exit branch with operation `opcode`, jump to `new_ebb`
    // without any arguments.
    fn rewrite_side_exit(&mut self, inst: Inst, opcode: Opcode, new_ebb: Ebb) {
        match opcode {
            Opcode::Brz => {
                let val = *self.cur.func.dfg.inst_args(inst).get(0).unwrap();
                self.cur.func.dfg.replace(inst).brz(val, new_ebb, &[]);
            }
            Opcode::Brnz => {
                let val = *self.cur.func.dfg.inst_args(inst).get(0).unwrap();
                self.cur.func.dfg.replace(inst).brnz(val, new_ebb, &[]);
            }
            Opcode::BrIcmp => {
                if let InstructionData::BranchIcmp { cond, .. } = self.cur.func.dfg[inst] {
                    let x = *self.cur.func.dfg.inst_args(inst).get(0).unwrap();
                    let y = *self.cur.func.dfg.inst_args(inst).get(1).unwrap();
                    self.cur.func.dfg.replace(inst).br_icmp(cond, x, y, new_ebb, &[]);
                }
            }
            Opcode::Brif => {
                if let InstructionData::BranchInt { cond, .. } = self.cur.func.dfg[inst] {
                    let val = *self.cur.func.dfg.inst_args(inst).get(0).unwrap();
                    self.cur.func.dfg.replace(inst).brif(cond, val, new_ebb, &[]);
                }
            }
            Opcode::Brff => {
                if let InstructionData::BranchFloat { cond, .. } = self.cur.func.dfg[inst] {
                    let val = *self.cur.func.dfg.inst_args(inst).get(0).unwrap();
                    self.cur.func.dfg.replace(inst).brff(cond, val, new_ebb, &[]);
                }
            }
            _ => {
                panic!("Unhandled side exit type");
            }
        }
        let ok = self.cur.func.update_encoding(inst, self.cur.isa).is_ok();
        debug_assert!(ok);
    }

    // For now, a new ebb must be inserted before the last ebb because the last ebb may have a
    // fallthrough_return and can't have anything after it.  TODO: This trick only works if there
    // are no Fallthrough instructions in the block graph.
    fn make_empty_ebb(&mut self) -> Ebb {
        let new_ebb = self.cur.func.dfg.make_ebb();
        let last_ebb = self.cur.layout().last_ebb().unwrap();
        self.cur.layout_mut().insert_ebb(new_ebb, last_ebb);
        self.new_blocks = true;
        new_ebb
    }
}
