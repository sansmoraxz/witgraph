import { failure } from '../accel/cnn.ts';
import type { Backend, TensorClass } from './tensors.ts';

/** How long after its last frame the camera stays on. */
const IDLE_MS = 2000;
/** A hidden page gets no video frame callbacks: go on at this pace instead. */
const HIDDEN_FRAME_MS = 100;

/** A frame's RGB through the page's memory, for a backend that needs that. */
function pixels(video: HTMLVideoElement, [height, width]: number[]): Uint8Array {
  const canvas = new OffscreenCanvas(width, height);
  const context = canvas.getContext('2d', { willReadFrequently: true });
  if (!context) failure('no 2D canvas to read the camera through');
  context.drawImage(video, 0, 0);
  const { data } = context.getImageData(0, 0, width, height);
  const rgb = new Uint8Array(width * height * 3);
  for (let i = 0; i < width * height; i++) {
    rgb.set(data.subarray(i * 4, i * 4 + 3), i * 3);
  }
  return rgb;
}

/**
 * `test:camera/camera` for jco: the page's camera, its frames as tensors on
 * `backend`. It is turned on by the first frame asked for, and off once none
 * has been for a while. Values are as jco hands them over; a `result`'s `err`
 * is thrown as `{ payload }`.
 */
export function camera(backend: Backend, Tensor: TensorClass): { nextFrame(): Promise<object> } {
  let opening: Promise<HTMLVideoElement> | undefined;
  let idle: ReturnType<typeof setTimeout> | undefined;

  const open = async (): Promise<HTMLVideoElement> => {
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({
        video: { width: { ideal: 640 }, height: { ideal: 480 } },
      });
    } catch (error) {
      opening = undefined;
      return failure(`no camera: ${error instanceof Error ? error.message : String(error)}`);
    }
    const video = document.createElement('video');
    video.muted = true;
    video.playsInline = true;
    video.srcObject = stream;
    await video.play();
    return video;
  };

  const close = async () => {
    const closing = opening;
    opening = undefined;
    const video = await closing?.catch(() => undefined);
    for (const track of (video?.srcObject as MediaStream | null)?.getTracks() ?? []) track.stop();
  };

  return {
    async nextFrame() {
      backend.name();
      clearTimeout(idle);
      opening ??= open();
      const video = await opening;
      await new Promise((resolve) => {
        const hidden = setTimeout(resolve, HIDDEN_FRAME_MS);
        video.requestVideoFrameCallback(() => {
          clearTimeout(hidden);
          resolve(undefined);
        });
      });
      idle = setTimeout(close, IDLE_MS);
      const shape = [video.videoHeight, video.videoWidth, 3];
      const storage = await (backend.capture?.(video, shape) ??
        backend.upload(shape, pixels(video, shape)));
      return Tensor.fromStorage(storage, shape);
    },
  };
}
