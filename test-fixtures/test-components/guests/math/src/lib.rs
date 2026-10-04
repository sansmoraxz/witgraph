wit_bindgen::generate!({ path: "wit", world: "math" });

use std::sync::atomic::{AtomicU32, Ordering};

use exports::test::math::ops::Guest;

static CALLS: AtomicU32 = AtomicU32::new(0);

struct Math;

impl Guest for Math {
    fn double(x: u32) -> u32 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        x * 2
    }

    async fn slow_double(x: u32) -> u32 {
        wit_bindgen::yield_async().await;
        x * 2
    }

    async fn count(n: u32) -> wit_bindgen::StreamReader<u32> {
        let (mut tx, rx) = wit_stream::new::<u32>();
        wit_bindgen::spawn_local(async move {
            for i in 0..n {
                if tx.write_one(i).await.is_some() {
                    break;
                }
            }
        });
        rx
    }

    fn calls() -> u32 {
        CALLS.load(Ordering::Relaxed)
    }
}

export!(Math);
