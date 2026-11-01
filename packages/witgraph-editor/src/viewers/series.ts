import type { PortViewer } from './types.ts';

const SVG_NS = 'http://www.w3.org/2000/svg';
/** The plot's own coordinates; it is stretched to the preview region. */
const WIDTH = 100;
const HEIGHT = 40;

/**
 * `value` as a number to plot: a number, a `BigInt`, a bool (0 or 1), or a
 * record's first field that is one of those. `undefined` for anything else.
 */
export function numeric(value: unknown): number | undefined {
  if (typeof value === 'number') return Number.isFinite(value) ? value : undefined;
  if (typeof value === 'bigint') return Number(value);
  if (typeof value === 'boolean') return value ? 1 : 0;
  // A record, not a list: a list is no one number.
  if (
    typeof value === 'object' &&
    value !== null &&
    !Array.isArray(value) &&
    !ArrayBuffer.isView(value)
  ) {
    for (const field of Object.values(value)) {
      if (typeof field === 'object') continue;
      const n = numeric(field);
      if (n !== undefined) return n;
    }
  }
  return undefined;
}

/** `values` as a polyline's points, scaled to fill the plot. One value is a level line. */
export function plotPoints(values: readonly number[]): string {
  if (values.length === 0) return '';
  if (values.length === 1) return plotPoints([values[0], values[0]]);
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const step = WIDTH / (values.length - 1);
  return values
    .map((v, i) => {
      const x = i * step;
      // A flat series sits in the middle.
      const y = max === min ? HEIGHT / 2 : HEIGHT - ((v - min) / span) * HEIGHT;
      return `${+x.toFixed(2)},${+y.toFixed(2)}`;
    })
    .join(' ');
}

/** A number for the label: short, without float noise. */
function label(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toPrecision(4);
}

/**
 * A line of an output's recent values: one point per tick for a Value
 * output, per item for a stream. The latest value, the range and the count
 * are written below it; the line's point count is in `data-points`.
 */
export const seriesViewer: PortViewer = {
  id: 'series',
  mount(host) {
    const root = document.createElement('div');
    root.className = 'preview-series';
    const svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('viewBox', `0 0 ${WIDTH} ${HEIGHT}`);
    svg.setAttribute('preserveAspectRatio', 'none');
    const area = document.createElementNS(SVG_NS, 'polygon');
    area.classList.add('series-area');
    const line = document.createElementNS(SVG_NS, 'polyline');
    line.classList.add('series-line');
    svg.append(area, line);
    const caption = document.createElement('div');
    caption.className = 'preview-series-label';
    root.append(svg, caption);
    host.appendChild(root);
    return {
      update(view) {
        const values = view.recent.map(numeric).filter((n): n is number => n !== undefined);
        const points = plotPoints(values);
        line.setAttribute('points', points);
        area.setAttribute('points', points ? `0,${HEIGHT} ${points} ${WIDTH},${HEIGHT}` : '');
        svg.dataset.points = String(values.length);
        if (values.length === 0) {
          caption.textContent = view.pending ? 'pending' : '-';
          return;
        }
        const latest = values[values.length - 1];
        const range = `${label(Math.min(...values))}…${label(Math.max(...values))}`;
        const count = view.kind === 'stream' ? ` · ${view.count} items` : '';
        caption.textContent = `${label(latest)}  [${range}]${count}`;
      },
      dispose() {
        root.remove();
      },
    };
  },
};
