import type { PortViewer } from 'witgraph-editor';
import type { TensorImage } from './tensor-view.ts';

/**
 * Draws what `tensorView` read in the worker as an image. Each drawn image counts in the
 * canvas's `data-frames`, and `data-backend` names where the tensor lived.
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

    const draw = ({ shape: [height, width, channels], bytes, backend }: TensorImage) => {
      if (!bytes) return;
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
      if (backend) canvas.dataset.backend = backend;
    };

    return {
      update({ latest }) {
        if (latest === last || !latest || typeof latest !== 'object' || !context) return;
        last = latest;
        draw(latest as TensorImage);
      },
      dispose() {
        canvas.remove();
      },
    };
  },
};
