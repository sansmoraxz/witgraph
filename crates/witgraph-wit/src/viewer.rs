//! Viewer hints: how an editor should preview a component's outputs, as the
//! component's node manifest declares them, per world:
//!
//! ```json
//! "viewers": { "demo:camera/capture@0.1.0": { "frames": "image" } }
//! ```
//!
//! The viewer (`image`) is opaque here: only an editor gives it meaning. A
//! hint is only a hint: one that is wrong is skipped, and never stops a
//! component from loading.

use std::collections::BTreeMap;

use witgraph_ir::ComponentContract;

/// Sets each hinted output's [`viewer`](witgraph_ir::PortDef::viewer):
/// `hints` maps output ports, by their WIT names, to viewers. A hint naming
/// an output `contract` does not have, or no viewer, is skipped: what is
/// returned says which.
pub fn apply(contract: &mut ComponentContract, hints: &BTreeMap<String, String>) -> Vec<String> {
    let mut problems = Vec::new();
    for (port, viewer) in hints {
        if viewer.is_empty() || viewer.contains(char::is_whitespace) {
            problems.push(format!(
                "the viewer `{viewer}` for output `{port}` of `{}` is not a viewer name",
                contract.id.world
            ));
            continue;
        }
        match contract
            .outputs
            .iter_mut()
            .find(|p| p.name.as_str() == port)
        {
            Some(output) => output.viewer = Some(viewer.clone()),
            None => problems.push(format!(
                "a viewer is given for output `{port}`, which `{}` does not have",
                contract.id.world
            )),
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_name_outputs_of_the_contract() {
        use wasm_wave::value::Type;
        use witgraph_ir::PortDef;
        let mut contract = ComponentContract {
            id: "c/a:b/c@0.1.0".parse().unwrap(),
            inputs: vec![PortDef::value("frames", Type::U32)],
            outputs: vec![PortDef::value("frames", Type::U32)],
            capabilities: Vec::new(),
            docs: None,
        };
        let hints = [
            ("missing", "text"),
            ("frames", "image"),
            ("frames-again", "two words"),
        ]
        .into_iter()
        .map(|(p, v)| (p.to_string(), v.to_string()))
        .collect();
        let problems = apply(&mut contract, &hints);
        assert_eq!(contract.outputs[0].viewer.as_deref(), Some("image"));
        assert_eq!(contract.inputs[0].viewer, None);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }
}
