import assert from 'node:assert/strict';
import { test } from 'node:test';
import { readable, readableLine } from '../src/util/readable.ts';

const reading = {
  quality: 'good',
  value: { tag: 'float-with-unit', val: { value: 3.49, unit: 'Cel' } },
  at: '2026-11-14T04:46:36.427Z',
};

test('a record shows its fields as rows, names as WIT writes them', () => {
  assert.equal(
    readable({ ...reading, observedAt: undefined }),
    [
      'quality      good',
      'value        float-with-unit(value: 3.49, unit: Cel)',
      'at           2026-11-14T04:46:36.427Z',
      'observed-at  —',
    ].join('\n'),
  );
});

test('a nested record that does not fit a line gets rows of its own', () => {
  const update = {
    source: 'observation',
    sample: { io: 'CR1_TEMP', ...reading, receivedAt: '2026-11-14T04:46:36.500Z' },
  };
  assert.deepEqual(readable(update).split('\n'), [
    'source  observation',
    'sample',
    '  io           CR1_TEMP',
    '  quality      good',
    '  value        float-with-unit(value: 3.49, unit: Cel)',
    '  at           2026-11-14T04:46:36.427Z',
    '  received-at  2026-11-14T04:46:36.500Z',
  ]);
});

test('one line drops the outer braces, and keeps the nested ones', () => {
  assert.equal(
    readableLine({ io: 'CR1_VALVE', status: 'ok', outcome: { tag: 'ok', val: { a: 1n } } }),
    'io: CR1_VALVE, status: ok, outcome: ok(a: 1)',
  );
  assert.equal(readableLine({ tag: 'none' }), 'none');
  assert.equal(readableLine([1, 2, 3]), '[1, 2, 3]');
  assert.equal(readableLine(0.1 + 0.2), '0.3');
  assert.equal(readableLine(''), '""');
  assert.equal(readableLine(new Uint8Array([104, 105])), '"hi"');
});

test('a list shows its latest items, one a line', () => {
  const items = Array.from({ length: 10 }, (_, i) => i);
  assert.deepEqual(readable(items).split('\n'), [
    '(the latest 8 of 10)',
    ...[2, 3, 4, 5, 6, 7, 8, 9].map((i) => `· ${i}`),
  ]);
});
