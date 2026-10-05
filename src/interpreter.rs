//! Phase 3 (tree-walking interpreter) + Phase 4 (actor runtime).
//!
//! Design notes matching the Memory Model (Language Spec §5) and the
//! Actor/Process Model (Compiler doc §2):
//!   - Each `actor` spawned gets its OWN OS thread and its OWN isolated
//!     variable scope — no shared mutable state with the parent. This is
//!     a direct (simplified) implementation of the "isolated heap"
//!     requirement, using Rust's ownership system to enforce it: actor
//!     threads only receive Values that are explicitly `send()`-ed to
//!     them (moved across an mpsc channel), never a live reference into
//!     the spawner's environment.
//!   - Global function + actor definitions are read-only after program
//!     start, so they are shared across threads via `Arc` (cheap,
//!     immutable sharing — this does not violate the isolation rule,
//!     since immutable code definitions aren't mutable state).
//!   - `spawn` returns a lightweight `ActorHandle`; `send` pushes a Value
//!     onto that actor's mailbox (a real `mpsc::Sender<Value>`).

use crate::ast::{BinOp, Expr, Program, RestartStrategy, Stmt};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(Vec<Value>),
    Tensor { data: Vec<f64>, shape: Vec<usize> },
    Message { tag: String, args: Vec<Value> },
    ActorHandle(usize),
    /// Struct instance: `Arc<Mutex<..>>` gives us real heap allocation +
    /// reference semantics (assigning/passing a struct shares the SAME
    /// underlying storage, matching Python/Java object semantics) +
    /// automatic reclamation via reference counting once the last Arc
    /// drops — this IS the "memory model / GC" deliverable, honestly:
    /// simple refcounting, not a tracing/cycle-collecting GC. `Arc`
    /// (not `Rc`) specifically because Values cross actor-thread
    /// boundaries via mailboxes, which requires `Send`.
    Struct(String, std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>),
    /// Native HashMap (Phase 2: Data Structures). Same `Arc<Mutex<>>`
    /// reference-counted heap model as `Struct` — see that variant's
    /// doc comment for why (real reference semantics + automatic
    /// reclamation, thread-safe for actor mailbox transport).
    Map(std::sync::Arc<std::sync::Mutex<HashMap<String, Value>>>),
    /// Enum instance: type name, variant name, positional payload values.
    Enum(String, String, Vec<Value>),
    /// Closure: captured environment is an immutable snapshot taken at
    /// creation time (no mutable upvalues) — a deliberate simplification
    /// that avoids needing `Arc<Mutex<Env>>` shared-mutable-capture
    /// machinery while still giving real lexical closure semantics for
    /// the common case (reading captured values).
    Closure(std::sync::Arc<ClosureData>),
    /// The result of calling an `async fn`. See the module doc comment's
    /// "Async model" note for what this does and doesn't do.
    Future(Box<Value>),
    Unit,
}

#[derive(Debug)]
pub struct ClosureData {
    pub params: Vec<String>,
    pub body: Expr,
    pub captured_env: Vec<HashMap<String, Value>>,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{}", n),
            Value::Float(x) => write!(f, "{}", x),
            Value::Str(s) => write!(f, "{}", s),
            Value::Bool(b) => write!(f, "{}", b),
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.to_string()).collect();
                write!(f, "[{}]", parts.join(", "))
            }
            Value::Tensor { data, shape } => {
                write!(f, "tensor(shape={:?}, data={:?})", shape, data)
            }
            Value::Message { tag, args } => {
                let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                write!(f, "{}({})", tag, parts.join(", "))
            }
            Value::ActorHandle(id) => write!(f, "<actor #{}>", id),
            Value::Struct(name, fields) => {
                let guard = fields.lock().unwrap();
                let mut parts: Vec<String> = guard.iter().map(|(k, v)| format!("{}: {}", k, v)).collect();
                parts.sort();
                write!(f, "{} {{ {} }}", name, parts.join(", "))
            }
            Value::Map(entries) => {
                let guard = entries.lock().unwrap();
                let mut parts: Vec<String> = guard.iter().map(|(k, v)| format!("\"{}\": {}", k, v)).collect();
                parts.sort();
                write!(f, "{{ {} }}", parts.join(", "))
            }
            Value::Enum(type_name, variant, args) => {
                if args.is_empty() {
                    write!(f, "{}.{}", type_name, variant)
                } else {
                    let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                    write!(f, "{}.{}({})", type_name, variant, parts.join(", "))
                }
            }
            Value::Closure(_) => write!(f, "<closure>"),
            Value::Future(inner) => write!(f, "<future: {}>", inner),
            Value::Unit => write!(f, "()"),
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    UndefinedVariable(String),
    UndefinedFunction(String),
    UndefinedActor(String),
    TypeError(String),
    ArityMismatch { name: String, expected: usize, got: usize },
    /// A user `raise <expr>` — carries the raised Value so `catch` can
    /// bind it directly (not just a stringified message).
    UserRaised(Value),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::UndefinedVariable(n) => write!(f, "undefined variable '{}'", n),
            RuntimeError::UndefinedFunction(n) => write!(f, "undefined function '{}'", n),
            RuntimeError::UndefinedActor(n) => write!(f, "undefined actor '{}'", n),
            RuntimeError::TypeError(msg) => write!(f, "type error: {}", msg),
            RuntimeError::ArityMismatch { name, expected, got } => write!(
                f,
                "'{}' expects {} argument(s), got {}",
                name, expected, got
            ),
            RuntimeError::UserRaised(v) => write!(f, "raised: {}", v),
        }
    }
}

type RResult<T> = Result<T, RuntimeError>;

#[derive(Clone)]
struct FnDef {
    params: Vec<String>,
    body: Vec<Stmt>,
    is_async: bool,
}

#[derive(Clone)]
struct ActorDef {
    state_vars: Vec<(String, Expr)>, // (field name, init expr) — type_ann dropped, not needed at runtime
    param_name: String,
    body: Vec<Stmt>,
}

/// Global, read-only-after-startup program definitions, shared across
/// actor threads via Arc (immutable sharing — not a violation of actor
/// memory isolation, since it's code, not mutable state).
struct Globals {
    functions: HashMap<String, FnDef>,
    actors: HashMap<String, ActorDef>,
    /// enum name -> variant names IN DECLARATION ORDER. Needed so
    /// `enum_tag(e)` can return a stable positional index (matches the
    /// codegen module's tagged-union representation — see codegen.rs's
    /// `EnumTypeInfo`).
    enums: HashMap<String, Vec<String>>,
}

/// Shared runtime services needed to spawn actors and route messages.
struct RuntimeShared {
    globals: Arc<Globals>,
    mailboxes: Mutex<HashMap<usize, Sender<Value>>>,
    next_actor_id: AtomicUsize,
    handles: Mutex<Vec<thread::JoinHandle<()>>>,
    /// One `Sender` per live supervisor coordinator. Used by
    /// `shutdown_actors` to tell every coordinator to wind down directly
    /// — this is race-free even if a crash/restart is in flight, unlike
    /// relying solely on the per-actor mailbox broadcast (see the
    /// `GlobalShutdown` handling in `run_supervisor_coordinator`).
    supervisor_shutdown_senders: Mutex<Vec<Sender<SupervisorEvent>>>,
}

/// Control-flow signal used internally to unwind out of loops/blocks on `return`.
enum Flow {
    Normal,
    Return(Value),
}

/// Events a supervised worker thread reports back to its coordinator
/// (Compiler doc §2.3 — crash handling flow).
enum SupervisorEvent {
    Crashed(usize),       // child index crashed (unhandled RuntimeError in a handler)
    ShutdownAck(usize),   // child index acknowledged a graceful __shutdown__
    GlobalShutdown,       // program is exiting; coordinator should wind down all children
}

pub struct Interpreter {
    runtime: Arc<RuntimeShared>,
    scopes: Vec<HashMap<String, Value>>,
    /// Recursion-depth guard: a naive tree-walking interpreter uses one
    /// real OS stack frame per Tridentix-level function call, so unbounded
    /// recursion eventually hits the OS stack limit and ABORTS THE WHOLE
    /// PROCESS (`fatal runtime error: stack overflow`) with no chance to
    /// report a clean error. This counter turns that into a normal,
    /// catchable `RuntimeError` well before the real limit.
    call_depth: usize,
}

/// Comfortably below typical OS thread stack sizes (8MB default on
/// Linux) for this interpreter's per-frame stack usage.
const MAX_CALL_DEPTH: usize = 300;

impl Interpreter {
    pub fn new(program: &Program) -> RResult<Self> {
        let mut functions = HashMap::new();
        let mut actors = HashMap::new();
        let mut enums = HashMap::new();
        // Prelude: Option/Result are always available without an
        // explicit `enum` declaration, same as most real languages'
        // built-in sum types (Rust's Option/Result, Swift's Optional).
        enums.insert("Option".to_string(), vec!["Some".to_string(), "None".to_string()]);
        enums.insert("Result".to_string(), vec!["Ok".to_string(), "Err".to_string()]);

        for stmt in program {
            match stmt {
                Stmt::FnDef { name, params, body, is_async, .. } => {
                    functions.insert(
                        name.clone(),
                        FnDef {
                            params: params.iter().map(|p| p.name.clone()).collect(),
                            body: body.clone(),
                            is_async: *is_async,
                        },
                    );
                }
                Stmt::Actor { name, state_vars, param_name, body, .. } => {
                    actors.insert(
                        name.clone(),
                        ActorDef {
                            state_vars: state_vars.iter().map(|(n, _, e)| (n.clone(), e.clone())).collect(),
                            param_name: param_name.clone(),
                            body: body.clone(),
                        },
                    );
                }
                Stmt::EnumDef { name, variants } => {
                    enums.insert(name.clone(), variants.iter().map(|v| v.name.clone()).collect());
                }
                _ => {}
            }
        }

        let runtime = Arc::new(RuntimeShared {
            globals: Arc::new(Globals { functions, actors, enums }),
            mailboxes: Mutex::new(HashMap::new()),
            next_actor_id: AtomicUsize::new(0),
            handles: Mutex::new(Vec::new()),
            supervisor_shutdown_senders: Mutex::new(Vec::new()),
        });

        Ok(Interpreter {
            runtime,
            scopes: vec![HashMap::new()],
            call_depth: 0,
        })
    }

    /// Runs `main()` if present, then any other top-level expression
    /// statements, then shuts down and joins all spawned actor threads.
    pub fn run(&mut self, program: &Program) -> RResult<()> {
        for stmt in program {
            if let Stmt::FnDef { name, .. } = stmt {
                if name == "main" {
                    self.call_function("main", vec![])?;
                }
            }
        }
        self.shutdown_actors();
        Ok(())
    }

    fn shutdown_actors(&mut self) {
        let mailboxes = self.runtime.mailboxes.lock().unwrap();
        for sender in mailboxes.values() {
            let _ = sender.send(Value::Message {
                tag: "__shutdown__".into(),
                args: vec![],
            });
        }
        drop(mailboxes);

        // Also tell every supervisor coordinator directly. This is the
        // race-free path: a coordinator might be mid-restart (its child's
        // mailbox entry briefly points at a not-yet-live thread), so the
        // plain per-actor broadcast above isn't guaranteed to reach a
        // supervised child in time. The coordinator itself always has an
        // up-to-date view of its own children's current mailbox ids.
        let sup_senders = self.runtime.supervisor_shutdown_senders.lock().unwrap();
        for sender in sup_senders.iter() {
            let _ = sender.send(SupervisorEvent::GlobalShutdown);
        }
        drop(sup_senders);

        let mut handles = self.runtime.handles.lock().unwrap();
        for h in handles.drain(..) {
            let _ = h.join();
        }
    }

    // ---- Scope helpers ----

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    fn define(&mut self, name: &str, value: Value) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), value);
    }
    fn assign(&mut self, name: &str, value: Value) -> RResult<()> {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return Ok(());
            }
        }
        Err(RuntimeError::UndefinedVariable(name.to_string()))
    }
    fn lookup(&self, name: &str) -> RResult<Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Ok(v.clone());
            }
        }
        Err(RuntimeError::UndefinedVariable(name.to_string()))
    }

    // ---- Statement execution ----

    fn exec_block(&mut self, block: &[Stmt]) -> RResult<Flow> {
        for stmt in block {
            match self.exec_stmt(stmt)? {
                Flow::Normal => {}
                flow @ Flow::Return(_) => return Ok(flow),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &Stmt) -> RResult<Flow> {
        match stmt {
            Stmt::FnDef { .. } | Stmt::Actor { .. } | Stmt::StructDef { .. } | Stmt::EnumDef { .. } => {
                Ok(Flow::Normal) // already registered globally / don't need runtime state
            }
            Stmt::Supervisor { name, strategy, max_restarts, time_window_secs, children } => {
                let bindings = self.spawn_supervisor(name, strategy.clone(), *max_restarts, *time_window_secs, children)?;
                for (bind_name, handle) in bindings {
                    self.define(&bind_name, handle);
                }
                Ok(Flow::Normal)
            }
            Stmt::Let { name, value, .. } => {
                let v = self.eval(value)?;
                self.define(name, v);
                Ok(Flow::Normal)
            }
            Stmt::Assign { name, value } => {
                let v = self.eval(value)?;
                self.assign(name, v)?;
                Ok(Flow::Normal)
            }
            Stmt::IndexAssign { base, index, value } => {
                let idx = self.eval(index)?.as_int()?;
                let new_val = self.eval(value)?;
                let base_name = match base {
                    Expr::Ident(n) => n.clone(),
                    _ => {
                        return Err(RuntimeError::TypeError(
                            "index-assignment target must be a plain variable, e.g. `list[i] = x` (not a computed expression)".into(),
                        ))
                    }
                };
                let mut current = self.lookup(&base_name)?;
                match &mut current {
                    Value::List(items) => {
                        if idx < 0 || idx as usize >= items.len() {
                            return Err(RuntimeError::TypeError(format!(
                                "index {} out of bounds (length {}) assigning to '{}'",
                                idx, items.len(), base_name
                            )));
                        }
                        items[idx as usize] = new_val;
                    }
                    other => {
                        return Err(RuntimeError::TypeError(format!("cannot index-assign into {}", other)))
                    }
                }
                self.assign(&base_name, current)?;
                Ok(Flow::Normal)
            }
            Stmt::FieldAssign { base, field, value } => {
                let base_val = self.eval(base)?;
                let new_val = self.eval(value)?;
                match base_val {
                    Value::Struct(type_name, fields) => {
                        let mut guard = fields.lock().unwrap();
                        if !guard.contains_key(field) {
                            return Err(RuntimeError::TypeError(format!(
                                "struct '{}' has no field '{}'",
                                type_name, field
                            )));
                        }
                        guard.insert(field.clone(), new_val);
                        Ok(Flow::Normal)
                    }
                    other => Err(RuntimeError::TypeError(format!(
                        "cannot assign field '.{}' on non-struct value {}",
                        field, other
                    ))),
                }
            }
            Stmt::ExprStmt(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::Send { target, message } => {
                let target_val = self.eval(target)?;
                let msg_val = self.eval(message)?;
                let id = match target_val {
                    Value::ActorHandle(id) => id,
                    other => {
                        return Err(RuntimeError::TypeError(format!(
                            "send() target must be an actor handle, got {}",
                            other
                        )))
                    }
                };
                let mailboxes = self.runtime.mailboxes.lock().unwrap();
                if let Some(sender) = mailboxes.get(&id) {
                    let _ = sender.send(msg_val);
                }
                Ok(Flow::Normal)
            }
            Stmt::Raise(e) => {
                let v = self.eval(e)?;
                Err(RuntimeError::UserRaised(v))
            }
            Stmt::Try { body, catch_var, catch_body } => {
                self.push_scope();
                let result = self.exec_block(body);
                self.pop_scope();
                match result {
                    Ok(flow) => Ok(flow),
                    Err(err) => {
                        // Any runtime error is catchable, not just an
                        // explicit `raise` — matches real-language
                        // try/catch (Python `except Exception`, JS
                        // `catch (e)`), so a caller doesn't need to
                        // predict every failure mode by name.
                        let bound = match err {
                            RuntimeError::UserRaised(v) => v,
                            other => Value::Str(other.to_string()),
                        };
                        self.push_scope();
                        self.define(catch_var, bound);
                        let flow = self.exec_block(catch_body);
                        self.pop_scope();
                        flow
                    }
                }
            }
            Stmt::Import(_) => {
                // Imports are resolved and spliced into the top-level
                // program by main.rs BEFORE the interpreter ever runs —
                // this arm only exists so the match stays exhaustive; it
                // should never actually execute.
                Ok(Flow::Normal)
            }
            Stmt::If { cond, then_block, elif_blocks, else_block } => {
                if self.eval(cond)?.truthy() {
                    self.push_scope();
                    let flow = self.exec_block(then_block)?;
                    self.pop_scope();
                    return Ok(flow);
                }
                for (c, block) in elif_blocks {
                    if self.eval(c)?.truthy() {
                        self.push_scope();
                        let flow = self.exec_block(block)?;
                        self.pop_scope();
                        return Ok(flow);
                    }
                }
                if let Some(block) = else_block {
                    self.push_scope();
                    let flow = self.exec_block(block)?;
                    self.pop_scope();
                    return Ok(flow);
                }
                Ok(Flow::Normal)
            }
            Stmt::Loop { var, start, end, body } => {
                let start_v = self.eval(start)?.as_int()?;
                let end_v = self.eval(end)?.as_int()?;
                for i in start_v..end_v {
                    self.push_scope();
                    self.define(var, Value::Int(i));
                    let flow = self.exec_block(body)?;
                    self.pop_scope();
                    if let Flow::Return(v) = flow {
                        return Ok(Flow::Return(v));
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::While { cond, body } => {
                while self.eval(cond)?.truthy() {
                    self.push_scope();
                    let flow = self.exec_block(body)?;
                    self.pop_scope();
                    if let Flow::Return(v) = flow {
                        return Ok(Flow::Return(v));
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Return(val) => {
                let v = match val {
                    Some(e) => self.eval(e)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(v))
            }
        }
    }

    // ---- Expression evaluation ----

    fn eval(&mut self, expr: &Expr) -> RResult<Value> {
        match expr {
            Expr::IntLit(n) => Ok(Value::Int(*n)),
            Expr::FloatLit(f) => Ok(Value::Float(*f)),
            Expr::StrLit(s) => Ok(Value::Str(s.clone())),
            Expr::BoolLit(b) => Ok(Value::Bool(*b)),
            Expr::ListLit(items) => {
                let vals: RResult<Vec<Value>> = items.iter().map(|e| self.eval(e)).collect();
                Ok(Value::List(vals?))
            }
            Expr::Ident(name) => self.lookup(name),
            Expr::Binary(lhs, op, rhs) => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                eval_binop(&l, op, &r)
            }
            Expr::Unary(op, e) => {
                let v = self.eval(e)?;
                match op {
                    crate::ast::UnOp::Neg => match v {
                        Value::Int(n) => n
                            .checked_neg()
                            .map(Value::Int)
                            .ok_or_else(|| RuntimeError::TypeError(format!("integer overflow: -({})", n))),
                        Value::Float(f) => Ok(Value::Float(-f)),
                        other => Err(RuntimeError::TypeError(format!("cannot negate {}", other))),
                    },
                    crate::ast::UnOp::Not => Ok(Value::Bool(!v.truthy())),
                }
            }
            Expr::Spawn(name, _args) => self.spawn_actor(name),
            Expr::StructLit(type_name, fields) => {
                let mut map = HashMap::new();
                for (fname, fexpr) in fields {
                    map.insert(fname.clone(), self.eval(fexpr)?);
                }
                Ok(Value::Struct(type_name.clone(), std::sync::Arc::new(std::sync::Mutex::new(map))))
            }
            Expr::FieldAccess(base, field) => {
                let v = self.eval(base)?;
                match v {
                    Value::Struct(type_name, fields) => {
                        let guard = fields.lock().unwrap();
                        guard.get(field).cloned().ok_or_else(|| {
                            RuntimeError::TypeError(format!("struct '{}' has no field '{}'", type_name, field))
                        })
                    }
                    other => Err(RuntimeError::TypeError(format!(
                        "field access '.{}' on non-struct value {}",
                        field, other
                    ))),
                }
            }
            Expr::EnumLit(type_name, variant, args) => {
                let vals: RResult<Vec<Value>> = args.iter().map(|e| self.eval(e)).collect();
                Ok(Value::Enum(type_name.clone(), variant.clone(), vals?))
            }
            Expr::Index(base, index) => {
                let base_val = self.eval(base)?;
                let idx = self.eval(index)?.as_int()?;
                match base_val {
                    Value::List(items) => {
                        if idx < 0 || idx as usize >= items.len() {
                            Err(RuntimeError::TypeError(format!(
                                "index {} out of bounds (length {})",
                                idx, items.len()
                            )))
                        } else {
                            Ok(items[idx as usize].clone())
                        }
                    }
                    other => Err(RuntimeError::TypeError(format!("cannot index into {}", other))),
                }
            }
            Expr::Closure(params, body) => {
                // Captures a SNAPSHOT of the current scope stack (see
                // ClosureData's doc comment for why this is immutable-by-
                // design rather than shared-mutable upvalues).
                Ok(Value::Closure(std::sync::Arc::new(ClosureData {
                    params: params.iter().map(|p| p.name.clone()).collect(),
                    body: (**body).clone(),
                    captured_env: self.scopes.clone(),
                })))
            }
            Expr::Await(inner) => {
                // Async model (honest simplification — see FnDef's
                // `is_async` handling in `call_function`): calling an
                // `async fn` runs it to completion immediately and wraps
                // the result in `Value::Future`. `await` just unwraps
                // that wrapper. There is NO real concurrent event loop
                // here — no actual suspension/yielding happens. This
                // means `await` never blocks anything else from making
                // progress (fine for CPU-bound logic), but it also means
                // "concurrent async tasks" aren't actually concurrent —
                // for real concurrency, use `actor`/`spawn` instead,
                // which DOES run on a real OS thread (see Compiler doc
                // §2). A true async runtime (task queue, non-blocking
                // I/O, a real scheduler) is a substantial follow-up
                // project, not something to fake here.
                let v = self.eval(inner)?;
                Ok(match v {
                    Value::Future(boxed) => *boxed,
                    other => other, // awaiting a non-future value just passes it through
                })
            }
            Expr::EnumIsVariant(subject, type_name, variant) => {
                let v = self.eval(subject)?;
                Ok(match v {
                    Value::Enum(t, var, _) => Value::Bool(&t == type_name && &var == variant),
                    _ => Value::Bool(false),
                })
            }
            Expr::EnumPayload(subject, type_name, variant, idx) => {
                let v = self.eval(subject)?;
                match v {
                    Value::Enum(t, var, args) if &t == type_name && &var == variant => args
                        .get(*idx)
                        .cloned()
                        .ok_or_else(|| RuntimeError::TypeError(format!("enum payload index {} out of bounds", idx))),
                    other => Err(RuntimeError::TypeError(format!(
                        "expected {}.{}, got {}",
                        type_name, variant, other
                    ))),
                }
            }
            Expr::Call(name, args) => self.eval_call(name, args),
        }
    }

    fn eval_call(&mut self, name: &str, arg_exprs: &[Expr]) -> RResult<Value> {
        let args: RResult<Vec<Value>> = arg_exprs.iter().map(|e| self.eval(e)).collect();
        let args = args?;

        // A local variable holding a Value::Closure takes priority over
        // both builtins and the global function table — this is what
        // makes `let f = fn(x) => x + 1; f(5)` work: `f` is resolved as
        // a normal variable lookup first, and if it's a closure, we call
        // IT rather than searching for a top-level function named "f".
        if let Ok(Value::Closure(closure)) = self.lookup(name) {
            return self.call_closure(&closure, args);
        }

        // Builtins
        match name {
            "print" => {
                let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect();
                println!("{}", parts.join(" "));
                return Ok(Value::Unit);
            }
            "tensor" => {
                let data = list_of_floats(&args)?;
                let len = data.len();
                return Ok(Value::Tensor { data, shape: vec![len] });
            }
            "tensor.zeros" => {
                let shape = match args.get(0) {
                    Some(Value::List(items)) => {
                        items.iter().map(|v| v.as_int().map(|n| n as usize)).collect::<RResult<Vec<_>>>()?
                    }
                    _ => {
                        return Err(RuntimeError::TypeError(
                            "tensor.zeros expects a shape list, e.g. tensor.zeros([3, 3])".into(),
                        ))
                    }
                };
                let total: usize = shape.iter().product();
                return Ok(Value::Tensor { data: vec![0.0; total], shape });
            }
            "tensor.sum" => {
                if let Some(Value::Tensor { data, .. }) = args.get(0) {
                    return Ok(Value::Float(data.iter().sum()));
                }
                return Err(RuntimeError::TypeError("tensor.sum expects a tensor".into()));
            }
            // --- Standard library (Step 2: minimal but real, not fake) ---
            "len" => {
                return Ok(match args.get(0) {
                    Some(Value::Str(s)) => Value::Int(s.chars().count() as i64),
                    Some(Value::List(items)) => Value::Int(items.len() as i64),
                    Some(Value::Tensor { data, .. }) => Value::Int(data.len() as i64),
                    Some(other) => {
                        return Err(RuntimeError::TypeError(format!("len() not supported for {}", other)))
                    }
                    None => return Err(RuntimeError::ArityMismatch { name: "len".into(), expected: 1, got: 0 }),
                });
            }
            "str_upper" => {
                return Ok(match args.get(0) {
                    Some(Value::Str(s)) => Value::Str(s.to_uppercase()),
                    _ => return Err(RuntimeError::TypeError("str_upper() expects a string".into())),
                });
            }
            "str_lower" => {
                return Ok(match args.get(0) {
                    Some(Value::Str(s)) => Value::Str(s.to_lowercase()),
                    _ => return Err(RuntimeError::TypeError("str_lower() expects a string".into())),
                });
            }
            "str_split" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(s)), Some(Value::Str(sep))) => {
                        let parts: Vec<Value> = s.split(sep.as_str()).map(|p| Value::Str(p.to_string())).collect();
                        Ok(Value::List(parts))
                    }
                    _ => Err(RuntimeError::TypeError("str_split() expects (string, string)".into())),
                };
            }
            "str_trim" => {
                return Ok(match args.get(0) {
                    Some(Value::Str(s)) => Value::Str(s.trim().to_string()),
                    _ => return Err(RuntimeError::TypeError("str_trim() expects a string".into())),
                });
            }
            "str_contains" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(s)), Some(Value::Str(needle))) => Ok(Value::Bool(s.contains(needle.as_str()))),
                    _ => Err(RuntimeError::TypeError("str_contains() expects (string, string)".into())),
                };
            }
            "int_to_str" => {
                return Ok(match args.get(0) {
                    Some(Value::Int(n)) => Value::Str(n.to_string()),
                    Some(Value::Float(f)) => Value::Str(f.to_string()),
                    _ => return Err(RuntimeError::TypeError("int_to_str() expects a number".into())),
                });
            }
            "str_to_int" => {
                return match args.get(0) {
                    Some(Value::Str(s)) => s
                        .trim()
                        .parse::<i64>()
                        .map(Value::Int)
                        .map_err(|_| RuntimeError::TypeError(format!("str_to_int(): '{}' is not a valid integer", s))),
                    _ => Err(RuntimeError::TypeError("str_to_int() expects a string".into())),
                };
            }
            "list_push" => {
                return match args.get(0) {
                    Some(Value::List(items)) => {
                        let mut new_items = items.clone();
                        if let Some(v) = args.get(1) {
                            new_items.push(v.clone());
                        }
                        Ok(Value::List(new_items))
                    }
                    _ => Err(RuntimeError::TypeError("list_push() expects a list as the first argument".into())),
                };
            }
            "list_get" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::List(items)), Some(Value::Int(idx))) => {
                        let i = *idx;
                        if i < 0 || i as usize >= items.len() {
                            Err(RuntimeError::TypeError(format!(
                                "list_get(): index {} out of bounds (length {})",
                                i, items.len()
                            )))
                        } else {
                            Ok(items[i as usize].clone())
                        }
                    }
                    _ => Err(RuntimeError::TypeError("list_get() expects (list, int)".into())),
                };
            }
            "range_list" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Int(start)), Some(Value::Int(end))) => {
                        Ok(Value::List((*start..*end).map(Value::Int).collect()))
                    }
                    _ => Err(RuntimeError::TypeError("range_list() expects (int, int)".into())),
                };
            }
            // --- Native Standard Library: File I/O, JSON, Regex, Network (see stdlib.rs) ---
            "file_read" => {
                return match args.get(0) {
                    Some(Value::Str(path)) => crate::stdlib::file_read(path),
                    _ => Err(RuntimeError::TypeError("file_read() expects a string path".into())),
                };
            }
            "file_write" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(path)), Some(Value::Str(content))) => crate::stdlib::file_write(path, content),
                    _ => Err(RuntimeError::TypeError("file_write() expects (string path, string content)".into())),
                };
            }
            "file_append" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(path)), Some(Value::Str(content))) => crate::stdlib::file_append(path, content),
                    _ => Err(RuntimeError::TypeError("file_append() expects (string path, string content)".into())),
                };
            }
            "file_exists" => {
                return match args.get(0) {
                    Some(Value::Str(path)) => Ok(crate::stdlib::file_exists(path)),
                    _ => Err(RuntimeError::TypeError("file_exists() expects a string path".into())),
                };
            }
            "json_stringify" => {
                return match args.get(0) {
                    Some(v) => crate::stdlib::json_stringify(v),
                    None => Err(RuntimeError::ArityMismatch { name: "json_stringify".into(), expected: 1, got: 0 }),
                };
            }
            "json_parse" => {
                return match args.get(0) {
                    Some(Value::Str(s)) => crate::stdlib::json_parse(s),
                    _ => Err(RuntimeError::TypeError("json_parse() expects a string".into())),
                };
            }
            "regex_match" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(pat)), Some(Value::Str(text))) => crate::stdlib::regex_match(pat, text),
                    _ => Err(RuntimeError::TypeError("regex_match() expects (string pattern, string text)".into())),
                };
            }
            "regex_find" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(pat)), Some(Value::Str(text))) => crate::stdlib::regex_find(pat, text),
                    _ => Err(RuntimeError::TypeError("regex_find() expects (string pattern, string text)".into())),
                };
            }
            "regex_replace" => {
                return match (args.get(0), args.get(1), args.get(2)) {
                    (Some(Value::Str(pat)), Some(Value::Str(text)), Some(Value::Str(repl))) => {
                        crate::stdlib::regex_replace(pat, text, repl)
                    }
                    _ => Err(RuntimeError::TypeError(
                        "regex_replace() expects (string pattern, string text, string replacement)".into(),
                    )),
                };
            }
            "net_tcp_send" => {
                return match (args.get(0), args.get(1), args.get(2)) {
                    (Some(Value::Str(host)), Some(Value::Int(port)), Some(Value::Str(msg))) => {
                        crate::stdlib::net_tcp_send(host, *port, msg)
                    }
                    _ => Err(RuntimeError::TypeError(
                        "net_tcp_send() expects (string host, int port, string message)".into(),
                    )),
                };
            }
            "enum_tag" => {
                return match args.get(0) {
                    Some(Value::Enum(type_name, variant, _)) => {
                        match self.runtime.globals.enums.get(type_name) {
                            Some(variant_order) => variant_order
                                .iter()
                                .position(|v| v == variant)
                                .map(|idx| Value::Int(idx as i64))
                                .ok_or_else(|| {
                                    RuntimeError::TypeError(format!(
                                        "enum_tag(): '{}' has no variant '{}'",
                                        type_name, variant
                                    ))
                                }),
                            None => Err(RuntimeError::TypeError(format!(
                                "enum_tag(): undefined enum type '{}'",
                                type_name
                            ))),
                        }
                    }
                    _ => Err(RuntimeError::TypeError("enum_tag() expects an enum value".into())),
                };
            }
            // --- Phase 3: Testing framework support ---
            "assert" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Bool(true)), _) => Ok(Value::Unit),
                    (Some(Value::Bool(false)), Some(Value::Str(msg))) => {
                        Err(RuntimeError::UserRaised(Value::Str(format!("assertion failed: {}", msg))))
                    }
                    (Some(Value::Bool(false)), None) => {
                        Err(RuntimeError::UserRaised(Value::Str("assertion failed".to_string())))
                    }
                    _ => Err(RuntimeError::TypeError("assert() expects (bool condition, optional string message)".into())),
                };
            }
            // --- Phase 2: Data Structures — native HashMap ---
            "map_new" => {
                return Ok(Value::Map(std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()))));
            }
            "map_set" => {
                return match (args.get(0), args.get(1), args.get(2)) {
                    (Some(Value::Map(m)), Some(Value::Str(k)), Some(v)) => {
                        m.lock().unwrap().insert(k.clone(), v.clone());
                        Ok(Value::Map(m.clone())) // same underlying map (Arc share), returned for chaining
                    }
                    _ => Err(RuntimeError::TypeError("map_set() expects (map, string key, value)".into())),
                };
            }
            "map_get" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Map(m)), Some(Value::Str(k))) => {
                        Ok(m.lock().unwrap().get(k).cloned().unwrap_or(Value::Unit))
                    }
                    _ => Err(RuntimeError::TypeError("map_get() expects (map, string key)".into())),
                };
            }
            "map_has" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Map(m)), Some(Value::Str(k))) => Ok(Value::Bool(m.lock().unwrap().contains_key(k))),
                    _ => Err(RuntimeError::TypeError("map_has() expects (map, string key)".into())),
                };
            }
            "map_delete" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Map(m)), Some(Value::Str(k))) => {
                        let removed = m.lock().unwrap().remove(k).is_some();
                        Ok(Value::Bool(removed))
                    }
                    _ => Err(RuntimeError::TypeError("map_delete() expects (map, string key)".into())),
                };
            }
            "map_keys" => {
                return match args.get(0) {
                    Some(Value::Map(m)) => {
                        Ok(Value::List(m.lock().unwrap().keys().map(|k| Value::Str(k.clone())).collect()))
                    }
                    _ => Err(RuntimeError::TypeError("map_keys() expects a map".into())),
                };
            }
            "map_len" => {
                return match args.get(0) {
                    Some(Value::Map(m)) => Ok(Value::Int(m.lock().unwrap().len() as i64)),
                    _ => Err(RuntimeError::TypeError("map_len() expects a map".into())),
                };
            }
            // --- Phase 2: System & I/O — environment, args, process ---
            "env_get" => {
                return match args.get(0) {
                    Some(Value::Str(k)) => Ok(match std::env::var(k) {
                        Ok(v) => Value::Enum("Option".into(), "Some".into(), vec![Value::Str(v)]),
                        Err(_) => Value::Enum("Option".into(), "None".into(), vec![]),
                    }),
                    _ => Err(RuntimeError::TypeError("env_get() expects a string key".into())),
                };
            }
            "env_set" => {
                return match (args.get(0), args.get(1)) {
                    (Some(Value::Str(k)), Some(Value::Str(v))) => {
                        std::env::set_var(k, v);
                        Ok(Value::Unit)
                    }
                    _ => Err(RuntimeError::TypeError("env_set() expects (string key, string value)".into())),
                };
            }
            "program_args" => {
                return Ok(Value::List(std::env::args().skip(1).map(Value::Str).collect()));
            }
            "process_exit" => {
                let code = match args.get(0) {
                    Some(Value::Int(n)) => *n as i32,
                    _ => 0,
                };
                // Runs before actor threads are joined — intentional
                // "exit now" semantics, matching real `process.exit()`
                // in Node/Python; anyone needing graceful actor
                // shutdown first should let main() return normally
                // instead of calling this.
                std::process::exit(code);
            }
            "process_run" => {
                return match args.get(0) {
                    Some(Value::Str(cmd)) => {
                        let cmd_args: Vec<String> = match args.get(1) {
                            Some(Value::List(items)) => items
                                .iter()
                                .map(|v| match v {
                                    Value::Str(s) => Ok(s.clone()),
                                    other => Err(RuntimeError::TypeError(format!(
                                        "process_run(): argument list must contain only strings, got {}",
                                        other
                                    ))),
                                })
                                .collect::<RResult<Vec<_>>>()?,
                            None => Vec::new(),
                            _ => return Err(RuntimeError::TypeError("process_run()'s 2nd argument must be a list of strings".into())),
                        };
                        crate::stdlib::process_run(cmd, &cmd_args)
                    }
                    _ => Err(RuntimeError::TypeError("process_run() expects a string command".into())),
                };
            }
            // --- Phase 2: Networking — UDP + HTTP client/server ---
            "udp_send" => {
                return match (args.get(0), args.get(1), args.get(2)) {
                    (Some(Value::Str(host)), Some(Value::Int(port)), Some(Value::Str(msg))) => {
                        crate::stdlib::udp_send(host, *port, msg)
                    }
                    _ => Err(RuntimeError::TypeError("udp_send() expects (string host, int port, string message)".into())),
                };
            }
            "http_get" => {
                return match (args.get(0), args.get(1), args.get(2)) {
                    (Some(Value::Str(host)), Some(Value::Int(port)), Some(Value::Str(path))) => {
                        crate::stdlib::http_get(host, *port, path)
                    }
                    _ => Err(RuntimeError::TypeError("http_get() expects (string host, int port, string path)".into())),
                };
            }
            "http_post" => {
                return match (args.get(0), args.get(1), args.get(2), args.get(3)) {
                    (Some(Value::Str(host)), Some(Value::Int(port)), Some(Value::Str(path)), Some(Value::Str(body))) => {
                        crate::stdlib::http_post(host, *port, path, body)
                    }
                    _ => Err(RuntimeError::TypeError(
                        "http_post() expects (string host, int port, string path, string body)".into(),
                    )),
                };
            }
            "http_serve" => {
                return match (args.get(0), args.get(1), args.get(2)) {
                    (Some(Value::Int(port)), Some(Value::Closure(handler)), Some(Value::Int(max_requests))) => {
                        self.http_serve(*port as u16, handler.clone(), *max_requests as usize)
                    }
                    _ => Err(RuntimeError::TypeError(
                        "http_serve() expects (int port, closure handler(method, path) -> string, int max_requests)".into(),
                    )),
                };
            }
            // --- Phase 2: Async & Actor Bridge ---
            "net_tcp_send_to_actor" => {
                return match (args.get(0), args.get(1), args.get(2), args.get(3)) {
                    (Some(Value::ActorHandle(id)), Some(Value::Str(host)), Some(Value::Int(port)), Some(Value::Str(msg))) => {
                        self.net_tcp_send_to_actor(*id, host.clone(), *port, msg.clone())
                    }
                    _ => Err(RuntimeError::TypeError(
                        "net_tcp_send_to_actor() expects (actor handle, string host, int port, string message)".into(),
                    )),
                };
            }
            _ => {}
        }

        // Tagged message constructor: capitalized name not matching a
        // known function/actor is treated as `TagName(args...)`.
        if name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
            && !self.runtime.globals.functions.contains_key(name)
        {
            return Ok(Value::Message { tag: name.to_string(), args });
        }

        self.call_function(name, args)
    }

    fn call_function(&mut self, name: &str, args: Vec<Value>) -> RResult<Value> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::TypeError(format!(
                "stack overflow: '{}' recursed past the maximum call depth ({}) — likely infinite or excessively deep recursion",
                name, MAX_CALL_DEPTH
            )));
        }

        let fn_def = self
            .runtime
            .globals
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| RuntimeError::UndefinedFunction(name.to_string()))?;

        if fn_def.params.len() != args.len() {
            return Err(RuntimeError::ArityMismatch {
                name: name.to_string(),
                expected: fn_def.params.len(),
                got: args.len(),
            });
        }

        self.call_depth += 1;
        self.push_scope();
        for (pname, pval) in fn_def.params.iter().zip(args.into_iter()) {
            self.define(pname, pval);
        }
        let flow = self.exec_block(&fn_def.body);
        self.pop_scope();
        self.call_depth -= 1;
        let flow = flow?;

        let result = match flow {
            Flow::Return(v) => v,
            Flow::Normal => Value::Unit,
        };
        Ok(if fn_def.is_async { Value::Future(Box::new(result)) } else { result })
    }

    /// Invokes a `Value::Closure`. The closure body is a SINGLE
    /// expression (see `ast::Expr::Closure`'s doc comment), evaluated in
    /// a fresh scope seeded from the closure's captured environment
    /// snapshot plus its own parameters — this is what gives it real
    /// lexical-scoping behavior (it can read variables from where it was
    /// DEFINED, not just where it's called from).
    fn call_closure(&mut self, closure: &ClosureData, args: Vec<Value>) -> RResult<Value> {
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::TypeError(format!(
                "stack overflow: closure call recursed past the maximum call depth ({})",
                MAX_CALL_DEPTH
            )));
        }
        if closure.params.len() != args.len() {
            return Err(RuntimeError::ArityMismatch {
                name: "<closure>".to_string(),
                expected: closure.params.len(),
                got: args.len(),
            });
        }

        // Swap in the closure's captured scope stack for the duration of
        // this call, then restore the caller's scopes afterward.
        let caller_scopes = std::mem::replace(&mut self.scopes, closure.captured_env.clone());
        self.push_scope();
        for (pname, pval) in closure.params.iter().zip(args.into_iter()) {
            self.define(pname, pval);
        }
        self.call_depth += 1;
        let result = self.eval(&closure.body);
        self.call_depth -= 1;
        self.scopes = caller_scopes;
        result
    }

    /// Spawns a new isolated actor thread (Compiler doc §2.1). The new
    /// thread gets its OWN Interpreter with a fresh scope stack — it does
    /// NOT share `self.scopes`. It only holds Arc-shared, read-only
    /// global function/actor definitions plus messages delivered to its
    /// mailbox.
    ///
    /// Plain (unsupervised) actors self-heal on handler errors — they log
    /// and keep processing the next message, matching the earlier Phase 4
    /// behavior. Supervised children (spawned via `spawn_supervised_child`)
    /// instead crash the thread and report to their coordinator, which is
    /// what makes supervision meaningful.
    /// Phase 2: Async & Actor Bridge — real HTTP server whose request
    /// handling logic is written IN Tridentix (a closure), not hardcoded
    /// Rust. Blocking, single-threaded, and bounded to `max_requests`
    /// (rather than looping forever) so it's testable in a script
    /// without hanging — a real server would loop indefinitely, which
    /// is a one-line change (`for stream in listener.incoming()` with
    /// no `handled >= max_requests` break) once you have a process
    /// supervisor to actually keep it running.
    fn http_serve(&mut self, port: u16, handler: std::sync::Arc<ClosureData>, max_requests: usize) -> RResult<Value> {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| RuntimeError::TypeError(format!("http_serve(): bind to port {} failed: {}", port, e)))?;

        let mut handled = 0usize;
        for stream in listener.incoming() {
            if handled >= max_requests {
                break;
            }
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let mut parts = request.split_whitespace();
            let method = parts.next().unwrap_or("GET").to_string();
            let path = parts.next().unwrap_or("/").to_string();

            // Real callback into the Tridentix interpreter — this is the
            // "bridge" part: network I/O happens in Rust, but the
            // RESPONSE LOGIC is whatever the user wrote as a closure.
            let response_body = match self.call_closure(&handler, vec![Value::Str(method), Value::Str(path)]) {
                Ok(v) => v.to_string(),
                Err(e) => format!("Internal Server Error: {}", e),
            };
            let http_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let _ = stream.write_all(http_response.as_bytes());
            handled += 1;
        }
        Ok(Value::Int(handled as i64))
    }

    /// Phase 2: Async & Actor Bridge — fires a TCP request on a
    /// background thread and delivers the result directly to an actor's
    /// mailbox as a tagged message (`NetworkResponse`/`NetworkError`),
    /// rather than blocking the caller. This is the literal "native
    /// wrapper for sending network payloads directly to actor mailboxes"
    /// the Phase 2 spec asked for — network I/O and actor message
    /// passing composed together in one call instead of the caller
    /// manually doing `let r = net_tcp_send(...); send(actor, r)`
    /// (which blocks the caller until the network call returns).
    fn net_tcp_send_to_actor(&mut self, actor_id: usize, host: String, port: i64, msg: String) -> RResult<Value> {
        let runtime = Arc::clone(&self.runtime);
        let handle = thread::spawn(move || {
            let result = crate::stdlib::net_tcp_send(&host, port, &msg);
            let payload = match result {
                Ok(v) => Value::Message { tag: "NetworkResponse".to_string(), args: vec![v] },
                Err(e) => Value::Message { tag: "NetworkError".to_string(), args: vec![Value::Str(e.to_string())] },
            };
            if let Some(sender) = runtime.mailboxes.lock().unwrap().get(&actor_id) {
                let _ = sender.send(payload);
            }
        });
        // Tracked in `handles` so `shutdown_actors()` joins it — without
        // this, `main()` could return and the process could exit before
        // the background network call even finishes, silently dropping
        // the message.
        self.runtime.handles.lock().unwrap().push(handle);
        Ok(Value::Unit)
    }

    fn spawn_actor(&mut self, name: &str) -> RResult<Value> {
        let actor_def = self
            .runtime
            .globals
            .actors
            .get(name)
            .cloned()
            .ok_or_else(|| RuntimeError::UndefinedActor(name.to_string()))?;

        let id = self.runtime.next_actor_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<Value>();
        self.runtime.mailboxes.lock().unwrap().insert(id, tx);

        let runtime = Arc::clone(&self.runtime);
        let handle = thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
            run_actor_worker(runtime, actor_def, id, rx, None);
        })
        .expect("failed to spawn actor thread");

        self.runtime.handles.lock().unwrap().push(handle);
        Ok(Value::ActorHandle(id))
    }

    /// Spawns one child actor under supervision: same isolation model as
    /// `spawn_actor`, but crashes are reported to `events` instead of
    /// being silently self-healed, so the coordinator thread can apply
    /// the restart strategy (Compiler doc §2.2-§2.3).
    fn spawn_supervised_child(
        runtime: &Arc<RuntimeShared>,
        actor_name: &str,
        child_index: usize,
        events: Sender<SupervisorEvent>,
    ) -> RResult<usize> {
        let actor_def = runtime
            .globals
            .actors
            .get(actor_name)
            .cloned()
            .ok_or_else(|| RuntimeError::UndefinedActor(actor_name.to_string()))?;

        let id = runtime.next_actor_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel::<Value>();
        runtime.mailboxes.lock().unwrap().insert(id, tx);

        let runtime_clone = Arc::clone(runtime);
        thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                run_actor_worker(runtime_clone, actor_def, id, rx, Some((events, child_index)));
            })
            .expect("failed to spawn supervised actor thread");

        Ok(id)
    }

    /// Executes a `supervisor` block (Compiler doc §2.2): spawns every
    /// child under a coordinator thread that watches for crashes and
    /// restarts them according to `strategy`, `max_restarts`, and
    /// `time_window_secs`. Returns immediately after spawning — the
    /// coordinator runs in the background for the lifetime of the program
    /// (it's joined during `shutdown_actors`, same as plain actors).
    fn spawn_supervisor(
        &mut self,
        sup_name: &str,
        strategy: RestartStrategy,
        max_restarts: i64,
        time_window_secs: i64,
        children: &[crate::ast::SupervisedChild],
    ) -> RResult<Vec<(String, Value)>> {
        let runtime = Arc::clone(&self.runtime);
        let (events_tx, events_rx) = mpsc::channel::<SupervisorEvent>();

        let mut child_ids = Vec::with_capacity(children.len());
        let mut bindings = Vec::with_capacity(children.len());
        for (idx, child) in children.iter().enumerate() {
            let id = Self::spawn_supervised_child(&runtime, &child.actor_name, idx, events_tx.clone())?;
            child_ids.push(id);
            bindings.push((child.bind_name.clone(), Value::ActorHandle(id)));
        }
        let coordinator_events_tx = events_tx.clone();
        self.runtime
            .supervisor_shutdown_senders
            .lock()
            .unwrap()
            .push(coordinator_events_tx.clone());
        drop(events_tx); // children hold their own clones; original no longer needed here

        let sup_name_owned = sup_name.to_string();
        let children_owned: Vec<crate::ast::SupervisedChild> = children.to_vec();
        let coordinator_runtime = Arc::clone(&self.runtime);

        let coordinator = thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
            run_supervisor_coordinator(
                coordinator_runtime,
                sup_name_owned,
                strategy,
                max_restarts,
                time_window_secs,
                children_owned,
                child_ids,
                coordinator_events_tx,
                events_rx,
            );
        })
        .expect("failed to spawn supervisor coordinator thread");

        self.runtime.handles.lock().unwrap().push(coordinator);
        Ok(bindings)
    }

}

/// The message-processing loop shared by plain and supervised actors.
/// - `events = None`: plain actor (self-healing — logs handler errors
///   and keeps processing the next message, original Phase 4 behavior).
/// - `events = Some((sender, child_index))`: supervised child — a
///   handler error crashes the thread (reports `Crashed`), and a
///   `__shutdown__` message reports `ShutdownAck` before exiting.
///   A `__supervisor_restart__` message (sent only by the coordinator
///   itself) exits silently with no event, since the coordinator already
///   knows it's replacing this worker.
fn run_actor_worker(
    runtime: Arc<RuntimeShared>,
    actor_def: ActorDef,
    id: usize,
    rx: Receiver<Value>,
    events: Option<(Sender<SupervisorEvent>, usize)>,
) {
    let mut actor_interp = Interpreter {
        runtime,
        scopes: vec![HashMap::new()],
        call_depth: 0,
    };
    // Persistent actor state (Phase 4): initialized ONCE here, into the
    // BASE scope (index 0) — which is never popped between messages
    // (each message only push_scope()s/pop_scope()s a scope ON TOP of
    // this one). Since `Interpreter::assign` already searches from the
    // innermost scope outward, a handler doing `count = count + 1`
    // naturally finds and updates THIS persistent binding rather than
    // shadowing it — no special-casing needed elsewhere.
    for (field_name, init_expr) in &actor_def.state_vars {
        match actor_interp.eval(init_expr) {
            Ok(v) => actor_interp.define(field_name, v),
            Err(e) => {
                eprintln!("[actor #{} state-init error] {}: {}", id, field_name, e);
                return; // can't safely run with broken state
            }
        }
    }
    loop {
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => break, // sender dropped, nothing left to do
        };
        if let Value::Message { tag, .. } = &msg {
            if tag == "__shutdown__" {
                if let Some((sender, idx)) = &events {
                    let _ = sender.send(SupervisorEvent::ShutdownAck(*idx));
                }
                break;
            }
            if tag == "__supervisor_restart__" {
                break; // coordinator-initiated restart; no event needed
            }
        }
        actor_interp.push_scope();
        actor_interp.define(&actor_def.param_name, msg);
        let result = actor_interp.exec_block(&actor_def.body);
        actor_interp.pop_scope();

        if let Err(e) = result {
            eprintln!("[actor #{} error] {}", id, e);
            match &events {
                Some((sender, idx)) => {
                    let _ = sender.send(SupervisorEvent::Crashed(*idx));
                    break; // supervised: crash ends this thread, coordinator decides what's next
                }
                None => continue, // unsupervised: self-heal, keep processing
            }
        }
    }
}

/// Coordinator thread body for one `supervisor` block (Compiler doc §2.2-2.3).
/// Owns the restart-strategy logic: watches `events_rx` for crashes/shutdowns
/// from its children and reacts according to `strategy`, restart-budget
/// (`max_restarts` within `time_window_secs`), exiting once every child has
/// acknowledged shutdown (or given up after exceeding its restart budget).
fn run_supervisor_coordinator(
    runtime: Arc<RuntimeShared>,
    sup_name: String,
    strategy: RestartStrategy,
    max_restarts: i64,
    time_window_secs: i64,
    children: Vec<crate::ast::SupervisedChild>,
    mut child_ids: Vec<usize>,
    events_tx: Sender<SupervisorEvent>,
    events_rx: Receiver<SupervisorEvent>,
) {
    let n = children.len();
    let mut done = vec![false; n];
    let mut given_up = vec![false; n];
    let mut restart_count = 0i64;
    let mut window_start = Instant::now();
    let window = Duration::from_secs(time_window_secs.max(0) as u64);
    // Once true, crashes no longer trigger restarts — the program is
    // exiting, so a crash just means "this child is done too" instead of
    // spawning a replacement that would never receive a shutdown signal
    // (this is exactly the race that used to deadlock `shutdown_actors`).
    let mut shutting_down = false;

    let respawn = |runtime: &Arc<RuntimeShared>, idx: usize, old_id: usize, actor_name: &str| -> usize {
        // Reuse the SAME actor id so any Value::ActorHandle(old_id) the
        // rest of the program already holds keeps working transparently
        // after the restart (send() re-resolves the sender by id at
        // send-time, so it naturally picks up the new mailbox).
        let actor_def = match runtime.globals.actors.get(actor_name).cloned() {
            Some(a) => a,
            None => return old_id, // shouldn't happen: validated at parse/spawn time
        };
        let (tx, rx) = mpsc::channel::<Value>();
        runtime.mailboxes.lock().unwrap().insert(old_id, tx);
        let runtime_clone = Arc::clone(runtime);
        let events_clone = events_tx.clone();
        thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                run_actor_worker(runtime_clone, actor_def, old_id, rx, Some((events_clone, idx)));
            })
            .expect("failed to spawn restarted actor thread");
        old_id
    };

    loop {
        if done.iter().all(|d| *d) {
            break;
        }
        let event = match events_rx.recv() {
            Ok(e) => e,
            Err(_) => break, // all senders dropped -> nothing left to supervise
        };

        match event {
            SupervisorEvent::ShutdownAck(idx) => {
                done[idx] = true;
            }
            SupervisorEvent::GlobalShutdown => {
                shutting_down = true;
                for i in 0..n {
                    if done[i] || given_up[i] {
                        continue;
                    }
                    if let Some(sender) = runtime.mailboxes.lock().unwrap().get(&child_ids[i]) {
                        let _ = sender.send(Value::Message {
                            tag: "__shutdown__".to_string(),
                            args: vec![],
                        });
                    }
                }
            }
            SupervisorEvent::Crashed(idx) => {
                if done[idx] || given_up[idx] {
                    continue;
                }
                if shutting_down {
                    // Program is exiting — don't respawn, just count this
                    // child as finished so the coordinator can exit too.
                    done[idx] = true;
                    continue;
                }

                // Sliding restart-budget window (Compiler doc §2.3).
                if window_start.elapsed() > window {
                    window_start = Instant::now();
                    restart_count = 0;
                }
                restart_count += 1;

                if restart_count > max_restarts {
                    eprintln!(
                        "[supervisor {}] child '{}' (index {}) exceeded max_restarts ({}) within {}s — escalating, giving up on this child",
                        sup_name, children[idx].bind_name, idx, max_restarts, time_window_secs
                    );
                    given_up[idx] = true;
                    done[idx] = true; // count it as finished so the coordinator can eventually exit
                    continue;
                }

                let targets: Vec<usize> = match strategy {
                    RestartStrategy::OneForOne => vec![idx],
                    RestartStrategy::OneForAll => (0..n).filter(|i| !done[*i] && !given_up[*i]).collect(),
                    RestartStrategy::RestForOne => (idx..n).filter(|i| !done[*i] && !given_up[*i]).collect(),
                };

                eprintln!(
                    "[supervisor {}] restarting {:?} (strategy={:?}, restart #{} in current window)",
                    sup_name,
                    targets.iter().map(|i| children[*i].bind_name.clone()).collect::<Vec<_>>(),
                    strategy,
                    restart_count
                );

                for &t in &targets {
                    if t != idx {
                        // Sibling is still alive: tell it to stop gracefully
                        // before replacing it. There is a brief window where
                        // the old thread is finishing up — acceptable for
                        // this reference implementation.
                        if let Some(sender) = runtime.mailboxes.lock().unwrap().get(&child_ids[t]) {
                            let _ = sender.send(Value::Message {
                                tag: "__supervisor_restart__".to_string(),
                                args: vec![],
                            });
                        }
                    }
                    let new_id = respawn(&runtime, t, child_ids[t], &children[t].actor_name);
                    child_ids[t] = new_id;
                }
            }
        }
    }
}


impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::List(items) => !items.is_empty(),
            Value::Unit => false,
            _ => true,
        }
    }

    fn as_int(&self) -> RResult<i64> {
        match self {
            Value::Int(n) => Ok(*n),
            other => Err(RuntimeError::TypeError(format!("expected int, got {}", other))),
        }
    }

    fn as_float(&self) -> RResult<f64> {
        match self {
            Value::Int(n) => Ok(*n as f64),
            Value::Float(f) => Ok(*f),
            other => Err(RuntimeError::TypeError(format!("expected number, got {}", other))),
        }
    }
}

fn list_of_floats(args: &[Value]) -> RResult<Vec<f64>> {
    if args.len() == 1 {
        if let Value::List(items) = &args[0] {
            return items.iter().map(|v| v.as_float()).collect();
        }
    }
    args.iter().map(|v| v.as_float()).collect()
}

/// Recursive structural equality for enum payload comparison (Tridentix has
/// no general `Value: PartialEq` — `Closure`/`Struct` don't have a
/// sensible equality notion — so this only handles the leaf types that
/// realistically show up as enum payloads).
fn values_structurally_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Enum(t1, v1, a1), Value::Enum(t2, v2, a2)) => {
            t1 == t2 && v1 == v2 && a1.len() == a2.len() && a1.iter().zip(a2.iter()).all(|(x, y)| values_structurally_equal(x, y))
        }
        _ => false,
    }
}

fn eval_binop(l: &Value, op: &BinOp, r: &Value) -> RResult<Value> {
    use BinOp::*;
    // String concatenation via '+' as a convenience for print-style code.
    if let (Value::Str(a), Add, Value::Str(b)) = (l, op, r) {
        return Ok(Value::Str(format!("{}{}", a, b)));
    }
    // String (in)equality/ordering — must be handled before the numeric
    // fallback below, otherwise e.g. `msg == "crash"` would incorrectly
    // try to convert both sides to int and fail with a spurious type error.
    if let (Value::Str(a), Value::Str(b)) = (l, r) {
        return Ok(match op {
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            Lt => Value::Bool(a < b),
            Gt => Value::Bool(a > b),
            LtEq => Value::Bool(a <= b),
            GtEq => Value::Bool(a >= b),
            Add => unreachable!("Add on two strings handled above"),
            Sub | Mul | Div => {
                return Err(RuntimeError::TypeError(format!(
                    "operator not supported between two strings"
                )))
            }
        });
    }
    // Enum structural equality (same type + variant + equal payload) —
    // needed for the (less common, but still valid) direct `enum1 ==
    // enum2` comparison, as opposed to the `match`-based tag/payload
    // check (`Expr::EnumIsVariant`/`Expr::EnumPayload`) that most enum
    // code should prefer.
    if let (Value::Enum(t1, v1, args1), Value::Enum(t2, v2, args2)) = (l, r) {
        let equal = t1 == t2 && v1 == v2 && args1.len() == args2.len() && args1
            .iter()
            .zip(args2.iter())
            .all(|(a, b)| values_structurally_equal(a, b));
        return Ok(match op {
            Eq => Value::Bool(equal),
            NotEq => Value::Bool(!equal),
            _ => {
                return Err(RuntimeError::TypeError(
                    "only == and != are supported between enum values".into(),
                ))
            }
        });
    }
    // Bool equality.
    if let (Value::Bool(a), Value::Bool(b)) = (l, r) {
        return Ok(match op {
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            _ => {
                return Err(RuntimeError::TypeError(
                    "only == and != are supported between bools".into(),
                ))
            }
        });
    }

    let is_float = matches!(l, Value::Float(_)) || matches!(r, Value::Float(_));
    if is_float {
        let a = l.as_float()?;
        let b = r.as_float()?;
        return Ok(match op {
            Add => Value::Float(a + b),
            Sub => Value::Float(a - b),
            Mul => Value::Float(a * b),
            Div => Value::Float(a / b),
            Eq => Value::Bool(a == b),
            NotEq => Value::Bool(a != b),
            Lt => Value::Bool(a < b),
            Gt => Value::Bool(a > b),
            LtEq => Value::Bool(a <= b),
            GtEq => Value::Bool(a >= b),
        });
    }

    let a = l.as_int()?;
    let b = r.as_int()?;
    Ok(match op {
        // checked_* instead of raw operators: a language runtime should
        // NEVER crash the whole process on overflow/divide-by-zero — it
        // should report a normal runtime error the program (or its
        // supervisor, for an actor) can handle, same as any other
        // RuntimeError.
        Add => Value::Int(a.checked_add(b).ok_or_else(|| {
            RuntimeError::TypeError(format!("integer overflow: {} + {}", a, b))
        })?),
        Sub => Value::Int(a.checked_sub(b).ok_or_else(|| {
            RuntimeError::TypeError(format!("integer overflow: {} - {}", a, b))
        })?),
        Mul => Value::Int(a.checked_mul(b).ok_or_else(|| {
            RuntimeError::TypeError(format!("integer overflow: {} * {}", a, b))
        })?),
        Div => {
            if b == 0 {
                return Err(RuntimeError::TypeError(format!("division by zero: {} / 0", a)));
            }
            Value::Int(a.checked_div(b).ok_or_else(|| {
                RuntimeError::TypeError(format!("integer overflow: {} / {}", a, b))
            })?)
        }
        Eq => Value::Bool(a == b),
        NotEq => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Gt => Value::Bool(a > b),
        LtEq => Value::Bool(a <= b),
        GtEq => Value::Bool(a >= b),
    })
}
