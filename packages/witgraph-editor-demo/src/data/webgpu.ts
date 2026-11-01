import { type Backend, type Storage, unavailable } from './tensors.ts';

/** `GPUBufferUsage` and `GPUMapMode` flags, as the WebGPU specification fixes them. */
const USAGE = { MAP_READ: 0x1, COPY_SRC: 0x4, COPY_DST: 0x8, UNIFORM: 0x40, STORAGE: 0x80 };
const MAP_READ = 0x1;

/** `255 - x` for each of `n` elements. */
const INVERT = /* wgsl */ `
@group(0) @binding(0) var<uniform> n: vec4<u32>;
@group(0) @binding(1) var<storage, read> input: array<f32>;
@group(0) @binding(2) var<storage, read_write> output: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  if (id.x >= n.x) { return; }
  output[id.x] = 255.0 - input[id.x];
}`;

/** One invocation per element of a `[h, w, c]` tensor, as the interface says. */
const CONV = /* wgsl */ `
@group(0) @binding(0) var<uniform> d: vec4<u32>;
@group(0) @binding(1) var<storage, read> input: array<f32>;
@group(0) @binding(2) var<storage, read> weights: array<f32>;
@group(0) @binding(3) var<storage, read_write> output: array<f32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
  let h = d.x; let w = d.y; let c = d.z;
  let i = id.x;
  if (i >= h * w * c) { return; }
  let ch = i % c;
  let x = (i / c) % w;
  let y = i / (c * w);
  var sum = 0.0;
  for (var k = 0u; k < 9u; k++) {
    let sy = i32(y) + i32(k / 3u) - 1;
    let sx = i32(x) + i32(k % 3u) - 1;
    if (sy >= 0 && sy < i32(h) && sx >= 0 && sx < i32(w)) {
      sum += weights[k] * input[(u32(sy) * w + u32(sx)) * c + ch];
    }
  }
  output[i] = clamp(trunc(sum), 0.0, 255.0);
}`;

/** Tensors in WebGPU storage buffers, an `f32` per element. */
export async function webgpu(): Promise<Backend> {
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return unavailable('WebGPU is not available in this browser');
  const device = await adapter.requestDevice();
  const pipeline = (code: string) =>
    device.createComputePipeline({
      layout: 'auto',
      compute: { module: device.createShaderModule({ code }), entryPoint: 'main' },
    });
  const invert = pipeline(INVERT);
  const conv = pipeline(CONV);
  const STORAGE = USAGE.STORAGE | USAGE.COPY_DST | USAGE.COPY_SRC;
  const elements = (shape: number[]) => shape.reduce((n, d) => n * d, 1);
  // Bindings want at least 16 bytes.
  const sized = (bytes: number) => Math.max(16, Math.ceil(bytes / 16) * 16);
  const buffer = (bytes: number, usage: number, data?: ArrayBufferView) => {
    const b = device.createBuffer({ size: sized(bytes), usage });
    if (data) device.queue.writeBuffer(b, 0, data.buffer, data.byteOffset, data.byteLength);
    return b;
  };
  /** Runs `pipeline` over `n` elements into a new buffer: queued, not awaited. */
  const run = (
    pipeline: GPUComputePipeline,
    n: number,
    uniform: Uint32Array,
    inputs: GPUBuffer[],
  ) => {
    const output = buffer(n * 4, STORAGE);
    const params = buffer(16, USAGE.UNIFORM | USAGE.COPY_DST, uniform);
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginComputePass();
    pass.setPipeline(pipeline);
    pass.setBindGroup(
      0,
      device.createBindGroup({
        layout: pipeline.getBindGroupLayout(0),
        entries: [params, ...inputs, output].map((b, binding) => ({
          binding,
          resource: { buffer: b },
        })),
      }),
    );
    pass.dispatchWorkgroups(Math.ceil(n / 64));
    pass.end();
    device.queue.submit([encoder.finish()]);
    // Queued work keeps what it uses alive: these may go now.
    params.destroy();
    return output;
  };
  return {
    name: () => 'webgpu',
    async upload(_, data) {
      return buffer(data.length * 4, STORAGE, Float32Array.from(data));
    },
    async download(storage, shape) {
      const bytes = elements(shape) * 4;
      const staging = buffer(bytes, USAGE.MAP_READ | USAGE.COPY_DST);
      const encoder = device.createCommandEncoder();
      encoder.copyBufferToBuffer(storage as GPUBuffer, 0, staging, 0, bytes);
      device.queue.submit([encoder.finish()]);
      try {
        await staging.mapAsync(MAP_READ, 0, bytes);
        return Uint8Array.from(new Float32Array(staging.getMappedRange(0, bytes)));
      } finally {
        staging.destroy();
      }
    },
    async invert(storage: Storage, shape) {
      const n = elements(shape);
      return run(invert, n, Uint32Array.of(n, 0, 0, 0), [storage as GPUBuffer]);
    },
    async conv3x3(storage, shape, weights) {
      const [h, w, c] = shape;
      const kernel = buffer(weights.byteLength, STORAGE, weights);
      const out = run(conv, h * w * c, Uint32Array.of(h, w, c, 0), [storage as GPUBuffer, kernel]);
      kernel.destroy();
      return out;
    },
    destroy: (storage) => (storage as GPUBuffer).destroy(),
  };
}
