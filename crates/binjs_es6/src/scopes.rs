use ast::*;
use binjs_shared::{ FromJSON, ToJSON, VisitMe };

use std::collections::{  HashSet, HashMap };

use itertools::Itertools;
use json::JsonValue as JSON;

#[derive(Debug, PartialEq, Eq)]
enum BindingKind {
    Var,
    NonConstLexical,
    ConstLexical,
    PositionalParam { depth: usize, index: u32 },
    RestParam,
    DestructuringParam { depth: usize },
    Bound,
}

struct VarAndLexNames {
    var_names: HashSet<String>,
    non_const_lexical_names: HashSet<String>,
    const_lexical_names: HashSet<String>,
}

enum ParamKind {
    Positional { index: u32, name: String },
    Destructuring { name: String },
    Rest { name: String },
}

#[derive(Default)]
pub struct AnnotationVisitor {
    // The following are stacks.
    var_names_stack: Vec<HashSet<String>>,
    non_const_lexical_names_stack: Vec<HashSet<String>>,
    const_lexical_names_stack: Vec<HashSet<String>>,
    params_stack: Vec<Vec<ParamKind>>,
    param_indices_stack: Vec<u32>,
    bound_names_stack: Vec<HashSet<String>>,
    binding_kind_stack: Vec<BindingKind>,
    apparent_direct_eval_stack: Vec<bool>,
    function_expression_name_stack: Vec<Option<BindingIdentifier>>,

    // 'true' if the free name has already cross a function boundary
    // 'false' until then.
    free_names_in_block_stack: Vec<HashMap<String, bool>>,
}
impl AnnotationVisitor {
    fn pop_captured_names(&mut self, bindings: &[&HashSet<String>]) -> HashSet<String> {
        let mut captured_names = HashSet::new();
        let my_free_names = self.free_names_in_block_stack.last_mut().unwrap();
        for binding in bindings {
            for name in *binding {
                if let Some(cross_function) = my_free_names.remove(name) {
                    // Free names across nested function boundaries are closed.
                    debug!(target: "annotating", "found captured name {}", name);
                    if cross_function {
                        captured_names.insert(name.clone());
                    }
                }
            }
        }

        captured_names
    }

    fn push_free_names(&mut self) {
        self.free_names_in_block_stack.push(HashMap::new());
    }
    fn pop_free_names(&mut self, bindings: &[&HashSet<String>], is_leaving_function_scope: bool) {
        let mut free_names_in_current_block = self.free_names_in_block_stack.pop().unwrap();
        for (name, old_cross_function) in free_names_in_current_block.drain() {
            let is_bound = bindings.iter()
                .find(|container| container.contains(&name))
                .is_some();
            if !is_bound {
                // Propagate free names up to the enclosing scope, for further analysis.
                // Actively propagate the closure flag as we go. It could have been set by
                //   A nested scope: old_cross_function
                //   This scope: is_leaving_function_scope
                //   Or, it could have already been in the parent scope from a sibling block.
                // Or everything together, so we don't forget if the binding was closed over.
                if let Some(mut parent_free) = self.free_names_in_block_stack.last_mut() {
                    let my_contribution = old_cross_function || is_leaving_function_scope;
                    parent_free.entry(name)
                        .and_modify(|p| { *p = *p || my_contribution })
                        .or_insert(my_contribution);
                }
            }
        }
    }

    fn push_direct_eval(&mut self) {
        // So far, we haven't spotted any direct eval.
        self.apparent_direct_eval_stack.push(false);
    }
    fn pop_direct_eval(&mut self) -> bool {
        let spotted_direct_eval = self.apparent_direct_eval_stack.pop().unwrap();
        if spotted_direct_eval {
            // If we have spotted a direct eval, well, the parents also have
            // a direct eval. Note that we will perform a second pass to
            // remove erroneous direct evals if we find out that name `eval`
            // was actually bound at some point.
            if let Some(parent) = self.apparent_direct_eval_stack.last_mut() {
                *parent = true;
            }
        }
        spotted_direct_eval
    }

    fn push_block_scope(&mut self, _path: &Path) {
        self.non_const_lexical_names_stack.push(HashSet::new());
        self.const_lexical_names_stack.push(HashSet::new());
        self.push_free_names();
        self.push_direct_eval();
    }
    fn pop_block_scope(&mut self, path: &Path) -> AssertedBlockScope {
        debug!(target: "annotating", "pop_block_scope at {:?}", path);
        let non_const_lexical_names = self.non_const_lexical_names_stack.pop().unwrap();
        let const_lexical_names = self.const_lexical_names_stack.pop().unwrap();

        debug!(target: "annotating", "pop_non_const_scope lex {:?}", non_const_lexical_names);
        debug!(target: "annotating", "pop_const_scope lex {:?}", const_lexical_names);

        let captured_names = self.pop_captured_names(&[&non_const_lexical_names, &const_lexical_names]);
        self.pop_free_names(&[&non_const_lexical_names, &const_lexical_names], /* is_leaving_function_scope = */false);

        let mut declared_names = vec![];
        for name in non_const_lexical_names.into_iter().sorted() {
            let is_captured = captured_names.contains(&name);
            declared_names.push(AssertedDeclaredName {
                name,
                kind: AssertedDeclaredKind::NonConstLexical,
                is_captured
            })
        }
        for name in const_lexical_names.into_iter().sorted() {
            let is_captured = captured_names.contains(&name);
            declared_names.push(AssertedDeclaredName {
                name,
                kind: AssertedDeclaredKind::ConstLexical,
                is_captured
            })
        }

        let has_direct_eval = self.pop_direct_eval();
        AssertedBlockScope {
            declared_names,
            has_direct_eval
        }
    }

    fn push_incomplete_var_scope(&mut self, _path: &Path) {
        self.var_names_stack.push(HashSet::new());
        self.non_const_lexical_names_stack.push(HashSet::new());
        self.const_lexical_names_stack.push(HashSet::new());
    }
    fn push_var_scope(&mut self, path: &Path) {
        debug!(target: "annotating", "push_var_scope at {:?}", path);
        self.push_incomplete_var_scope(path);
        self.push_direct_eval();
        self.push_free_names();
    }
    fn pop_incomplete_var_scope(&mut self, path: &Path) -> VarAndLexNames {
        debug!(target: "annotating", "pop_incomplete_var_scope at {:?}", path);
        let var_names = self.var_names_stack.pop().unwrap();
        let non_const_lexical_names = self.non_const_lexical_names_stack.pop().unwrap();
        let const_lexical_names = self.const_lexical_names_stack.pop().unwrap();

        debug!(target: "annotating", "pop_incomplete_var_scope var {:?}", var_names);
        debug!(target: "annotating", "pop_incomplete_var_scope non_const {:?}", non_const_lexical_names);
        debug!(target: "annotating", "pop_incomplete_var_scope const {:?}", const_lexical_names);

        // Check that a name isn't defined twice in the same scope.
        for name in var_names.intersection(&non_const_lexical_names) {
            panic!("This name is both non-const-lexical-bound and var-bound: {}", name);
        }
        for name in var_names.intersection(&const_lexical_names) {
            panic!("This name is both const-lexical-bound and var-bound: {}", name);
        }
        VarAndLexNames {
            var_names,
            non_const_lexical_names,
            const_lexical_names,
        }
    }
    fn pop_var_and_lex_declared_names(&mut self, path: &Path) -> Vec<AssertedDeclaredName> {
        let VarAndLexNames { var_names, non_const_lexical_names, const_lexical_names } = self.pop_incomplete_var_scope(path);
        let captured_names = self.pop_captured_names(&[&var_names, &non_const_lexical_names, &const_lexical_names]);
        self.pop_free_names(&[&var_names, &non_const_lexical_names, &const_lexical_names], /* is_leaving_function_scope = */true);

        let mut declared_names = vec![];
        for name in var_names.into_iter().sorted() {
            let is_captured = captured_names.contains(&name);
            declared_names.push(AssertedDeclaredName {
                name,
                kind: AssertedDeclaredKind::Var,
                is_captured
            })
        }
        for name in non_const_lexical_names.into_iter().sorted() {
            let is_captured = captured_names.contains(&name);
            declared_names.push(AssertedDeclaredName {
                name,
                kind: AssertedDeclaredKind::NonConstLexical,
                is_captured
            })
        }
        for name in const_lexical_names.into_iter().sorted() {
            let is_captured = captured_names.contains(&name);
            declared_names.push(AssertedDeclaredName {
                name,
                kind: AssertedDeclaredKind::ConstLexical,
                is_captured
            })
        }

        declared_names
    }

    fn pop_var_scope(&mut self, path: &Path) -> AssertedVarScope {
        let declared_names = self.pop_var_and_lex_declared_names(path);
        let has_direct_eval = self.pop_direct_eval();

        AssertedVarScope {
            declared_names,
            has_direct_eval
        }
    }
    fn pop_script_global_scope(&mut self, path: &Path) -> AssertedScriptGlobalScope {
        let declared_names = self.pop_var_and_lex_declared_names(path);
        let has_direct_eval = self.pop_direct_eval();

        AssertedScriptGlobalScope {
            declared_names,
            has_direct_eval
        }
    }

    fn push_this_captured(&mut self) {
        self.push_free_names();
    }
    fn pop_this_captured(&mut self) -> bool {
        let this_name = "this".to_string();
        let mut this_names = HashSet::new();
        this_names.insert(this_name.clone());
        let captured_names = self.pop_captured_names(&[&this_names]);
        self.pop_free_names(&[&this_names], /* is_leaving_function_scope = */false);
        captured_names.contains(&this_name)
    }

    fn push_function_name_captured(&mut self) {
        self.push_free_names();
    }
    fn pop_function_name_captured(&mut self, name: String) -> bool {
        let mut names = HashSet::new();
        names.insert(name.clone());
        let captured_names = self.pop_captured_names(&[&names]);
        self.pop_free_names(&[&names], /* is_leaving_function_scope = */false);
        captured_names.contains(&name)
    }

    fn push_param_scope(&mut self, _path: &Path) {
        debug!(target: "annotating", "push_param_scope at {:?}", _path);
        self.params_stack.push(vec![]);
        self.param_indices_stack.push(0);
        self.push_free_names();
        self.push_direct_eval();
    }
    fn pop_param_names(&mut self) -> Vec<AssertedMaybePositionalParameterName> {
        fn to_name(param: &ParamKind) -> String {
            match param {
                ParamKind::Positional { index: _, name } => name.clone(),
                ParamKind::Destructuring { name } => name.clone(),
                ParamKind::Rest { name } => name.clone(),
            }
        };
        let params = self.params_stack.pop().unwrap();
        self.param_indices_stack.pop();
        let names = params.as_slice().into_iter().map(to_name).collect();
        let captured_names = self.pop_captured_names(&[&names]);
        self.pop_free_names(&[&names], /* is_leaving_function_scope = */false);

        // In the case of `function foo(j) {var j;}`, the `var j` is not the true declaration.
        // Remove it from parameters.
        for name in &names {
            if self.var_names_stack.last_mut()
                .unwrap()
                .remove(name)
            {
                debug!(target: "annotating", "pop_param_scope removing {:?}", name);
            }
        }

        let mut param_names = vec![];
        for param in params.into_iter() {
            let is_captured = captured_names.contains(&to_name(&param));
            param_names.push(match param {
                ParamKind::Positional { index, name } => AssertedMaybePositionalParameterName::AssertedPositionalParameterName(Box::new(AssertedPositionalParameterName {
                    index,
                    name,
                    is_captured
                })),
                ParamKind::Destructuring { name } => AssertedMaybePositionalParameterName::AssertedParameterName(Box::new(AssertedParameterName {
                    name,
                    is_captured
                })),
                ParamKind::Rest { name } => AssertedMaybePositionalParameterName::AssertedRestParameterName(Box::new(AssertedRestParameterName {
                    name,
                    is_captured
                })),
            })
        }

        param_names
    }
    fn pop_param_scope(&mut self, path: &Path, parameter_scope: &AssertedParameterScope) -> AssertedParameterScope {
        debug!(target: "annotating", "pop_param_scope at {:?}", path);
        let param_names = self.pop_param_names();
        let has_direct_eval = self.pop_direct_eval();
        AssertedParameterScope {
            param_names,
            has_direct_eval,
            is_simple_parameter_list: parameter_scope.is_simple_parameter_list,
        }
    }
    fn push_bound_scope(&mut self, _path: &Path) {
        debug!(target: "annotating", "push_bound_scope at {:?}", _path);
        self.bound_names_stack.push(HashSet::new());
        self.push_free_names();
        self.push_direct_eval();
    }
    fn pop_bound_names(&mut self) -> Vec<AssertedBoundName> {
        let names = self.bound_names_stack.pop().unwrap();
        let captured_names = self.pop_captured_names(&[&names]);
        self.pop_free_names(&[&names], /* is_leaving_function_scope = */false);

        let mut bound_names = vec![];
        for name in names.into_iter().sorted() {
            let is_captured = captured_names.contains(&name);
            bound_names.push(AssertedBoundName {
                name,
                is_captured
            })
        }

        bound_names
    }
    fn pop_bound_names_scope(&mut self, path: &Path) -> AssertedBoundNamesScope {
        debug!(target: "annotating", "pop_bound_names_scope at {:?}", path);
        let bound_names = self.pop_bound_names();
        let has_direct_eval = self.pop_direct_eval();
        AssertedBoundNamesScope {
            bound_names,
            has_direct_eval
        }
    }
    fn function_expression_name(&self) -> Option<String> {
        match self.function_expression_name_stack.last().unwrap() {
            Some(identifier) => {
                Some(identifier.name.clone())
            }
            _ => {
                None
            }
        }
    }
    fn push_function_expression_name(&mut self, name: Option<BindingIdentifier>) {
        self.function_expression_name_stack.push(name);
    }
    fn pop_function_expression_name(&mut self) {
        self.function_expression_name_stack.pop();
    }
}

fn is_positional_parameter(visitor: &AnnotationVisitor, path: &Path) -> bool {
    match visitor.binding_kind_stack.last() {
        Some(BindingKind::RestParam) => {},
        _ => { return false; },
    };
    match path.get(0) {
        Some(&PathItem { interface: ASTNode::BindingWithInitializer, field: ASTField::Binding }) => {
            match path.get(1) {
                Some(&PathItem { interface: ASTNode::SetterContents, field: ASTField::Param }) |
                Some(&PathItem { interface: ASTNode::FormalParameters, field: ASTField::Items }) => true,
                _ => false,
            }
        },
        Some(&PathItem { interface: ASTNode::SetterContents, field: ASTField::Param }) |
        Some(&PathItem { interface: ASTNode::FormalParameters, field: ASTField::Items }) => true,
        _ => false,
    }
}
fn maybe_enter_positional_parameter(visitor: &mut AnnotationVisitor, path: &Path) {
    if !is_positional_parameter(visitor, path) {
        return;
    }

    let i = *visitor.param_indices_stack.last().unwrap();
    visitor.binding_kind_stack.push(BindingKind::PositionalParam {
        depth: path.len(),
        index: i,
    });
}
fn maybe_exit_positional_parameter(visitor: &mut AnnotationVisitor, path: &Path) {
    if visitor.param_indices_stack.is_empty() {
        return;
    }

    let i = *visitor.param_indices_stack.last().unwrap();
    match visitor.binding_kind_stack.last() {
        Some(BindingKind::PositionalParam { depth, index })
            if *depth == path.len() && *index == i => {},
        _ => { return; }
    }

    *(visitor.param_indices_stack.last_mut().unwrap()) = i + 1;
    visitor.binding_kind_stack.pop();
}

fn is_destructuring_parameter(visitor: &AnnotationVisitor, path: &Path) -> bool {
    match visitor.binding_kind_stack.last() {
        Some(BindingKind::RestParam) => {},
        _ => { return false; },
    };
    match path.get(0) {
        Some(&PathItem { interface: ASTNode::BindingWithInitializer, field: ASTField::Binding }) => {
            match path.get(1) {
                Some(&PathItem { interface: ASTNode::SetterContents, field: ASTField::Param }) |
                Some(&PathItem { interface: ASTNode::FormalParameters, field: ASTField::Items }) => true,
                _ => false,
            }
        },
        Some(&PathItem { interface: ASTNode::SetterContents, field: ASTField::Param }) |
        Some(&PathItem { interface: ASTNode::FormalParameters, field: ASTField::Items }) => true,
        _ => false,
    }
}
fn maybe_enter_destructuing_parameter(visitor: &mut AnnotationVisitor, path: &Path) {
    if !is_destructuring_parameter(visitor, path) {
        return;
    }

    visitor.binding_kind_stack.push(BindingKind::DestructuringParam {
        depth: path.len(),
    });
}
fn maybe_exit_destructuing_parameter(visitor: &mut AnnotationVisitor, path: &Path) {
    match visitor.binding_kind_stack.last() {
        Some(BindingKind::DestructuringParam { depth })
            if *depth == path.len() => {},
        _ => { return; }
    }

    *(visitor.param_indices_stack.last_mut().unwrap()) += 1;
    visitor.binding_kind_stack.pop();
}

impl Visitor<()> for AnnotationVisitor {
    // Identifiers

    fn exit_call_expression(&mut self, _path: &Path, node: &mut CallExpression) -> Result<Option<CallExpression>, ()> {
        if let ExpressionOrSuper::IdentifierExpression(box ref id) = node.callee {
            if id.name == "eval" {
                *self.apparent_direct_eval_stack.last_mut()
                    .unwrap() = true;
            }
        }
        Ok(None)
    }

    fn exit_identifier_expression(&mut self, _path: &Path, node: &mut IdentifierExpression) -> Result<Option<IdentifierExpression>, ()> {
        debug!(target: "annotating", "exit_identifier_expression {} at {:?}", node.name, _path);
        let names = self.free_names_in_block_stack.last_mut().unwrap();
        if !names.contains_key(&node.name) {
            names.insert(node.name.clone(), false);
        }
        Ok(None)
    }
    fn exit_this_expression(&mut self, _path: &Path, _node: &mut ThisExpression) -> Result<Option<ThisExpression>, ()> {
        debug!(target: "annotating", "exit_this_expression at {:?}", _path);
        let names = self.free_names_in_block_stack.last_mut().unwrap();
        let this_name = "this".to_string();
        if !names.contains_key(&this_name) {
            names.insert(this_name.clone(), false);
        }
        Ok(None)
    }

    fn exit_assignment_target_identifier(&mut self, _path: &Path, node: &mut AssignmentTargetIdentifier) -> Result<Option<AssignmentTargetIdentifier>, ()> {
        let names = self.free_names_in_block_stack.last_mut().unwrap();
        if !names.contains_key(&node.name) {
            names.insert(node.name.clone(), false);
        }
        Ok(None)
    }

    fn enter_binding_identifier(&mut self, path: &Path, _node: &mut BindingIdentifier) -> Result<VisitMe<()>, ()> {
        maybe_enter_positional_parameter(self, path);
        Ok(VisitMe::HoldThis(()))
    }

    fn exit_binding_identifier(&mut self, path: &Path, node: &mut BindingIdentifier) -> Result<Option<BindingIdentifier>, ()> {
        match path.get(0) {
            Some(&PathItem { interface: ASTNode::EagerFunctionDeclaration, field: ASTField::Name})
            | Some(&PathItem { interface: ASTNode::EagerFunctionExpression, field: ASTField::Name})
            | Some(&PathItem { interface: ASTNode::EagerMethod, field: ASTField::Name})
            | Some(&PathItem { interface: ASTNode::EagerGetter, field: ASTField::Name})
            | Some(&PathItem { interface: ASTNode::EagerSetter, field: ASTField::Name})
            => {
                // Function names are special.
                // They are handled in the respective `exit_*` methods.
                return Ok(None)
            }
            _ => {}
        }
        debug!(target: "annotating", "exit_binding identifier – marking {name} as {kind:?} at {path:?}",
            name = node.name,
            kind = self.binding_kind_stack.last().unwrap(),
            path = path);
        match *self.binding_kind_stack.last().unwrap() {
            BindingKind::Var => {
                self.var_names_stack.last_mut()
                    .unwrap()
                    .insert(node.name.clone());
            }
            BindingKind::NonConstLexical => {
                self.non_const_lexical_names_stack.last_mut()
                    .unwrap()
                    .insert(node.name.clone());
            }
            BindingKind::ConstLexical => {
                self.const_lexical_names_stack.last_mut()
                    .unwrap()
                    .insert(node.name.clone());
            }
            BindingKind::PositionalParam { depth: _, index } => {
                self.params_stack.last_mut()
                    .unwrap()
                    .push(ParamKind::Positional {
                        index,
                        name: node.name.clone(),
                    });
            }
            BindingKind::RestParam => {
                self.params_stack.last_mut()
                    .unwrap()
                    .push(ParamKind::Rest {
                        name: node.name.clone(),
                    });
            }
            BindingKind::DestructuringParam { depth: _ } => {
                self.params_stack.last_mut()
                    .unwrap()
                    .push(ParamKind::Destructuring {
                        name: node.name.clone(),
                    });
            }
            BindingKind::Bound => {
                self.bound_names_stack.last_mut()
                    .unwrap()
                    .insert(node.name.clone());
            }
        }
        maybe_exit_positional_parameter(self, path);
        Ok(None)
    }


    // Blocks

    fn enter_block(&mut self, path: &Path, _node: &mut Block) -> Result<VisitMe<()>, ()> {
        self.push_block_scope(path);
        Ok(VisitMe::HoldThis(()))
    }

    fn exit_block(&mut self, path: &Path, node: &mut Block) -> Result<Option<Block>, ()> {
        node.scope = self.pop_block_scope(path);
        Ok(None)
    }

    fn enter_script(&mut self, path: &Path, _node: &mut Script) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_script(&mut self, path: &Path, node: &mut Script) -> Result<Option<Script>, ()> {
        node.scope = self.pop_script_global_scope(path);
        Ok(None)
    }

    fn enter_module(&mut self, path: &Path, _node: &mut Module) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_module(&mut self, path: &Path, node: &mut Module) -> Result<Option<Module>, ()> {
        node.scope = self.pop_var_scope(path);
        Ok(None)
    }

    // Try/Catch
    fn enter_catch_clause(&mut self, path: &Path, _node: &mut CatchClause) -> Result<VisitMe<()>, ()> {
        self.binding_kind_stack.push(BindingKind::Bound);

        // We need to differentiate between
        // `var ex; try { ... } catch(ex) { ... }` (both instances of `ex` are distinct)
        // and
        // `try { ... } catch(ex) { var ex; ... }` (both instances of `ex` are the same)
        // so we introduce a var scope in `catch(ex)`, as if `catch(ex) { ... }` was
        // a function.

        self.push_incomplete_var_scope(path);
        self.push_bound_scope(path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_catch_clause(&mut self, path: &Path, node: &mut CatchClause) -> Result<Option<CatchClause>, ()> {
        assert_matches!(self.binding_kind_stack.pop(), Some(BindingKind::Bound));
        node.binding_scope = self.pop_bound_names_scope(path);
        let var_scope = self.pop_incomplete_var_scope(path);

        assert_eq!(var_scope.non_const_lexical_names.len(), 0, "The implicit scope of a catch should not contain lexically declared names. This requires an actual block.");
        assert_eq!(var_scope.const_lexical_names.len(), 0, "The implicit scope of a catch should not contain lexically declared names. This requires an actual block.");

        // Propagate any var_declared_names.
        for name in var_scope.var_names.into_iter() {
            debug!(target: "annotating", "exit_catch_clause: reinserting {}", name);
            self.var_names_stack.last_mut()
                .unwrap()
                .insert(name);
        }

        Ok(None)
    }

    // Explicit variable declarations

    fn enter_for_in_of_binding(&mut self, _path: &Path, node: &mut ForInOfBinding) -> Result<VisitMe<()>, ()> {
        let kind = match node.kind {
            VariableDeclarationKind::Let => BindingKind::NonConstLexical,
            VariableDeclarationKind::Const => BindingKind::ConstLexical,
            VariableDeclarationKind::Var => BindingKind::Var,
        };
        self.binding_kind_stack.push(kind);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_for_in_of_binding(&mut self, _path: &Path, node: &mut ForInOfBinding) -> Result<Option<ForInOfBinding>, ()> {
        let kind = match node.kind {
            VariableDeclarationKind::Let => BindingKind::NonConstLexical,
            VariableDeclarationKind::Const => BindingKind::ConstLexical,
            VariableDeclarationKind::Var => BindingKind::Var,
        };
        assert_eq!(self.binding_kind_stack.pop().unwrap(), kind);
        Ok(None)
    }

    fn enter_variable_declaration(&mut self, _path: &Path, node: &mut VariableDeclaration) -> Result<VisitMe<()>, ()> {
        let kind = match node.kind {
            VariableDeclarationKind::Let => BindingKind::NonConstLexical,
            VariableDeclarationKind::Const => BindingKind::ConstLexical,
            VariableDeclarationKind::Var => BindingKind::Var,
        };
        self.binding_kind_stack.push(kind);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_variable_declaration(&mut self, _path: &Path, node: &mut VariableDeclaration) -> Result<Option<VariableDeclaration>, ()> {
        let kind = match node.kind {
            VariableDeclarationKind::Let => BindingKind::NonConstLexical,
            VariableDeclarationKind::Const => BindingKind::ConstLexical,
            VariableDeclarationKind::Var => BindingKind::Var,
        };
        assert_eq!(self.binding_kind_stack.pop().unwrap(), kind);
        Ok(None)
    }

    // Functions, methods, arguments.
    fn enter_setter_contents(&mut self, path: &Path, _node: &mut SetterContents) -> Result<VisitMe<()>, ()> {
        self.push_param_scope(path);
        self.push_var_scope(path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_setter_contents(&mut self, path: &Path, node: &mut SetterContents) -> Result<Option<SetterContents>, ()> {
        // Commit parameter scope and var scope.
        node.parameter_scope = self.pop_param_scope(path, &node.parameter_scope);
        node.body_scope = self.pop_var_scope(path);

        Ok(None)
    }

    fn enter_eager_setter(&mut self, _path: &Path, _node: &mut EagerSetter) -> Result<VisitMe<()>, ()> {
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_setter(&mut self, _path: &Path, _node: &mut EagerSetter) -> Result<Option<EagerSetter>, ()> {
        Ok(None)
    }

    fn enter_getter_contents(&mut self, path: &Path, _node: &mut GetterContents) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        Ok(VisitMe::HoldThis(()))
    }

    fn exit_getter_contents(&mut self, path: &Path, node: &mut GetterContents) -> Result<Option<GetterContents>, ()> {
        node.body_scope = self.pop_var_scope(path);

        Ok(None)
    }

    fn enter_eager_getter(&mut self, _path: &Path, _node: &mut EagerGetter) -> Result<VisitMe<()>, ()> {
        Ok(VisitMe::HoldThis(()))
    }

    fn exit_eager_getter(&mut self, _path: &Path, _node: &mut EagerGetter) -> Result<Option<EagerGetter>, ()> {
        Ok(None)
    }

    fn enter_function_or_method_contents(&mut self, path: &Path, _node: &mut FunctionOrMethodContents) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        self.push_param_scope(path);
        self.push_this_captured();
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_function_or_method_contents(&mut self, path: &Path, node: &mut FunctionOrMethodContents) -> Result<Option<FunctionOrMethodContents>, ()> {
        node.is_this_captured = self.pop_this_captured();

        // Commit parameter scope and var scope.
        node.parameter_scope = self.pop_param_scope(path, &node.parameter_scope);
        node.body_scope = self.pop_var_scope(path);

        Ok(None)
    }

    fn enter_eager_method(&mut self, _path: &Path, _node: &mut EagerMethod) -> Result<VisitMe<()>, ()> {
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_method(&mut self, _path: &Path, _node: &mut EagerMethod) -> Result<Option<EagerMethod>, ()> {
        Ok(None)
    }

    fn enter_arrow_expression_contents_with_function_body(&mut self, path: &Path, _node: &mut ArrowExpressionContentsWithFunctionBody) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        self.push_param_scope(path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_arrow_expression_contents_with_function_body(&mut self, path: &Path, node: &mut ArrowExpressionContentsWithFunctionBody) -> Result<Option<ArrowExpressionContentsWithFunctionBody>, ()> {
        // Commit parameter scope and var scope.
        node.parameter_scope = self.pop_param_scope(path, &node.parameter_scope);
        node.body_scope = self.pop_var_scope(path);

        Ok(None)
    }
    fn enter_arrow_expression_contents_with_expression(&mut self, path: &Path, _node: &mut ArrowExpressionContentsWithExpression) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        self.push_param_scope(path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_arrow_expression_contents_with_expression(&mut self, path: &Path, node: &mut ArrowExpressionContentsWithExpression) -> Result<Option<ArrowExpressionContentsWithExpression>, ()> {
        // Commit parameter scope and var scope.
        node.parameter_scope = self.pop_param_scope(path, &node.parameter_scope);
        node.body_scope = self.pop_var_scope(path);

        Ok(None)
    }

    fn enter_eager_arrow_expression_with_function_body(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithFunctionBody) -> Result<VisitMe<()>, ()> {
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_arrow_expression_with_function_body(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithFunctionBody) -> Result<Option<EagerArrowExpressionWithFunctionBody>, ()> {
        Ok(None)
    }
    fn enter_eager_arrow_expression_with_expression(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithExpression) -> Result<VisitMe<()>, ()> {
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_arrow_expression_with_expression(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithExpression) -> Result<Option<EagerArrowExpressionWithExpression>, ()> {
        Ok(None)
    }

    fn enter_function_expression_contents(&mut self, path: &Path, _node: &mut FunctionExpressionContents) -> Result<VisitMe<()>, ()> {
        self.push_var_scope(path);
        self.push_param_scope(path);
        self.push_this_captured();
        if let Some(_) = self.function_expression_name() {
            self.push_function_name_captured();
        }
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_function_expression_contents(&mut self, path: &Path, node: &mut FunctionExpressionContents) -> Result<Option<FunctionExpressionContents>, ()> {
        if let Some(ref name) = self.function_expression_name() {
            node.is_function_name_captured = self.pop_function_name_captured(name.clone());
        } else {
            node.is_function_name_captured = false;
        }
        node.is_this_captured = self.pop_this_captured();

        node.parameter_scope = self.pop_param_scope(path, &node.parameter_scope);
        node.body_scope = self.pop_var_scope(path);
        Ok(None)
    }

    fn enter_eager_function_expression(&mut self, _path: &Path, node: &mut EagerFunctionExpression) -> Result<VisitMe<()>, ()> {
        self.push_function_expression_name(node.name.clone());
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_function_expression(&mut self, _path: &Path, _node: &mut EagerFunctionExpression) -> Result<Option<EagerFunctionExpression>, ()> {
        self.pop_function_expression_name();
        Ok(None)
    }

    fn enter_eager_function_declaration(&mut self, _path: &Path, _node: &mut EagerFunctionDeclaration) -> Result<VisitMe<()>, ()> {
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_function_declaration(&mut self, path: &Path, node: &mut EagerFunctionDeclaration) -> Result<Option<EagerFunctionDeclaration>, ()> {
        debug!(target: "annotating", "exit_eager_function_declaration {} at {:?}", node.name.name, path);

        // If a name declaration was specified, remove it from `unknown`.
        let ref name = node.name.name;

        // Var scope is already committed in exit_function_or_method_contents.
        // The function's name is not actually bound in the function; the outer var binding is used.
        // Anything we do from this point affects the scope outside the function.

        // 1. If the declaration is at the toplevel, the name is declared as a `var`.
        // 2. If the declaration is in a function's toplevel block, the name is declared as a `var`.
        // 3. Otherwise, the name is declared as a `let`.
        let name = name.to_string();
        debug!(target: "annotating", "exit_eager_function_declaration sees {} at {:?}", node.name.name, path.get(0));
        match path.get(0).expect("Impossible AST walk") {
            &PathItem { field: ASTField::Statements, interface: ASTNode::Script } |
            &PathItem { field: ASTField::Statements, interface: ASTNode::Module } => {
                // Case 1.
                debug!(target: "annotating", "exit_eager_function_declaration says it's a var (case 1)");
                self.var_names_stack.last_mut()
                    .unwrap()
                    .insert(name);
            }
            &PathItem { field: ASTField::Body, interface: ASTNode::GetterContents } |
            &PathItem { field: ASTField::Body, interface: ASTNode::SetterContents } |
            &PathItem { field: ASTField::Body, interface: ASTNode::ArrowExpressionContentsWithFunctionBody } |
            &PathItem { field: ASTField::Body, interface: ASTNode::ArrowExpressionContentsWithExpression } |
            &PathItem { field: ASTField::Body, interface: ASTNode::FunctionExpressionContents } |
            &PathItem { field: ASTField::Body, interface: ASTNode::FunctionOrMethodContents } =>
            {
                // Case 2.
                debug!(target: "annotating", "exit_eager_function_declaration says it's a var (case 2)");
                self.var_names_stack.last_mut()
                    .unwrap()
                    .insert(name);
            }
            _ => {
                // Case 3.
                debug!(target: "annotating", "exit_eager_function_declaration says it's a non const lexical (case 3)");
                self.non_const_lexical_names_stack.last_mut()
                    .unwrap()
                    .insert(name);
            }
        }
        Ok(None)
    }

    fn enter_formal_parameters(&mut self, _path: &Path, _node: &mut FormalParameters) -> Result<VisitMe<()>, ()> {
        // Handle rest parameter field here.  Other parameters are handled in
        // BindingIdentifier/ObjectBinding/ArrayBinding.
        self.binding_kind_stack.push(BindingKind::RestParam);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_formal_parameters(&mut self, _path: &Path, _node: &mut FormalParameters) -> Result<Option<FormalParameters>, ()> {
        assert_matches!(self.binding_kind_stack.pop(), Some(BindingKind::RestParam));
        Ok(None)
    }

    fn enter_object_binding(&mut self, path: &Path, _node: &mut ObjectBinding) -> Result<VisitMe<()>, ()> {
        maybe_enter_destructuing_parameter(self, path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_object_binding(&mut self, path: &Path, _node: &mut ObjectBinding) -> Result<Option<ObjectBinding>, ()> {
        maybe_exit_destructuing_parameter(self, path);
        Ok(None)
    }

    fn enter_array_binding(&mut self, path: &Path, _node: &mut ArrayBinding) -> Result<VisitMe<()>, ()> {
        maybe_enter_destructuing_parameter(self, path);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_array_binding(&mut self, path: &Path, _node: &mut ArrayBinding) -> Result<Option<ArrayBinding>, ()> {
        maybe_exit_destructuing_parameter(self, path);
        Ok(None)
    }
}


/// Perform a second pass to cleanup incorrect instances of `eval`.
struct EvalCleanupAnnotator {
    /// `true` if name `eval` was bound at this level or higher in the tree.
    eval_bindings: Vec<bool>,
}
impl Visitor<()> for EvalCleanupAnnotator {
    // FIXME: Anything that has a scope (including CatchClause and its invisible scope) should push an `eval_bindings`.
    // on entering, pop it on exit.
    fn enter_eager_function_declaration(&mut self, _path: &Path, _node: &mut EagerFunctionDeclaration) -> Result<VisitMe<()>, ()> {
        // By default, adopt parent's behavior.
        // If necessary, reading the scope information will amend it.
        let has_eval_binding = *self.eval_bindings.last().unwrap();
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_function_declaration(&mut self, _path: &Path, _node: &mut EagerFunctionDeclaration) -> Result<Option<EagerFunctionDeclaration>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_eager_function_expression(&mut self, _path: &Path, node: &mut EagerFunctionExpression) -> Result<VisitMe<()>, ()> {
        // By default, adopt parent's behavior.
        // Don't forget that the internal name of the function may mask `eval`.
        let mut has_eval_binding = *self.eval_bindings.last().unwrap();
        if let Some(ref name) = node.name {
            has_eval_binding = has_eval_binding || &name.name == "eval";
        }
        self.eval_bindings.push(has_eval_binding);
        // If necessary, reading the scope information will amend it.
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_function_expression(&mut self, _path: &Path, _node: &mut EagerFunctionExpression) -> Result<Option<EagerFunctionExpression>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_eager_arrow_expression_with_function_body(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithFunctionBody) -> Result<VisitMe<()>, ()> {
        // By default, adopt parent's behavior.
        // If necessary, reading the scope information will amend it.
        let has_eval_binding = *self.eval_bindings.last().unwrap();
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_arrow_expression_with_function_body(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithFunctionBody) -> Result<Option<EagerArrowExpressionWithFunctionBody>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_eager_arrow_expression_with_expression(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithExpression) -> Result<VisitMe<()>, ()> {
        // By default, adopt parent's behavior.
        // If necessary, reading the scope information will amend it.
        let has_eval_binding = *self.eval_bindings.last().unwrap();
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_arrow_expression_with_expression(&mut self, _path: &Path, _node: &mut EagerArrowExpressionWithExpression) -> Result<Option<EagerArrowExpressionWithExpression>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_eager_getter(&mut self, _path: &Path, node: &mut EagerGetter) -> Result<VisitMe<()>, ()> {
        // Don't forget that the internal name of the getter may mask `eval`.
        let mut has_eval_binding = *self.eval_bindings.last().unwrap();
        if let PropertyName::LiteralPropertyName(ref name) = node.name {
            has_eval_binding = has_eval_binding || &name.value == "eval";
        }
        // If necessary, reading the scope information will amend it.
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_getter(&mut self, _path: &Path, _node: &mut EagerGetter) -> Result<Option<EagerGetter>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_eager_setter(&mut self, _path: &Path, node: &mut EagerSetter) -> Result<VisitMe<()>, ()> {
        // Don't forget that the internal name of the setter may mask `eval`.
        let mut has_eval_binding = *self.eval_bindings.last().unwrap();
        if let PropertyName::LiteralPropertyName(ref name) = node.name {
            has_eval_binding = has_eval_binding || &name.value == "eval";
        }
        // If necessary, reading the scope information will amend it.
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_setter(&mut self, _path: &Path, _node: &mut EagerSetter) -> Result<Option<EagerSetter>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_eager_method(&mut self, _path: &Path, node: &mut EagerMethod) -> Result<VisitMe<()>, ()> {
        // Don't forget that the internal name of the method may mask `eval`.
        let mut has_eval_binding = *self.eval_bindings.last().unwrap();
        if let PropertyName::LiteralPropertyName(ref name) = node.name {
            has_eval_binding = has_eval_binding || &name.value == "eval";
        }
        // If necessary, reading the scope information will amend it.
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_eager_method(&mut self, _path: &Path, _node: &mut EagerMethod) -> Result<Option<EagerMethod>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }
    fn enter_catch_clause(&mut self, _path: &Path, node: &mut CatchClause) -> Result<VisitMe<()>, ()> {
        // Don't forget that the implicitly declared variable may mask `eval`.
        let mut has_eval_binding = *self.eval_bindings.last().unwrap();
        match node.binding {
            Binding::BindingIdentifier(ref binding) => {
                has_eval_binding = has_eval_binding || &binding.name == "eval";
            }
            _ => unimplemented!() // FIXME: Patterns may also mask `eval`.
        }
        self.eval_bindings.push(has_eval_binding);
        Ok(VisitMe::HoldThis(()))
    }
    fn exit_catch_clause(&mut self, _path: &Path, _node: &mut CatchClause) -> Result<Option<CatchClause>, ()> {
        self.eval_bindings.pop().unwrap();
        Ok(None)
    }



    // Update scopes themselves.
    fn exit_asserted_block_scope(&mut self, _path: &Path, node: &mut AssertedBlockScope) -> Result<Option<AssertedBlockScope>, ()> {
        if node.declared_names.iter()
            .find(|e| e.name == "eval")
            .is_some()
        {
            *self.eval_bindings.last_mut()
                .unwrap() = true;
        }
        if *self.eval_bindings.last()
            .unwrap()
        {
            node.has_direct_eval = false;
        }
        Ok(None)
    }
    fn exit_asserted_var_scope(&mut self, _path: &Path, node: &mut AssertedVarScope) -> Result<Option<AssertedVarScope>, ()> {
        if node.declared_names.iter()
            .find(|e| e.name == "eval")
            .is_some()
        {
            *self.eval_bindings.last_mut()
                .unwrap() = true;
        }
        if *self.eval_bindings.last()
            .unwrap()
        {
            node.has_direct_eval = false;
        }
        Ok(None)
    }
    fn exit_asserted_parameter_scope(&mut self, _path: &Path, node: &mut AssertedParameterScope) -> Result<Option<AssertedParameterScope>, ()> {
        if node.param_names.iter()
            .find(|param| match param {
                AssertedMaybePositionalParameterName::AssertedPositionalParameterName(ref p) => p.name == "eval",
                AssertedMaybePositionalParameterName::AssertedParameterName(ref p) => p.name == "eval",
                AssertedMaybePositionalParameterName::AssertedRestParameterName(ref p) => p.name == "eval",
            })
            .is_some()
        {
            *self.eval_bindings.last_mut()
                .unwrap() = true;
        }
        if *self.eval_bindings.last()
            .unwrap()
        {
            node.has_direct_eval = false;
        }
        Ok(None)
    }
}

impl AnnotationVisitor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn annotate_script(&mut self, script: &mut Script) {
        // Annotate.

        // At this stage, we may have false positives for `hasDirectEval`.
        script.walk(&mut Path::new(), self)
            .expect("Could not walk script");

        // Cleanup false positives for `hasDirectEval`.
        let mut cleanup = EvalCleanupAnnotator {
            eval_bindings: vec![false]
        };
        script.walk(&mut Path::new(), &mut cleanup)
            .expect("Could not walk script for eval cleanup");
    }
    pub fn annotate(&mut self, ast: &mut JSON) {
        // Import script
        let mut script = Script::import(ast)
            .expect("Invalid script"); // FIXME: Error values would be nicer.

        self.annotate_script(&mut script);

        // Reexport the AST to JSON.
        *ast = script.export();
    }
}
