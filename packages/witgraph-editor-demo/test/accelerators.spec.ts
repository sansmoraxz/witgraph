import { expect, type Page, test } from '@playwright/test';
import type { WitgraphCanvas } from 'witgraph-editor';

// The edge detection graphs run a small CNN on the browser's accelerator.
// The `chromium` project has neither WebGPU nor WebNN; the
// `chromium-accelerated` project has both.
const accelerated = () => test.info().project.name === 'chromium-accelerated';

const ACCELERATORS = [
  { graph: 'webgpu', name: 'WebGPU' },
  { graph: 'webnn', name: 'WebNN' },
];

async function open(page: Page, graph: string): Promise<void> {
  await page.goto(`/?graph=${graph}`);
  await page.waitForFunction(
    () => /ready|error/.test(document.getElementById('status')?.className ?? ''),
    { timeout: 30_000 },
  );
  // Loads either way: what the browser lacks shows only when the graph runs.
  await expect(page.locator('#status')).toHaveClass(/ready/);
}

/** The edge node's preview: frames drawn, and the share of bright pixels. */
function edges(page: Page) {
  return page.locator('witgraph-canvas').evaluate((el: HTMLElement) => {
    const canvas = el.shadowRoot?.querySelector<HTMLCanvasElement>(
      '[data-node-id="edges"] .node-preview canvas',
    );
    const frames = Number(canvas?.dataset.frames ?? 0);
    if (!canvas || frames === 0) return { frames, bright: 0 };
    const { data } = canvas.getContext('2d')!.getImageData(0, 0, canvas.width, canvas.height);
    let bright = 0;
    for (let i = 0; i < data.length; i += 4) if (data[i] > 128) bright++;
    return { frames, bright: bright / (data.length / 4) };
  });
}

for (const { graph, name } of ACCELERATORS) {
  test.describe(`edge detection on ${name}`, () => {
    test('runs on the accelerator, or faults saying it is missing', async ({ page }) => {
      await open(page, graph);
      const canvas = page.locator('witgraph-canvas');
      await expect(canvas.locator('[data-node-id="edges"] .node-label')).toHaveText(
        `Edges (${name})`,
      );
      await page.locator('witgraph-controls').locator('button.run').click();

      if (accelerated()) {
        await expect
          .poll(async () => (await edges(page)).frames, { timeout: 15_000 })
          .toBeGreaterThan(3);
        // Edges are lines: some of the frame, far from all of it.
        const { bright } = await edges(page);
        expect(bright).toBeGreaterThan(0.01);
        expect(bright).toBeLessThan(0.5);
        const backend = await canvas.evaluate((el: WitgraphCanvas) =>
          el.controller?.latestOutput('edges', 'backend'),
        );
        expect(backend).toBe(graph);
        await page.locator('witgraph-controls').locator('button.stop').click();
      } else {
        await expect(canvas.locator('[data-node-id="edges"] .node-header')).toHaveClass(/faulted/, {
          timeout: 15_000,
        });
        await canvas.locator('[data-node-id="edges"]').click();
        await expect(page.locator('witgraph-inspector')).toContainText(
          `${name} is not available in this browser`,
        );
        expect((await edges(page)).frames).toBe(0);
      }
    });

    test('computes what the CPU reference computes', async ({ page }) => {
      test.skip(!accelerated(), `needs ${name}`);
      await open(page, graph);
      // In the page, through Vite: the demo's own providers.
      const difference = await page.evaluate(async (graph) => {
        const { cpu } = await import('/src/accel/cnn.ts');
        const provider = await import(`/src/accel/${graph}.ts`);
        const accelerator = await provider[graph]();
        const reference = cpu();
        const random = (n: number) => Float32Array.from({ length: n }, () => Math.random() * 2 - 1);
        const layers = [
          {
            tag: 'conv',
            val: { outChannels: 3, size: 3, weights: random(3 * 2 * 9), bias: random(3) },
          },
          { tag: 'relu' },
          {
            tag: 'conv',
            val: { outChannels: 2, size: 1, weights: random(2 * 3), bias: random(2) },
          },
          { tag: 'clamp', val: [-0.5, 0.5] },
        ];
        const input = { channels: 2, height: 13, width: 17, data: random(2 * 13 * 17) };
        const [want, got] = await Promise.all([
          reference.run(layers, input),
          accelerator.run(layers, input),
        ]);
        if (got.channels !== want.channels || got.data.length !== want.data.length) return Infinity;
        return Math.max(...want.data.map((v: number, i: number) => Math.abs(v - got.data[i])));
      }, graph);
      expect(difference).toBeLessThan(1e-3);
    });
  });
}
