/**
 * Copies test components into components/ so the demo page can fetch them, and builds each
 * with debug info too, into components/debug/, for the page's `?debug`. That takes a while the
 * first time. `--no-build` copies only what was built already, as CI has it from another job,
 * and `--list` names the guests.
 *
 * Prerequisites:
 *   cargo build -p test-components
 *   cd packages/witgraph-loader && pnpm build:wasm
 *
 * Usage (from packages/witgraph-editor-demo):
 *   node setup.mjs
 *   # or: pnpm run setup
 */

import { execSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readdirSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
process.chdir(__dirname);

const GUESTS = [
  'relay',
  'echo',
  'stream-producer',
  'stream-consumer',
  'future-writer',
  'future-reader',
  'configurable',
  'calc',
  'mqtt-node',
  'bytes-consumer',
  'video-source',
  'frame-grayscale',
  'frame-invert',
  'video-sink',
  'edge-detect',
  'edge-detect-webgpu',
  'tensor-source',
  'tensor-edge',
  'tensor-invert',
  'tensor-sink',
  'tensor-stream-source',
  'tensor-stream-invert',
  'tensor-stream-edge',
  'tensor-stream-sink',
  'camera-source',
  'busy-loop',
  'line-walk',
];

// For CI to build these with debug info, in a job of its own.
if (process.argv.includes('--list')) {
  console.log(GUESTS.join(','));
  process.exit(0);
}

// Resolve the cargo target directory via `cargo metadata`.
let targetDir;
try {
  const raw = execSync('cargo metadata --format-version 1 --no-deps', {
    encoding: 'utf-8',
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const match = raw.match(/"target_directory"\s*:\s*"([^"]+)"/);
  if (match) targetDir = match[1];
} catch {
  /* handled below */
}

if (!targetDir) {
  console.error('error: could not determine cargo target directory');
  console.error('       is Rust / Cargo installed?');
  process.exit(1);
}

/** Walk a directory tree and return the most-recently modified file matching a test. */
function findNewest(root, test) {
  let best = { path: '', mtime: 0 };
  function walk(dir) {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
        continue;
      }
      if (!test(full)) continue;
      const mt = statSync(full).mtimeMs;
      if (mt > best.mtime) best = { path: full, mtime: mt };
    }
  }
  walk(root);
  return best.path || null;
}

const build = !process.argv.includes('--no-build');
if (build) {
  console.log('Building the guests with debug info...');
  execSync('cargo build -p test-components', {
    stdio: 'inherit',
    env: { ...process.env, WITGRAPH_DEBUG_GUESTS: GUESTS.join(',') },
  });
}

/** Copies the newest build of `file` to `dst`. */
function copyNewest(file, dst) {
  const buildDir = join(targetDir, 'debug', 'build');
  const escaped = file.replaceAll('.', '\\.');
  const pattern = new RegExp(`test-components-[^/\\\\]+[/\\\\]out[/\\\\]${escaped}$`);
  const src = findNewest(buildDir, (p) => pattern.test(p));
  if (!src) {
    console.error(`error: ${file} not found in ${buildDir}`);
    console.error('       build the guests first:  cargo build -p test-components');
    process.exit(1);
  }
  copyFileSync(src, dst);
  console.log(`Copied ${src}`);
  console.log(`  -> ${dst}`);
}

mkdirSync(join('components', 'debug'), { recursive: true });

for (const guest of GUESTS) {
  copyNewest(`${guest}.wasm`, join('components', `${guest}.wasm`));
  copyNewest(`${guest}.debug.wasm`, join('components', 'debug', `${guest}.wasm`));
}
