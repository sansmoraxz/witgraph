wit_bindgen::generate!({ path: "wit", world: "future-writer" });

use std::sync::atomic::{AtomicU32, Ordering};

use exports::node::{Guest, Inputs, Outputs};

struct Writer;

/// How the latest write ended.
static LAST: AtomicU32 = AtomicU32::new(0);

impl Guest for Writer {
    async fn run(_: Inputs) -> Outputs {
        let (tx, rx) = wit_future::new::<u32>(|| 0);
        wit_bindgen::spawn_local(async move {
            let ended = if tx.write(7).await.is_ok() { 1 } else { 2 };
            LAST.store(ended, Ordering::SeqCst);
        });
        Outputs {
            done: rx,
            last: LAST.load(Ordering::SeqCst),
        }
    }
}

export!(Writer);
