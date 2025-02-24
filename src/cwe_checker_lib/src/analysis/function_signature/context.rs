use crate::abstract_domain::{AbstractDomain, AbstractIdentifier, BitvectorDomain, DataDomain};
use crate::{
    analysis::{forward_interprocedural_fixpoint, graph::Graph},
    intermediate_representation::Project,
};

use super::*;

/// The context struct for the fixpoint algorithm.
pub struct Context<'a> {
    graph: &'a Graph<'a>,
    project: &'a Project,
}

impl<'a> Context<'a> {
    /// Generate a new context object.
    pub fn new(project: &'a Project, graph: &'a Graph<'a>) -> Self {
        Context { graph, project }
    }

    /// Compute the return values of a call and return them (without adding them to the caller state).
    ///
    /// The `callee_state` is the state of the callee at the return site.
    /// The return values are expressed in the abstract IDs that are known to the caller.
    /// If a return value may contain `Top` values,
    /// i.e. values for which the origin is not known or not expressible in the abstract IDs known to the caller,
    /// then a call- and register-specific abstract ID is added to the corresponding return value.
    /// This ID is not added to the tracked IDs of the caller state.
    fn compute_return_values_of_call<'cconv>(
        &self,
        caller_state: &mut State,
        callee_state: &State,
        calling_convention: &'cconv CallingConvention,
        call: &Term<Jmp>,
    ) -> Vec<(&'cconv Variable, DataDomain<BitvectorDomain>)> {
        let mut return_value_list = Vec::new();
        for return_register in &calling_convention.integer_return_register {
            let return_value = self.compute_return_register_value_of_call(
                caller_state,
                callee_state,
                return_register,
                call,
            );
            return_value_list.push((return_register, return_value));
        }
        for return_expr in &calling_convention.float_return_register {
            for return_register in return_expr.input_vars() {
                let return_value = self.compute_return_register_value_of_call(
                    caller_state,
                    callee_state,
                    return_register,
                    call,
                );
                return_value_list.push((return_register, return_value));
            }
        }
        return_value_list
    }

    /// Compute the return value for the given register.
    ///
    /// The return value contains the IDs of all possible input IDs of the call that it may reference.
    /// If the value may also contain a value not originating from the caller
    /// then replace it with a call- and register-specific abstract ID.
    fn compute_return_register_value_of_call(
        &self,
        caller_state: &mut State,
        callee_state: &State,
        return_register: &Variable,
        call: &Term<Jmp>,
    ) -> DataDomain<BitvectorDomain> {
        let callee_value = callee_state.get_register(return_register);
        let mut return_value: DataDomain<BitvectorDomain> =
            DataDomain::new_empty(return_register.size);
        // For absolute or Top-values originating in the callee the Top-flag of the return value is set.
        if callee_value.contains_top() || callee_value.get_absolute_value().is_some() {
            return_value.set_contains_top_flag();
        }
        // For every relative value in the callee we check whether it is relative a parameter to the callee.
        // If yes, we can compute it relative to the value of the parameter at the callsite and add the result to the return value.
        // Else we just set the Top-flag of the return value to indicate some value originating in the callee.
        for (callee_id, callee_offset) in callee_value.get_relative_values() {
            if let Some(param_arg) = callee_state.get_arg_corresponding_to_id(callee_id) {
                let param_value = caller_state.eval_parameter_arg(&param_arg);
                if param_value.contains_top() || param_value.get_absolute_value().is_some() {
                    return_value.set_contains_top_flag()
                }
                for (param_id, param_offset) in param_value.get_relative_values() {
                    let value = DataDomain::from_target(
                        param_id.clone(),
                        param_offset.clone() + callee_offset.clone(),
                    );
                    return_value = return_value.merge(&value);
                }
            } else {
                return_value.set_contains_top_flag();
            }
        }
        // If the Top-flag of the return value was set we replace it with an ID representing the return register
        // to indicate where the unknown value originated from.
        if return_value.contains_top() {
            let id = AbstractIdentifier::new_from_var(call.tid.clone(), return_register);
            let value =
                DataDomain::from_target(id, Bitvector::zero(return_register.size.into()).into());
            return_value = return_value.merge(&value);
            return_value.unset_contains_top_flag();
        }

        return_value
    }
}

impl<'a> forward_interprocedural_fixpoint::Context<'a> for Context<'a> {
    type Value = State;

    fn get_graph(&self) -> &Graph<'a> {
        self.graph
    }

    fn merge(&self, state_left: &State, state_right: &State) -> State {
        state_left.merge(state_right)
    }

    fn update_def(&self, state: &State, def: &Term<Def>) -> Option<State> {
        let mut new_state = state.clone();
        match &def.term {
            Def::Assign { var, value } => {
                new_state.set_read_flag_for_input_ids_of_expression(value);
                new_state.set_register(var, state.eval(value));
            }
            Def::Load { var, address } => {
                new_state.set_deref_flag_for_input_ids_of_expression(address);
                let value = new_state.load_value(new_state.eval(address), var.size);
                new_state.set_register(var, value);
            }
            Def::Store { address, value } => {
                new_state.set_mutable_deref_flag_for_input_ids_of_expression(address);
                if state
                    .get_offset_if_exact_stack_pointer(&state.eval(address))
                    .is_some()
                {
                    // Only flag inputs of non-trivial expressions as accessed to prevent flagging callee-saved registers as parameters.
                    // Sometimes parameter registers are callee-saved (for no apparent reason).
                    new_state.set_read_flag_for_input_ids_of_nontrivial_expression(value);
                } else {
                    new_state.set_read_flag_for_input_ids_of_expression(value);
                }
                new_state.write_value(new_state.eval(address), new_state.eval(value));
            }
        }
        Some(new_state)
    }

    fn update_jump(
        &self,
        state: &State,
        jump: &Term<Jmp>,
        _untaken_conditional: Option<&Term<Jmp>>,
        _target: &Term<Blk>,
    ) -> Option<State> {
        let mut new_state = state.clone();
        match &jump.term {
            Jmp::BranchInd(address) | Jmp::Return(address) => {
                new_state.set_read_flag_for_input_ids_of_expression(address);
            }
            Jmp::CBranch { condition, .. } => {
                new_state.set_read_flag_for_input_ids_of_expression(condition);
            }
            _ => (),
        }
        Some(new_state)
    }

    fn update_call(
        &self,
        _state: &State,
        _call: &Term<Jmp>,
        _target: &crate::analysis::graph::Node,
        _calling_convention: &Option<String>,
    ) -> Option<State> {
        // No knowledge is transferred from the caller to the callee.
        None
    }

    fn update_call_stub(&self, state: &State, call: &Term<Jmp>) -> Option<State> {
        let mut new_state = state.clone();
        match &call.term {
            Jmp::CallInd { target, .. } => {
                new_state.set_read_flag_for_input_ids_of_expression(target);
                if let Some(cconv) = self.project.get_standard_calling_convention() {
                    new_state.handle_unknown_function_stub(call, cconv);
                    return Some(new_state);
                }
            }
            Jmp::Call { target, .. } => {
                if let Some(extern_symbol) = self.project.program.term.extern_symbols.get(target) {
                    let cconv = self.project.get_calling_convention(extern_symbol);
                    new_state.handle_extern_symbol(call, extern_symbol, cconv);
                    if !extern_symbol.no_return {
                        return Some(new_state);
                    }
                } else if let Some(cconv) = self.project.get_standard_calling_convention() {
                    new_state.handle_unknown_function_stub(call, cconv);
                    return Some(new_state);
                }
            }
            _ => (),
        }
        // The call could not be properly handled, so we treat it as a dead end in the control flow graph.
        None
    }

    fn update_return(
        &self,
        state: Option<&State>,
        state_before_call: Option<&State>,
        call_term: &Term<Jmp>,
        _return_term: &Term<Jmp>,
        _calling_convention: &Option<String>,
    ) -> Option<State> {
        if state.is_none() || state_before_call.is_none() {
            return None;
        }
        let calling_convention = match self.project.get_standard_calling_convention() {
            Some(cconv) => cconv,
            None => return None,
        };
        let old_state = state_before_call.unwrap();
        let callee_state = state.unwrap();
        let mut new_state = old_state.clone();
        // Merge parameter access patterns with the access patterns from the callee.
        let parameters = callee_state.get_params_of_current_function();
        new_state.merge_parameter_access(&parameters);
        // Compute values for return register (but do not add them to `new_state` yet)
        let return_value_list = self.compute_return_values_of_call(
            &mut new_state,
            callee_state,
            calling_convention,
            call_term,
        );
        // From now on the operations on new_state are allowed to modify register values.
        // Only retain callee-saved register from the caller register values.
        new_state.clear_non_callee_saved_register(&calling_convention.callee_saved_register);
        // Now we can insert the return values into the state
        for (var, value) in return_value_list {
            new_state.set_register(var, value);
        }
        Some(new_state)
    }

    fn specialize_conditional(
        &self,
        state: &State,
        condition: &Expression,
        _block_before_condition: &Term<Blk>,
        _is_true: bool,
    ) -> Option<State> {
        let mut new_state = state.clone();
        new_state.set_read_flag_for_input_ids_of_expression(condition);
        Some(new_state)
    }
}

#[cfg(test)]
pub mod tests;
