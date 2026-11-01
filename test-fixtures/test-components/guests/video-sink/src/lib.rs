wit_bindgen::generate!({ path: "wit", world: "video-sink" });

use exports::node::{Guest, Inputs, Outputs};

struct Sink;

impl Guest for Sink {
    async fn run(inputs: Inputs) -> Outputs {
        let Inputs { mut frames } = inputs;
        let mut outputs = Outputs {
            count: 0,
            gray: true,
            first_pixel: 0,
        };
        while let Some(frame) = frames.next().await {
            outputs.count += 1;
            outputs.gray &= frame
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|[r, g, b, _]| r == g && g == b);
            if let Some(&[r, g, b, a]) = frame.pixels.first_chunk::<4>() {
                outputs.first_pixel = u32::from_be_bytes([r, g, b, a]);
            }
        }
        outputs
    }
}

export!(Sink);
