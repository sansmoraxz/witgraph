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

struct Point {
    x: i32,
    y: f64,
}

enum Shape {
    Dot,
    Circle(f32),
    Rect { w: u32, h: u32 },
}

/// A variable of each kind a debugger has to show.
fn kinds(seed: u32) -> u32 {
    let count = seed + 1;
    let negative = -(seed as i64) - 7;
    let ratio = 1.5f64;
    let flag = true;
    let letter = 'x';
    let point = Point { x: 3, y: 2.5 };
    let text: &str = "hello";
    let owned = String::from("owned");
    let list = vec![10u32, 20, 30];
    let slice: &[u32] = &list;
    let array = [1u8, 2, 3];
    let some = Some(7u32);
    let none: Option<u32> = None;
    let dot = Shape::Dot;
    let circle = Shape::Circle(0.5);
    let rect = Shape::Rect { w: 4, h: 5 };
    let pair = (1u8, 2u16);
    let reference = &point;
    let boxed = Box::new(9u32);
    core::hint::black_box((&negative, &ratio, &flag, &letter, &reference, &text)); // mark: kinds-used
    core::hint::black_box((&owned, &slice, &array, &some, &none, &dot, &circle, &rect, &pair));
    core::hint::black_box(&boxed); // mark: kinds-end
    count
}

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
        if let Some(seed) = inputs.kinds {
            out = out.wrapping_add(kinds(seed)); // mark: run-kinds
        }
        if let Some(rounds) = inputs.spin {
            out = out.wrapping_add(spin(rounds)); // mark: run-spin
        }
        Outputs { out } // mark: run-end
    }
}

export!(LineWalk);
