//! Graphs that compile, run, and every output compared with what it should hold.

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| witgraph_fuzz::graph_run(data));
