//! Editor-facing component metadata, generated from lowered contracts.
//!
//! WIT is the source of truth; this catalog is a derived artifact for
//! frontend editors (palette entries, port pickers, tooltips).

use serde::Serialize;
use witgraph_ir::{ComponentContract, HandleType, PortDef, PortKind};

use crate::lower::Lowered;

/// The catalog format version this crate emits.
pub const SCHEMA_VERSION: u32 = 6;

/// The full set of components available to an editor.
#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// One entry per contract (a world, as a component provides it), sorted
    /// by component and world.
    pub components: Vec<ComponentMeta>,
}

/// Editor-facing view of one component contract.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentMeta {
    /// The contract's id, `component/world` (`sensor/demo:graph/sensor@0.1.0`),
    /// parsed back by [`ContractId`](witgraph_ir::ContractId)'s `FromStr`.
    pub id: String,
    /// The component, by its name in the graph's component table.
    pub component: String,
    /// The world, e.g. `demo:graph/sensor@0.1.0`.
    pub world: String,
    /// The world's WIT package, e.g. `demo:graph@0.1.0`.
    pub package: String,
    /// Doc comment from the WIT world, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// The component's input ports.
    pub inputs: Vec<PortMeta>,
    /// The component's output ports.
    pub outputs: Vec<PortMeta>,
    /// What the component requires from its host.
    pub capabilities: Vec<CapabilityMeta>,
    /// Named types referenced by the ports, for tooltips and pickers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<TypeMeta>,
    /// What was wrong with the component but did not stop it lowering:
    /// viewer hints skipped ([`Lowered::warnings`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Editor-facing view of one capability a component requires.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityMeta {
    /// The capability as the component imports it: an interface id, an
    /// inline interface's import name, a `func:`-prefixed bare function
    /// import, a `resource:`-prefixed world resource, or the label of a
    /// labelled interface import.
    pub name: String,
    /// For a labelled import, the full id of the interface the label
    /// stands for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implements: Option<String>,
}

/// Editor-facing view of one named type referenced by a port.
#[derive(Debug, Clone, Serialize)]
pub struct TypeMeta {
    /// The type's WIT kebab-case name.
    pub name: String,
    /// The named interface declaring it, by full id; absent when it is
    /// declared in an anonymous inline interface. Two types may share a
    /// name when their owners differ.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// WIT-syntax rendering of the type, for display.
    pub type_display: String,
    /// Doc comment from the WIT declaration, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
}

/// Editor-facing view of one port.
#[derive(Debug, Clone, Serialize)]
pub struct PortMeta {
    /// The port's WIT kebab-case name.
    pub name: String,
    /// The port's delivery semantics; serialized lowercase.
    pub kind: PortKind,
    /// WIT-syntax rendering of the payload type (`_` for a bare `stream` or
    /// `future`, `interface.resource` for a handle). Editors resolve
    /// structure by re-deriving the contract from WIT, never from this
    /// string.
    pub type_display: String,
    /// Handle ports only: the resource the port carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<HandleType>,
    /// Value input ports only: the port may be left unconnected.
    pub optional: bool,
    /// Doc comment from the WIT field, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Output ports only: the viewer the component's node manifest names
    /// for it, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
}

fn port_meta(port: &PortDef) -> PortMeta {
    PortMeta {
        name: port.name.to_string(),
        kind: port.kind(),
        type_display: port.type_display(),
        handle: port.handle_type().cloned(),
        optional: port.is_optional(),
        docs: port.docs.clone(),
        viewer: port.viewer.clone(),
    }
}

impl ComponentMeta {
    /// The editor's view of `contract`, without named types (which only a
    /// lowered world has; see [`generate_catalog`]).
    pub fn of(contract: &ComponentContract) -> Self {
        Self {
            id: contract.id.to_string(),
            component: contract.id.component.to_string(),
            world: contract.id.world.to_string(),
            package: contract.id.world.package.to_string(),
            docs: contract.docs.clone(),
            inputs: contract.inputs.iter().map(port_meta).collect(),
            outputs: contract.outputs.iter().map(port_meta).collect(),
            capabilities: contract
                .capabilities
                .iter()
                .map(|c| CapabilityMeta {
                    name: c.interface.clone(),
                    implements: c.implements.clone(),
                })
                .collect(),
            types: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// Build a catalog from lowered worlds. Entries are sorted by their
/// structured id, not the rendered string, so versions order numerically
/// (`0.2.0` before `0.10.0`) and the output is stable regardless of load
/// order.
pub fn generate_catalog(lowered: &[Lowered]) -> Catalog {
    let mut sorted: Vec<&Lowered> = lowered.iter().collect();
    sorted.sort_by(|a, b| a.contract.id.cmp(&b.contract.id));
    let components = sorted
        .into_iter()
        .map(
            |Lowered {
                 contract,
                 types,
                 warnings,
                 ..
             }| ComponentMeta {
                warnings: warnings.clone(),
                types: types
                    .iter()
                    .map(|decl| TypeMeta {
                        name: decl.name.clone(),
                        owner: decl.owner.clone(),
                        type_display: decl.ty.to_string(),
                        docs: decl.docs.clone(),
                    })
                    .collect(),
                ..ComponentMeta::of(contract)
            },
        )
        .collect();
    Catalog {
        schema_version: SCHEMA_VERSION,
        components,
    }
}

/// Stable pretty JSON with a trailing newline, suitable for committing.
pub fn to_json(catalog: &Catalog) -> serde_json::Result<String> {
    let mut json = serde_json::to_string_pretty(catalog)?;
    json.push('\n');
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::load_str;
    use crate::lower::lower;

    fn c() -> witgraph_ir::ComponentName {
        "c".parse().unwrap()
    }

    #[test]
    fn catalog_is_sorted_and_versioned() {
        let source = load_str(
            "test.wit",
            r#"
            package demo:test@0.1.0;
            world zeta {
                export node: interface {
                    record outputs { out: stream<u32> }
                    run: async func() -> outputs;
                }
            }
            world alpha {
                export node: interface {
                    record inputs { rate: option<u32> }
                    run: async func(inputs: inputs);
                }
            }
            "#,
        )
        .unwrap();
        let catalog = generate_catalog(&lower(&source, &c()).unwrap());

        assert_eq!(catalog.schema_version, SCHEMA_VERSION);
        let ids: Vec<&str> = catalog
            .components
            .iter()
            .map(|c| c.id.split('#').next().unwrap_or_default())
            .collect();
        assert_eq!(ids, ["c/demo:test/alpha@0.1.0", "c/demo:test/zeta@0.1.0"]);

        let alpha = &catalog.components[0];
        assert_eq!(alpha.package, "demo:test@0.1.0");
        assert_eq!(alpha.component, "c");
        assert_eq!(alpha.world, "demo:test/alpha@0.1.0");
        assert_eq!(alpha.inputs[0].kind, PortKind::Value);
        assert_eq!(alpha.inputs[0].type_display, "u32");
        assert!(alpha.inputs[0].optional);
        let zeta = &catalog.components[1];
        assert_eq!(zeta.outputs[0].kind, PortKind::Stream);
        assert_eq!(zeta.outputs[0].type_display, "u32");
        let json = to_json(&catalog).unwrap();
        assert!(json.ends_with('\n'));
        assert!(
            !json.contains("\"type\""),
            "no structured types in the catalog: {json}"
        );
    }

    #[test]
    fn versions_sort_numerically_not_lexically() {
        let wit = |version: &str| {
            format!(
                r#"
                package demo:test@{version};
                world w {{
                    export node: interface {{
                        record outputs {{ out: u32 }}
                        run: async func() -> outputs;
                    }}
                }}
                "#
            )
        };
        let mut contracts = lower(&load_str("a.wit", &wit("0.10.0")).unwrap(), &c()).unwrap();
        contracts.extend(lower(&load_str("b.wit", &wit("0.2.0")).unwrap(), &c()).unwrap());
        let catalog = generate_catalog(&contracts);
        let ids: Vec<&str> = catalog
            .components
            .iter()
            .map(|c| c.id.split('#').next().unwrap_or_default())
            .collect();
        assert_eq!(ids, ["c/demo:test/w@0.2.0", "c/demo:test/w@0.10.0"]);
    }

    #[test]
    fn named_types_are_emitted() {
        let source = load_str(
            "test.wit",
            r#"
            package demo:test@0.1.0;
            interface node {
                /// A reading.
                record reading { value: f64 }
                record outputs { latest: reading }
                run: async func() -> outputs;
            }
            world w { export node; }
            "#,
        )
        .unwrap();
        let catalog = generate_catalog(&lower(&source, &c()).unwrap());
        let types = &catalog.components[0].types;
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "reading");
        assert!(types[0].type_display.contains("value"));
        assert_eq!(types[0].docs.as_deref(), Some("A reading."));

        // With no named types, the field is omitted from the JSON entirely.
        let bare = load_str(
            "bare.wit",
            r#"
            package demo:test@0.1.0;
            world w {
                export node: interface {
                    record outputs { out: u32 }
                    run: async func() -> outputs;
                }
            }
            "#,
        )
        .unwrap();
        let json = to_json(&generate_catalog(&lower(&bare, &c()).unwrap())).unwrap();
        assert!(!json.contains("\"types\""), "{json}");
    }
}
