import { failure } from '../accel/cnn.ts';
import type { Backend, TensorClass } from './tensors.ts';

/** A frame's RGB through this thread's memory, for a backend that needs that. */
function pixels(frame: VideoFrame, [height, width]: number[]): Uint8Array {
  const canvas = new OffscreenCanvas(width, height);
  const context = canvas.getContext('2d', { willReadFrequently: true });
  if (!context) failure('no 2D canvas to read the camera through');
  context.drawImage(frame, 0, 0);
  const { data } = context.getImageData(0, 0, width, height);
  const rgb = new Uint8Array(width * height * 3);
  for (let i = 0; i < width * height; i++) {
    rgb.set(data.subarray(i * 4, i * 4 + 3), i * 3);
  }
  return rgb;
}

/**
 * Frames from the page a worker belongs to, which has the camera
 * (`serveCameraFrames`, in `camera-page.ts`).
 */
export function framesFrom(page: {
  postMessage(message: unknown): void;
  addEventListener(type: 'message', listener: (event: MessageEvent) => void): void;
}): () => Promise<VideoFrame> {
  const waiting = new Map<number, { resolve(frame: VideoFrame): void; reject(e: Error): void }>();
  let next = 0;
  page.addEventListener('message', ({ data }) => {
    const asked = waiting.get(data?.cameraFrame);
    if (!asked) return;
    waiting.delete(data.cameraFrame);
    if (data.error) asked.reject(new Error(data.error));
    else asked.resolve(data.frame);
  });
  return () =>
    new Promise((resolve, reject) => {
      const cameraFrame = next++;
      waiting.set(cameraFrame, { resolve, reject });
      page.postMessage({ cameraFrame });
    });
}

/**
 * `test:camera/camera` for jco: `frames`, as tensors on `backend`. Values are as jco hands
 * them over; a `result`'s `err` is thrown as `{ payload }`.
 */
export function camera(
  backend: Backend,
  Tensor: TensorClass,
  frames: () => Promise<VideoFrame>,
): { nextFrame(): Promise<object> } {
  return {
    async nextFrame() {
      backend.name();
      const frame = await frames().catch((error: Error) => failure(error.message));
      try {
        const shape = [frame.displayHeight, frame.displayWidth, 3];
        const storage = await (backend.capture?.(frame, shape) ??
          backend.upload(shape, pixels(frame, shape)));
        return Tensor.fromStorage(storage, shape);
      } finally {
        frame.close();
      }
    },
  };
}
