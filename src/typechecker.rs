//! Phase 5: a basic static type checker. This runs BEFORE the interpreter
//! and catches a useful subset of mistakes without doing full Hindley-
//! Milner-style inference (that's future work). It checks:
//!   - undefined variable references
//!   - calls to undefined functions/actors (built-ins and capitalized
//!     message-tag constructors are exempted)
//!   - function call arity mismatches
//!   - obvious literal-vs-declared-type mismatches on `let`
//!
//! This intentionally does NOT implement full tensor ownership/borrow
//! checking (Language Spec §5.2) yet — that needs a dataflow pass this
//! simple scope-walk doesn't have. It's flagged as a known gap below.

use crate::ast::{Expr, Program, Stmt};
use std::collections::{HashMap, HashSet};

/// Ownership state of a tensor-typed binding (Language Spec §5.2):
/// each tensor has exactly one owner at a time; passing it by value
/// (as a plain identifier, not through `borrow()`/`borrow_mut()`) MOVES
/// it, and using a moved-from binding afterward is a compile error.
#[derive(Debug, Clone, PartialEq)]
enum TensorState {
    Owned,
    Moved,
}

#[derive(Debug, Clone)]
pub struct TypeError {
    pub message: String,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

const BUILTINS: &[(&str, usize)] = &[
    ("print", usize::MAX), // variadic
    ("tensor", usize::MAX),
    ("tensor.zeros", 1),
    ("tensor.sum", 1),
    ("len", 1),
    ("str_upper", 1),
    ("str_lower", 1),
    ("str_split", 2),
    ("str_trim", 1),
    ("str_contains", 2),
    ("int_to_str", 1),
    ("str_to_int", 1),
    ("list_push", 2),
    ("list_get", 2),
    ("range_list", 2),
    ("file_read", 1),
    ("file_write", 2),
    ("file_append", 2),
    ("file_exists", 1),
    ("json_stringify", 1),
    ("json_parse", 1),
    ("regex_match", 2),
    ("regex_find", 2),
    ("regex_replace", 3),
    ("net_tcp_send", 3),
    ("map_new", 0),
    ("map_set", 3),
    ("map_get", 2),
    ("map_has", 2),
    ("map_delete", 2),
    ("map_keys", 1),
    ("map_len", 1),
    ("env_get", 1),
    ("env_set", 2),
    ("program_args", 0),
    ("process_exit", 1),
    ("process_run", usize::MAX), // (cmd) or (cmd, args_list)
    ("udp_send", 3),
    ("http_get", 3),
    ("http_post", 4),
    ("http_serve", 3),
    ("net_tcp_send_to_actor", 4),
    ("assert", usize::MAX), // (cond) or (cond, message)
];

struct Checker {
    functions: HashMap<String, usize>, // name -> arity
    actors: HashSet<String>,
    structs: HashMap<String, HashSet<String>>, // struct name -> field names
    enums: HashMap<String, HashSet<String>>,   // enum name -> variant names
    /// The enclosing function's declared return type, if any — used by
    /// `Stmt::Return` checking (literal-vs-declared-type mismatch, same
    /// scope-based approach as `let`'s type-annotation check). `None`
    /// both outside any function AND inside a function with no declared
    /// return type (both cases: nothing to check against).
    current_fn_return_type: Option<String>,
    scopes: Vec<HashMap<String, Option<String>>>, // var -> declared type (if any)
    tensor_scopes: Vec<HashMap<String, TensorState>>, // Language Spec §5.2 ownership tracking
    /// Per-scope list of (source_tensor, is_mut) borrows created in that
    /// scope, so they can be released when the scope pops (simplified
    /// lexical-lifetime model — a borrow lives as long as the `let` that
    /// captured it).
    borrow_scopes: Vec<Vec<(String, bool)>>,
    /// Current totals per tensor: (immutable borrow count, mutable borrow active).
    /// Enforces Rust-style exclusivity: any number of immutable borrows OR
    /// exactly one mutable borrow, never both at once.
    active_borrows: HashMap<String, (u32, bool)>,
    errors: Vec<TypeError>,
}

pub fn check(program: &Program) -> Vec<TypeError> {
    let mut functions = HashMap::new();
    let mut actors = HashSet::new();
    let mut structs: HashMap<String, HashSet<String>> = HashMap::new(); // struct name -> field names
    let mut enums: HashMap<String, HashSet<String>> = HashMap::new(); // enum name -> variant names
    enums.insert("Option".to_string(), ["Some", "None"].iter().map(|s| s.to_string()).collect());
    enums.insert("Result".to_string(), ["Ok", "Err"].iter().map(|s| s.to_string()).collect());

    for stmt in program {
        match stmt {
            Stmt::FnDef { name, params, .. } => {
                functions.insert(name.clone(), params.len());
            }
            Stmt::Actor { name, .. } => {
                actors.insert(name.clone());
            }
            Stmt::StructDef { name, fields } => {
                structs.insert(name.clone(), fields.iter().map(|f| f.name.clone()).collect());
            }
            Stmt::EnumDef { name, variants } => {
                enums.insert(name.clone(), variants.iter().map(|v| v.name.clone()).collect());
            }
            _ => {}
        }
    }

    let mut checker = Checker {
        functions,
        actors,
        structs,
        enums,
        current_fn_return_type: None,
        scopes: vec![HashMap::new()],
        tensor_scopes: vec![HashMap::new()],
        borrow_scopes: vec![Vec::new()],
        active_borrows: HashMap::new(),
        errors: Vec::new(),
    };

    for stmt in program {
        checker.check_stmt(stmt);
    }

    checker.errors
}

impl Checker {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.tensor_scopes.push(HashMap::new());
        self.borrow_scopes.push(Vec::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.tensor_scopes.pop();
        if let Some(released) = self.borrow_scopes.pop() {
            for (source, is_mut) in released {
                self.release_borrow(&source, is_mut);
            }
        }
    }
    fn declare(&mut self, name: &str, ty: Option<String>) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
    }
    fn is_declared(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|s| s.contains_key(name))
    }
    fn err(&mut self, message: impl Into<String>) {
        self.errors.push(TypeError { message: message.into() });
    }

    /// Declares a NEW tensor binding (from `tensor(...)`/`tensor.zeros(...)`
    /// or as the destination of a move) as `Owned` in the innermost scope.
    fn declare_tensor_owned(&mut self, name: &str) {
        self.tensor_scopes.last_mut().unwrap().insert(name.to_string(), TensorState::Owned);
    }
    /// True if `name` is currently tracked as a tensor binding anywhere
    /// in the visible scope chain.
    fn is_tensor(&self, name: &str) -> bool {
        self.tensor_scopes.iter().rev().any(|s| s.contains_key(name))
    }
    fn tensor_state(&self, name: &str) -> Option<TensorState> {
        for scope in self.tensor_scopes.iter().rev() {
            if let Some(state) = scope.get(name) {
                return Some(state.clone());
            }
        }
        None
    }
    /// Marks `name`'s tensor as moved-out-of in whichever scope actually
    /// owns it (searching outward, matching normal lexical scoping).
    fn mark_moved(&mut self, name: &str) {
        for scope in self.tensor_scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), TensorState::Moved);
                return;
            }
        }
    }

    /// Language Spec §5.2 enforcement: if `expr` is a bare identifier
    /// referring to a currently-owned tensor, using it here MOVES it
    /// (ownership transfer — ties into the Compiler doc §2's "sending a
    /// tensor between actors moves it" rule too, since `send()` message
    /// expressions go through this same helper). Flags use-after-move.
    /// `context` is used only for the error message (e.g. "argument",
    /// "let-binding value", "send() message").
    fn check_tensor_move(&mut self, expr: &Expr, context: &str) {
        if let Expr::Ident(name) = expr {
            match self.tensor_state(name) {
                Some(TensorState::Moved) => {
                    self.err(format!(
                        "use of moved tensor '{}' as {} — it was already moved to a new owner earlier (Language Spec §5.2: a tensor has exactly one owner at a time)",
                        name, context
                    ));
                }
                Some(TensorState::Owned) => {
                    if self.has_active_borrow(name) {
                        self.err(format!(
                            "cannot move '{}' as {} while it is still borrowed (Language Spec §5.2: the owner can't move a value out from under an active borrow)",
                            name, context
                        ));
                    } else {
                        self.mark_moved(name);
                    }
                }
                None => {} // not a tracked tensor binding — nothing to move
            }
        }
    }

    /// True if `expr` is a bare identifier referring to a tracked tensor
    /// (used to decide whether a call-site/send-site needs move checking).
    fn is_tensor_ident(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Ident(name) if self.is_tensor(name))
    }

    /// True if `name` currently has any active borrow (mutable or
    /// immutable) — a moved-tensor check consults this to forbid moving
    /// out from under a live borrow (Language Spec §5.2/§5.3).
    fn has_active_borrow(&self, name: &str) -> bool {
        matches!(self.active_borrows.get(name), Some((count, is_mut)) if *count > 0 || *is_mut)
    }

    /// Validates that creating a new borrow of `source` (mutable or not)
    /// doesn't violate exclusivity: any number of immutable borrows can
    /// coexist, but a mutable borrow must be the ONLY borrow active.
    fn check_borrow_conflict(&mut self, source: &str, want_mut: bool) -> bool {
        let (immut_count, mut_active) = self.active_borrows.get(source).copied().unwrap_or((0, false));
        if want_mut {
            if immut_count > 0 || mut_active {
                self.err(format!(
                    "cannot borrow_mut('{}') — it is already borrowed elsewhere (Language Spec §5.2: a mutable borrow must be exclusive)",
                    source
                ));
                return false;
            }
        } else if mut_active {
            self.err(format!(
                "cannot borrow('{}') — it is currently mutably borrowed elsewhere (Language Spec §5.2: no other borrows allowed while a mutable borrow is active)",
                source
            ));
            return false;
        }
        true
    }

    /// Registers a borrow that outlives the single expression it was
    /// created in (i.e. `let r = borrow(x)`, as opposed to an inline
    /// `f(borrow(x))` temporary that releases immediately). Tied to the
    /// CURRENT scope so it's automatically released on `pop_scope`.
    fn register_persistent_borrow(&mut self, source: &str, is_mut: bool) {
        let entry = self.active_borrows.entry(source.to_string()).or_insert((0, false));
        if is_mut {
            entry.1 = true;
        } else {
            entry.0 += 1;
        }
        self.borrow_scopes.last_mut().unwrap().push((source.to_string(), is_mut));
    }

    fn release_borrow(&mut self, source: &str, is_mut: bool) {
        if let Some(entry) = self.active_borrows.get_mut(source) {
            if is_mut {
                entry.1 = false;
            } else {
                entry.0 = entry.0.saturating_sub(1);
            }
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FnDef { params, ret_type, body, .. } => {
                self.push_scope();
                for p in params {
                    self.declare(&p.name, p.type_ann.clone());
                    if p.type_ann.as_deref() == Some("tensor") {
                        self.declare_tensor_owned(&p.name);
                    }
                }
                let outer_ret_type = self.current_fn_return_type.take();
                self.current_fn_return_type = ret_type.clone();
                for s in body {
                    self.check_stmt(s);
                }
                self.current_fn_return_type = outer_ret_type;
                self.pop_scope();
            }
            Stmt::Actor { state_vars, param_name, param_type, body, .. } => {
                self.push_scope();
                for (sname, stype, sinit) in state_vars {
                    self.check_expr(sinit);
                    self.declare(sname, stype.clone());
                }
                self.declare(param_name, param_type.clone());
                for s in body {
                    self.check_stmt(s);
                }
                self.pop_scope();
            }
            Stmt::Let { name, type_ann, value, .. } => {
                // Tensor-constructing calls declare a fresh Owned tensor.
                let is_tensor_ctor = matches!(
                    value,
                    Expr::Call(fname, _) if fname == "tensor" || fname == "tensor.zeros"
                );
                // `let r = borrow(x)` / `let r = borrow_mut(x)`: a
                // PERSISTENT borrow that outlives this statement (unlike
                // an inline `f(borrow(x))` temporary), tracked until `r`'s
                // scope ends.
                let borrow_call = match value {
                    Expr::Call(fname, args) if (fname == "borrow" || fname == "borrow_mut") && args.len() == 1 => {
                        if let Expr::Ident(src) = &args[0] {
                            Some((src.clone(), fname == "borrow_mut"))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                if is_tensor_ctor {
                    self.check_expr(value); // still validate args (e.g. undefined vars in shape list)
                    self.declare_tensor_owned(name);
                } else if let Some((src, is_mut)) = borrow_call {
                    if self.is_tensor(&src) {
                        if self.check_borrow_conflict(&src, is_mut) {
                            self.register_persistent_borrow(&src, is_mut);
                        }
                    } else {
                        self.err(format!("borrow()/borrow_mut() target '{}' is not a tensor", src));
                    }
                } else if let Expr::Ident(src) = value {
                    // `let y = x` where x is a tracked tensor: this is a
                    // MOVE, not a copy (Language Spec §5.2).
                    if self.is_tensor(src) {
                        self.check_tensor_move(value, "let-binding value");
                        self.declare_tensor_owned(name);
                    } else {
                        self.check_expr(value);
                    }
                } else {
                    self.check_expr(value);
                }
                if let Some(declared) = type_ann {
                    if let Some(inferred) = literal_type(value) {
                        if &inferred != declared {
                            self.err(format!(
                                "type mismatch: 'let {}: {}' but value is {}",
                                name, declared, inferred
                            ));
                        }
                    }
                }
                self.declare(name, type_ann.clone());
            }
            Stmt::Assign { name, value } => {
                if !self.is_declared(name) {
                    self.err(format!("assignment to undeclared variable '{}'", name));
                }
                self.check_expr(value);
            }
            Stmt::IndexAssign { base, index, value } => {
                self.check_expr(base);
                self.check_expr(index);
                self.check_expr(value);
            }
            Stmt::FieldAssign { base, value, .. } => {
                self.check_expr(base);
                self.check_expr(value);
            }
            Stmt::Send { target, message } => {
                self.check_expr(target);
                // Sending a tensor moves it to the receiving actor
                // (Stdlib/AI Integration doc §1.4 — zero-copy handoff;
                // Compiler doc §2 — messages carrying tensors transfer
                // ownership rather than sharing memory).
                if self.is_tensor_ident(message) {
                    self.check_tensor_move(message, "send() message");
                } else {
                    self.check_expr(message);
                }
            }
            Stmt::Raise(e) => {
                self.check_expr(e);
            }
            Stmt::Try { body, catch_var, catch_body } => {
                self.push_scope();
                for s in body {
                    self.check_stmt(s);
                }
                self.pop_scope();
                self.push_scope();
                self.declare(catch_var, None);
                for s in catch_body {
                    self.check_stmt(s);
                }
                self.pop_scope();
            }
            Stmt::Import(_) => {
                // Resolved/spliced by main.rs before type-checking runs.
            }
            Stmt::Supervisor { children, .. } => {
                for c in children {
                    if !self.actors.contains(&c.actor_name) {
                        self.err(format!(
                            "supervisor child references undefined actor '{}'",
                            c.actor_name
                        ));
                    }
                    self.declare(&c.bind_name, Some("actor_handle".to_string()));
                }
            }
            Stmt::StructDef { .. } | Stmt::EnumDef { .. } => {
                // Already registered in the pre-scan pass (see `check()`).
            }
            Stmt::ExprStmt(e) => self.check_expr(e),
            Stmt::If { cond, then_block, elif_blocks, else_block } => {
                self.check_expr(cond);
                self.push_scope();
                for s in then_block {
                    self.check_stmt(s);
                }
                self.pop_scope();
                for (c, block) in elif_blocks {
                    self.check_expr(c);
                    self.push_scope();
                    for s in block {
                        self.check_stmt(s);
                    }
                    self.pop_scope();
                }
                if let Some(block) = else_block {
                    self.push_scope();
                    for s in block {
                        self.check_stmt(s);
                    }
                    self.pop_scope();
                }
            }
            Stmt::Loop { var, start, end, body } => {
                self.check_expr(start);
                self.check_expr(end);
                self.push_scope();
                self.declare(var, Some("int".to_string()));
                for s in body {
                    self.check_stmt(s);
                }
                self.pop_scope();
            }
            Stmt::While { cond, body } => {
                self.check_expr(cond);
                self.push_scope();
                for s in body {
                    self.check_stmt(s);
                }
                self.pop_scope();
            }
            Stmt::Return(val) => {
                if let Some(e) = val {
                    if self.is_tensor_ident(e) {
                        self.check_tensor_move(e, "return value");
                    } else {
                        self.check_expr(e);
                    }
                    if let Some(declared) = self.current_fn_return_type.clone() {
                        if let Some(inferred) = literal_type(e) {
                            if inferred != declared {
                                self.err(format!(
                                    "return type mismatch: function declared to return '{}' but this `return` gives {}",
                                    declared, inferred
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StrLit(_) | Expr::BoolLit(_) => {}
            Expr::ListLit(items) => {
                for e in items {
                    self.check_expr(e);
                }
            }
            Expr::Ident(name) => {
                if !self.is_declared(name) {
                    self.err(format!("undefined variable '{}'", name));
                }
            }
            Expr::Binary(lhs, _, rhs) => {
                self.check_expr(lhs);
                self.check_expr(rhs);
            }
            Expr::Unary(_, e) => {
                self.check_expr(e);
            }
            Expr::StructLit(type_name, fields) => {
                if let Some(known_fields) = self.structs.get(type_name).cloned() {
                    for (fname, _) in fields {
                        if !known_fields.contains(fname) {
                            self.err(format!("struct '{}' has no field '{}'", type_name, fname));
                        }
                    }
                    for kf in &known_fields {
                        if !fields.iter().any(|(fname, _)| fname == kf) {
                            self.err(format!("struct literal for '{}' is missing field '{}'", type_name, kf));
                        }
                    }
                } else {
                    self.err(format!("use of undefined struct '{}'", type_name));
                }
                for (_, fvalue) in fields {
                    self.check_expr(fvalue);
                }
            }
            Expr::FieldAccess(base, _field) => {
                // Field-name validation against the struct's known fields
                // would need real type inference on `base` (we'd have to
                // know its static type, not just check it's some
                // expression) — out of scope for this checker's current
                // design (see typechecker.rs's module doc comment on
                // scope-based, not fully-inferred, checking). We still
                // recurse into `base` so undefined-variable errors etc.
                // inside it are caught.
                self.check_expr(base);
            }
            Expr::Index(base, index) => {
                self.check_expr(base);
                self.check_expr(index);
            }
            Expr::EnumLit(type_name, variant, args) => {
                match self.enums.get(type_name) {
                    Some(known_variants) => {
                        if !known_variants.contains(variant) {
                            self.err(format!("enum '{}' has no variant '{}'", type_name, variant));
                        }
                    }
                    None => self.err(format!("use of undefined enum '{}'", type_name)),
                }
                for a in args {
                    self.check_expr(a);
                }
            }
            Expr::Closure(params, body) => {
                self.push_scope();
                for p in params {
                    self.declare(&p.name, p.type_ann.clone());
                }
                self.check_expr(body);
                self.pop_scope();
            }
            Expr::Await(e) => {
                self.check_expr(e);
            }
            Expr::EnumIsVariant(subject, type_name, variant) => {
                self.check_expr(subject);
                match self.enums.get(type_name) {
                    Some(known_variants) => {
                        if !known_variants.contains(variant) {
                            self.err(format!("enum '{}' has no variant '{}'", type_name, variant));
                        }
                    }
                    None => self.err(format!("use of undefined enum '{}' in match pattern", type_name)),
                }
            }
            Expr::EnumPayload(subject, type_name, variant, _idx) => {
                self.check_expr(subject);
                match self.enums.get(type_name) {
                    Some(known_variants) => {
                        if !known_variants.contains(variant) {
                            self.err(format!("enum '{}' has no variant '{}'", type_name, variant));
                        }
                        // Payload-count/index validation against the
                        // variant's declared payload arity would need
                        // `EnumVariant.payload_names` threaded into this
                        // checker's `enums` map (currently just variant
                        // names) — a reasonable, cheap follow-up, not
                        // done here yet (documented gap, not a silent one).
                    }
                    None => self.err(format!("use of undefined enum '{}' in match pattern", type_name)),
                }
            }
            Expr::Spawn(name, args) => {
                if !self.actors.contains(name) {
                    self.err(format!("spawn of undefined actor '{}'", name));
                }
                for a in args {
                    self.check_expr(a);
                }
            }
            Expr::Call(name, args) => {
                if name == "borrow" || name == "borrow_mut" {
                    // Inline/temporary borrow (e.g. `inspect(borrow(x))`):
                    // validate exclusivity at creation time, but don't
                    // register it persistently — it's released as soon as
                    // this call expression finishes evaluating.
                    if args.len() == 1 {
                        if let Expr::Ident(src) = &args[0] {
                            if self.is_tensor(src) {
                                self.check_borrow_conflict(src, name == "borrow_mut");
                            } else {
                                self.err(format!("borrow()/borrow_mut() target '{}' is not a tensor", src));
                            }
                        } else {
                            self.check_expr(&args[0]);
                        }
                    }
                    return;
                }
                let is_borrow_fn = name == "print";
                for a in args {
                    if !is_borrow_fn && self.is_tensor_ident(a) {
                        // Passing a tensor by bare identifier moves it.
                        self.check_tensor_move(a, "function argument");
                    } else {
                        self.check_expr(a);
                    }
                }
                if is_borrow_fn {
                    return;
                }
                if let Some((_, arity)) = BUILTINS.iter().find(|(n, _)| n == name) {
                    if *arity != usize::MAX && *arity != args.len() {
                        self.err(format!(
                            "'{}' expects {} argument(s), got {}",
                            name, arity, args.len()
                        ));
                    }
                    return;
                }
                // Capitalized name = message-tag constructor, always OK.
                if name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return;
                }
                match self.functions.get(name) {
                    Some(arity) if *arity != args.len() => {
                        self.err(format!(
                            "'{}' expects {} argument(s), got {}",
                            name, arity, args.len()
                        ));
                    }
                    Some(_) => {}
                    None => {
                        // Could be a local variable holding a closure
                        // (`let f = fn(x) => ...; f(5)`) — we don't do
                        // full type inference here, so we can't check
                        // its arity, but we CAN at least confirm it's a
                        // real declared name rather than a typo.
                        if !self.is_declared(name) {
                            self.err(format!("call to undefined function '{}'", name));
                        }
                    }
                }
            }
        }
    }
}

fn literal_type(expr: &Expr) -> Option<String> {
    match expr {
        Expr::IntLit(_) => Some("int".to_string()),
        Expr::FloatLit(_) => Some("float".to_string()),
        Expr::StrLit(_) => Some("string".to_string()),
        Expr::BoolLit(_) => Some("bool".to_string()),
        _ => None, // calls/idents/binary exprs: not statically known here
    }
}
