//! Writes each Rust snippet of the README and docs as a function, with what snippets
//! assume to be in scope ([`STAND_INS`]). A fence marked `rust,ignore` is left out:
//! those are guests, built for wasm with `wit-bindgen`.

use std::path::Path;
use std::{env, fs};

/// What snippets take for granted, in scope of each. Pasted in as text, as a macro's
/// `let`s would be hidden from the snippet.
const STAND_INS: &str = r#"
let graph: Graph = unimplemented!();
let contracts: Vec<ComponentContract> = unimplemented!();
let compiled: CompiledGraph = unimplemented!();
let wasm: HashMap<ComponentRef, &'static [u8]> = unimplemented!();
let config: RuntimeConfig = unimplemented!();
let engine: Engine = unimplemented!();
let host: witgraph_runtime::NoCapabilities = unimplemented!();
let mut rt: RuntimeGraph = unimplemented!();
let reader: ValReader = unimplemented!();
let calc: ComponentContract = unimplemented!();
let math_ref: ComponentRef = unimplemented!();
let sensor: ComponentRef = unimplemented!();
let alarm: ComponentRef = unimplemented!();
let digest: Digest = unimplemented!();
let dir: PathBuf = unimplemented!();
let cache_dir: PathBuf = unimplemented!();
let mut linker: Linker<PluginData> = unimplemented!();
let node: NodeId = unimplemented!();
let port: PortName = unimplemented!();
let seen: PortVersion = unimplemented!();
"#;

const DOCS: [&str; 5] = [
    "README.md",
    "docs/components.md",
    "docs/graphs.md",
    "docs/javascript.md",
    "docs/running.md",
];

/// The body of every fence that is exactly ```` ```rust ````.
fn snippets(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        match &mut open {
            Some(body) if line.trim_end() == "```" => {
                found.push(std::mem::take(body));
                open = None;
            }
            Some(body) => {
                body.push_str(line);
                body.push('\n');
            }
            None if line.trim_end() == "```rust" => open = Some(String::new()),
            None => {}
        }
    }
    found
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = String::new();
    let mut count = 0;
    for doc in DOCS {
        let path = root.join(doc);
        println!("cargo::rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap();
        for (at, body) in snippets(&text).iter().enumerate() {
            let name: String = doc
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '_'
                    }
                })
                .collect();
            out.push_str(&format!(
                "pub async fn {name}_{at}() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {{\n\
                 {STAND_INS}\n{body}\nOk(())\n}}\n\n"
            ));
            count += 1;
        }
    }
    out.push_str(&format!("pub const SNIPPETS: usize = {count};\n"));
    let dir = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&dir).join("snippets.rs"), out).unwrap();
}
