import { readFileSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import type { WitgraphCanvas } from 'witgraph-editor';

const text = readFileSync(
  new URL('../../../test-fixtures/test-components/guests/line-walk/src/lib.rs', import.meta.url),
  'utf8',
);

/** The line of the `// mark: {name}` comment in `line-walk`'s source. */
function mark(name: string): number {
  const at = text.split('\n').findIndex((line) => line.endsWith(`// mark: ${name}`));
  expect(at, name).toBeGreaterThanOrEqual(0);
  return at + 1;
}

test.describe('the source panel', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/?graph=lines&debug');
    await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  });

  test('sets breakpoints, follows stops, and steps with the usual keys', async ({ page }) => {
    const source = page.locator('witgraph-source');
    const controls = page.locator('witgraph-controls');
    const row = (line: number) => source.locator(`.row[data-line="${line}"]`);
    const current = source.locator('.row.current');

    await expect(source).toBeVisible();
    // Scrolling the code leaves the header where it is.
    await source.locator('.rows').evaluate((el) => {
      el.scrollTop = el.scrollHeight;
    });
    expect(await source.locator('.rows').evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
    await expect(source.locator('select.file')).toBeInViewport();
    await source.locator('.rows').evaluate((el) => {
      el.scrollTop = 0;
    });
    await expect(row(mark('walk-first'))).toContainText('let a = add(x, 1);');
    await expect(controls.locator('button.step-in')).toBeVisible();
    await expect(controls.locator('button.step-out')).toBeDisabled();

    // A line with no code lands on the next that has it.
    await row(mark('above-walk')).locator('.gutter').click();
    await expect(source.locator('.row.breakpoint')).toHaveCount(1);
    await source.locator('.row.breakpoint .gutter').click();
    await expect(source.locator('.row.breakpoint')).toHaveCount(0);

    await row(mark('walk-first')).locator('.gutter').click();
    await expect(row(mark('walk-first'))).toHaveClass(/breakpoint/);

    await controls.locator('button.run').click();
    await expect(current).toHaveAttribute('data-line', String(mark('walk-first')));
    await expect(controls.locator('.badge')).toContainText('Paused');
    // Where it stopped is on show, with what says so, however little room there is.
    await expect(current).toBeInViewport();
    await expect(source.locator('select.file')).toBeInViewport();
    await expect(page.locator('#status')).toContainText(`Stopped at line ${mark('walk-first')}`);
    await expect(source.locator('.where')).toContainText('in first');
    await expect(source.locator('.frames .frame').first()).toContainText('walk');
    await expect(
      page.locator('witgraph-canvas').locator('[data-node-id="first"].paused-here'),
    ).toBeAttached();

    await page.keyboard.press('F10');
    await expect(current).toHaveAttribute('data-line', String(mark('walk-second')));
    await page.keyboard.press('F11');
    await expect(source.locator('.frames .frame').first()).toContainText('add');
    await page.keyboard.press('Shift+F11');
    await expect(source.locator('.frames .frame').first()).toContainText('walk');
    await expect(controls.locator('.badge')).toContainText('Paused');

    // Both nodes run this code: Continue stops at the same line in the other.
    await page.keyboard.press('F5');
    await expect(source.locator('.where')).toContainText('in second');
    await expect(current).toHaveAttribute('data-line', String(mark('walk-first')));

    await controls.locator('button.resume').click();
    await expect(controls.locator('button.run')).toBeEnabled();
    await expect(current).toHaveCount(0);
    const output = await page
      .locator('witgraph-canvas')
      .evaluate((el: WitgraphCanvas) => el.controller?.readOutput('second', 'out'));
    expect(output).toBe(11);
  });

  test('variables show what they hold, and mark what changed', async ({ page }) => {
    const source = page.locator('witgraph-source');
    const controls = page.locator('witgraph-controls');
    const variables = source.locator('.variables');
    const named = (path: string) => variables.locator(`[data-path="${path}"]`);

    await expect(variables).toBeHidden();
    await source.locator(`.row[data-line="${mark('walk-first')}"] .gutter`).click();
    await controls.locator('button.run').click();
    await expect(source.locator('.row.current')).toBeVisible();
    await expect(variables).toBeInViewport();
    await expect(named('x')).toContainText('x: 5');
    await expect(named('x')).toContainText('u32');
    await expect(named('a')).toHaveCount(0);

    await page.keyboard.press('F10');
    await expect(named('a')).toContainText('a: 6');
    await expect(named('x')).not.toHaveClass(/changed/);
    await expect(variables).not.toHaveClass(/stale/);
    // Into another function: its `a` is not the one before.
    await page.keyboard.press('F11');
    await expect(named('a')).toContainText('a: 6');
    await expect(source.locator('.frames .frame').first()).toContainText('add');
    await expect(named('a')).not.toHaveClass(/changed/);
    await page.keyboard.press('Shift+F11');
    await expect(source.locator('.frames .frame').first()).toContainText('walk');
    await expect(controls.locator('.badge')).toContainText('Paused');
    await page.keyboard.press('F5');
    await expect(source.locator('.where')).toContainText('in second');
    // `second` reads what `first` made of it.
    await expect(named('x')).toContainText('x: 8');
    await expect(named('x')).toHaveClass(/changed/);
    await page.keyboard.press('F5');
    await expect(controls.locator('button.run')).toBeEnabled();
    await expect(variables).toBeHidden();
  });

  test('a variable with parts opens, and stays open through a step', async ({ page }) => {
    const source = page.locator('witgraph-source');
    const controls = page.locator('witgraph-controls');
    const variables = source.locator('.variables');
    await page
      .locator('witgraph-canvas')
      .evaluate((el: WitgraphCanvas) => el.controller?.injectValue('first', 'kinds', '4'));
    await source.locator(`.row[data-line="${mark('kinds-used')}"] .gutter`).click();
    await controls.locator('button.run').click();
    await expect(source.locator('.row.current')).toBeVisible();

    const rect = variables.locator('[data-path="rect"]');
    await expect(rect).toContainText('Rect { w: 4, h: 5 }');
    await expect(variables.locator('[data-path="text"]')).toContainText('"hello"');
    await expect(variables.locator('[data-path="owned"]')).toContainText('"owned"');
    await expect(variables.locator('[data-path="list"]')).toContainText('[10, 20, 30]');
    await expect(variables.locator('[data-path="rect.w"]')).toBeHidden();
    await rect.locator('summary').click();
    await expect(variables.locator('[data-path="rect.w"]')).toContainText('w: 4');

    await page.keyboard.press('F10');
    await expect(source.locator('.row.current')).toHaveAttribute(
      'data-line',
      String(mark('kinds-used') + 1),
    );
    await expect(variables.locator('[data-path="rect.w"]')).toBeVisible();
    await controls.locator('button.stop').click();
  });

  test('the two levels are told apart, and each button does one thing', async ({ page }) => {
    const source = page.locator('witgraph-source');
    const controls = page.locator('witgraph-controls');
    const badge = controls.locator('.badge');
    const over = controls.locator('button.step-over');
    const next = controls.locator('button.step');
    await expect(next).toContainText('Next node');
    await expect(over).toBeDisabled();

    // Stepping a node: no line, so nothing to do in code but go into it.
    await next.click();
    await expect(badge).toContainText('Paused at a node');
    await expect(page.locator('#status')).toContainText('Paused before node first');
    await expect(over).toBeDisabled();
    await expect(controls.locator('button.step-in')).toBeEnabled();
    await expect(controls.locator('button.step-out')).toBeDisabled();
    await expect(source.locator('.row.current')).toHaveCount(0);

    // Into its code: a line, with the same button for the next node still meaning that.
    await controls.locator('button.step-in').click();
    await expect(badge).toContainText('Paused on a line');
    // With both levels' buttons in the toolbar, it wraps rather than pushing this off the page.
    await expect(badge).toBeInViewport();
    await expect(page.locator('#status')).toContainText('Stopped at line');
    await expect(source.locator('.row.current')).toBeVisible();
    await expect(over).toBeEnabled();
    await next.click();
    await expect(badge).toContainText('Paused at a node');
    await expect(page.locator('#status')).toContainText('Paused before node second');
    await expect(source.locator('.row.current')).toHaveCount(0);
    await controls.locator('button.stop').click();
  });

  test('a caller in the stack shows the line it is waiting on', async ({ page }) => {
    const source = page.locator('witgraph-source');
    const controls = page.locator('witgraph-controls');
    await source.locator(`.row[data-line="${mark('add-body')}"] .gutter`).click();
    await controls.locator('button.run').click();
    await expect(source.locator('.row.current')).toHaveAttribute(
      'data-line',
      String(mark('add-body')),
    );
    const frames = source.locator('.frames .frame');
    await expect(frames.first()).toContainText(`lib.rs:${mark('add-body')}`);
    await expect(frames.nth(1)).toContainText('walk');
    await expect(frames.nth(1)).toContainText(`lib.rs:${mark('walk-first')}`);

    await frames.nth(1).click();
    const line = source.locator(`.row[data-line="${mark('walk-first')}"]`);
    await expect(line).toHaveClass(/frame-line/);
    await expect(line).toBeInViewport();
    await expect(source.locator('.caller-note')).toBeVisible();
    await frames.first().click();
    await expect(line).not.toHaveClass(/frame-line/);
    await expect(source.locator('.caller-note')).toBeHidden();
    await controls.locator('button.stop').click();
  });

  test('stop ends a guest stopped on a line, and leaves its breakpoints set', async ({ page }) => {
    const source = page.locator('witgraph-source');
    const controls = page.locator('witgraph-controls');
    await source.locator(`.row[data-line="${mark('walk-first')}"] .gutter`).click();
    await controls.locator('button.run').click();
    await expect(source.locator('.row.current')).toBeVisible();

    await controls.locator('button.stop').click();
    await expect(controls.locator('button.run')).toBeEnabled();
    await expect(source.locator('.row.current')).toHaveCount(0);
    await expect(page.locator('witgraph-timeline')).not.toContainText('did not stop');
    await expect(source.locator('.row.breakpoint')).toHaveCount(1);

    await controls.locator('button.run').click();
    await expect(source.locator('.row.current')).toBeVisible();
  });
});

test('debugging lines takes `?debug`, even for a guest that could be', async ({ page }) => {
  await page.goto('/?graph=lines');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  await expect(page.locator('witgraph-source')).toBeHidden();
  await expect(page.locator('witgraph-controls').locator('button.step-in')).toBeHidden();
  const files = await page
    .locator('witgraph-canvas')
    .evaluate((el: WitgraphCanvas) => el.controller?.lineFiles.length);
  expect(files).toBe(0);
});

test('a graph with no `?debug` has no source panel', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  await expect(page.locator('witgraph-source')).toBeHidden();
  await expect(page.locator('witgraph-controls').locator('button.step-in')).toBeHidden();
});

test('a breakpoint in a stream loop comes back for each item', async ({ page }) => {
  await page.goto('/?debug');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  const canvas = page.locator('witgraph-canvas');
  // On the line `while let Some(frame) = frames.next().await`, which has many places in the code.
  await canvas.evaluate(async (el: WitgraphCanvas) => {
    const controller = el.controller;
    if (!controller) throw new Error('no controller');
    const file = controller.lineFiles.find((name) => name.endsWith('video-sink/src/lib.rs'));
    if (!file) throw new Error('no debug build of the sink');
    const text = (await controller.sourceOf(file)) ?? '';
    const line = text.split('\n').findIndex((row) => row.includes('frames.next()')) + 1;
    controller.toggleLineBreakpoint(file, line);
  });
  await page.locator('witgraph-controls').locator('button.run').click();
  for (let stop = 0; stop < 4; stop++) {
    await expect
      .poll(() => canvas.evaluate((el: WitgraphCanvas) => el.controller?.lineStop?.node ?? null))
      .toBe('video-sink');
    await canvas.evaluate((el: WitgraphCanvas) => el.controller?.resumeBreakpoint());
  }
  await canvas.evaluate((el: WitgraphCanvas) => el.controller?.stop());
});

test('a node paused at in a stream says it runs once per stream', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  const canvas = page.locator('witgraph-canvas');
  await canvas.evaluate((el: WitgraphCanvas) => el.controller?.toggleBreakpoint('consumer'));
  await page.locator('witgraph-controls').locator('button.run').click();
  await expect(page.locator('#status')).toContainText('It runs once per stream');
  await page.locator('witgraph-controls').locator('button.stop').click();
});

test('`?debug` loads debug builds, so any graph has its source to stop in', async ({ page }) => {
  test.setTimeout(90_000);
  await page.goto('/?debug');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 60_000 });
  const source = page.locator('witgraph-source');
  const controls = page.locator('witgraph-controls');
  await expect(source).toBeVisible();

  // A file for each kind of guest in the graph: `relay` is one that has no debug info otherwise.
  const files = await source.locator('select.file option').allTextContents();
  expect(files.length).toBeGreaterThan(3);
  const relay = files.find(
    (file) => file.startsWith('relay/src/lib.rs') || file.includes('/relay/src/lib.rs'),
  );
  expect(relay, files.join()).toBeTruthy();
  await source.locator('select.file').selectOption({ label: relay as string });

  // In its `run`, as only this guest has it: the file draws once its text is there.
  const line = source.locator('.row', { hasText: 'inputs.in_.wrapping_add' });
  await line.locator('.gutter').click();
  await controls.locator('button.run').click();
  await expect(source.locator('.row.current')).toBeVisible({ timeout: 30_000 });
  await expect(source.locator('.where')).toContainText(/in (relay-|display)/);
  await expect(source.locator('.variables')).toContainText('inputs');
  await controls.locator('button.stop').click();
  await expect(controls.locator('button.run')).toBeEnabled({ timeout: 10_000 });
});
