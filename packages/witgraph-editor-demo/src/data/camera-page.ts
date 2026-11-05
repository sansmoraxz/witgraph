/** How long after its last frame the camera stays on. */
const IDLE_MS = 2000;
/** A hidden page gets no video frame callbacks: go on at this pace instead. */
const HIDDEN_FRAME_MS = 100;

/**
 * The page's camera, as frames: only a page can open one. It is turned on by the first
 * frame asked for, and off once none has been for a while. Each frame is the caller's to
 * close.
 */
export function cameraFrames(): () => Promise<VideoFrame> {
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
      throw new Error(`no camera: ${error instanceof Error ? error.message : String(error)}`);
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

  return async () => {
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
    return new VideoFrame(video);
  };
}

/** Answers `worker`'s requests for frames ({@link framesFrom}) from the page's camera. */
export function serveCameraFrames(worker: Worker): void {
  const next = cameraFrames();
  worker.addEventListener('message', ({ data }) => {
    if (data?.cameraFrame === undefined) return;
    next().then(
      (frame) => worker.postMessage({ cameraFrame: data.cameraFrame, frame }, [frame]),
      (error: unknown) =>
        worker.postMessage({
          cameraFrame: data.cameraFrame,
          error: error instanceof Error ? error.message : String(error),
        }),
    );
  });
}
