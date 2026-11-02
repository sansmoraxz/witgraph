import { expect, type Page, test } from '@playwright/test';

// A stream of tensors from a camera (Chromium's fake one), kept where a backend
// keeps them. The CPU graph runs everywhere; the WebGPU and WebNN ones need the
// `chromium-accelerated` project's flags.
const accelerated = () => test.info().project.name === 'chromium-accelerated';

// Live frames through three nodes, on pages other tests compete with.
test.describe.configure({ timeout: 90_000 });

const BACKENDS = [
  { graph: 'camera', name: 'CPU', backend: 'cpu', everywhere: true },
  { graph: 'camera-webgpu', name: 'WebGPU', backend: 'webgpu', everywhere: false },
  { graph: 'camera-webnn', name: 'WebNN', backend: 'webnn', everywhere: false },
];

async function open(page: Page, graph: string): Promise<void> {
  await page.goto(`/?graph=${graph}`);
  await page.waitForFunction(
    () => /ready|error/.test(document.getElementById('status')?.className ?? ''),
    { timeout: 30_000 },
  );
  await expect(page.locator('#status')).toHaveClass(/ready/);
}

/** A node's preview: images drawn, the backend, and its pixels. */
function preview(page: Page, node: string) {
  return page.locator('witgraph-canvas').evaluate((el: HTMLElement, node) => {
    const canvas = el.shadowRoot?.querySelector<HTMLCanvasElement>(
      `[data-node-id="${node}"] .node-preview canvas`,
    );
    const frames = Number(canvas?.dataset.frames ?? 0);
    if (!canvas || frames === 0) return { frames, backend: undefined, pixels: [] as number[] };
    const image = canvas.getContext('2d')?.getImageData(0, 0, canvas.width, canvas.height);
    return { frames, backend: canvas.dataset.backend, pixels: Array.from(image?.data ?? []) };
  }, node);
}

const run = (page: Page) => page.locator('witgraph-controls').locator('button.run').click();

for (const { graph, name, backend, everywhere } of BACKENDS) {
  test(`the camera streams through nodes on ${name}`, async ({ page }) => {
    test.skip(accelerated() && everywhere, 'runs in the plain project');
    await open(page, graph);
    await run(page);
    const canvas = page.locator('witgraph-canvas');

    if (!everywhere && !accelerated()) {
      await expect(canvas.locator('[data-node-id="camera"] .node-header')).toHaveClass(/faulted/, {
        timeout: 15_000,
      });
      await canvas.locator('[data-node-id="camera"]').click();
      await expect(page.locator('witgraph-inspector')).toContainText(
        `${name} is not available in this browser`,
      );
      return;
    }

    // Live: previews keep drawing new frames at every node.
    for (const node of ['camera', 'edges', 'invert']) {
      await expect
        .poll(async () => (await preview(page, node)).frames, { timeout: 20_000 })
        .toBeGreaterThan(3);
    }
    const [frame, edges, inverted] = await Promise.all(
      ['camera', 'edges', 'invert'].map((node) => preview(page, node)),
    );
    expect(edges.backend).toBe(backend);
    expect(frame.pixels.length).toBeGreaterThan(0);
    expect(new Set(frame.pixels).size, 'the frame is an image, not one color').toBeGreaterThan(8);
    expect(edges.pixels).not.toEqual(frame.pixels);
    expect(inverted.pixels).not.toEqual(edges.pixels);
    const nodes = page.locator('witgraph-canvas');
    await expect(nodes.locator('.node-header.faulted')).toHaveCount(0);
    await page.locator('witgraph-controls').locator('button.stop').click();
    const stopped = (await preview(page, 'camera')).frames;
    await page.waitForTimeout(500);
    const later = (await preview(page, 'camera')).frames;
    expect(later - stopped, 'stopped, or as good as').toBeLessThan(3);
  });
}

test('without a camera, its node faults saying why', async ({ page }) => {
  test.skip(accelerated(), 'runs in the plain project');
  await page.addInitScript(() => {
    navigator.mediaDevices.getUserMedia = () =>
      Promise.reject(new Error('Requested device not found'));
  });
  await open(page, 'camera');
  await run(page);
  const canvas = page.locator('witgraph-canvas');
  await expect(canvas.locator('[data-node-id="camera"] .node-header')).toHaveClass(/faulted/, {
    timeout: 15_000,
  });
  await canvas.locator('[data-node-id="camera"]').click();
  await expect(page.locator('witgraph-inspector')).toContainText(
    'no camera: Requested device not found',
  );
});
