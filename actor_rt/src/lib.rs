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

const SHUTDOWN_SENTINEL: i64 = i64::MIN;

/// The C ABI contract every native-compiled Tridentix `actor`'s lifted
/// handler function must satisfy: `extern "C" fn(i64) -> i64`, taking
pub type ActorHandlerFn = extern "C" fn(i64) -> i64;


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
pub extern "C" fn tridentix_rt_send(actor_id: i64, payload: i64) {
    if let Some(sender) = mailboxes().lock().unwrap().get(&actor_id) {
        let _ = sender.send(payload);
    }
}
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
