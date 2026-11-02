import { test } from '@playwright/test';

declare const bench: any;

const FULL = process.env.BENCH === 'full';
const SIZES: [number, number, number][] = FULL
  ? [
      [16, 16, 300],
      [640, 480, 200],
      [1920, 1080, 100],
    ]
  : [[16, 16, 30]];
const BACKENDS = FULL ? ['cpu', 'webgpu', 'webnn'] : ['cpu', 'webgpu'];

test('streams of handles', async ({ page }) => {
  page.on('console', (m) => {
    if (m.type() === 'error') console.log('page error:', m.text().slice(0, 200));
  });
  page.on('pageerror', (e) => console.log('page threw:', String(e).slice(0, 200)));
  await page.goto('/bench.html');
  await page.waitForFunction(() => document.title === 'ready');
  console.log('adapter', await page.evaluate(() => bench.adapter()));
  const run = async (what: string, arg: unknown) => {
    try {
      console.log(
        JSON.stringify(await page.evaluate(([w, a]) => bench[w as string](a), [what, arg])),
      );
    } catch (error) {
      console.log('FAILED', what, JSON.stringify(arg), String(error).slice(0, 300));
    }
  };
  if (process.env.BENCH === 'chain') {
    for (const filters of [1, 2, 3, 5, 8]) {
      for (const backend of ['webgpu', 'cpu']) {
        await run('handles', {
          backend,
          width: 640,
          height: 480,
          frames: 200,
          reuse: true,
          readLast: true,
          filters,
        });
      }
      await run('handles', {
        backend: 'webgpu',
        width: 1920,
        height: 1080,
        frames: 100,
        reuse: true,
        readLast: true,
        filters,
      });
    }
    for (const backend of ['webgpu', 'webnn', 'cpu']) {
      await run('cameraChain', { backend, frames: 200 });
    }
    return;
  }
  await run('numbers', FULL ? 300 : 30);
  await run('copied', FULL ? 200 : 20);
  for (const backend of BACKENDS) {
    for (const [width, height, frames] of SIZES) {
      for (const readLast of [true, false]) {
        await run('handles', { backend, width, height, frames, reuse: true, readLast });
      }
    }
    if (FULL) {
      await run('handles', {
        backend,
        width: 1920,
        height: 1080,
        frames: 30,
        reuse: false,
        readLast: true,
      });
    }
  }
});
