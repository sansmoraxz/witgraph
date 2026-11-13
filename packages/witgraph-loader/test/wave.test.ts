// WAVE text read as the values a graph run in the page gives.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fromWave } from '../src/attach/wave.ts';

test('WAVE text reads as the values jco lifts', () => {
  assert.equal(fromWave('3'), 3);
  assert.equal(fromWave('-2.5e3'), -2500);
  assert.equal(fromWave('-inf'), Number.NEGATIVE_INFINITY);
  assert.ok(Number.isNaN(fromWave('nan')));
  assert.equal(fromWave('true'), true);
  assert.equal(fromWave('"a \\"b\\"\\n\\u{1F600}"'), 'a "b"\n😀');
  assert.equal(fromWave("'x'"), 'x');
  assert.deepEqual(fromWave('[1, 2, 3,]'), [1, 2, 3]);
  assert.deepEqual(fromWave('("a", 1)'), ['a', 1]);
  assert.equal(fromWave('none'), undefined);
  assert.equal(fromWave('some(4)'), 4);
  assert.deepEqual(fromWave('ok(1)'), { tag: 'ok', val: 1 });
  assert.equal(fromWave('good'), 'good', 'a case with no payload is its name');
  assert.equal(fromWave('%ok'), 'ok', 'a keyword escaped');
  assert.deepEqual(fromWave('{read, write}'), { read: true, write: true });
  assert.deepEqual(fromWave('{}'), {});
});

test('a record of a record of a variant reads whole, its fields camelCased', () => {
  const sample = fromWave(
    '[{io: "T1", quality: good, value: some(float(22.5)), observed-at: "a"}, ' +
      '{io: "T1", quality: bad, observed-at: "b"}]',
  );
  assert.deepEqual(sample, [
    { io: 'T1', quality: 'good', value: { tag: 'float', val: 22.5 }, observedAt: 'a' },
    { io: 'T1', quality: 'bad', observedAt: 'b' },
  ]);
});

test('text that is not WAVE is undefined', () => {
  for (const text of ['', '[1, 2', '{a: }', '"open', '1 2', "'ab'"]) {
    assert.equal(fromWave(text), undefined, text);
  }
});
