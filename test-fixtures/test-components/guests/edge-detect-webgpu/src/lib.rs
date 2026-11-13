wit_bindgen::generate!({ path: "wit", world: "edge-detect-webgpu", generate_all });

use exports::node::{Frame, Guest, Inputs, Outputs};
use wasi::webgpu::webgpu::{
    self, GpuBindGroupDescriptor, GpuBindGroupEntry, GpuBindingResource, GpuBuffer,
    GpuBufferDescriptor, GpuBufferUsage, GpuCommandEncoder, GpuComputePipeline,
    GpuComputePipelineDescriptor, GpuDevice, GpuLayoutMode, GpuMapMode, GpuProgrammableStage,
    GpuShaderModuleDescriptor,
};
use witgraph::runtime::host;

/// A convolution: one invocation per output value.
const CONV: &str = "
struct Dims { in_c: u32, out_c: u32, h: u32, w: u32, k: u32 }
@group(0) @binding(0) var<uniform> d: Dims;
@group(0) @binding(1) var<storage, read> input: array<f32>;
@group(0) @binding(2) var<storage, read> weights: array<f32>;
@group(0) @binding(3) var<storage, read> bias: array<f32>;
@group(0) @binding(4) var<storage, read_write> output: array<f32>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  let x = id.x;
  let y = id.y;
  let o = id.z;
  if (x >= d.w || y >= d.h || o >= d.out_c) { return; }
  let r = i32(d.k / 2u);
  var sum = bias[o];
  for (var c = 0u; c < d.in_c; c++) {
    for (var ky = 0u; ky < d.k; ky++) {
      for (var kx = 0u; kx < d.k; kx++) {
        let sy = i32(y) + i32(ky) - r;
        let sx = i32(x) + i32(kx) - r;
        if (sy >= 0 && sy < i32(d.h) && sx >= 0 && sx < i32(d.w)) {
          sum += input[(c * d.h + u32(sy)) * d.w + u32(sx)]
            * weights[((o * d.in_c + c) * d.k + ky) * d.k + kx];
        }
      }
    }
  }
  output[(o * d.h + y) * d.w + x] = sum;
}";

/// ReLU (`mode` 0) or a clamp to `[lo, hi]` (1), in place.
const ACTIVATION: &str = "
struct Params { n: u32, mode: u32, lo: f32, hi: f32 }
@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read_write> data: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= p.n) { return; }
  let v = data[id.x];
  data[id.x] = select(clamp(v, p.lo, p.hi), max(v, 0.0), p.mode == 0u);
}";

/// The Sobel kernel across `x`: right minus left.
const SOBEL_X: [f32; 9] = [-1.0, 0.0, 1.0, -2.0, 0.0, 2.0, -1.0, 0.0, 1.0];
/// The Sobel kernel across `y`: below minus above.
const SOBEL_Y: [f32; 9] = [-1.0, -2.0, -1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 1.0];

/// One layer of the network.
enum Layer {
    /// A 2-D convolution, stride 1 and padded to keep the size.
    Conv {
        out_channels: u32,
        size: u32,
        weights: Vec<f32>,
        bias: Vec<f32>,
    },
    Relu,
    /// Every value clamped to `[min, max]`.
    Clamp(f32, f32),
}

/// The network: a 3x3 convolution into four channels (each Sobel kernel,
/// and its negation, so a rising and a falling edge both come through the
/// ReLU), then a 1x1 convolution summing them, clamped to `[0, 1]`.
fn network() -> Vec<Layer> {
    let negated = |k: [f32; 9]| k.map(|w| -w);
    let weights = [SOBEL_X, negated(SOBEL_X), SOBEL_Y, negated(SOBEL_Y)].concat();
    vec![
        Layer::Conv {
            out_channels: 4,
            size: 3,
            weights,
            bias: vec![0.0; 4],
        },
        Layer::Relu,
        Layer::Conv {
            out_channels: 1,
            size: 1,
            weights: vec![0.5; 4],
            bias: vec![0.0],
        },
        Layer::Clamp(0.0, 1.0),
    ]
}

/// A device and what the network needs of it, made once for a run.
struct Gpu {
    device: GpuDevice,
    conv: GpuComputePipeline,
    activation: GpuComputePipeline,
}

impl Gpu {
    /// The host's GPU, or why there is none.
    async fn new() -> Result<Self, String> {
        let adapter = webgpu::get_gpu()
            .request_adapter(None)
            .await
            .ok_or("WebGPU is not available in this browser")?;
        let device = adapter
            .request_device(None)
            .await
            .map_err(|e| format!("WebGPU: {}", e.message))?;
        let conv = pipeline(&device, CONV);
        let activation = pipeline(&device, ACTIVATION);
        Ok(Self {
            device,
            conv,
            activation,
        })
    }

    /// A buffer of at least `size` bytes: bindings want at least 4, uniforms
    /// a multiple of 16. It is written `data`, if any.
    fn buffer(&self, size: u64, usage: GpuBufferUsage, data: Option<&[u8]>) -> GpuBuffer {
        let buffer = self.device.create_buffer(&GpuBufferDescriptor {
            size: size.div_ceil(16).max(1) * 16,
            usage,
            mapped_at_creation: None,
            label: None,
        });
        if let Some(data) = data {
            // Only a bad range fails, and this one is the buffer's own.
            let _ = self
                .device
                .queue()
                .write_buffer_with_copy(&buffer, 0, data, None, None);
        }
        buffer
    }

    /// `layers`, in order, over one channel of `height` x `width` values.
    async fn run(
        &self,
        layers: &[Layer],
        height: u32,
        width: u32,
        input: &[f32],
    ) -> Result<Vec<f32>, String> {
        let storage = GpuBufferUsage::STORAGE | GpuBufferUsage::COPY_DST | GpuBufferUsage::COPY_SRC;
        let uniform = GpuBufferUsage::UNIFORM | GpuBufferUsage::COPY_DST;
        let mut made = Vec::new();
        let encoder = self.device.create_command_encoder(None);
        let mut channels = 1;
        let mut current = self.buffer(input.len() as u64 * 4, storage, Some(&bytes(input)));
        for layer in layers {
            match layer {
                Layer::Conv {
                    out_channels,
                    size,
                    weights,
                    bias,
                } => {
                    let dims = [channels, *out_channels, height, width, *size]
                        .map(u32::to_le_bytes)
                        .concat();
                    let values = out_channels * height * width;
                    let entries = vec![
                        self.buffer(dims.len() as u64, uniform, Some(&dims)),
                        current,
                        self.buffer(weights.len() as u64 * 4, storage, Some(&bytes(weights))),
                        self.buffer(bias.len() as u64 * 4, storage, Some(&bytes(bias))),
                        self.buffer(u64::from(values) * 4, storage, None),
                    ];
                    let groups = [width.div_ceil(8), height.div_ceil(8), *out_channels];
                    self.dispatch(&encoder, &self.conv, &entries, groups);
                    let mut entries = entries;
                    current = entries.remove(4);
                    made.extend(entries);
                    channels = *out_channels;
                }
                Layer::Relu | Layer::Clamp(..) => {
                    let n = channels * height * width;
                    let (mode, lo, hi) = match layer {
                        Layer::Clamp(lo, hi) => (1u32, *lo, *hi),
                        _ => (0, 0.0, 0.0),
                    };
                    let params = [
                        n.to_le_bytes(),
                        mode.to_le_bytes(),
                        lo.to_le_bytes(),
                        hi.to_le_bytes(),
                    ]
                    .concat();
                    let entries = vec![self.buffer(16, uniform, Some(&params)), current];
                    self.dispatch(&encoder, &self.activation, &entries, [n.div_ceil(64), 1, 1]);
                    let mut entries = entries;
                    current = entries.remove(1);
                    made.extend(entries);
                }
            }
        }
        let size = u64::from(channels * height * width) * 4;
        let staging = self.buffer(size, GpuBufferUsage::MAP_READ | GpuBufferUsage::COPY_DST, None);
        encoder.copy_buffer_to_buffer(&current, Some(0), &staging, Some(0), Some(size));
        let commands = encoder.finish(None);
        self.device.queue().submit(&[&commands]);
        let mapped = staging
            .map_async(GpuMapMode::READ, Some(0), Some(size))
            .await
            .map_err(|e| format!("WebGPU: {}", e.message));
        let read = mapped.and_then(|()| {
            staging
                .get_mapped_range_get_with_copy(Some(0), Some(size))
                .map_err(|e| format!("WebGPU: {}", e.message))
        });
        made.push(current);
        made.push(staging);
        for buffer in &made {
            buffer.destroy();
        }
        Ok(read?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&b| f32::from_le_bytes(b))
            .collect())
    }

    /// A compute pass of `pipeline` over `buffers`, bound in order.
    fn dispatch(
        &self,
        encoder: &GpuCommandEncoder,
        pipeline: &GpuComputePipeline,
        buffers: &[GpuBuffer],
        [x, y, z]: [u32; 3],
    ) {
        let layout = pipeline.get_bind_group_layout(0);
        let entries = buffers
            .iter()
            .zip(0..)
            .map(|(buffer, binding)| GpuBindGroupEntry {
                binding,
                resource: GpuBindingResource::GpuBuffer(buffer),
            })
            .collect::<Vec<_>>();
        let group = self.device.create_bind_group(&GpuBindGroupDescriptor {
            layout: &layout,
            entries,
            label: None,
        });
        let pass = encoder.begin_compute_pass(None);
        pass.set_pipeline(pipeline);
        // Only bad dynamic offsets fail, and there are none.
        let _ = pass.set_bind_group(0, Some(&group), None, None, None);
        pass.dispatch_workgroups(x, Some(y), Some(z));
        pass.end();
    }
}

/// A compute pipeline running `code`'s `main`.
fn pipeline(device: &GpuDevice, code: &str) -> GpuComputePipeline {
    let module = device.create_shader_module(&GpuShaderModuleDescriptor {
        code: code.into(),
        compilation_hints: None,
        label: None,
    });
    device.create_compute_pipeline(GpuComputePipelineDescriptor {
        compute: GpuProgrammableStage {
            module: &module,
            entry_point: Some("main".into()),
            constants: None,
        },
        layout: GpuLayoutMode::Auto,
        label: None,
    })
}

fn bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

struct EdgeDetect;

impl Guest for EdgeDetect {
    async fn run(inputs: Inputs) -> Outputs {
        // Without a GPU there is nothing to run on: say why.
        let gpu = Gpu::new().await.unwrap_or_else(|reason| {
            host::fatal(&reason);
            unreachable!("`fatal` does not return")
        });
        let Inputs { mut frames } = inputs;
        let (mut tx, rx) = wit_stream::new::<Frame>();
        wit_bindgen::spawn_local(async move {
            let layers = network();
            while let Some(frame) = frames.next().await {
                let edges = gpu
                    .run(&layers, frame.height, frame.width, &luma(&frame))
                    .await;
                let Ok(edges) = edges else {
                    break;
                };
                // `Some` back means the reader was dropped.
                if tx
                    .write_one(image(frame.width, frame.height, &edges))
                    .await
                    .is_some()
                {
                    break;
                }
            }
            // Dropping the reader tells the upstream writer to stop.
            drop(frames);
        });
        Outputs {
            edges: rx,
            backend: "webgpu".into(),
        }
    }
}

/// A frame's brightness, one channel in `[0, 1]`.
fn luma(frame: &Frame) -> Vec<f32> {
    frame
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&[r, g, b, _]| (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0)
        .collect()
}

/// One channel in `[0, 1]` as a gray frame.
fn image(width: u32, height: u32, values: &[f32]) -> Frame {
    let pixels = values
        .iter()
        .flat_map(|v| {
            let g = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            [g, g, g, 255]
        })
        .collect();
    Frame {
        width,
        height,
        pixels,
    }
}

export!(EdgeDetect);
