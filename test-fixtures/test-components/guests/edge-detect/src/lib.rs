wit_bindgen::generate!({ path: "wit", world: "edge-detect", generate_all });

use exports::node::{Frame, Guest, Inputs, Outputs};
use test::accel::cnn::{self, Conv, Layer, Tensor};
use witgraph::runtime::host;

/// The Sobel kernel across `x`: right minus left.
const SOBEL_X: [f32; 9] = [-1.0, 0.0, 1.0, -2.0, 0.0, 2.0, -1.0, 0.0, 1.0];
/// The Sobel kernel across `y`: below minus above.
const SOBEL_Y: [f32; 9] = [-1.0, -2.0, -1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 1.0];

/// The network: a 3x3 convolution into four channels (each Sobel kernel,
/// and its negation, so a rising and a falling edge both come through the
/// ReLU), then a 1x1 convolution summing them, clamped to `[0, 1]`.
fn network() -> Vec<Layer> {
    let negated = |k: [f32; 9]| k.map(|w| -w);
    let weights = [SOBEL_X, negated(SOBEL_X), SOBEL_Y, negated(SOBEL_Y)].concat();
    vec![
        Layer::Conv(Conv {
            out_channels: 4,
            size: 3,
            weights,
            bias: vec![0.0; 4],
        }),
        Layer::Relu,
        Layer::Conv(Conv {
            out_channels: 1,
            size: 1,
            weights: vec![0.5; 4],
            bias: vec![0.0],
        }),
        Layer::Clamp((0.0, 1.0)),
    ]
}

struct EdgeDetect;

impl Guest for EdgeDetect {
    async fn run(inputs: Inputs) -> Outputs {
        // Without an accelerator there is nothing to run on: say why.
        let backend = cnn::backend().unwrap_or_else(|reason| {
            host::fatal(&reason);
            unreachable!("`fatal` does not return")
        });
        let Inputs { mut frames } = inputs;
        let (mut tx, rx) = wit_stream::new::<Frame>();
        wit_bindgen::spawn_local(async move {
            let layers = network();
            while let Some(frame) = frames.next().await {
                let Ok(edges) = cnn::run(layers.clone(), luma(&frame)).await else {
                    break;
                };
                // `Some` back means the reader was dropped.
                if tx.write_one(image(&edges)).await.is_some() {
                    break;
                }
            }
            // Dropping the reader tells the upstream writer to stop.
            drop(frames);
        });
        Outputs { edges: rx, backend }
    }
}

/// A frame's brightness, one channel in `[0, 1]`.
fn luma(frame: &Frame) -> Tensor {
    let data = frame
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&[r, g, b, _]| (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0)
        .collect();
    Tensor {
        channels: 1,
        height: frame.height,
        width: frame.width,
        data,
    }
}

/// A one-channel tensor in `[0, 1]` as a gray frame.
fn image(tensor: &Tensor) -> Frame {
    let pixels = tensor
        .data
        .iter()
        .flat_map(|v| {
            let g = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            [g, g, g, 255]
        })
        .collect();
    Frame {
        width: tensor.width,
        height: tensor.height,
        pixels,
    }
}

export!(EdgeDetect);
