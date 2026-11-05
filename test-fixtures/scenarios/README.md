# Scenarios

What a graph should do, written once and run on both hosts: by
`crates/witgraph-runtime/tests/scenarios.rs` on wasmtime, and by
`packages/witgraph-loader/test/scenarios.test.ts` on JavaScript. A difference
between the hosts fails on one of them.

A scenario is `nodes` (an `id`, a `guest`, as `test-components` names it, and perhaps
`resources`, each an amount claimed),
`connections` (`from` and `to` as `node.port`, and `feedback`), and `steps`, each of
which `do`es one of these:

| Step       | Fields                       | What it does                                                       |
| ---------- | ---------------------------- | ------------------------------------------------------------------ |
| `inject`   | `at`, `wave`, `error`        | Writes WAVE text to an input.                                      |
| `clear`    | `at`, `error`                | Clears an input.                                                   |
| `rerun`    | `node`, `error`              | Makes a node owe a run.                                            |
| `shutdown` |                              | Stops the graph.                                                   |
| `tick`     | `expect`                     | Runs one iteration, and checks how it ended.                       |
| `settle`   |                              | Ticks until a tick does nothing.                                   |
| `output`   | `at`, `expect`               | Reads an output as WAVE text, or `null` for none.                  |
| `runs`     | `expect`                     | Counts the runs started since the last `runs`, of every node.      |
| `faults`   | `expect`                     | Takes the faults: each a kind, or a `kind` and a `culprit`.        |
| `phase`    | `node`, `expect`             | Reads a node's phase.                                              |
| `snapshot` | `expect`                     | Takes a snapshot, and checks the fields listed.                    |
| `restore`  | `snapshot`, `patch`, `error` | Restores the snapshot given, or else the last taken, with `patch`. |

`error` says the step fails, with a message holding that text.

Those in `generated/` are written by `fuzz/tests/real.rs`, as `fuzz/README.md` says.
