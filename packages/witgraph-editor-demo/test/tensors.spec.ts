import { expect, type Page, test } from '@playwright/test';

// Tensors kept where a backend keeps them: nodes pass handles to them, and
// their operations run there. The CPU graph runs everywhere; the WebGPU
// and WebNN ones need the `chromium-accelerated` project's flags.
const accelerated = () => test.info().project.name === 'chromium-accelerated';

const BACKENDS = [
  { graph: 'tensors', name: 'CPU', backend: 'cpu', everywhere: true },
  { graph: 'tensors-webgpu', name: 'WebGPU', backend: 'webgpu', everywhere: false },
  { graph: 'tensors-webnn', name: 'WebNN', backend: 'webnn', everywhere: false },
];

async function open(page: Page, graph: string): Promise<void> {
  await page.goto(`/?graph=${graph}`);
  await page.waitForFunction(
    () => /ready|error/.test(document.getElementById('status')?.className ?? ''),
    { timeout: 30_000 },
  );
  await expect(page.locator('#status')).toHaveClass(/ready/);
}

/** A node's tensor preview: images drawn, the backend, and its pixels. */
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

for (const { graph, name, backend, everywhere } of BACKENDS) {
  test.describe(`tensors on ${name}`, () => {
    test('stay where they live, and preview where nodes pass them', async ({ page }) => {
      test.skip(accelerated() && everywhere, 'runs in the plain project');
      await open(page, graph);
      const canvas = page.locator('witgraph-canvas');
      await page.locator('witgraph-controls').locator('button.run').click();

      if (everywhere || accelerated()) {
        for (const node of ['image', 'edges', 'invert']) {
          await expect
            .poll(async () => (await preview(page, node)).frames, { timeout: 15_000 })
            .toBeGreaterThan(0);
        }
        const [image, edges, inverted] = await Promise.all(
          ['image', 'edges', 'invert'].map((node) => preview(page, node)),
        );
        expect(edges.backend).toBe(backend);
        // Edges are lines: some of the image, far from all of it.
        const bright = edges.pixels.filter((v, i) => i % 4 === 0 && v > 128).length;
        expect(bright).toBeGreaterThan(0);
        expect(bright).toBeLessThan(edges.pixels.length / 4 / 2);
        expect(inverted.pixels.slice(0, 3)).toEqual(edges.pixels.slice(0, 3).map((v) => 255 - v));
        expect(image.pixels).not.toEqual(edges.pixels);
        const sum = await canvas.evaluate(
          (el: HTMLElement & { controller?: { latestOutput(n: string, p: string): unknown } }) =>
            el.controller?.latestOutput('sum', 'len'),
        );
        expect(sum).toBe(BigInt(64 * 48 * 3));
      } else {
        await expect(canvas.locator('[data-node-id="image"] .node-header')).toHaveClass(/faulted/, {
          timeout: 15_000,
        });
        await canvas.locator('[data-node-id="image"]').click();
        await expect(page.locator('witgraph-inspector')).toContainText(
          `${name} is not available in this browser`,
        );
      }
    });
  });
}
