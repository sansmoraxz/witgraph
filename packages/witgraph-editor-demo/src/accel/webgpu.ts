import { type Cnn, checkConv, failure, type Layer, type Tensor, unavailable } from './cnn.ts';

/**
 * `GPUBufferUsage` and `GPUMapMode` flags, as the WebGPU specification
 * fixes them (TypeScript types the API but not these globals).
 */
const USAGE = { MAP_READ: 0x1, COPY_SRC: 0x4, COPY_DST: 0x8, UNIFORM: 0x40, STORAGE: 0x80 };
const MAP_READ = 0x1;

/** A convolution: one invocation per output value. */
const CONV = /* wgsl */ `
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
}`;

/** ReLU (`mode` 0) or a clamp to `[lo, hi]` (1), in place. */
const ACTIVATION = /* wgsl */ `
struct Params { n: u32, mode: u32, lo: f32, hi: f32 }
@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read_write> data: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= p.n) { return; }
  let v = data[id.x];
  data[id.x] = select(clamp(v, p.lo, p.hi), max(v, 0.0), p.mode == 0u);
}`;

/** The network on the GPU, through WebGPU compute shaders. */
export async function webgpu(): Promise<Cnn> {
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return unavailable('WebGPU is not available in this browser');
  const device = await adapter.requestDevice();
  const pipeline = (code: string) =>
    device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code }), entryPoint: 'main' },
    });
  const conv = pipeline(CONV);
  const activation = pipeline(ACTIVATION);
  return {
    backend: () => 'webgpu',
    run: (layers, input) => run(device, conv, activation, layers, input),
  };
}

async function run(
  device: GPUDevice,
  conv: GPUComputePipeline,
  activation: GPUComputePipeline,
  layers: Layer[],
  input: Tensor,
): Promise<Tensor> {
  const made: GPUBuffer[] = [];
  const buffer = (size: number, usage: GPUBufferUsageFlags, data?: ArrayBufferView) => {
    // Bindings want a size of at least 4 bytes, uniforms a multiple of 16.
    const b = device.createBuffer({ size: Math.max(16, Math.ceil(size / 16) * 16), usage });
    if (data) device.queue.writeBuffer(b, 0, data.buffer, data.byteOffset, data.byteLength);
    made.push(b);
    return b;
  };
  const STORAGE = USAGE.STORAGE | USAGE.COPY_DST | USAGE.COPY_SRC;
  const UNIFORM = USAGE.UNIFORM | USAGE.COPY_DST;
  try {
    const { height: h, width: w } = input;
    let channels = input.channels;
    let current = buffer(input.data.byteLength, STORAGE, input.data);
    const encoder = device.createCommandEncoder();
    const dispatch = (
      pipeline: GPUComputePipeline,
      entries: GPUBuffer[],
      groups: [number, number, number],
    ) => {
      const pass = encoder.beginComputePass();
      pass.setPipeline(pipeline);
      pass.setBindGroup(
        0,
        device.createBindGroup({
          layout: pipeline.getBindGroupLayout(0),
          entries: entries.map((b, binding) => ({ binding, resource: { buffer: b } })),
        }),
      );
      pass.dispatchWorkgroups(...groups);
      pass.end();
    };
    for (const layer of layers) {
      if (layer.tag === 'conv') {
        const { outChannels: o, size: k, weights, bias } = layer.val;
        checkConv(layer.val, channels);
        const dims = buffer(20, UNIFORM, Uint32Array.of(channels, o, h, w, k));
        const out = buffer(o * h * w * 4, STORAGE);
        dispatch(
          conv,
          [
            dims,
            current,
            buffer(weights.byteLength, STORAGE, weights),
            buffer(bias.byteLength, STORAGE, bias),
            out,
          ],
          [Math.ceil(w / 8), Math.ceil(h / 8), o],
        );
        current = out;
        channels = o;
      } else {
        const n = channels * h * w;
        const params = new DataView(new ArrayBuffer(16));
        params.setUint32(0, n, true);
        params.setUint32(4, layer.tag === 'relu' ? 0 : 1, true);
        if (layer.tag === 'clamp') {
          params.setFloat32(8, layer.val[0], true);
          params.setFloat32(12, layer.val[1], true);
        }
        dispatch(activation, [buffer(16, UNIFORM, params), current], [Math.ceil(n / 64), 1, 1]);
      }
    }
    const size = channels * h * w * 4;
    const staging = buffer(size, USAGE.MAP_READ | USAGE.COPY_DST);
    encoder.copyBufferToBuffer(current, 0, staging, 0, size);
    device.queue.submit([encoder.finish()]);
    await staging.mapAsync(MAP_READ, 0, size);
    const data = new Float32Array(staging.getMappedRange(0, size).slice(0));
    staging.unmap();
    return { channels, height: h, width: w, data };
  } catch (error) {
    // A `result`'s error: a message, not a trap.
    if (error && typeof error === 'object' && 'payload' in error) throw error;
    return failure(`WebGPU: ${error instanceof Error ? error.message : String(error)}`);
  } finally {
    for (const b of made) b.destroy();
  }
}
