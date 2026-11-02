import type { PortViewer } from 'witgraph-editor';

/** A tensor as the `tensor` viewer reads one (`test:data/tensor`, in JS). */
interface TensorLike {
  shape(): ArrayLike<number>;
  read(offset: bigint, len: bigint): Promise<Uint8Array>;
  readonly backend?: string;
}

function isTensor(value: unknown): value is TensorLike {
  const t = value as Partial<TensorLike> | null;
  return typeof t?.shape === 'function' && typeof t.read === 'function';
}

/**
 * Draws a `[height, width, channels]` `uint8` tensor (1, 3 or 4 channels)
 * as an image, reading it back from wherever it lives. Each drawn image
 * counts in the canvas's `data-frames`, and `data-backend` names where the
 * tensor lived.
 */
export const tensorViewer: PortViewer = {
  id: 'tensor',
  mount(host) {
    const canvas = document.createElement('canvas');
    canvas.className = 'preview-image';
    canvas.width = 0;
    canvas.height = 0;
    host.appendChild(canvas);
    const context = canvas.getContext('2d');
    let last: unknown;
    let frames = 0;
    // A read is asynchronous: only the newest is drawn.
    let reading = 0;
    return {
      update({ latest }) {
        if (latest === last || !isTensor(latest) || !context) return;
        last = latest;
        const [height, width, channels] = Array.from(latest.shape());
        if (![1, 3, 4].includes(channels)) return;
        const ticket = ++reading;
        const backend = latest.backend;
        latest.read(0n, BigInt(height * width * channels)).then(
          (bytes) => {
            if (ticket !== reading) return;
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
            context.putImageData(new ImageData(rgba, width, height), 0, 0);
            frames += 1;
            canvas.dataset.frames = String(frames);
            if (backend) canvas.dataset.backend = backend;
          },
          // Replaced, and disposed, before it was read back: the next one shows.
          () => {},
        );
      },
      dispose() {
        reading += 1;
        canvas.remove();
      },
    };
  },
};
