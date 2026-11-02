/**
 * `test:data/tensor@0.1.0` as the demo provides it: `uint8` tensors whose
 * elements live where the chosen backend keeps them (on the CPU, in a
 * WebGPU buffer, in a WebNN tensor), and whose operations run there.
 * Nodes only pass handles to them; a node's `read` is the one copy into
 * its memory. witgraph disposes each tensor (`[Symbol.dispose]`) once
 * nothing holds it, which frees what the backend holds for it.
 *
 * Values are as jco hands them over; a `result`'s `err` is thrown as
 * `{ payload }`.
 */

import { failure } from '../accel/cnn.ts';

/** Where a backend keeps a tensor's elements: its own. */
export type Storage = object;

/** Where tensors live, and their operations run. */
export interface Backend {
  /** `cpu`, `webgpu` or `webnn`; throws why when there is none. */
  name(): string;
  upload(shape: number[], data: Uint8Array): Promise<Storage>;
  download(storage: Storage, shape: number[]): Promise<Uint8Array>;
  invert(storage: Storage, shape: number[]): Promise<Storage>;
  /** Of a `[height, width, channels]` tensor, as the interface says. */
  conv3x3(storage: Storage, shape: number[], weights: Float32Array): Promise<Storage>;
  destroy(storage: Storage): void;
}

/** A tensor's elements, shared by every reference to it. */
interface Data {
  storage: Storage;
  shape: number[];
  /** References not yet disposed. */
  refs: number;
  /** The elements, read back once. */
  bytes?: Promise<Uint8Array>;
}

/** The `test:data/tensor` import for jco, over `backend`. */
export function tensors(backend: Backend): { Tensor: unknown } {
  const elements = (shape: number[]) => shape.reduce((n, d) => n * d, 1);
  /** Runs a backend's operation; a failure is the operation's `err`. */
  const attempt = async <T>(what: () => Promise<T>): Promise<T> => {
    try {
      return await what();
    } catch (error) {
      if (error && typeof error === 'object' && 'payload' in error) throw error;
      return failure(error instanceof Error ? error.message : String(error));
    }
  };

  class Tensor {
    readonly #data: Data;
    #disposed = false;

    private constructor(data: Data) {
      this.#data = data;
    }

    static #made(storage: Storage, shape: number[]): Tensor {
      return new Tensor({ storage, shape, refs: 1 });
    }

    static async fromBytes(dtype: string, shape: ArrayLike<number>, data: Uint8Array) {
      const dims = Array.from(shape);
      if (dtype !== 'uint8') failure(`only uint8 tensors here, not ${dtype}`);
      if (elements(dims) !== data.length) {
        failure(`${data.length} bytes for ${elements(dims)} elements`);
      }
      backend.name();
      return Tensor.#made(await attempt(() => backend.upload(dims, data.slice())), dims);
    }

    /** The backend the tensor lives on. */
    get backend(): string {
      return backend.name();
    }

    dtype(): string {
      return 'uint8';
    }

    shape(): number[] {
      return this.#data.shape;
    }

    async read(offset: bigint, len: bigint): Promise<Uint8Array> {
      const data = this.#data;
      data.bytes ??= backend.download(data.storage, data.shape);
      const bytes = await data.bytes;
      return bytes.slice(Number(offset), Number(offset + len));
    }

    clone(): Tensor {
      // Its own object: jco disposes it when the guest drops it.
      this.#data.refs += 1;
      return new Tensor(this.#data);
    }

    async invert(): Promise<Tensor> {
      const { storage, shape } = this.#data;
      return Tensor.#made(await backend.invert(storage, shape), shape);
    }

    async conv3x3(weights: Float32Array | number[]): Promise<Tensor> {
      const { storage, shape } = this.#data;
      if (shape.length !== 3) failure('a [height, width, channels] tensor');
      if (weights.length !== 9) failure('9 weights');
      const kernel = Float32Array.from(weights);
      return Tensor.#made(await attempt(() => backend.conv3x3(storage, shape, kernel)), shape);
    }

    toString(): string {
      return `uint8[${this.#data.shape.join(', ')}] on ${backend.name()}`;
    }

    [Symbol.dispose](): void {
      if (this.#disposed) return;
      this.#disposed = true;
      this.#data.refs -= 1;
      if (this.#data.refs === 0) backend.destroy(this.#data.storage);
    }
  }
  return { Tensor };
}

/** No such backend: making a tensor fails, saying why. */
export function unavailable(reason: string): Backend {
  const fail = (): never => failure(reason);
  return {
    name: fail,
    upload: async () => fail(),
    download: async () => fail(),
    invert: async () => fail(),
    conv3x3: async () => fail(),
    destroy: () => {},
  };
}

/** `[height, width, channels]` uint8 elements convolved as the interface says. */
export function conv3x3(input: Uint8Array, shape: number[], weights: Float32Array): Uint8Array {
  const [h, w, c] = shape;
  const out = new Uint8Array(input.length);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      for (let ch = 0; ch < c; ch++) {
        let sum = 0;
        for (let k = 0; k < 9; k++) {
          const sy = y + Math.floor(k / 3) - 1;
          const sx = x + (k % 3) - 1;
          if (sy >= 0 && sy < h && sx >= 0 && sx < w)
            sum += weights[k] * input[(sy * w + sx) * c + ch];
        }
        out[(y * w + x) * c + ch] = Math.min(255, Math.max(0, Math.trunc(sum)));
      }
    }
  }
  return out;
}

/** Tensors in this page's memory. */
export function cpu(): Backend {
  const bytes = (storage: Storage) => storage as Uint8Array;
  return {
    name: () => 'cpu',
    upload: async (_, data) => data,
    download: async (storage) => bytes(storage),
    invert: async (storage) => bytes(storage).map((b) => 255 - b),
    conv3x3: async (storage, shape, weights) => conv3x3(bytes(storage), shape, weights),
    destroy: () => {},
  };
}
