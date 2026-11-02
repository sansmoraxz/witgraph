//! Viewer hints: how an editor should preview a component's outputs, as
//! the component itself declares them in a `witgraph-viewer` custom
//! section.
//!
//! The section is UTF-8 text, one hint per line:
//!
//! ```text
//! # comments and blank lines are ignored
//! output.frames=image
//! ```
//!
//! The viewer (`image`) is opaque here: only an editor gives it meaning.
//! Being line-based, several sections of the same name concatenated (which
//! is what the linker does with them) read as one, as long as each ends
//! with a newline.
//!
//! A guest written in Rust embeds the section with a static; it ends up in
//! the component's core module:
//!
//! ```ignore
//! #[used]
//! #[unsafe(link_section = "witgraph-viewer")]
//! static VIEWER: [u8; 20] = *b"output.frames=image\n";
//! ```

use witgraph_ir::ComponentContract;

/// The custom section's name.
pub const SECTION: &str = "witgraph-viewer";

/// One hint: output `port` is previewed with `viewer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// The output port, by its WIT name.
    pub port: String,
    /// The viewer, as the editor names it.
    pub viewer: String,
}

/// What the `witgraph-viewer` sections of a component say: the hints
/// that could be read, and what was wrong with the rest. A hint is only a
/// hint: one that is wrong is skipped, and never stops a component from
/// loading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hints {
    /// The hints, in order; each output at most once.
    pub hints: Vec<Hint>,
    /// What was skipped, and why.
    pub problems: Vec<String>,
}

/// The hints of every `witgraph-viewer` section of `bytes`, in order.
///
/// Only the component's own sections count: those at its root and those of
/// the core modules directly inside it. A section of a component nested in
/// it (a link's provider, composed in) belongs to that component, whose
/// ports are not this one's.
pub fn read(bytes: &[u8]) -> Hints {
    let (texts, mut problems) = crate::section::texts(bytes, SECTION);
    let mut text = String::new();
    for data in texts {
        text.push_str(&data);
        // A section that does not end its last line still ends it.
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    let mut hints = parse(&text);
    problems.append(&mut hints.problems);
    hints.problems = problems;
    hints
}

/// The hints of a section's text. A line that is not a hint, or names an
/// output named before, is a problem.
pub fn parse(text: &str) -> Hints {
    let mut parsed = Hints::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |message: String| format!("`{SECTION}` line {}: {message}", index + 1);
        let problem = (|| {
            let Some((key, viewer)) = line.split_once('=') else {
                return Err(format!("`{line}` is not `output.<port>=<viewer>`"));
            };
            let (key, viewer) = (key.trim(), viewer.trim());
            let Some(port) = key.strip_prefix("output.") else {
                return Err(format!("`{key}` does not name an output (`output.<port>`)"));
            };
            if port.is_empty() {
                return Err("no port is named".into());
            }
            if viewer.is_empty() || viewer.contains(char::is_whitespace) {
                return Err(format!("`{viewer}` is not a viewer name"));
            }
            if parsed.hints.iter().any(|h| h.port == port) {
                return Err(format!("output `{port}` is named twice"));
            }
            Ok(Hint {
                port: port.into(),
                viewer: viewer.into(),
            })
        })();
        match problem {
            Ok(hint) => parsed.hints.push(hint),
            Err(message) => parsed.problems.push(at(message)),
        }
    }
    parsed
}

/// Sets each hinted output's [`viewer`](witgraph_ir::PortDef::viewer).
/// A hint naming an output `contract` does not have is skipped: what is
/// returned says which.
pub fn apply(contract: &mut ComponentContract, hints: &[Hint]) -> Vec<String> {
    let mut problems = Vec::new();
    for hint in hints {
        match contract
            .outputs
            .iter_mut()
            .find(|p| p.name.as_str() == hint.port)
        {
            Some(port) => port.viewer = Some(hint.viewer.clone()),
            None => problems.push(format!(
                "the `{SECTION}` section names output `{}`, which the component does not have",
                hint.port
            )),
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(port: &str, viewer: &str) -> Hint {
        Hint {
            port: port.into(),
            viewer: viewer.into(),
        }
    }

    #[test]
    fn lines_comments_and_blanks() {
        let text = "# previews\n\noutput.frames=image\n  output.level = series  \n";
        assert_eq!(
            parse(text),
            Hints {
                hints: vec![hint("frames", "image"), hint("level", "series")],
                problems: Vec::new(),
            }
        );
        assert_eq!(parse(""), Hints::default());
    }

    #[test]
    fn malformed_lines_are_skipped_as_problems() {
        for (text, why) in [
            ("output.frames", "no `=`"),
            ("input.frames=image", "not an output"),
            ("frames=image", "no direction"),
            ("output.=image", "no port"),
            ("output.frames=", "no viewer"),
            ("output.frames=an image", "a viewer with a space"),
            ("output.frames=image\noutput.frames=text", "a port twice"),
        ] {
            assert_eq!(parse(text).problems.len(), 1, "{why}: {text:?}");
        }
        // The good lines still count.
        let parsed = parse("# ok\noutput.x=image\nbad\noutput.x=text");
        assert_eq!(parsed.hints, [hint("x", "image")]);
        assert_eq!(parsed.problems.len(), 2);
        assert!(
            parsed.problems[0].contains("line 3"),
            "{:?}",
            parsed.problems
        );
    }

    /// `bytes` with a custom section appended at the end of its root.
    fn with_section(bytes: &[u8], name: &str, data: &[u8]) -> Vec<u8> {
        fn leb(mut n: usize, out: &mut Vec<u8>) {
            loop {
                let byte = (n & 0x7f) as u8;
                n >>= 7;
                if n == 0 {
                    out.push(byte);
                    return;
                }
                out.push(byte | 0x80);
            }
        }
        let mut body = Vec::new();
        leb(name.len(), &mut body);
        body.extend_from_slice(name.as_bytes());
        body.extend_from_slice(data);
        let mut out = bytes.to_vec();
        out.push(0);
        leb(body.len(), &mut out);
        out.extend(body);
        out
    }

    #[test]
    fn sections_are_read_at_the_root_and_in_its_modules_only() {
        let hints = |bytes: &[u8]| {
            let read = read(bytes);
            assert_eq!(read.problems, Vec::<String>::new());
            read.hints
        };
        let root =
            wat::parse_str(r#"(component (@custom "witgraph-viewer" "output.a=image"))"#).unwrap();
        assert_eq!(hints(&root), [hint("a", "image")]);

        let module = wat::parse_str(
            r#"(component (core module (@custom "witgraph-viewer" "output.b=text\n")))"#,
        )
        .unwrap();
        assert_eq!(hints(&module), [hint("b", "text")]);

        // A section of a component nested in this one is the nested
        // component's: at its root, or in its modules.
        for nested in [
            r#"(component (component (@custom "witgraph-viewer" "output.c=image")))"#,
            r#"(component (component (core module (@custom "witgraph-viewer" "output.c=image"))))"#,
        ] {
            assert_eq!(hints(&wat::parse_str(nested).unwrap()), [], "{nested}");
        }
        // After a nested component ends, the root's sections count again.
        let after = wat::parse_str(
            r#"(component
                (component (@custom "witgraph-viewer" "output.c=image"))
                (@custom "witgraph-viewer" "output.d=image"))"#,
        )
        .unwrap();
        assert_eq!(hints(&after), [hint("d", "image")]);

        // Sections of the same name concatenate, even without a final
        // newline.
        let both = with_section(&module, SECTION, b"output.a=image");
        assert_eq!(hints(&both), [hint("b", "text"), hint("a", "image")]);

        let other = with_section(&module, "something-else", b"output.z=image");
        assert_eq!(hints(&other), [hint("b", "text")]);

        // A section that is not text is skipped; the others still count.
        let bad = read(&with_section(&module, SECTION, &[0xff, 0xfe]));
        assert_eq!(bad.hints, [hint("b", "text")]);
        assert!(bad.problems[0].contains("UTF-8"), "{:?}", bad.problems);
    }

    #[test]
    fn hints_name_outputs_of_the_contract() {
        use wasm_wave::value::Type;
        use witgraph_ir::{PortDef, PortKind};
        let mut contract = ComponentContract {
            id: "a:b/c@0.1.0".parse().unwrap(),
            inputs: vec![PortDef::new("frames", PortKind::Value, Type::U32)],
            outputs: vec![PortDef::new("frames", PortKind::Value, Type::U32)],
            capabilities: Vec::new(),
            docs: None,
        };
        let problems = apply(
            &mut contract,
            &[hint("missing", "text"), hint("frames", "image")],
        );
        assert_eq!(contract.outputs[0].viewer.as_deref(), Some("image"));
        assert_eq!(contract.inputs[0].viewer, None);
        assert_eq!(
            problems.len(),
            1,
            "the missing output is skipped: {problems:?}"
        );
    }
}
