import { expect, type Page, test } from '@playwright/test';
import type { WitgraphCanvas, WitgraphTimeline } from 'witgraph-editor';

test.describe('WIT Graph Editor Demo', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await page.waitForFunction(
      () => {
        const el = document.getElementById('status');
        return el?.classList.contains('ready') || el?.classList.contains('error');
      },
      { timeout: 30_000 },
    );
  });

  test('demo loads and graph compiles', async ({ page }) => {
    const status = page.locator('#status');
    await expect(status).toHaveClass(/ready/);
    await expect(status).toContainText('Graph compiles');
  });

  test('all web components are rendered', async ({ page }) => {
    await expect(page.locator('witgraph-controls')).toBeAttached();
    await expect(page.locator('witgraph-canvas')).toBeAttached();
    await expect(page.locator('witgraph-inspector')).toBeAttached();
    await expect(page.locator('witgraph-timeline')).toBeAttached();
    await expect(page.locator('witgraph-catalog')).toBeAttached();
  });

  test('canvas renders 16 nodes and 10 edges', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await expect(canvas.locator('svg .node-group')).toHaveCount(16);
    await expect(canvas.locator('svg path.edge')).toHaveCount(10);
  });

  test('relay-a is pre-selected in inspector', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    await expect(inspector.locator('.node-id')).toHaveValue('relay-a');
  });

  test('clicking a node selects it', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await canvas.locator('[data-node-id="relay-b"]').click({ timeout: 10_000 });
    await expect(page.locator('witgraph-inspector').locator('.node-id')).toHaveValue('relay-b');
  });

  test('controls toolbar buttons exist', async ({ page }) => {
    const controls = page.locator('witgraph-controls');
    await expect(controls.locator('button.run')).toBeAttached();
    await expect(controls.locator('button.step')).toBeAttached();
    await expect(controls.locator('button.stop')).toBeAttached();
    await expect(controls.locator('.badge')).toBeAttached();
  });

  test('connected port shows source in dropdown', async ({ page }) => {
    await page.locator('witgraph-canvas').locator('[data-node-id="relay-b"]').click();
    const selected = await page.locator('witgraph-inspector').evaluate((el: HTMLElement) => {
      const select = el.shadowRoot?.querySelector('select') as HTMLSelectElement | null;
      return select?.value ?? '';
    });
    expect(selected).toContain('relay-a');
  });

  test('auto-layout produces distinct positions', async ({ page }) => {
    const groups = page.locator('witgraph-canvas').locator('.node-group');
    const count = await groups.count();
    expect(count).toBe(16);
    const transforms = new Set<string>();
    for (let i = 0; i < count; i++) {
      transforms.add((await groups.nth(i).getAttribute('transform')) ?? '');
    }
    expect(transforms.size).toBe(16);
  });

  test('catalog shows components', async ({ page }) => {
    const text = await page
      .locator('witgraph-catalog')
      .evaluate((el: HTMLElement) => el.shadowRoot?.textContent ?? '');
    expect(text).toContain('Components');
    for (const component of ['video-source', 'frame-grayscale', 'frame-invert', 'video-sink']) {
      expect(text).toContain(component);
    }
  });

  test('value, stream and future outputs preview inside their nodes', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    const preview = (node: string) => canvas.locator(`[data-node-id="${node}"] .node-preview`);
    // Four relays, configurable and the producer plot; the future writer
    // and the MQTT node print; the camera and both filters show frames.
    await expect(canvas.locator('.node-preview')).toHaveCount(11);
    await expect(preview('writer')).toContainText('-');

    await page.locator('witgraph-controls').locator('button.step').click();
    const timeout = 20_000;
    // A future: its value once it resolves.
    await expect(preview('writer')).toHaveText('7', { timeout });
    // A stream of numbers: a point per item.
    await expect(preview('producer').locator('svg')).toHaveAttribute('data-points', '5', {
      timeout,
    });
    await expect(preview('producer')).toContainText('5 items');
    // A stream of bytes: its items, as text.
    await expect(preview('mqtt')).toContainText('"HELLO"', { timeout });
    await expect(preview('mqtt')).toContainText('3 items');
    // Values: a point a tick.
    await expect(preview('configurable')).toContainText('7.5', { timeout });
    await expect(preview('relay-a').locator('svg')).toHaveAttribute('data-points', '1', {
      timeout,
    });
    // Frames.
    await expect
      .poll(
        () =>
          preview('grayscale')
            .locator('canvas')
            .evaluate((c: HTMLCanvasElement) => Number(c.dataset.frames ?? 0)),
        { timeout },
      )
      .toBeGreaterThan(0);
  });

  test('a run goes on until it is stopped', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    const controls = page.locator('witgraph-controls');
    const points = () =>
      canvas
        .locator('[data-node-id="relay-a"] .node-preview svg')
        .evaluate((svg: SVGElement) => Number(svg.dataset.points ?? 0));
    const frames = () =>
      canvas
        .locator('[data-node-id="grayscale"] .node-preview canvas')
        .evaluate((c: HTMLCanvasElement) => Number(c.dataset.frames ?? 0));

    await controls.locator('button.run').click();
    await page.waitForTimeout(3_000);
    // The feedback loop and the endless camera both keep it going.
    await expect(controls.locator('.badge')).toContainText(/running/i);
    const [ticked, drawn] = [await points(), await frames()];
    expect(ticked).toBeGreaterThan(10);
    expect(drawn).toBeGreaterThan(10);
    await page.waitForTimeout(1_000);
    expect(await points()).toBeGreaterThan(ticked);
    expect(await frames()).toBeGreaterThan(drawn);

    await controls.locator('button.stop').click();
    await expect(controls.locator('.badge')).not.toContainText(/running/i);
    await page.waitForTimeout(300);
    const stopped = await frames();
    await page.waitForTimeout(500);
    expect(await frames()).toBe(stopped);
  });

  test('the side panes are not redrawn while what they show is unchanged', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    const controls = page.locator('witgraph-controls');
    await page
      .locator('witgraph-canvas')
      .evaluate((el: WitgraphCanvas) => el.controller?.select(null));
    await expect(inspector).toContainText('Graph Properties');
    const catalog = page.locator('witgraph-catalog');
    // Runs in the page: tags the pane's content, or tells whether it is tagged.
    const panel = (el: HTMLElement, tag: boolean) => {
      const root = el.shadowRoot?.querySelector('.inspector-root, .catalog-wrap') as Element & {
        marked?: boolean;
      };
      if (tag) root.marked = true;
      return root.marked === true;
    };
    await inspector.evaluate(panel, true);
    await catalog.evaluate(panel, true);
    await controls.locator('button.run').click();
    await page.waitForTimeout(2_000);
    expect(await inspector.evaluate(panel, false), 'the same inspector').toBe(true);
    expect(await catalog.evaluate(panel, false), 'the same catalog').toBe(true);

    // A field being edited is left alone while the graph runs.
    await page.locator('witgraph-canvas').locator('[data-node-id="relay-a"]').click();
    const label = inspector.locator('input[placeholder="Node label..."]');
    await label.click();
    await label.press('End');
    await label.pressSequentially(' edited');
    await page.waitForTimeout(1_000);
    await expect(label).toBeFocused();
    await expect(label).toHaveValue('Relay A edited');
    await controls.locator('button.stop').click();
  });

  test('the tick rate slider paces a run', async ({ page }) => {
    const controls = page.locator('witgraph-controls');
    await expect(controls.locator('.rate-value')).toHaveText('Max');
    await controls.locator('.rate-slider').fill('0');
    await expect(controls.locator('.rate-value')).toHaveText('1/s');

    await controls.locator('button.run').click();
    await page.waitForTimeout(2_500);
    const points = await page
      .locator('witgraph-canvas')
      .locator('[data-node-id="relay-a"] .node-preview svg')
      .evaluate((svg: SVGElement) => Number(svg.dataset.points ?? 0));
    // A tick a second: two or three of them, where Max makes dozens.
    expect(points).toBeGreaterThanOrEqual(1);
    expect(points).toBeLessThanOrEqual(4);
    await controls.locator('button.stop').click();
  });

  test('the canvas draws again when it is moved in the page', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await expect(canvas.locator('svg .node-group')).toHaveCount(16);
    await page.evaluate(() => {
      const el = document.querySelector('witgraph-canvas')!;
      const parent = el.parentElement!;
      el.remove();
      parent.appendChild(el);
    });
    await expect(canvas.locator('svg .node-group')).toHaveCount(16);
    // And it follows the controller still.
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.setNodeLabel('relay-b', 'Moved'));
    await expect(canvas.locator('[data-node-id="relay-b"] .node-label')).toHaveText('Moved');
  });

  test('the timeline names the nodes of a generation', async ({ page }) => {
    await page.locator('witgraph-controls').locator('button.step').click();
    const timeline = page.locator('witgraph-timeline');
    const started = timeline.locator('.event', { hasText: 'generation-started' }).first();
    await expect(started).toBeVisible({ timeout: 10_000 });
    // Its island's nodes, and which generation.
    await expect(started.locator('.underline').first()).not.toBeEmpty();
    await expect(started).toContainText(/\(generation \d+\)/);
    // The video island's four nodes, together.
    await expect(
      timeline.locator('.event', { hasText: 'camera, grayscale, invert, video-sink' }).first(),
    ).toBeVisible();
  });

  test('the timeline keeps up once the trace is full', async ({ page }) => {
    test.setTimeout(60_000);
    const timeline = page.locator('witgraph-timeline');
    const traceLength = () =>
      timeline.evaluate((el: WitgraphTimeline) => el.controller?.traceEvents.length);
    /** The trace's newest entry, and the row the timeline shows last. */
    const newest = () =>
      timeline.evaluate((el: WitgraphTimeline) => ({
        entry: el.controller?.traceEvents.at(-1)?.timestamp,
        row: el.shadowRoot?.querySelector('.event:last-child')?.firstChild?.textContent,
        rows: el.shadowRoot?.querySelectorAll('.event').length,
      }));

    const controls = page.locator('witgraph-controls');
    await controls.locator('button.run').click();
    // The trace holds 1000 entries, and from there on only its contents change.
    await expect.poll(traceLength, { timeout: 40_000 }).toBe(1000);

    const full = (await newest()).entry ?? 0;
    await expect.poll(async () => (await newest()).entry).toBeGreaterThan(full);
    const { entry, row, rows } = await newest();
    expect(row).toBe(`${((entry ?? 0) / 1000).toFixed(2)}s `);
    // One row an entry: the oldest go when their entries do.
    expect(rows).toBe(1000);

    await controls.locator('button.stop').click();
    // A trace started over takes its rows with it.
    await timeline.evaluate(async (el: WitgraphTimeline) => {
      const controller = el.controller;
      await controller?.importGraph(controller.exportGraph());
    });
    await expect.poll(async () => (await newest()).rows).toBeLessThan(1000);
  });

  test('the timeline follows the trace still after it is moved in the page', async ({ page }) => {
    const timeline = page.locator('witgraph-timeline');
    const rows = () =>
      timeline.evaluate((el: HTMLElement) => el.shadowRoot?.querySelectorAll('.event').length ?? 0);
    const controls = page.locator('witgraph-controls');
    await controls.locator('button.run').click();
    await expect.poll(rows).toBeGreaterThan(0);

    await page.evaluate(() => {
      const el = document.querySelector('witgraph-timeline')!;
      const parent = el.parentElement!;
      el.remove();
      parent.appendChild(el);
    });
    const moved = await rows();
    await expect.poll(rows).toBeGreaterThan(moved);
    await controls.locator('button.stop').click();
  });

  test('injected values from attrs are visible', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    const text = await inspector.evaluate((el: HTMLElement) => el.shadowRoot?.textContent ?? '');
    expect(text).toContain('Injected');
  });
});

test.describe('Video pipeline demo', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/?graph=video');
    await page.waitForFunction(
      () => {
        const el = document.getElementById('status');
        return el?.classList.contains('ready') || el?.classList.contains('error');
      },
      { timeout: 30_000 },
    );
  });

  /** The latest frame a node previews: how many it drew, and one pixel of it. */
  const sample = (page: Page, node: string) =>
    page.locator('witgraph-canvas').evaluate((el: HTMLElement, node: string) => {
      const canvas = el.shadowRoot?.querySelector<HTMLCanvasElement>(
        `[data-node-id="${node}"] .node-preview canvas`,
      );
      const frames = Number(canvas?.dataset.frames ?? 0);
      if (!canvas || frames === 0) return { frames, rgb: [] as number[] };
      // Row 40, column 50 of the 96x64 frame.
      const [r, g, b] = canvas.getContext('2d')!.getImageData(50, 40, 1, 1).data;
      return { frames, rgb: [r, g, b] };
    }, node);

  test('every filter previews the frames it streams', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await expect(page.locator('#status')).toHaveClass(/ready/);
    await expect(canvas.locator('svg .node-group')).toHaveCount(7);
    // Both cameras and all three filters; not the sinks.
    await expect(canvas.locator('.node-preview')).toHaveCount(5);

    await page.locator('witgraph-controls').locator('button.run').click();
    for (const node of ['camera', 'gray', 'gray-invert', 'camera-b', 'invert']) {
      await expect
        .poll(async () => (await sample(page, node)).frames, { timeout: 15_000, message: node })
        .toBeGreaterThan(2);
    }

    const isGray = ([r, g, b]: number[]) => r === g && g === b;
    expect(isGray((await sample(page, 'gray')).rgb)).toBe(true);
    expect(isGray((await sample(page, 'gray-invert')).rgb)).toBe(true);
    expect(isGray((await sample(page, 'camera')).rgb)).toBe(false);
    expect(isGray((await sample(page, 'invert')).rgb)).toBe(false);

    // Stopping stops the streams: no more frames are drawn.
    await page.locator('witgraph-controls').locator('button.stop').click();
    await page.waitForTimeout(300);
    const stopped = (await sample(page, 'gray')).frames;
    await page.waitForTimeout(500);
    expect((await sample(page, 'gray')).frames).toBe(stopped);
  });

  test('a node keeps the size it is resized to', async ({ page }) => {
    const node = page.locator('witgraph-canvas').locator('[data-node-id="gray"]');
    const before = await node.locator('.node-body').boundingBox();
    const handle = await node.locator('.node-resize').boundingBox();
    expect(before && handle).toBeTruthy();
    await page.mouse.move(handle!.x + handle!.width / 2, handle!.y + handle!.height / 2);
    await page.mouse.down();
    await page.mouse.move(handle!.x + 80, handle!.y + 60, { steps: 5 });
    await page.mouse.up();
    const after = await node.locator('.node-body').boundingBox();
    expect(after!.width).toBeGreaterThan(before!.width + 40);
    expect(after!.height).toBeGreaterThan(before!.height + 30);
    // The preview region grew with it.
    const preview = await node.locator('.node-preview').boundingBox();
    expect(preview!.width).toBeGreaterThan(before!.width + 40);
  });
});
