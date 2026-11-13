wit_bindgen::generate!({ path: "wit", world: "frame-invert" });

use exports::node::{Frame, Guest, Inputs, Outputs};

struct Invert;

impl Guest for Invert {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut frames } = inputs;
        let (mut tx, rx) = wit_stream::new::<Frame>();
        wit_bindgen::spawn_local(async move {
            while let Some(mut frame) = frames.next().await {
                for pixel in frame.pixels.as_chunks_mut::<4>().0 {
                    for channel in &mut pixel[..3] {
                        *channel = 255 - *channel;
                    }
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

export!(Invert);
