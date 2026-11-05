//! Whatever bytes, as a graph's JSON, against the same rules.

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| witgraph_fuzz::graph_json(data));
