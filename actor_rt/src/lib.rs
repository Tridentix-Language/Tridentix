//! Module 1, Part C: a minimal but REAL native actor runtime, exposed
//! via `extern "C"` so LLVM-compiled code can call it DIRECTLY — no
//! interpreter involved anywhere in this path. Real OS threads, real
//! `mpsc` channel mailboxes, real thread safety.
//!
//! Honest scope: this is a genuinely separate, smaller actor model than
//! the interpreter's (Compiler doc §2's full supervisor-tree design) —
//! messages here are a single `i64` (not an arbitrary `Value`), and
//! there's no supervision/restart yet. It's the real, working
//! foundation such a system would be built on, not a stand-in for the
//! interpreter's richer model. See this file's doc comment on
//! `tridentix_rt_spawn` for the exact ABI contract compiled code must follow.

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;

fn mailboxes() -> &'static Mutex<HashMap<i64, Sender<i64>>> {
    static MAILBOXES: OnceLock<Mutex<HashMap<i64, Sender<i64>>>> = OnceLock::new();
    MAILBOXES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> &'static Mutex<i64> {
    static NEXT_ID: OnceLock<Mutex<i64>> = OnceLock::new();
    NEXT_ID.get_or_init(|| Mutex::new(0))
}

fn handles() -> &'static Mutex<Vec<thread::JoinHandle<()>>> {
    static HANDLES: OnceLock<Mutex<Vec<thread::JoinHandle<()>>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Sentinel payload value telling a worker thread to stop (rather than a
/// separate out-of-band signal — keeps the ABI to a single `i64` channel
/// type). `i64::MIN` is never a value a real Tridentix int-message would
/// plausibly collide with in practice, but see this module's "Known
/// gaps" note in `codegen.rs`'s actor-lowering doc comment for the
/// honest caveat that this IS a real (if narrow) collision risk.
const SHUTDOWN_SENTINEL: i64 = i64::MIN;

/// The C ABI contract every native-compiled Tridentix `actor`'s lifted
/// handler function must satisfy: `extern "C" fn(i64) -> i64`, taking
/// the message payload and returning a value (currently unused/ignored
/// by the runtime, reserved for a future ack/reply channel).
pub type ActorHandlerFn = extern "C" fn(i64) -> i64;

/// Spawns a new actor: a real OS thread running a `recv()` loop over a
/// real `mpsc` mailbox, calling `handler` for every message until it
/// sees the shutdown sentinel. Returns an opaque `i64` actor id — the
/// SAME representation `tridentix_rt_send` expects as its target.
#[no_mangle]
pub extern "C" fn tridentix_rt_spawn(handler: ActorHandlerFn) -> i64 {
    let id = {
        let mut n = next_id().lock().unwrap();
        let assigned = *n;
        *n += 1;
        assigned
    };

    let (tx, rx) = mpsc::channel::<i64>();
    mailboxes().lock().unwrap().insert(id, tx);

    let join_handle = thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                if msg == SHUTDOWN_SENTINEL {
                    break;
                }
                handler(msg);
            }
        })
        .expect("tridentix_rt_spawn: failed to spawn native actor thread");

    handles().lock().unwrap().push(join_handle);
    id
}

/// Sends `payload` to actor `actor_id`'s mailbox. If the actor has
/// already shut down (or `actor_id` never existed), this is a silent
/// no-op — matches the interpreter runtime's own `send()` semantics
/// (see interpreter.rs's `Stmt::Send` handling).
#[no_mangle]
pub extern "C" fn tridentix_rt_send(actor_id: i64, payload: i64) {
    if let Some(sender) = mailboxes().lock().unwrap().get(&actor_id) {
        let _ = sender.send(payload);
    }
}

/// Signals every live actor to stop, then joins every spawned thread.
/// MUST be called before the AOT-compiled program's `main` returns —
/// unlike the JIT/interpreter paths (which have a Rust-level `main()`
/// around them to clean up), an AOT binary's `main` IS the whole
/// process, so without this call the process could exit while actor
/// threads are still mid-message, silently dropping work.
#[no_mangle]
pub extern "C" fn tridentix_rt_shutdown_and_join() {
    {
        let boxes = mailboxes().lock().unwrap();
        for sender in boxes.values() {
            let _ = sender.send(SHUTDOWN_SENTINEL);
        }
    }
    let mut hs = handles().lock().unwrap();
    for h in hs.drain(..) {
        let _ = h.join();
    }
}
