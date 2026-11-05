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

  test('a value injected while the graph runs is written', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await page.locator('witgraph-controls').locator('button.run').click();
    await expect(page.locator('#status')).toHaveClass(/running/);
    // Each lands between ticks: the run goes on ticking all the while.
    const outcomes = await canvas.evaluate(async (el: WitgraphCanvas) => {
      const controller = el.controller;
      const results: string[] = [];
      for (const value of ['2', '3', '4']) {
        results.push(
          await controller
            ?.injectValue('relay-a', 'add', value)
            .then(() => 'ok')
            .catch((error: unknown) => String(error)),
        );
      }
      return results;
    });
    expect(outcomes).toEqual(['ok', 'ok', 'ok']);
    await expect(page.locator('#status')).toHaveClass(/running/);
    await expect(page.locator('witgraph-inspector')).toContainText(/Injected:\s*4/);
    await page.locator('witgraph-controls').locator('button.stop').click();
  });

  test('relay-a is pre-selected in inspector', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    await expect(inspector.locator('.node-id')).toHaveValue('relay-a');
  });

  test('a claim too large is said on the node, and a capacity makes room for it', async ({
    page,
  }) => {
    const inspector = page.locator('witgraph-inspector');
    const add = inspector.locator('.resource-claims .resource-add');
    await add.locator('input[type=text]').fill('gpu');
    await add.locator('input[type=number]').fill('2');
    await add.locator('button').click();
    const claim = inspector.locator('.resource-claims .resource-row');
    await expect(claim).toContainText('of 1');
    await expect(inspector).toContainText('claims more than all of resource `gpu`');

    await page
      .locator('witgraph-canvas')
      .evaluate((el: WitgraphCanvas) => el.controller?.select(null));
    const capacity = inspector.locator('.resource-capacities .resource-row');
    await expect(capacity).toContainText('2 claimed');
    await capacity.locator('input[type=number]').fill('2');
    await capacity.locator('input[type=number]').blur();
    await expect(page.locator('#status')).toHaveClass(/ready/);
    await expect(inspector).not.toContainText('claims more than all');

    // The node says so too, and undoing takes the capacity back.
    await page.locator('witgraph-canvas').evaluate((el: WitgraphCanvas) => {
      el.controller?.select('relay-a');
    });
    await expect(claim).toContainText('of 2');
    await page.locator('witgraph-controls').locator('button.undo').click();
    await expect(claim).toContainText('of 1');
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

    await page.locator('witgraph-controls').locator('button.run').click();
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
    // Values: a point a tick, of a loop that goes on.
    await expect(preview('configurable')).toContainText('7.5', { timeout });
    await expect
      .poll(
        () =>
          preview('relay-a')
            .locator('svg')
            .evaluate((svg: SVGElement) => Number(svg.dataset.points ?? 0)),
        { timeout },
      )
      .toBeGreaterThan(0);
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
    await catalog.evaluate(panel, true);
    await controls.locator('button.run').click();
    // Its fields that change the graph are disabled, once, and then it is left alone.
    await expect(inspector.locator('.inspector-root.locked')).toBeAttached();
    await inspector.evaluate(panel, true);
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

  test('the catalog describes a component by its world docs', async ({ page }) => {
    const catalog = page.locator('witgraph-catalog');
    for (const [world, docs] of [
      ['relay', 'Passes a u32 through'],
      ['stream-producer', 'Streams 0, 1, 2'],
      // None of the default graph's nodes is a camera.
      ['camera-source', "Streams the host's camera"],
    ]) {
      const card = catalog.locator('.card', { hasText: `${world}@` });
      await expect(card.locator('.docs')).toContainText(docs);
    }
  });

  test('the inspector lists snapshots, each with icon buttons to restore and delete it', async ({
    page,
  }) => {
    const save = page.locator('witgraph-controls').locator('button.snap-save');
    await save.click();
    await save.click();
    await page
      .locator('witgraph-canvas')
      .evaluate((el: WitgraphCanvas) => el.controller?.select(null));
    const inspector = page.locator('witgraph-inspector');
    await expect(inspector).toContainText('Snapshots (2)');
    // An icon each, not text.
    await expect(inspector.locator('button.icon-btn svg')).toHaveCount(4);
    await expect(inspector.getByRole('button', { name: 'Restore snap-1' })).toBeVisible();
    await inspector.getByRole('button', { name: 'Delete snap-1' }).click();
    await expect(inspector).toContainText('Snapshots (1)');
    await expect(inspector.getByRole('button', { name: 'Delete snap-1' })).toHaveCount(0);
  });

  test("the status is marked by its state in the theme's colours", async ({ page }) => {
    // The colour a theme variable has, as the page computes it.
    const themed = (variable: string) =>
      page.evaluate((variable) => {
        const probe = document.createElement('div');
        probe.style.color = `var(${variable})`;
        document.body.appendChild(probe);
        const colour = getComputedStyle(probe).color;
        probe.remove();
        return colour;
      }, variable);
    const edge = () =>
      page.locator('#status').evaluate((el) => getComputedStyle(el).borderLeftColor);
    await expect(page.locator('#status')).toHaveClass(/ready/);
    expect(await edge()).toBe(await themed('--wg-success'));
    await page.locator('#status').evaluate((el) => {
      el.classList.remove('ready');
      el.classList.add('error');
    });
    expect(await edge()).toBe(await themed('--wg-error'));
  });

  test('what changes the graph is disabled while it runs', async ({ page }) => {
    const controls = page.locator('witgraph-controls');
    const inspector = page.locator('witgraph-inspector');
    await page.locator('witgraph-canvas').locator('[data-node-id="relay-a"]').click();
    const rename = inspector.locator('input.node-id');
    await expect(rename).toBeEnabled();
    await expect(controls.locator('button.reset')).toBeEnabled();

    await controls.locator('button.run').click();
    await expect(controls.locator('button.reset')).toBeDisabled();
    await expect(controls.locator('button.load')).toBeDisabled();
    await expect(controls.locator('button.undo')).toBeDisabled();
    await expect(rename).toBeDisabled();
    // The graph itself is not touched by an attempt.
    const nodes = () =>
      page.locator('witgraph-canvas').evaluate((el: WitgraphCanvas) => el.controller?.nodes.size);
    expect(await nodes()).toBe(16);
    await page.locator('witgraph-canvas').evaluate((el: WitgraphCanvas) => {
      el.controller?.removeNode('relay-c');
    });
    expect(await nodes()).toBe(16);

    await controls.locator('button.stop').click();
    await expect(controls.locator('button.reset')).toBeEnabled();
    await expect(controls.locator('button.load')).toBeEnabled();
    await expect(rename).toBeEnabled();
  });

  test('the status follows a run', async ({ page }) => {
    const status = page.locator('#status');
    const controls = page.locator('witgraph-controls');
    await controls.locator('button.run').click();
    await expect(status).toContainText('Running');
    await expect(status).toHaveClass(/running/);
    // It counts the ticks as they are made.
    const tick = async () => Number(/tick (\d+)/.exec((await status.textContent()) ?? '')?.[1]);
    await expect.poll(tick).toBeGreaterThan(0);
    const before = await tick();
    await expect.poll(tick).toBeGreaterThan(before);

    await controls.locator('button.stop').click();
    await expect(status).toContainText('Graph compiles');
    await expect(status).toHaveClass(/ready/);
  });

  test('a graph that stops compiling marks what it is about, in the theme', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    const inspector = page.locator('witgraph-inspector');
    const themed = (variable: string) =>
      page.evaluate((variable) => {
        const probe = document.createElement('div');
        probe.style.color = `var(${variable})`;
        document.body.appendChild(probe);
        const colour = getComputedStyle(probe).color;
        probe.remove();
        return colour;
      }, variable);
    const stroke = (selector: string) =>
      canvas
        .locator(selector)
        .first()
        .evaluate((el) => getComputedStyle(el).stroke);
    const errors = canvas.locator('svg .node-group.has-error');
    await expect(errors).toHaveCount(0);

    // `relay-a` feeds `relay-b`, so this closes a cycle, which is for the next edit to mend.
    await canvas.evaluate((el: WitgraphCanvas) =>
      el.controller?.addConnection(
        'back',
        { node: 'relay-b', port: 'out' },
        { node: 'relay-a', port: 'add' },
      ),
    );
    await expect(page.locator('#status')).toHaveClass(/error/);
    await expect(errors).toHaveCount(2);
    await expect(errors.first().locator('title.diagnostics')).toContainText('cycle');
    expect(await stroke('svg .node-group.has-error .node-body')).toBe(await themed('--wg-error'));

    // The inspector says the same of the node. (A locator of two would fail.)
    await errors.first().click();
    await expect(inspector.locator('.fault-bg')).toContainText('cycle');

    await canvas.evaluate((el: WitgraphCanvas) =>
      el.controller?.setConnectionFeedback('back', true),
    );
    await expect(page.locator('#status')).toHaveClass(/ready/);
    await expect(errors).toHaveCount(0);
  });

  test('what compiling warns of is marked, and does not stop the graph', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    const inspector = page.locator('witgraph-inspector');
    const warned = canvas.locator('[data-node-id="relay-a"].has-warning');
    // `relay-a.in` is fed only by feedback, and has a value injected, as the warning asks:
    // off the canvas, and in the inspector as settled.
    await expect(warned).toHaveCount(0);
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.select('relay-a'));
    await expect(inspector.locator('.settled-bg')).toContainText('Settled');
    await expect(inspector.locator('.warning-bg')).toHaveCount(0);

    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.clearInput('relay-a', 'in'));
    await expect(warned).toHaveCount(1);
    await expect(page.locator('#status')).toHaveClass(/ready/);
    await expect(warned.locator('title.diagnostics')).toContainText('fed only by feedback');
    // Its outline has to show against its header.
    const paint = (selector: string, property: 'fill' | 'stroke') =>
      warned.locator(selector).evaluate((el, property) => getComputedStyle(el)[property], property);
    expect(await paint('.node-body', 'stroke')).not.toBe(await paint('.node-header', 'fill'));
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.select('relay-a'));
    await expect(inspector.locator('.warning-bg')).toContainText('fed only by feedback');
    await expect(inspector.locator('.fault-bg')).toHaveCount(0);

    // A connection that does nothing is marked on the connection itself.
    await canvas.evaluate((el: WitgraphCanvas) =>
      el.controller?.setConnectionFeedback('a-to-b', true),
    );
    const edge = canvas.locator('svg path.edge.has-warning').first();
    await expect(edge).toBeAttached();
    const colour = await page.evaluate(() => {
      const probe = document.createElement('div');
      probe.style.color = 'var(--wg-warning)';
      document.body.appendChild(probe);
      const themed = getComputedStyle(probe).color;
      probe.remove();
      return themed;
    });
    expect(await edge.evaluate((el) => getComputedStyle(el).stroke)).toBe(colour);
  });

  test('the inspector says why a connection cannot be feedback', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    const canvas = page.locator('witgraph-canvas');
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.selectEdge('stream'));
    await expect(inspector).toContainText('A stream connection cannot be feedback.');
    await expect(inspector.locator('.toggle-track')).toHaveCount(0);

    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.selectEdge('a-to-b'));
    await expect(inspector.locator('.toggle-track')).toHaveCount(1);
  });

  test('a stream group that has finished shows idle beside one that has not', async ({ page }) => {
    const controls = page.locator('witgraph-controls');
    const phase = (id: string) =>
      page
        .locator('witgraph-canvas')
        .evaluate((el: WitgraphCanvas, id) => el.controller?.nodes.get(id)?.phase, id);
    await controls.locator('button.run').click();
    // A burst of five, and a future: they end, while the camera never does.
    for (const finished of ['producer', 'consumer', 'writer', 'reader']) {
      await expect.poll(() => phase(finished), finished).toBe('idle');
    }
    expect(await phase('camera')).toBe('running');
    expect(await phase('video-sink')).toBe('running');
    await controls.locator('button.stop').click();
  });

  test('the debugger steps, continues, pauses and restarts', async ({ page }) => {
    const controls = page.locator('witgraph-controls');
    const canvas = page.locator('witgraph-canvas');
    const paused = () => canvas.evaluate((el: WitgraphCanvas) => el.controller?.pausedNode ?? null);
    const badge = controls.locator('.badge');
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.toggleBreakpoint('relay-b'));

    // Run stops at the breakpoint, shown on the node, with what else there is to do.
    await controls.locator('button.run').click();
    await expect.poll(paused).toBe('relay-b');
    await expect(badge).toContainText('Paused');
    await expect(canvas.locator('[data-node-id="relay-b"].paused-here')).toBeAttached();
    await expect(page.locator('witgraph-inspector').locator('.node-id')).toHaveValue('relay-b');
    await expect(controls.locator('button.step')).toBeEnabled();
    await expect(controls.locator('button.resume')).toBeEnabled();
    await expect(controls.locator('button.run')).toBeDisabled();

    // Stepping again and again moves on, a node at a time, with no error.
    const seen = new Set<string>();
    let previous = await paused();
    for (let at = 0; at < 6; at++) {
      await controls.locator('button.step').click();
      await expect.poll(paused).not.toBe(previous);
      previous = await paused();
      seen.add(previous ?? '');
    }
    // Around the feedback loop this graph has.
    expect(seen.size).toBeGreaterThan(1);
    const errors = () =>
      canvas.evaluate(
        (el: WitgraphCanvas) =>
          el.controller?.traceEvents.filter((e) => e.event === 'error').length,
      );
    expect(await errors()).toBe(0);

    // The keys do the same.
    const before = await paused();
    await page.keyboard.press('F10');
    await expect.poll(paused).not.toBe(before);

    // Continue goes on, Pause holds what starts next, Stop ends it.
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.toggleBreakpoint('relay-b'));
    await controls.locator('button.resume').click();
    await expect(badge).toContainText('Running');
    await controls.locator('button.pause').click();
    await expect(badge).toContainText('Paused');
    await page.keyboard.press('Shift+F5');
    await expect(badge).not.toContainText('Paused');
    expect(await errors()).toBe(0);

    // Restart runs it from the top.
    await controls.locator('button.restart').click();
    await expect(badge).toContainText('Running');
    await controls.locator('button.stop').click();
  });

  test('the inspector describes the selected node by its component', async ({ page }) => {
    await page.locator('witgraph-canvas').locator('[data-node-id="relay-a"]').click();
    await expect(page.locator('witgraph-inspector').locator('.node-docs')).toContainText(
      'Passes a u32 through',
    );
  });

  test('the timeline names the node of an event', async ({ page }) => {
    await page.locator('witgraph-controls').locator('button.run').click();
    const timeline = page.locator('witgraph-timeline');
    const started = timeline.locator('.event', { hasText: 'run-started' }).first();
    await expect(started).toBeVisible({ timeout: 10_000 });
    await expect(started.locator('.underline').first()).not.toBeEmpty();
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
    await expect(controls.locator('button.run')).toBeEnabled();
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
      const el = document.querySelector('witgraph-timeline') as HTMLElement;
      const parent = el.parentElement as HTMLElement;
      el.remove();
      parent.appendChild(el);
    });
    const moved = await rows();
    await expect.poll(rows).toBeGreaterThan(moved);
    await controls.locator('button.stop').click();
  });

  test('the timeline leaves a reader who scrolled up where they are', async ({ page }) => {
    const timeline = page.locator('witgraph-timeline');
    const scroll = () =>
      timeline.evaluate((el: HTMLElement) => {
        const box = el.shadowRoot?.querySelector('.events-wrap') as HTMLElement;
        return {
          top: box.scrollTop,
          toEnd: box.scrollHeight - box.scrollTop - box.clientHeight,
          rows: box.querySelectorAll('.event').length,
        };
      });
    const controls = page.locator('witgraph-controls');
    await controls.locator('button.run').click();
    // Rows enough to scroll through, which stay at the end while followed.
    await expect.poll(async () => (await scroll()).rows, { timeout: 20_000 }).toBeGreaterThan(60);
    expect((await scroll()).toEnd).toBeLessThanOrEqual(8);

    await timeline.evaluate((el: HTMLElement) => {
      (el.shadowRoot?.querySelector('.events-wrap') as HTMLElement).scrollTop = 0;
    });
    const { rows } = await scroll();
    await expect.poll(async () => (await scroll()).rows).toBeGreaterThan(rows);
    expect((await scroll()).top).toBe(0);

    // Back at the end, it follows again.
    await timeline.evaluate((el: HTMLElement) => {
      const box = el.shadowRoot?.querySelector('.events-wrap') as HTMLElement;
      box.scrollTop = box.scrollHeight;
    });
    const { rows: before } = await scroll();
    await expect.poll(async () => (await scroll()).rows).toBeGreaterThan(before);
    expect((await scroll()).toEnd).toBeLessThanOrEqual(8);
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

  test('after a tick, what still streams shows as running', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.tick());
    const phase = (id: string) =>
      canvas.evaluate((el: WitgraphCanvas, id) => el.controller?.nodes.get(id)?.phase, id);
    await expect.poll(() => phase('camera')).toBe('running');
    // Not only the last of the chain, whose `run` is the one still in flight.
    for (const id of ['gray', 'gray-invert', 'gray-sink', 'camera-b', 'invert', 'sink']) {
      expect(await phase(id), id).toBe('running');
    }
    const controls = page.locator('witgraph-controls');
    await expect(controls.locator('.badge')).toContainText('Running');
    const status = page.locator('#status');
    await expect(status).toHaveClass(/running/);
    await expect(status).toContainText('Still running — 7 of 7 nodes');
    // Nothing here is wrong, so nothing should read as though it were.
    const inspector = page.locator('witgraph-inspector');
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.select('gray'));
    await expect(inspector).toContainText('streaming, latest:');
    await expect(inspector).not.toContainText('is not an output port');
    await expect(inspector).not.toContainText('feedback connection');

    // The step is over and the stream is not, so it can still be stopped.
    const stop = page.locator('witgraph-controls').locator('button.stop');
    await stop.click();
    await expect.poll(() => phase('camera')).toBe('cancelled');
    await expect(stop).toBeDisabled();
    await expect(controls.locator('.badge')).toContainText('Idle');
    await expect(status).toHaveClass(/ready/);
    await expect(status).toContainText('Graph compiles');
    await expect(inspector).toContainText('stream ended');
  });

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

test('a node that spins leaves the page live', async ({ page }) => {
  await page.goto('/?graph=spin');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  const canvas = page.locator('witgraph-canvas');
  await canvas.evaluate((el: WitgraphCanvas) =>
    el.controller?.injectValue('busy', 'action', 'spin'),
  );
  await page.locator('witgraph-controls').locator('button.run').click();
  await expect(page.locator('witgraph-controls').locator('.badge')).toContainText('Running');
  // Long enough for the node to be well into its loop.
  await page.waitForTimeout(500);
  const frames = await page.evaluate(
    () =>
      new Promise<number>((resolve) => {
        let count = 0;
        const started = performance.now();
        const frame = () => {
          count += 1;
          if (performance.now() - started < 500) requestAnimationFrame(frame);
          else resolve(count);
        };
        requestAnimationFrame(frame);
      }),
  );
  expect(frames).toBeGreaterThan(5);
  // And it still answers: choosing nothing, then the node again, shows in the inspector.
  await canvas.evaluate((el: WitgraphCanvas) => el.controller?.select(null));
  await canvas.evaluate((el: WitgraphCanvas) => el.controller?.select('busy'));
  await expect(page.locator('witgraph-inspector').locator('.node-id')).toHaveValue('busy');

  // Stop can't reach it, so it ends the worker, and the graph loads in another.
  const controls = page.locator('witgraph-controls');
  await controls.locator('button.stop').click();
  await expect(controls.locator('button.run')).toBeEnabled({ timeout: 10_000 });
  await expect(controls.locator('.badge')).not.toContainText('Running');
  await expect(page.locator('witgraph-timeline')).toContainText('did not stop');

  // Told otherwise, it runs as any node does.
  await canvas.evaluate((el: WitgraphCanvas) =>
    el.controller?.injectValue('busy', 'action', 'finish'),
  );
  await controls.locator('button.run').click();
  await expect
    .poll(() => canvas.evaluate((el: WitgraphCanvas) => el.controller?.readOutput('busy', 'done')))
    .toBe(1);
});
