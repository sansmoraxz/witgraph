import { type Cnn, checkConv, failure, type Layer, type Tensor, unavailable } from './cnn.ts';

// The little of WebNN used here: TypeScript's DOM library has none of it.
interface MLOperandDescriptor {
  dataType: 'float32';
  shape: number[];
}
interface MLTensorDescriptor extends MLOperandDescriptor {
  readable?: boolean;
  writable?: boolean;
}
type MLOperand = object;
type MLTensor = object;
type MLGraph = object;
interface MLContext {
  createTensor(descriptor: MLTensorDescriptor): Promise<MLTensor>;
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
  conv2d(
    input: MLOperand,
    filter: MLOperand,
    options: { padding: number[]; bias?: MLOperand },
  ): MLOperand;
  relu(input: MLOperand): MLOperand;
  clamp(input: MLOperand, options: { minValue: number; maxValue: number }): MLOperand;
  build(outputs: Record<string, MLOperand>): Promise<MLGraph>;
}
declare global {
  interface Navigator {
    ml?: { createContext(): Promise<MLContext> };
  }
  var MLGraphBuilder: new (context: MLContext) => MLGraphBuilder;
}

/** A network built for one input shape, with its tensors. */
interface Built {
  graph: MLGraph;
  input: MLTensor;
  output: MLTensor;
  shape: [number, number, number];
  /** The run using the tensors last: the next one waits for it. */
  turn: Promise<unknown>;
}

/** The network through WebNN: built into a graph once per network and shape. */
export async function webnn(): Promise<Cnn> {
  const context = await navigator.ml?.createContext().catch(() => undefined);
  if (!context) return unavailable('WebNN is not available in this browser');
  const built = new Map<string, Promise<Built>>();
  return {
    backend: () => 'webnn',
    async run(layers, input) {
      const key = JSON.stringify([
        input.channels,
        input.height,
        input.width,
        layers.map((l) =>
          l.tag === 'conv'
            ? [l.val.outChannels, l.val.size, [...l.val.weights], [...l.val.bias]]
            : l,
        ),
      ]);
      try {
        let network = built.get(key);
        if (!network) {
          network = build(context, layers, input);
          built.set(key, network);
          network.catch(() => built.delete(key));
        }
        const net = await network;
        const { graph, input: x, output: y, shape } = net;
        // One run at a time uses the network's tensors.
        const turn = net.turn.then(async () => {
          context.writeTensor(x, input.data);
          context.dispatch(graph, { x }, { y });
          return new Float32Array(await context.readTensor(y));
        });
        net.turn = turn.catch(() => {});
        const data = await turn;
        const [channels, height, width] = shape;
        return { channels, height, width, data } satisfies Tensor;
      } catch (error) {
        if (error && typeof error === 'object' && 'payload' in error) throw error;
        return failure(`WebNN: ${error instanceof Error ? error.message : String(error)}`);
      }
    },
  };
}

async function build(context: MLContext, layers: Layer[], input: Tensor): Promise<Built> {
  const { height: h, width: w } = input;
  let channels = input.channels;
  const builder = new MLGraphBuilder(context);
  const x = builder.input('x', { dataType: 'float32', shape: [1, channels, h, w] });
  let y = x;
  for (const layer of layers) {
    if (layer.tag === 'conv') {
      const { outChannels: o, size: k, weights, bias } = layer.val;
      checkConv(layer.val, channels);
      const r = Math.floor(k / 2);
      y = builder.conv2d(
        y,
        builder.constant({ dataType: 'float32', shape: [o, channels, k, k] }, weights),
        {
          padding: [r, k - 1 - r, r, k - 1 - r],
          bias: builder.constant({ dataType: 'float32', shape: [o] }, bias),
        },
      );
      channels = o;
    } else if (layer.tag === 'relu') {
      y = builder.relu(y);
    } else {
      y = builder.clamp(y, { minValue: layer.val[0], maxValue: layer.val[1] });
    }
  }
  const graph = await builder.build({ y });
  const tensor = (shape: number[], access: Partial<MLTensorDescriptor>) =>
    context.createTensor({ dataType: 'float32', shape, ...access });
  return {
    graph,
    input: await tensor([1, input.channels, h, w], { writable: true }),
    output: await tensor([1, channels, h, w], { readable: true }),
    shape: [channels, h, w],
    turn: Promise.resolve(),
  };
}
