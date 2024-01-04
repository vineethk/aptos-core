// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

//! Implements the "definitely available copies" analysis, also called "available copies" analysis (in short).
//! This analysis is a prerequisite for the copy propagation transformation.
//!
//! A copy is of the form `a := b` (i.e., `a` is assigned `b`), where `a` and `b` are locals/temporaries.
//!
//! A definitely available copy at a given program point `P` is a copy `a := b` that has reached `P`
//! along all possible program paths such that neither `a` nor `b` is overwritten along any of these paths.
//! That is, `a` and `b` are always available unmodified at `P` after the copy `a := b`,
//! making it definitely available.
//!
//! This is a forward "must" analysis.
//! In a forward analysis, we reason about facts at a program point `P` using facts at its predecessors.
//! The "must" qualifier means that the analysis facts must be true at `P`,
//! irrespective of what path lead to `P`.

use itertools::Itertools;
use move_binary_format::file_format::CodeOffset;
use move_model::{ast::TempIndex, model::FunctionEnv};
use move_stackless_bytecode::{
    dataflow_analysis::{DataflowAnalysis, TransferFunctions},
    dataflow_domains::{AbstractDomain, JoinResult},
    function_target::{FunctionData, FunctionTarget},
    function_target_pipeline::{FunctionTargetProcessor, FunctionTargetsHolder},
    stackless_bytecode::{AbortAction, Bytecode, Operation},
    stackless_control_flow_graph::StacklessControlFlowGraph,
};
use std::collections::{BTreeMap, BTreeSet};

/// Collection of definitely available copies.
/// For a copy `a := b`, we store the key-value pair `(a, b)` in the internal map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailCopies(BTreeMap</*dst*/ TempIndex, /*src*/ TempIndex>);

impl AvailCopies {
    /// Create a new (empty) collection of definitely available copies.
    fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Make a copy `dst := src` available.
    /// If either `dst` or `src` is in `borrowed_locals`, then the copy is not made available.
    /// To call this method, `dst := x` should not already be available for any `x`.
    fn make_copy_available(
        &mut self,
        dst: TempIndex,
        src: TempIndex,
        borrowed_locals: &BTreeSet<TempIndex>,
    ) {
        // Note that we are conservative here for the sake of simplicity, and disallow copies
        // when either `dst` or `src` is borrowed. We could track more copies as available by using
        // reference analysis.
        if !borrowed_locals.contains(&dst) && !borrowed_locals.contains(&src) {
            let old_src = self.0.insert(dst, src);
            if let Some(old_src) = old_src {
                panic!(
                    "ICE: copy `$t{} = $t{}` already available, \
                        cannot have `$t{} = $t{}` available as well",
                    dst, old_src, dst, src
                );
            }
        }
    }

    /// Kill all available copies of the form `x := y` where `x` or `y` is `tmp`.
    /// If `tmp` is in `borrowed_locals`, then no copies are killed.
    fn kill_copies_with(&mut self, tmp: TempIndex, borrowed_locals: &BTreeSet<TempIndex>) {
        if !borrowed_locals.contains(&tmp) {
            // TODO: consider optimizing the following operation by keeping a two-way map between
            // `dst -> src` and `src -> set(dst)`. Another optimization to consider is to use im::OrdMap.
            self.0.retain(|dst, src| *dst != tmp && *src != tmp);
        }
    }

    /// Given a set of available copies: `tmp_1 := tmp_0, tmp_2 := tmp_1,..., tmp_n := tmp_n-1`, forming
    /// the copy chain: `tmp_0 --copied-to--> tmp_1 --copied-to--> tmp_2 -> ... -> tmp_n-1 -> tmp_n`,
    /// return the head of the copy chain `tmp_0` for any input `tmp_x` (x in 0..=n) in the chain.
    ///
    /// Note that it is required that the copy chain is acyclic: we don't check for this explicitly,
    /// but the natural way of constructing the copy chain for move bytecode ensures this.
    pub fn get_head_of_copy_chain(&self, mut tmp: TempIndex) -> TempIndex {
        while let Some(src) = self.0.get(&tmp) {
            tmp = *src;
        }
        tmp
    }
}

impl Default for AvailCopies {
    /// Create a default (empty) collection of definitely available copies.
    fn default() -> Self {
        Self::new()
    }
}

impl AbstractDomain for AvailCopies {
    /// Keep only those copies in `self` that are available in both `self` and `other`.
    /// Report if `self` has changed.
    fn join(&mut self, other: &Self) -> JoinResult {
        let mut result = JoinResult::Unchanged;
        let prev_copies = std::mem::take(&mut self.0);
        for (dst_, src_) in &other.0 {
            if let Some(src) = prev_copies.get(dst_) {
                if src != src_ {
                    // We are removing the available copy (dst, src) from self.
                    result = JoinResult::Changed;
                } else {
                    self.0.insert(*dst_, *src_);
                }
            }
            // else: a copy of the form (dst_, _) is not already available in self, so no change to self.
        }
        if prev_copies.len() != self.0.len() {
            // We have removed some copies from self.
            result = JoinResult::Changed;
        }
        result
    }
}

/// Definitely available copies before and after a stackless bytecode instruction.
#[derive(Clone)]
struct AvailCopiesState {
    before: AvailCopies,
    after: AvailCopies,
}

/// Mapping from code offsets to definitely available copies before and after the instruction at the code offset.
#[derive(Clone)]
pub struct AvailCopiesAnnotation(BTreeMap<CodeOffset, AvailCopiesState>);

impl AvailCopiesAnnotation {
    /// Get the definitely available copies before the instruction at the given `code_offset`.
    pub fn before(&self, code_offset: &CodeOffset) -> Option<&AvailCopies> {
        if let Some(state) = self.0.get(code_offset) {
            Some(&state.before)
        } else {
            None
        }
    }
}

/// The definitely available copies analysis for a function.
pub struct AvailCopiesAnalysis {
    borrowed_locals: BTreeSet<TempIndex>, // Locals borrowed in the function being analyzed.
}

impl AvailCopiesAnalysis {
    /// Create a new instance of definitely available copies analysis.
    /// `code` is the bytecode of the function being analyzed.
    pub fn new(code: &[Bytecode]) -> Self {
        Self {
            borrowed_locals: Self::get_borrowed_locals(code),
        }
    }

    /// Analyze the given function and return the definitely available copies annotation.
    fn analyze(&self, func_target: &FunctionTarget) -> AvailCopiesAnnotation {
        let code = func_target.get_bytecode();
        let cfg = StacklessControlFlowGraph::new_forward(code);
        let block_state_map = self.analyze_function(AvailCopies::new(), code, &cfg);
        let per_bytecode_state =
            self.state_per_instruction(block_state_map, code, &cfg, |before, after| {
                AvailCopiesState {
                    before: before.clone(),
                    after: after.clone(),
                }
            });
        AvailCopiesAnnotation(per_bytecode_state)
    }

    /// Get the set of locals that have been borrowed in the function being analyzed.
    fn get_borrowed_locals(code: &[Bytecode]) -> BTreeSet<TempIndex> {
        code.iter()
            .filter_map(|bc| {
                if let Bytecode::Call(_, _, Operation::BorrowLoc, srcs, _) = bc {
                    // BorrowLoc should have only one source.
                    srcs.first().cloned()
                } else {
                    None
                }
            })
            .collect()
    }
}

impl TransferFunctions for AvailCopiesAnalysis {
    type State = AvailCopies;

    // This is a forward analysis.
    const BACKWARD: bool = false;

    fn execute(&self, state: &mut Self::State, instr: &Bytecode, _offset: CodeOffset) {
        use Bytecode::*;
        match instr {
            Assign(_, dst, src, _) => {
                state.kill_copies_with(*dst, &self.borrowed_locals);
                state.make_copy_available(*dst, *src, &self.borrowed_locals);
            },
            Load(_, dst, _) => {
                state.kill_copies_with(*dst, &self.borrowed_locals);
            },
            Call(_, dsts, _, _, on_abort) => {
                for dst in dsts {
                    state.kill_copies_with(*dst, &self.borrowed_locals);
                }
                if let Some(AbortAction(_, dst)) = on_abort {
                    state.kill_copies_with(*dst, &self.borrowed_locals);
                }
            },
            _ => (),
        }
    }
}

impl DataflowAnalysis for AvailCopiesAnalysis {}

/// Processor for the definitely available copies analysis.
pub struct AvailCopiesAnalysisProcessor();

impl FunctionTargetProcessor for AvailCopiesAnalysisProcessor {
    fn process(
        &self,
        _targets: &mut FunctionTargetsHolder,
        func_env: &FunctionEnv,
        mut data: FunctionData,
        _scc_opt: Option<&[FunctionEnv]>,
    ) -> FunctionData {
        if func_env.is_native() {
            return data;
        }
        let target = FunctionTarget::new(func_env, &data);
        let analysis = AvailCopiesAnalysis::new(target.get_bytecode());
        let annotation = analysis.analyze(&target);
        data.annotations.set(annotation, true);
        data
    }

    fn name(&self) -> String {
        "available_copies".to_string()
    }
}

impl AvailCopiesAnalysisProcessor {
    /// Registers annotation formatter at the given function target.
    /// Helps with testing and debugging.
    pub fn register_formatters(target: &FunctionTarget) {
        target.register_annotation_formatter(Box::new(format_avail_copies_annotation));
    }
}

// ====================================================================
// Formatting functionality for available copies annotation.

pub fn format_avail_copies_annotation(
    target: &FunctionTarget<'_>,
    code_offset: CodeOffset,
) -> Option<String> {
    let AvailCopiesAnnotation(map) = target.get_annotations().get::<AvailCopiesAnnotation>()?;
    let AvailCopiesState { before, after } = map.get(&code_offset)?;
    let mut s = String::new();
    s.push_str("before: ");
    s.push_str(&format_avail_copies(before, target));
    s.push_str(", after: ");
    s.push_str(&format_avail_copies(after, target));
    Some(s)
}

fn format_avail_copies(state: &AvailCopies, target: &FunctionTarget<'_>) -> String {
    let mut s = String::new();
    s.push_str("{");
    let mut first = true;
    for (dst, src) in &state.0 {
        if first {
            first = false;
        } else {
            s.push_str(", ");
        }
        s.push_str(
            &vec![dst, src]
                .into_iter()
                .map(|tmp| {
                    let name = target.get_local_raw_name(*tmp);
                    name.display(target.symbol_pool()).to_string()
                })
                .join(" := "),
        );
    }
    s.push_str("}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_avail_copies_join() {
        let mut a = AvailCopies::new();
        let mut b = AvailCopies::new();
        let borrowed_locals: BTreeSet<TempIndex> = BTreeSet::new();
        a.make_copy_available(1, 2, &borrowed_locals);
        b.make_copy_available(3, 4, &borrowed_locals);
        // a = (1, 2), b = (3, 4)
        assert_eq!(a.join(&b), JoinResult::Changed);
        assert_eq!(a.0.len(), 0);
        a.make_copy_available(3, 4, &borrowed_locals);
        // a = (3, 4), b = (3, 4)
        assert_eq!(a.join(&b), JoinResult::Unchanged);
        assert_eq!(a, b);
        a.make_copy_available(1, 2, &borrowed_locals);
        // a = (1, 2), (3, 4), b = (3, 4)
        assert_eq!(a.join(&b), JoinResult::Changed);
        assert_eq!(a, b);
        b.make_copy_available(1, 2, &borrowed_locals);
        // a = (3, 4), b = (1, 2), (3, 4)
        assert_eq!(a.join(&b), JoinResult::Unchanged);
        assert_eq!(a.0.len(), 1);
    }

    #[test]
    fn test_get_head_of_copy_chain() {
        let mut copies = AvailCopies::new();
        let borrowed_locals: BTreeSet<TempIndex> = BTreeSet::new();
        copies.make_copy_available(1, 0, &borrowed_locals);
        copies.make_copy_available(2, 1, &borrowed_locals);
        copies.make_copy_available(3, 2, &borrowed_locals);
        copies.make_copy_available(4, 3, &borrowed_locals);
        copies.make_copy_available(44, 14, &borrowed_locals);
        // copies = (1, 0), (2, 1), (3, 2), (4, 3), (44, 14)
        for i in 0..=4 {
            assert_eq!(copies.get_head_of_copy_chain(i), 0);
        }
    }
}
