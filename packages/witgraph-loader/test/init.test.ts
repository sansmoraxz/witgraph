// The module-level functions before and after the witgraph-web module is
// initialised. A file of its own: the test runner gives each file a fresh
// process, so nothing has initialised the module here yet.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';

import { describeComponent, init, jsName } from '../src/index.ts';

test('describeComponent and jsName work once init has run, and say so before', async () => {
  assert.throws(() => jsName('burst-size'), /not initialised: call `init`/);
  assert.throws(() => describeComponent('test:echo/echo@0.1.0', new Uint8Array()), /not initialised/);
  await init(await readFile(new URL('../pkg/witgraph_web_bg.wasm', import.meta.url)));
  assert.equal(jsName('burst-size'), 'burstSize');
  // Not a component: the module itself answers.
  assert.throws(
    () => describeComponent('test:echo/echo@0.1.0', new Uint8Array()),
    (error: Error) => !/not initialised/.test(error.message),
  );
});
