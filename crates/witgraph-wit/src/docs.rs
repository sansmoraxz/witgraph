//! What a component says of itself: the description an editor shows beside
//! it, as the component declares it in a `witgraph-docs` custom section.
//!
//! The section is UTF-8 text, the description whole. A component's bytes
//! keep no doc comments, so a component that wants a description carries
//! one. Several sections of the same name concatenated (which is what the
//! linker does with them) read as one.
//!
//! A guest written in Rust embeds the section with a static; it ends up in
//! the component's core module:
//!
//! ```ignore
//! #[used]
//! #[unsafe(link_section = "witgraph-docs")]
//! static DOCS: [u8; 25] = *b"Doubles what it is given.";
//! ```
//!
//! A description is only that: one that cannot be read is skipped, and
//! never stops a component from loading. A world's doc comment in the WIT a
//! contract was lowered from wins over the section.

/// The custom section's name.
pub const SECTION: &str = "witgraph-docs";

/// The description of every `witgraph-docs` section of `bytes` (none when
/// there are none, or they are blank), and what was wrong with the rest.
///
/// Only the component's own sections count, as for
/// [`viewer::read`](crate::viewer::read).
pub fn read(bytes: &[u8]) -> (Option<String>, Vec<String>) {
    let (texts, problems) = crate::section::texts(bytes, SECTION);
    let text = texts.concat();
    let text = text.trim();
    ((!text.is_empty()).then(|| text.to_owned()), problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_is_the_text_of_the_section() {
        let read = |wat: &str| read(&wat::parse_str(wat).unwrap());
        assert_eq!(
            read(r#"(component (@custom "witgraph-docs" "Doubles it.\nTwice over.\n"))"#),
            (Some("Doubles it.\nTwice over.".into()), Vec::new())
        );
        // Of a module in it, and concatenated.
        assert_eq!(
            read(
                r#"(component
                    (core module (@custom "witgraph-docs" "One, "))
                    (@custom "witgraph-docs" "and two."))"#
            ),
            (Some("One, and two.".into()), Vec::new())
        );
        // None: no section, a blank one, or a nested component's.
        for wat in [
            "(component)",
            r#"(component (@custom "witgraph-docs" " \n"))"#,
            r#"(component (component (@custom "witgraph-docs" "Not mine.")))"#,
        ] {
            assert_eq!(read(wat), (None, Vec::new()), "{wat}");
        }
    }

    #[test]
    fn a_section_that_is_not_text_is_a_problem_not_a_description() {
        let mut bytes = wat::parse_str("(component)").unwrap();
        // A custom section named `witgraph-docs` holding 0xff 0xfe.
        bytes.extend_from_slice(&[0, 16, 13]);
        bytes.extend_from_slice(SECTION.as_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let (docs, problems) = read(&bytes);
        assert_eq!(docs, None);
        assert!(problems[0].contains("UTF-8"), "{problems:?}");
    }
}
