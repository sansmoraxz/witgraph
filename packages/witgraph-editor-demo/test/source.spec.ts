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
    await expect(source.locator('.frames')).toContainText('walk');
    await expect(
      page.locator('witgraph-canvas').locator('[data-node-id="first"].paused-here'),
    ).toBeAttached();

    await page.keyboard.press('F10');
    await expect(current).toHaveAttribute('data-line', String(mark('walk-second')));
    await page.keyboard.press('F11');
    await expect(source.locator('.frames')).toHaveText(/^[^←]*add/);
    await page.keyboard.press('Shift+F11');
    await expect(source.locator('.frames')).toHaveText(/^[^←]*walk/);
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
    await expect(source.locator('.frames')).toHaveText(/^[^←]*add/);
    await expect(named('a')).not.toHaveClass(/changed/);
    await page.keyboard.press('Shift+F11');
    await expect(source.locator('.frames')).toHaveText(/^[^←]*walk/);
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

test('a graph with no debug info has no source panel', async ({ page }) => {
  await page.goto('/?debug');
  await expect(page.locator('#status')).toHaveClass(/ready/, { timeout: 30_000 });
  await expect(page.locator('witgraph-source')).toBeHidden();
  await expect(page.locator('witgraph-controls').locator('button.step-in')).toBeHidden();
});
