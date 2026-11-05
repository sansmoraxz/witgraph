//! Graphs from a generator that mostly wires them correctly, against the rules.

#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| witgraph_fuzz::graph_model(data));
