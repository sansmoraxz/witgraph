/**
 * Values as people read them: what an output, an edge or a stream item shows. A value comes as
 * jco gives it, or as an attached program's WAVE text reads (the same shapes): records as
 * objects with camelCased fields, variants as `{ tag, val }`, enum cases and strings as
 * strings, an absent option as `undefined`, lists as arrays, 64-bit integers as `BigInt`s.
 *
 * A record shows its fields as `field  value` rows, a nested one indented under its field when
 * it does not fit a line. A variant shows as `case(payload)`, or its case alone; field names
 * are written as in WIT (`observed-at`), strings without quotes, and what is absent as `—`.
 */

/** The longest line a field's value is written on before it gets rows of its own. */
const LINE = 56;
/** The most items of a list written out. */
const ITEMS = 8;
/** The longest one-line rendering. */
const MAX_LINE = 400;

const ABSENT = '—';

type Variant = { tag: string; val?: unknown };

function isVariant(value: unknown): value is Variant {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  return (
    typeof (value as Variant).tag === 'string' &&
    keys.every((key) => key === 'tag' || key === 'val')
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return (
    typeof value === 'object' &&
    value !== null &&
    !Array.isArray(value) &&
    !ArrayBuffer.isView(value) &&
    !isVariant(value)
  );
}

/** A field's name as WIT writes it. */
function fieldName(key: string): string {
  return key.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`);
}

function bytes(value: Uint8Array): string {
  if (value.length > 0 && value.every((b) => b >= 0x20 && b < 0x7f))
    return JSON.stringify(new TextDecoder().decode(value));
  const hex = Array.from(value.subarray(0, 16), (b) => b.toString(16).padStart(2, '0')).join(' ');
  return `<${hex}${value.length > 16 ? ` … ${value.length} bytes` : ''}>`;
}

function number(value: number): string {
  if (!Number.isFinite(value) || Number.isInteger(value)) return String(value);
  // What floating point adds past what was meant: 0.1 + 0.2 is 0.3.
  return String(Number(value.toPrecision(12)));
}

/** `value` on one line: a record's fields without its braces. */
export function readableLine(value: unknown): string {
  const text = isRecord(value) && Object.keys(value).length > 0 ? inner(value) : line(value);
  return text.length > MAX_LINE ? `${text.slice(0, MAX_LINE)}…` : text;
}

function line(value: unknown): string {
  if (value === undefined || value === null) return ABSENT;
  if (typeof value === 'string') return value === '' ? '""' : value;
  if (typeof value === 'number') return number(value);
  if (typeof value === 'bigint' || typeof value === 'boolean') return String(value);
  if (value instanceof Uint8Array) return bytes(value);
  if (ArrayBuffer.isView(value)) return `<${value.constructor.name}, ${value.byteLength} bytes>`;
  if (Array.isArray(value)) {
    if (value.length === 0) return '[]';
    const shown = value.slice(0, ITEMS).map(line);
    const more = value.length > ITEMS ? `, … ${value.length} items` : '';
    return `[${shown.join(', ')}${more}]`;
  }
  if (isVariant(value)) {
    return value.val === undefined ? value.tag : `${value.tag}(${inner(value.val)})`;
  }
  if (isRecord(value)) return `{${inner(value)}}`;
  return String(value);
}

/** A record's fields without braces, as a variant's payload shows them. */
function inner(value: unknown): string {
  if (!isRecord(value)) return line(value);
  return Object.entries(value)
    .map(([key, field]) => `${fieldName(key)}: ${line(field)}`)
    .join(', ');
}

/**
 * `value` as rows: a record's fields one a line, a list's items one a line, nested ones
 * indented when they do not fit; anything else on one line.
 */
export function readable(value: unknown): string {
  return rows(value, '').join('\n');
}

function rows(value: unknown, indent: string): string[] {
  if (isRecord(value)) {
    const entries = Object.entries(value);
    if (entries.length === 0) return [`${indent}{}`];
    const width = Math.max(...entries.map(([key]) => fieldName(key).length));
    return entries.flatMap(([key, field]) => {
      const name = fieldName(key);
      const flat = line(field);
      if (flat.length <= LINE || !nests(field)) return [`${indent}${name.padEnd(width)}  ${flat}`];
      // A variant of a record: its case on the field's line, its fields under it.
      if (isVariant(field) && isRecord(field.val)) {
        return [`${indent}${name.padEnd(width)}  ${field.tag}`, ...rows(field.val, `${indent}  `)];
      }
      return [`${indent}${name}`, ...rows(field, `${indent}  `)];
    });
  }
  if (Array.isArray(value) && value.length > 0) {
    const shown = value.slice(-ITEMS).flatMap((item) => {
      const flat = line(item);
      if (flat.length <= LINE || !nests(item)) return [`${indent}· ${flat}`];
      const [first, ...rest] = rows(item, `${indent}  `);
      return [`${indent}· ${first.trimStart()}`, ...rest];
    });
    const more = value.length > ITEMS ? [`${indent}(the latest ${ITEMS} of ${value.length})`] : [];
    return [...more, ...shown];
  }
  if (isVariant(value) && isRecord(value.val) && line(value).length > LINE) {
    return [`${indent}${value.tag}`, ...rows(value.val, `${indent}  `)];
  }
  return [`${indent}${readableLine(value)}`];
}

/** Whether `value` has rows of its own to break into. */
function nests(value: unknown): boolean {
  return (
    isRecord(value) ||
    (Array.isArray(value) && value.length > 0) ||
    (isVariant(value) && isRecord(value.val))
  );
}

/** The longest a field's value is on its row in {@link readableElement}, before it nests. */
const CELL = 28;

/**
 * `value` as {@link readable} lays it out, as elements: a record a grid of its fields, names
 * in one column and values in the other, so that a value too long for its column wraps in it,
 * and nested ones indented under their field. Styled by the `readable` classes.
 */
export function readableElement(value: unknown): HTMLElement {
  const element = document.createElement('div');
  element.className = 'readable';
  fill(element, value);
  return element;
}

function part(tag: string, className: string, text?: string): HTMLElement {
  const element = document.createElement(tag);
  element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function fill(into: HTMLElement, value: unknown): void {
  if (isRecord(value) && Object.keys(value).length > 0) {
    const grid = part('div', 'readable-fields');
    for (const [key, field] of Object.entries(value)) {
      grid.append(part('span', 'readable-key', fieldName(key)));
      const flat = line(field);
      if (flat.length <= CELL || !nests(field)) {
        grid.append(part('span', 'readable-value', flat));
        continue;
      }
      // A variant of a record: its case on the field's row, its fields under it.
      const variant = isVariant(field) && isRecord(field.val);
      grid.append(part('span', 'readable-value readable-case', variant ? field.tag : ''));
      const nested = part('div', 'readable-nested');
      fill(nested, variant ? field.val : field);
      grid.append(nested);
    }
    into.append(grid);
    return;
  }
  if (Array.isArray(value) && value.length > 0) {
    if (value.length > ITEMS) {
      into.append(part('div', 'readable-note', `the latest ${ITEMS} of ${value.length}`));
    }
    for (const item of value.slice(-ITEMS)) {
      const row = part('div', 'readable-item');
      if (line(item).length <= CELL || !nests(item)) row.textContent = line(item);
      else fill(row, item);
      into.append(row);
    }
    return;
  }
  if (isVariant(value) && isRecord(value.val) && line(value).length > CELL) {
    into.append(part('div', 'readable-case', value.tag));
    const nested = part('div', 'readable-nested');
    fill(nested, value.val);
    into.append(nested);
    return;
  }
  into.append(part('span', 'readable-value', readableLine(value)));
}

/** The styles of {@link readableElement}'s classes. */
export const READABLE_STYLES = `
  .readable { white-space: normal; }
  .readable-fields { display: grid; grid-template-columns: fit-content(50%) minmax(0, 1fr);
    column-gap: 1.5ch; row-gap: 1px; }
  .readable-key { color: var(--wg-text-dim); overflow-wrap: anywhere; }
  .readable-value { overflow-wrap: anywhere; }
  .readable-case { font-style: italic; }
  .readable-nested { grid-column: 1 / -1; padding-left: 1.5ch;
    border-left: 1px solid var(--wg-surface-hover); margin-left: 0.25ch; }
  .readable-item + .readable-item { border-top: 1px dashed var(--wg-surface-hover);
    margin-top: 2px; padding-top: 2px; }
  .readable-note { color: var(--wg-text-dim); font-style: italic; }
`;
