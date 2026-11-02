wit_bindgen::generate!({ path: "wit", world: "labelled", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct Labelled;

impl Guest for Labelled {
    async fn run(inputs: Inputs) -> Outputs {
        let key = inputs.key.unwrap_or_default();
        Outputs {
            primary: read(primary::open("default").map(|b| b.get(&key))),
            backup: read(backup::open("default").map(|b| b.get(&key))),
        }
    }
}

/// What a store answered as text; empty when it has nothing there or fails.
fn read<E, F>(answer: Result<Result<Option<Vec<u8>>, F>, E>) -> String {
    match answer {
        Ok(Ok(Some(value))) => String::from_utf8_lossy(&value).into_owned(),
        _ => String::new(),
    }
}

export!(Labelled);
