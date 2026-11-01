import type { PortViewer } from './types.ts';

/** An image as the `image` viewer accepts one. */
export type ImageValue =
  /** Raw pixels: `width` x `height`, row by row, RGBA, a byte each. */
  | { kind: 'rgba'; width: number; height: number; pixels: Uint8ClampedArray }
  /** An encoded image, of the MIME type its leading bytes give. */
  | { kind: 'encoded'; type: string; bytes: Uint8Array };

/** Leading bytes of each encoded format the viewer decodes. */
const SIGNATURES: [string, (number | null)[]][] = [
  ['image/png', [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]],
  ['image/jpeg', [0xff, 0xd8, 0xff]],
  ['image/gif', [0x47, 0x49, 0x46, 0x38]],
  // `RIFF`, the size, then `WEBP`.
  ['image/webp', [0x52, 0x49, 0x46, 0x46, null, null, null, null, 0x57, 0x45, 0x42, 0x50]],
];

/** The MIME type of the encoded image `bytes` hold, or `undefined`. */
export function sniffImageType(bytes: Uint8Array): string | undefined {
  return SIGNATURES.find(
    ([, signature]) =>
      bytes.length >= signature.length && signature.every((b, i) => b === null || b === bytes[i]),
  )?.[0];
}

/**
 * `value` as an image: a record `{ width, height, pixels }` of RGBA pixels
 * (`pixels` a `list<u8>`), or the bytes of a PNG, JPEG, GIF or WebP image.
 * `undefined` for anything else.
 */
export function asImage(value: unknown): ImageValue | undefined {
  if (value instanceof Uint8Array) {
    const type = sniffImageType(value);
    return type ? { kind: 'encoded', type, bytes: value } : undefined;
  }
  if (typeof value !== 'object' || value === null) return undefined;
  const { width, height, pixels } = value as Record<string, unknown>;
  if (typeof width !== 'number' || typeof height !== 'number') return undefined;
  if (!(width > 0 && height > 0)) return undefined;
  const bytes =
    pixels instanceof Uint8Array || pixels instanceof Uint8ClampedArray
      ? new Uint8ClampedArray(pixels.buffer, pixels.byteOffset, pixels.byteLength)
      : Array.isArray(pixels)
        ? Uint8ClampedArray.from(pixels as number[])
        : undefined;
  if (!bytes || bytes.length !== width * height * 4) return undefined;
  return { kind: 'rgba', width, height, pixels: bytes };
}

/**
 * Draws the latest image on a canvas scaled to fit the preview region.
 * Each drawn image counts in the canvas's `data-frames` attribute.
 */
export const imageViewer: PortViewer = {
  id: 'image',
  mount(host) {
    const canvas = document.createElement('canvas');
    canvas.className = 'preview-image';
    canvas.width = 0;
    canvas.height = 0;
    host.appendChild(canvas);
    const context = canvas.getContext('2d');
    let last: unknown;
    let frames = 0;
    // An encoded image decodes asynchronously: only the newest is drawn.
    let decoding = 0;
    const drawn = () => {
      frames += 1;
      canvas.dataset.frames = String(frames);
    };
    const fit = (width: number, height: number) => {
      if (canvas.width !== width) canvas.width = width;
      if (canvas.height !== height) canvas.height = height;
    };
    return {
      update({ latest: value }) {
        if (value === last) return;
        last = value;
        const image = asImage(value);
        if (!image || !context) return;
        if (image.kind === 'rgba') {
          fit(image.width, image.height);
          // `ImageData` wants a buffer it may own: give it a copy.
          const pixels = new Uint8ClampedArray(image.pixels);
          context.putImageData(new ImageData(pixels, image.width, image.height), 0, 0);
          drawn();
          return;
        }
        const ticket = ++decoding;
        const blob = new Blob([new Uint8Array(image.bytes)], { type: image.type });
        createImageBitmap(blob).then(
          (bitmap) => {
            if (ticket === decoding) {
              fit(bitmap.width, bitmap.height);
              context.drawImage(bitmap, 0, 0);
              drawn();
            }
            bitmap.close();
          },
          () => {},
        );
      },
      dispose() {
        decoding += 1;
        canvas.remove();
      },
    };
  },
};
