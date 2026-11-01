/**
 * `test:accel/cnn@0.1.0` as the demo provides it: a small convolutional
 * network run on the CPU, on WebGPU or on WebNN. Values are as jco hands
 * them over; a `result`'s `err` is thrown as `{ payload }` (jco takes a
 * thrown `Error` for a trap instead).
 */

/** Values, channels first: `channels` x `height` x `width` of them. */
export interface Tensor {
  channels: number;
  height: number;
  width: number;
  data: Float32Array;
}

/** A 2-D convolution, stride 1, padded with zeros to keep the size. */
export interface Conv {
  outChannels: number;
  size: number;
  /** `outChannels` x the input's channels x `size` x `size`. */
  weights: Float32Array;
  /** One value per output channel. */
  bias: Float32Array;
}

/** One layer of a network. */
export type Layer =
  | { tag: 'conv'; val: Conv }
  | { tag: 'relu' }
  | { tag: 'clamp'; val: [number, number] };

/** The interface, as jco calls it. */
export interface Cnn {
  /** The accelerator `run` uses; throws why there is none. */
  backend(): string;
  /** `layers`, in order, over `input`. */
  run(layers: Layer[], input: Tensor): Promise<Tensor>;
}

/** Ends a call returning a `result` with `err(reason)`. */
export function failure(reason: string): never {
  throw { payload: reason };
}

/** No accelerator: every call fails, saying why. */
export function unavailable(reason: string): Cnn {
  return {
    backend: () => failure(reason),
    run: async () => failure(reason),
  };
}

/** The shape a convolution's weights must have for an input of `channels`. */
export function checkConv(conv: Conv, channels: number): void {
  const { outChannels: o, size: k } = conv;
  if (conv.weights.length !== o * channels * k * k || conv.bias.length !== o) {
    failure("the convolution's weights do not fit its input");
  }
}

/** The network on the CPU: the reference the accelerators are held to. */
export function cpu(): Cnn {
  return {
    backend: () => 'cpu',
    async run(layers, input) {
      let t = input;
      for (const layer of layers) {
        if (layer.tag === 'conv') {
          checkConv(layer.val, t.channels);
          t = convolve(layer.val, t);
        } else if (layer.tag === 'relu') {
          t = { ...t, data: t.data.map((v) => Math.max(v, 0)) };
        } else {
          const [lo, hi] = layer.val;
          t = { ...t, data: t.data.map((v) => Math.min(Math.max(v, lo), hi)) };
        }
      }
      return t;
    },
  };
}

function convolve({ outChannels: o, size: k, weights, bias }: Conv, t: Tensor): Tensor {
  const { channels: c, height: h, width: w, data } = t;
  const r = Math.floor(k / 2);
  const out = new Float32Array(o * h * w);
  for (let oc = 0; oc < o; oc++) {
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        let sum = bias[oc];
        for (let ic = 0; ic < c; ic++) {
          for (let ky = 0; ky < k; ky++) {
            const sy = y + ky - r;
            if (sy < 0 || sy >= h) continue;
            for (let kx = 0; kx < k; kx++) {
              const sx = x + kx - r;
              if (sx < 0 || sx >= w) continue;
              sum += data[(ic * h + sy) * w + sx] * weights[((oc * c + ic) * k + ky) * k + kx];
            }
          }
        }
        out[(oc * h + y) * w + x] = sum;
      }
    }
  }
  return { channels: o, height: h, width: w, data: out };
}
