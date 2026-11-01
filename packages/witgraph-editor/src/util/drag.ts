export interface DragCallbacks {
  onStart?: (e: PointerEvent) => void;
  onMove: (dx: number, dy: number, e: PointerEvent) => void;
  onEnd?: (e: PointerEvent) => void;
}

export function makeDraggable(
  element: Element,
  callbacks: DragCallbacks,
  ignoreSelector?: string,
): () => void {
  let startX = 0;
  let startY = 0;
  let dragging = false;

  const onPointerDown = (ev: Event) => {
    const e = ev as PointerEvent;
    if (e.button !== 0) return;
    if (ignoreSelector && (e.target as Element).closest(ignoreSelector)) return;
    startX = e.clientX;
    startY = e.clientY;
    dragging = true;
    element.setPointerCapture(e.pointerId);
    callbacks.onStart?.(e);
  };

  const onPointerMove = (ev: Event) => {
    if (!dragging) return;
    const e = ev as PointerEvent;
    callbacks.onMove(e.clientX - startX, e.clientY - startY, e);
  };

  const onPointerUp = (ev: Event) => {
    if (!dragging) return;
    dragging = false;
    const e = ev as PointerEvent;
    try { element.releasePointerCapture(e.pointerId); } catch {}
    callbacks.onEnd?.(e);
  };

  const onLostCapture = () => {
    if (!dragging) return;
    dragging = false;
    callbacks.onEnd?.(new PointerEvent('pointerup'));
  };

  element.addEventListener('pointerdown', onPointerDown);
  element.addEventListener('pointermove', onPointerMove);
  element.addEventListener('pointerup', onPointerUp);
  element.addEventListener('lostpointercapture', onLostCapture);

  return () => {
    dragging = false;
    element.removeEventListener('pointerdown', onPointerDown);
    element.removeEventListener('pointermove', onPointerMove);
    element.removeEventListener('pointerup', onPointerUp);
    element.removeEventListener('lostpointercapture', onLostCapture);
  };
}
