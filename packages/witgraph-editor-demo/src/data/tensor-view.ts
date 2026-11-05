import type { HandleView } from 'witgraph-loader';

/** A tensor as the view reads one (`test:data/tensor`, in JS). */
interface TensorLike {
  shape(): ArrayLike<number>;
  read(offset: bigint, len: bigint): Promise<Uint8Array>;
  clone(): TensorLike;
  readonly backend?: string;
}

function isTensor(value: unknown): value is TensorLike {
  const t = value as Partial<TensorLike> | null;
  return typeof t?.shape === 'function' && typeof t.read === 'function';
}

/** What the `tensor` viewer draws, as it crosses from the worker. */
export interface TensorImage {
  /** `[height, width, channels]`. */
  shape: number[];
  /** Absent for what is not an image: the viewer leaves it. */
  bytes?: Uint8Array;
  /** Where the tensor lived. */
  backend?: string;
}

/**
 * Reads a `[height, width, channels]` `uint8` tensor (1, 3 or 4 channels) back from
 * wherever it lives, in the worker, for the `tensor` viewer to draw on the page.
 */
export const tensorView: HandleView<TensorLike | undefined> = {
  keep: (handle) => (isTensor(handle) ? handle.clone() : undefined),
  async read(tensor): Promise<TensorImage | undefined> {
    if (!tensor) return undefined;
    const shape = Array.from(tensor.shape());
    const { backend } = tensor;
    if (shape.length !== 3 || ![1, 3, 4].includes(shape[2])) return { shape, backend };
    const bytes = await tensor.read(0n, BigInt(shape[0] * shape[1] * shape[2]));
    return { shape, bytes, backend };
  },
};
