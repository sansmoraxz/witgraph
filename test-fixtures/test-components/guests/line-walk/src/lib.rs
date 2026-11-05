wit_bindgen::generate!({ path: "wit", world: "line-walk", generate_all });

use exports::node::{Guest, Inputs, Outputs};

struct LineWalk;

fn add(a: u32, b: u32) -> u32 {
    let sum = a.wrapping_add(b); // mark: add-body
    sum
} // mark: add-end

// mark: above-walk
fn walk(x: u32) -> u32 {
    let a = add(x, 1); // mark: walk-first
    let b = add(a, 2); // mark: walk-second
    b
} // mark: walk-end

fn spin(rounds: u32) -> u32 {
    let mut x = 0u32;
    for i in 0..rounds {
        x = core::hint::black_box(x.wrapping_add(i)); // mark: spin-body
    }
    x
}

impl Guest for LineWalk {
    async fn run(inputs: Inputs) -> Outputs {
        let mut out = walk(inputs.in_.unwrap_or(0)); // mark: run-walk
        if let Some(rounds) = inputs.spin {
            out = out.wrapping_add(spin(rounds)); // mark: run-spin
        }
        Outputs { out } // mark: run-end
    }
}

export!(LineWalk);
