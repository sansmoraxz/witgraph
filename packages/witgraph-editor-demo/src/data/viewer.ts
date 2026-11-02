import type { PortViewer } from 'witgraph-editor';

/** A tensor as the `tensor` viewer reads one (`test:data/tensor`, in JS). */
interface TensorLike {
  shape(): ArrayLike<number>;
  read(offset: bigint, len: bigint): Promise<Uint8Array>;
  clone(): TensorLike;
  readonly backend?: string;
  [Symbol.dispose]?(): void;
}

function isTensor(value: unknown): value is TensorLike {
  const t = value as Partial<TensorLike> | null;
  return typeof t?.shape === 'function' && typeof t.read === 'function';
}

/**
 * Draws a `[height, width, channels]` `uint8` tensor (1, 3 or 4 channels)
 * as an image, reading it back from wherever it lives. Each drawn image
 * counts in the canvas's `data-frames`, and `data-backend` names where the
 * tensor lived. One read runs at a time: what arrives meanwhile waits, the
 * newest only, so a stream of tensors is sampled as fast as they read back,
 * and the last is always drawn.
 */
export const tensorViewer: PortViewer = {
  id: 'tensor',
  keep: (item) => (isTensor(item) ? item.clone() : undefined),
  mount(host) {
    const canvas = document.createElement('canvas');
    canvas.className = 'preview-image';
    canvas.width = 0;
    canvas.height = 0;
    host.appendChild(canvas);
    const context = canvas.getContext('2d');
    let last: unknown;
    let frames = 0;
    let reading = false;
    let waiting: TensorLike | undefined;
    let mounted = true;

    const draw = (tensor: TensorLike, bytes: Uint8Array) => {
      const [height, width, channels] = Array.from(tensor.shape());
      const rgba = new Uint8ClampedArray(width * height * 4);
      for (let i = 0; i < width * height; i++) {
        const at = i * channels;
        const [r, g, b] =
          channels === 1
            ? [bytes[at], bytes[at], bytes[at]]
            : [bytes[at], bytes[at + 1], bytes[at + 2]];
        rgba.set([r, g, b, channels === 4 ? bytes[at + 3] : 255], i * 4);
      }
      if (canvas.width !== width) canvas.width = width;
      if (canvas.height !== height) canvas.height = height;
      context?.putImageData(new ImageData(rgba, width, height), 0, 0);
      frames += 1;
      canvas.dataset.frames = String(frames);
      if (tensor.backend) canvas.dataset.backend = tensor.backend;
    };

    /** Reads and draws `tensor`, a reference of our own, then whatever waits. */
    const read = (tensor: TensorLike) => {
      reading = true;
      const [height, width, channels] = Array.from(tensor.shape());
      tensor
        .read(0n, BigInt(height * width * channels))
        .then(
          (bytes) => {
            if (mounted) draw(tensor, bytes);
          },
          // Backends may fail a read: the next one shows.
          () => {},
        )
        .finally(() => {
          tensor[Symbol.dispose]?.();
          reading = false;
          const next = waiting;
          waiting = undefined;
          if (next && mounted) read(next);
          else next?.[Symbol.dispose]?.();
        });
    };

    return {
      update({ latest }) {
        if (latest === last || !isTensor(latest) || !context) return;
        last = latest;
        if (![1, 3, 4].includes(Array.from(latest.shape())[2])) return;
        // What it was given may be disposed once the next one replaces it.
        const mine = latest.clone();
        if (reading) {
          waiting?.[Symbol.dispose]?.();
          waiting = mine;
        } else {
          read(mine);
        }
      },
      dispose() {
        mounted = false;
        waiting?.[Symbol.dispose]?.();
        waiting = undefined;
        canvas.remove();
      },
    };
  },
};
