//! Custom sections a component declares for witgraph itself, such as
//! [`viewer`](crate::viewer)'s and [`docs`](crate::docs)'.

/// The text of every custom section of `bytes` named `name`, in order, and
/// what was wrong with the rest. A section that is not UTF-8 is skipped.
///
/// Only the component's own sections count: those at its root and those of
/// the core modules directly inside it. A section of a component nested in
/// it (a link's provider, composed in) belongs to that component, whose
/// ports are not this one's.
pub(crate) fn texts(bytes: &[u8], name: &str) -> (Vec<String>, Vec<String>) {
    use wasmparser::{Parser, Payload};

    /// What a section nests in, outermost first.
    #[derive(PartialEq)]
    enum Scope {
        Module,
        Component,
    }

    let mut texts = Vec::new();
    let mut problems = Vec::new();
    let mut scopes: Vec<Scope> = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = match payload {
            Ok(payload) => payload,
            Err(e) => {
                problems.push(format!("the component could not be read for `{name}`: {e}"));
                break;
            }
        };
        match payload {
            Payload::ModuleSection { .. } => scopes.push(Scope::Module),
            Payload::ComponentSection { .. } => scopes.push(Scope::Component),
            Payload::End(_) => {
                scopes.pop();
            }
            Payload::CustomSection(reader)
                if reader.name() == name && matches!(scopes.as_slice(), [] | [Scope::Module]) =>
            {
                match std::str::from_utf8(reader.data()) {
                    Ok(data) => texts.push(data.to_owned()),
                    Err(e) => problems.push(format!("a `{name}` section is not UTF-8: {e}")),
                }
            }
            _ => {}
        }
    }
    (texts, problems)
}
