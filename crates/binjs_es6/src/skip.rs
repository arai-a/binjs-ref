use ast::*;

use binjs_shared::{ Offset, VisitMe };

use std;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
struct LevelGuard {
    level: Rc<RefCell<u32>>
}
impl LevelGuard {
    fn new(owner: &LazifierVisitor) -> Self {
        let result = Self {
            level: owner.level.clone(),
        };
        *result.level.borrow_mut() += 1;
        result
    }
}
impl Drop for LevelGuard {
    fn drop(&mut self) {
        *self.level.borrow_mut() -= 1;
    }
}

/// A visitor in charge of rewriting an AST to introduce laziness.
pub struct LazifierVisitor {
    /// A nesting level at which to stop.
    ///
    /// 0 = lazify nothing
    /// 1 = lazify functions defined at topevel
    /// 2 = lazify functions defined at toplevel and functions defined immediately inside them
    /// ...
    threshold: u32,

    /// Current nesting level.
    ///
    /// Increased by one every time we enter a function/method/...
    level: Rc<RefCell<u32>>,
}

impl LazifierVisitor {
    fn steal<T: Default, F, U>(source: &mut T, decorator: F) -> Result<Option<U>, ()>
        where F: FnOnce(T) -> U
    {
        // FIXME: We could swap in some unitialized memory and ensure that it is forgotten.
        let mut stolen = T::default();
        std::mem::swap(source, &mut stolen);
        Ok(Some(decorator(stolen)))
    }

    fn cut_at_threshold(&mut self) -> Result<VisitMe<LevelGuard>, ()> {
        if *self.level.borrow() >= self.threshold {
            return Ok(VisitMe::DoneHere);
        }
        Ok(VisitMe::HoldThis(LevelGuard::new(self)))
    }
}

impl Visitor<(), LevelGuard> for LazifierVisitor {
    // Functions, methods, arguments.
    fn enter_method_definition(&mut self, _path: &Path, _node: &mut MethodDefinition) -> Result<VisitMe<LevelGuard>, ()> {
        self.cut_at_threshold()
    }

    fn exit_method_definition(&mut self, _path: &Path, node: &mut MethodDefinition) -> Result<Option<MethodDefinition>, ()> {
        // If we reach this point, convert eager getter, setter, method to skippable.
        match *node {
            MethodDefinition::EagerGetter(box ref mut steal) => {
                Self::steal(steal, |stolen| {
                    SkippableGetter {
                        offset: Offset::default(),
                        skipped: stolen,
                    }.into()
                })
            }
            MethodDefinition::EagerSetter(box ref mut steal) => {
                Self::steal(steal, |stolen| {
                    SkippableSetter {
                        offset: Offset::default(),
                        skipped: stolen,
                    }.into()
                })
            }
            MethodDefinition::EagerMethod(box ref mut steal) => {
                Self::steal(steal, |stolen| {
                    SkippableMethod {
                        offset: Offset::default(),
                        skipped: stolen,
                    }.into()
                })
            }
            _ => Ok(None)
        }
    }

    fn enter_function_declaration(&mut self, _path: &Path, _node: &mut FunctionDeclaration) -> Result<VisitMe<LevelGuard>, ()> {
        self.cut_at_threshold()
    }
    fn exit_function_declaration(&mut self, _path: &Path, node: &mut FunctionDeclaration) -> Result<Option<FunctionDeclaration>, ()> {
        match *node {
            FunctionDeclaration::EagerFunctionDeclaration(box ref mut steal) => {
                Self::steal(steal, |stolen| {
                    SkippableFunctionDeclaration {
                        offset: Offset::default(),
                        skipped: stolen,
                    }.into()
                })
            }
            _ => Ok(None)
        }
    }

    fn enter_function_expression(&mut self, _path: &Path, _node: &mut FunctionExpression) -> Result<VisitMe<LevelGuard>, ()> {
        self.cut_at_threshold()
    }
    fn exit_function_expression(&mut self, path: &Path, node: &mut FunctionExpression) -> Result<Option<FunctionExpression>, ()> {
        // Don't lazify code that's going to be used immediately.
        if let Some(PathItem { interface: ASTNode::CallExpression, field: ASTField::Callee }) = path.get(0) {
            return Ok(None)
        }
        match *node {
            FunctionExpression::EagerFunctionExpression(box ref mut steal) => {
                Self::steal(steal, |stolen| {
                    SkippableFunctionExpression {
                        offset: Offset::default(),
                        skipped: stolen,
                    }.into()
                })
            }
            _ => Ok(None)
        }
    }
}