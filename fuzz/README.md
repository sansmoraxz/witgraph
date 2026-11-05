# Fuzzing witgraph's graphs

Each generated graph is held to rules worked out here apart from the code they check:

- **`graph_model`** compiles a graph, and compares every diagnostic with the documented
  rules, which have their own resolution, cycle search, groups and exact arithmetic. It
  also checks what must hold whatever the rules are: a graph compiles the same in any
  declaration order, and survives JSON.
- **`graph_json`** does the same from raw bytes, as a graph's JSON.
- **`graph_run`** runs a graph that compiled on the scheduler the hosts share, with each
  node a function whose answer is known, and compares which nodes ran, how often, and
  what every output holds, including after host writes.

`cargo test -p witgraph-fuzz` runs these checks on stable, over seeded inputs and
every graph in `regressions/graph/`. It fails if a rule was never reached, as agreement on a rule
never reached means nothing. `WITGRAPH_FUZZ_CASES` runs more.

To search with coverage guidance, which needs nightly and `cargo install cargo-fuzz`:

```sh
cd fuzz
cargo +nightly fuzz run --features libfuzzer graph_run -- -fork=8 -max_len=4096 -len_control=20
```

Keep a graph that found a bug in `regressions/graph/`, as its JSON (a failure prints it), with
a name saying what it was: raw fuzzer input means nothing once the generator changes.
