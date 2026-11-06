import { type ChildProcess, execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import { createInterface } from 'node:readline';
import { expect, test } from '@playwright/test';
import type { WitgraphCanvas } from 'witgraph-editor';

// Attaching to a graph a program runs on wasmtime, served with `witgraph-remote` over
// WebTransport: the `serve` example of that crate is the program.
const root = new URL('../../../', import.meta.url).pathname;
let host: ChildProcess | undefined;
let attach = '';

test.beforeAll(async () => {
  test.setTimeout(600_000);
  execFileSync('cargo', ['build', '-q', '-p', 'witgraph-remote', '--example', 'serve'], {
    cwd: root,
    stdio: 'inherit',
  });
  host = spawn(`${root}target/debug/examples/serve`, [], { stdio: ['ignore', 'pipe', 'inherit'] });
  if (!host.stdout) throw new Error('the program has no output to read');
  const lines = createInterface({ input: host.stdout });
  const [first] = (await once(lines, 'line')) as [string];
  const { url, certificateHash } = JSON.parse(first) as { url: string; certificateHash: string };
  attach = `/?attach=${encodeURIComponent(url)}&hash=${certificateHash}`;
});

test.afterAll(() => {
  host?.kill();
});

test('the editor attaches, and holds a node of the graph its program runs', async ({ page }) => {
  await page.goto(attach);
  const canvas = page.locator('witgraph-canvas');
  await expect(canvas.locator('[data-node-id="walker"]')).toBeVisible({ timeout: 20_000 });
  const controls = page.locator('witgraph-controls');
  await expect(controls.locator('.badge')).toHaveText('Attached');
  // The program runs it: here it is only debugged.
  await expect(controls.locator('button.run')).toBeHidden();
  await expect(controls.locator('button.stop')).toBeHidden();
  await expect(controls.locator('.rate-group')).toBeHidden();
  // The rest looks as it does on a graph of the page's own.
  await expect(page.locator('#graph-picker')).toContainText('Attached: 127.0.0.1');
  const catalog = page.locator('witgraph-catalog');
  await expect(catalog.locator('.card')).toHaveCount(4);
  await expect(catalog).toContainText('test:line-walk/line-walk@0.1.0');
  await expect(catalog).not.toContainText('#');
  await expect(page.locator('witgraph-timeline')).toContainText('run-returned', {
    timeout: 10_000,
  });

  await canvas.evaluate((el: WitgraphCanvas) => el.controller?.toggleBreakpoint('walker'));
  await expect(controls.locator('.badge')).toHaveText('Paused at a node', { timeout: 10_000 });
  await expect(page.locator('#status')).toContainText('Paused before node walker');
  // What it will run on, as WAVE, as outputs are shown.
  const inspector = page.locator('witgraph-inspector');
  await expect(inspector.locator('.bp-bg')).toContainText(/in\s*\d+/);
  await expect(inspector.locator('.bp-bg')).toContainText(/spin\s*-/);

  // It goes on, and stops there again on its next run.
  await controls.locator('button.resume').click();
  await expect(controls.locator('.badge')).toHaveText('Paused at a node', { timeout: 10_000 });
  await canvas.evaluate((el: WitgraphCanvas) => el.controller?.toggleBreakpoint('walker'));
  await controls.locator('button.resume').click();
  await expect(controls.locator('.badge')).toHaveText('Attached');
});

test("the editor stops on a line of the program's guest, with its source", async ({ page }) => {
  await page.goto(attach);
  const canvas = page.locator('witgraph-canvas');
  await expect(canvas.locator('[data-node-id="walker"]')).toBeVisible({ timeout: 20_000 });
  const source = page.locator('witgraph-source');
  // Nothing is selected yet, so no source is on show.
  await expect(source).toContainText('Select a node to see its source.');
  await canvas.locator('[data-node-id="walker"]').click();
  await expect(source.locator('select.file option')).toHaveText(
    ['line-walk/src/lib.rs', 'line-walk/src/scale.rs'],
    { useInnerText: true },
  );
  await source.locator('select.file').selectOption({ label: 'line-walk/src/lib.rs' });
  // Its text comes from the program's machine.
  await expect(source.locator('.row', { hasText: 'mark: walk-first' })).toBeVisible({
    timeout: 20_000,
  });
  await source.locator('.row', { hasText: 'mark: walk-first' }).locator('.gutter').click();
  await expect(source.locator('.row.current')).toContainText('mark: walk-first', {
    timeout: 20_000,
  });
  await expect(source.locator('.where')).toContainText('in walker');
  await expect(source).toContainText('x');

  await source.locator('.row', { hasText: 'mark: walk-first' }).locator('.gutter').click();
  await page.locator('witgraph-controls').locator('button.resume').click();
  await expect(page.locator('witgraph-controls').locator('.badge')).toHaveText('Attached');
});

test("the editor stops on a line of the program's guest written in C", async ({ page }) => {
  await page.goto(attach);
  const canvas = page.locator('witgraph-canvas');
  await expect(canvas.locator('[data-node-id="c-walker"]')).toBeVisible({ timeout: 20_000 });
  const source = page.locator('witgraph-source');
  await canvas.locator('[data-node-id="c-walker"]').click();
  await expect(source.locator('select.file option')).toHaveText(
    ['c-walk/src/scale.c', 'c-walk/src/walk.c'],
    { useInnerText: true },
  );
  await source.locator('select.file').selectOption({ label: 'c-walk/src/walk.c' });
  const row = source.locator('.row', { hasText: 'mark: walk-first' });
  await expect(row).toContainText('uint32_t a = add(x, 1);', { timeout: 20_000 });
  await row.locator('.gutter').click();
  await expect(source.locator('.row.current')).toContainText('mark: walk-first', {
    timeout: 20_000,
  });
  await expect(source.locator('.where')).toContainText('in c-walker');
  await expect(source.locator('.frames .frame').first()).toContainText('walk');
  const x = source.locator('.variables [data-path="x"]');
  await expect(x).toContainText('uint32_t');

  // Into `add`, as in a Rust guest.
  await page.locator('witgraph-controls').locator('button.step-in').click();
  await expect(source.locator('.row.current')).toContainText('mark: add-body');
  await expect(source.locator('.frames .frame').first()).toContainText('add');

  // And in its second file, which the program runs.
  await row.locator('.gutter').click();
  await source.locator('select.file').selectOption({ label: 'c-walk/src/scale.c' });
  const scaleRow = source.locator('.row', { hasText: 'mark: scale-add' });
  await scaleRow.locator('.gutter').click();
  await page.locator('witgraph-controls').locator('button.resume').click();
  await expect(source.locator('.row.current')).toContainText('mark: scale-add', {
    timeout: 20_000,
  });
  await expect(source.locator('.where')).toContainText('in c-walker');

  await scaleRow.locator('.gutter').click();
  await page.locator('witgraph-controls').locator('button.resume').click();
  await expect(page.locator('witgraph-controls').locator('.badge')).toHaveText('Attached');
});

test("the editor stops on a line of the program's guest written in Zig", async ({ page }) => {
  await page.goto(attach);
  const canvas = page.locator('witgraph-canvas');
  await expect(canvas.locator('[data-node-id="zig-walker"]')).toBeVisible({ timeout: 20_000 });
  const source = page.locator('witgraph-source');
  await canvas.locator('[data-node-id="zig-walker"]').click();
  await expect(source.locator('select.file option')).toHaveText(
    ['zig-walk/src/scale.zig', 'zig-walk/src/walk.zig'],
    { useInnerText: true },
  );
  await source.locator('select.file').selectOption({ label: 'zig-walk/src/walk.zig' });
  const row = source.locator('.row', { hasText: 'mark: walk-first' });
  await expect(row).toContainText('const a = add(x, 1);', { timeout: 20_000 });
  await row.locator('.gutter').click();
  await expect(source.locator('.row.current')).toContainText('mark: walk-first', {
    timeout: 20_000,
  });
  await expect(source.locator('.where')).toContainText('of walk.zig in zig-walker');
  await expect(source.locator('.frames .frame').first()).toContainText('walk.walk');
  await expect(source.locator('.variables [data-path="x"]')).toContainText('u32');

  await row.locator('.gutter').click();
  await page.locator('witgraph-controls').locator('button.resume').click();
  await expect(page.locator('witgraph-controls').locator('.badge')).toHaveText('Attached');
});

test('an editor attaching takes over from one gone without a word', async ({ page, context }) => {
  // One left stopped on a line, as a page reloaded while stopped leaves it: the program only
  // notices it has gone once it times out.
  await page.goto(attach);
  const source = page.locator('witgraph-source');
  const walker = page.locator('witgraph-canvas').locator('[data-node-id="walker"]');
  await expect(walker).toBeVisible({ timeout: 20_000 });
  await walker.click();
  await source.locator('select.file').selectOption({ label: 'line-walk/src/lib.rs' });
  const row = source.locator('.row', { hasText: 'mark: walk-first' });
  await expect(row).toBeVisible({ timeout: 20_000 });
  await row.locator('.gutter').click();
  await expect(source.locator('.row.current')).toBeVisible({ timeout: 20_000 });

  const next = await context.newPage();
  await next.goto(attach);
  const canvas = next.locator('witgraph-canvas');
  await expect(canvas.locator('[data-node-id="walker"]')).toBeVisible({ timeout: 20_000 });
  // Its graph runs on, stopping at no breakpoint of the other's.
  const walked = () =>
    canvas.evaluate((el: WitgraphCanvas) => el.controller?.readOutputWave('walker', 'out'));
  const first = await walked();
  await expect.poll(walked, { timeout: 10_000 }).not.toBe(first);
  await expect(next.locator('witgraph-controls').locator('.badge')).toHaveText('Attached');
  await expect(next.locator('witgraph-source').locator('.row.current')).toHaveCount(0);
  await page.close();
});
