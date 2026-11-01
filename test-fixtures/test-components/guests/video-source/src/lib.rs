wit_bindgen::generate!({ path: "wit", world: "video-source", generate_all });

use exports::node::{Frame, Guest, Inputs, Outputs};
use test::video::clock;

/// Declares how an editor previews this node's outputs (the
/// `witgraph-viewer` custom section).
macro_rules! viewer {
    ($hints:literal) => {
        #[used]
        #[unsafe(link_section = "witgraph-viewer")]
        static VIEWER: [u8; $hints.len()] = *$hints;
    };
}

viewer!(b"output.frames=image\n");

const WIDTH: u32 = 96;
const HEIGHT: u32 = 64;
const SQUARE: u32 = 16;

struct Source;

impl Guest for Source {
    async fn run(inputs: Inputs) -> Outputs {
        let (mut tx, rx) = wit_stream::new::<Frame>();
        // Return the reader first and write afterwards: the readers can only
        // start once this `run` has handed over its stream.
        wit_bindgen::spawn_local(async move {
            let mut t = 0u32;
            while inputs.frames.is_none_or(|n| t < n) {
                if t > 0
                    && let Some(fps) = inputs.fps.filter(|&fps| fps > 0)
                {
                    clock::sleep(1000 / fps).await;
                }
                // `Some` back means the reader was dropped.
                if tx.write_one(render(t)).await.is_some() {
                    break;
                }
                t = t.wrapping_add(1);
            }
        });
        Outputs { frames: rx }
    }
}

/// Frame `t`: a hue gradient scrolling right, darker towards the bottom,
/// under a white square bouncing off the edges.
fn render(t: u32) -> Frame {
    let sx = bounce(t.wrapping_mul(2), WIDTH - SQUARE);
    let sy = bounce(t.wrapping_mul(3) / 2, HEIGHT - SQUARE);
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let in_square = (sx..sx + SQUARE).contains(&x) && (sy..sy + SQUARE).contains(&y);
            let [r, g, b] = if in_square {
                [255, 255, 255]
            } else {
                let hue = (x * 360 / WIDTH + t.wrapping_mul(6)) % 360;
                let shade = 255 - y * 2;
                hue_rgb(hue).map(|c| (u32::from(c) * shade / 255) as u8)
            };
            pixels.extend([r, g, b, 255]);
        }
    }
    Frame {
        width: WIDTH,
        height: HEIGHT,
        pixels,
    }
}

/// `n` folded back and forth over `0..=span`.
fn bounce(n: u32, span: u32) -> u32 {
    let p = n % (2 * span);
    if p <= span { p } else { 2 * span - p }
}

/// The fully saturated colour of `hue` degrees.
fn hue_rgb(hue: u32) -> [u8; 3] {
    let rise = ((hue % 60) * 255 / 60) as u8;
    match hue / 60 {
        0 => [255, rise, 0],
        1 => [255 - rise, 255, 0],
        2 => [0, 255, rise],
        3 => [0, 255 - rise, 255],
        4 => [rise, 0, 255],
        _ => [255, 0, 255 - rise],
    }
}

export!(Source);
