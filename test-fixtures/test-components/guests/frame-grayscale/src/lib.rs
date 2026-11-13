wit_bindgen::generate!({ path: "wit", world: "frame-grayscale" });

use exports::node::{Frame, Guest, Inputs, Outputs};

struct Grayscale;

impl Guest for Grayscale {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut frames } = inputs;
        let (mut tx, rx) = wit_stream::new::<Frame>();
        wit_bindgen::spawn_local(async move {
            while let Some(mut frame) = frames.next().await {
                for pixel in frame.pixels.as_chunks_mut::<4>().0 {
                    // Rec. 601 luma, in integers.
                    let [r, g, b, _] = pixel.map(u32::from);
                    let luma = ((299 * r + 587 * g + 114 * b) / 1000) as u8;
                    pixel[..3].fill(luma);
                }
                // `Some` back means the reader was dropped.
                if tx.write_one(frame).await.is_some() {
                    break;
                }
            }
            // Dropping the reader tells the upstream writer to stop.
            drop(frames);
        });
        Outputs { frames: rx }
    }
}

export!(Grayscale);
