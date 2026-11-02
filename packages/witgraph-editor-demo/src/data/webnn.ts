import { type Backend, unavailable } from './tensors.ts';

// The little of WebNN used here: TypeScript's DOM library has none of it.
interface MLOperandDescriptor {
  dataType: 'float32';
  shape: number[];
}
type MLOperand = object;
interface MLTensor {
  destroy(): void;
}
type MLGraph = object;
interface MLContext {
  createTensor(
    descriptor: MLOperandDescriptor & { readable?: boolean; writable?: boolean },
  ): Promise<MLTensor>;
  writeTensor(tensor: MLTensor, data: ArrayBufferView): void;
  readTensor(tensor: MLTensor): Promise<ArrayBuffer>;
  dispatch(
    graph: MLGraph,
    inputs: Record<string, MLTensor>,
    outputs: Record<string, MLTensor>,
  ): void;
}
interface MLGraphBuilder {
  input(name: string, descriptor: MLOperandDescriptor): MLOperand;
  constant(descriptor: MLOperandDescriptor, data: ArrayBufferView): MLOperand;
  sub(a: MLOperand, b: MLOperand): MLOperand;
  floor(x: MLOperand): MLOperand;
  clamp(x: MLOperand, options: { minValue: number; maxValue: number }): MLOperand;
  conv2d(
    input: MLOperand,
    filter: MLOperand,
    options: { padding: number[]; groups: number; inputLayout: 'nhwc'; filterLayout: 'ohwi' },
  ): MLOperand;
  build(outputs: Record<string, MLOperand>): Promise<MLGraph>;
}
/** WebNN's graph builder, where the browser has one. */
const Builder = (
  globalThis as unknown as { MLGraphBuilder: new (context: MLContext) => MLGraphBuilder }
).MLGraphBuilder;

/** Tensors in WebNN tensors, `float32` `[1, height, width, channels]` (or `[n]`). */
export async function webnn(): Promise<Backend> {
  const ml = (navigator as { ml?: { createContext(): Promise<MLContext> } }).ml;
  const context = await ml?.createContext().catch(() => undefined);
  if (!context) return unavailable('WebNN is not available in this browser');
  const dims = (shape: number[]) => (shape.length === 3 ? [1, ...shape] : shape);
  const descriptor = (shape: number[]): MLOperandDescriptor => ({
    dataType: 'float32',
    shape: dims(shape),
  });
  const tensor = (shape: number[]) =>
    context.createTensor({ ...descriptor(shape), readable: true, writable: true });
  /** Graphs built once per operation, shape and kernel. */
  const graphs = new Map<string, Promise<MLGraph>>();
  const graph = (key: string, build: (b: MLGraphBuilder) => MLOperand) => {
    let built = graphs.get(key);
    if (!built) {
      const builder = new Builder(context);
      built = builder.build({ y: build(builder) });
      graphs.set(key, built);
      built.catch(() => graphs.delete(key));
    }
    return built;
  };
  /** Runs a graph over `x` into a new tensor: dispatched, not awaited. */
  const run = async (built: Promise<MLGraph>, x: MLTensor, shape: number[]) => {
    const y = await tensor(shape);
    context.dispatch(await built, { x }, { y });
    return y;
  };
  return {
    name: () => 'webnn',
    async upload(shape, data) {
      const t = await tensor(shape);
      context.writeTensor(t, Float32Array.from(data));
      return t;
    },
    async download(storage) {
      return Uint8Array.from(new Float32Array(await context.readTensor(storage as MLTensor)));
    },
    async invert(storage, shape) {
      const built = graph(`invert ${shape}`, (b) => {
        const x = b.input('x', descriptor(shape));
        const max = b.constant(descriptor(shape), new Float32Array(count(shape)).fill(255));
        return b.sub(max, x);
      });
      return run(built, storage as MLTensor, shape);
    },
    async conv3x3(storage, shape, weights) {
      const [, , c] = shape;
      const built = graph(`conv ${shape} ${weights}`, (b) => {
        const x = b.input('x', descriptor(shape));
        // Depthwise: each channel by the same kernel.
        const filter = new Float32Array(c * 9);
        for (let ch = 0; ch < c; ch++) filter.set(weights, ch * 9);
        const y = b.conv2d(x, b.constant({ dataType: 'float32', shape: [c, 3, 3, 1] }, filter), {
          padding: [1, 1, 1, 1],
          groups: c,
          inputLayout: 'nhwc',
          filterLayout: 'ohwi',
        });
        // Truncated toward zero, then clamped: for what survives the clamp, a floor.
        return b.clamp(b.floor(y), { minValue: 0, maxValue: 255 });
      });
      return run(built, storage as MLTensor, shape);
    },
    destroy: (storage) => (storage as MLTensor).destroy(),
  };
}

function count(shape: number[]): number {
  return shape.reduce((n, d) => n * d, 1);
}
