import { readFileSync } from 'node:fs';
import { expect, type Page, test } from '@playwright/test';

const source = readFileSync(
  new URL('../../../test-fixtures/test-components/guests/line-walk/src/lib.rs', import.meta.url),
  'utf8',
);

/** The line of the `// mark: {name}` comment in `line-walk`'s source. */
function mark(name: string): number {
  const at = source.split('\n').findIndex((line) => line.endsWith(`// mark: ${name}`));
  expect(at, name).toBeGreaterThanOrEqual(0);
  return at + 1;
}

type Dbg = Record<string, (...args: unknown[]) => unknown>;

const dbg = <T>(page: Page, name: string, ...args: unknown[]): Promise<T> =>
  page.evaluate(
    ([name, args]) =>
      (window as unknown as { dbg: Dbg }).dbg[name as string](...(args as unknown[])),
    [name, args] as const,
  ) as Promise<T>;

interface Stop {
  reason: string;
  line: number;
  node?: string;
  frames: string[];
}

async function nthStop(page: Page, nth: number): Promise<Stop> {
  await expect
    .poll(async () => (await dbg<Stop[]>(page, 'stops')).length, { timeout: 15_000 })
    .toBeGreaterThanOrEqual(nth);
  return (await dbg<Stop[]>(page, 'stops'))[nth - 1];
}

test.beforeEach(async ({ page }) => {
  await page.goto('/test/lines/index.html');
  await page.waitForFunction(() => (window as unknown as { ready?: boolean }).ready, undefined, {
    timeout: 30_000,
  });
});

test('a guest stops on a source line, with the page live', async ({ page }) => {
  expect(await dbg(page, 'isolated')).toBe(true);
  expect(await dbg(page, 'file')).toContain('line-walk/src/lib.rs');
  expect(await dbg(page, 'set', mark('walk-first'))).toBe(mark('walk-first'));
  await dbg(page, 'inject', 'in', 5);
  await dbg(page, 'tick');

  const first = await nthStop(page, 1);
  expect(first).toMatchObject({ reason: 'breakpoint', line: mark('walk-first'), node: 'walker' });
  expect(first.frames[0]).toContain('walk');

  // Everything on the page still answers, for as long as the guest is held.
  const before = await dbg<number>(page, 'frames');
  await page.locator('#live').click();
  await page.locator('#live').click();
  await page.waitForTimeout(300);
  expect((await dbg<number>(page, 'frames')) - before).toBeGreaterThan(3);
  await expect(page.locator('#live')).toHaveText('2');

  await dbg(page, 'clear', mark('walk-first'));
  await dbg(page, 'stepOver');
  expect((await nthStop(page, 2)).line).toBe(mark('walk-second'));
  await dbg(page, 'stepIn');
  const inside = await nthStop(page, 3);
  expect(inside.frames[0]).toContain('add');
  await dbg(page, 'stepOut');
  expect((await nthStop(page, 4)).frames[0]).toContain('walk');

  await dbg(page, 'resume');
  await expect.poll(() => dbg(page, 'ticked')).toBeTruthy();
  expect(await dbg(page, 'output')).toBe(8);
});

test('pausing stops a guest in the middle of a spin', async ({ page }) => {
  await dbg(page, 'inject', 'spin', 4_000_000_000);
  await dbg(page, 'tick');
  await page.waitForTimeout(300);
  await dbg(page, 'pause');
  const stop = await nthStop(page, 1);
  expect(stop.reason).toBe('pause');
  expect(stop.frames[0]).toContain('spin');

  // And a breakpoint set while it spins is seen, with no message read in the worker.
  await dbg(page, 'resume');
  await page.waitForTimeout(100);
  expect(await dbg(page, 'set', mark('spin-body'))).toBe(mark('spin-body'));
  expect(await nthStop(page, 2)).toMatchObject({ reason: 'breakpoint', line: mark('spin-body') });
});
