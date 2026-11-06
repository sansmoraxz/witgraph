//! Stopping a guest on a line of its source: the same on every host.

use std::future::Future;

use witgraph_ir::NodeId;

use crate::executor::MaybeSend;

/// A variable, or part of one, of a guest stopped on a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceVariable {
    /// Its name: a field's, or an item's index.
    pub name: String,
    /// Its type, as the source has it.
    pub type_name: String,
    /// What it holds, on one line.
    pub value: String,
    /// What it is made of, to look into.
    pub children: Vec<SourceVariable>,
}

/// Which of a guest's source lines count as its own, to stop and step on: what the
/// debugger probes, lists and steps through. The rest runs as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnLines {
    /// Every line the debug info has, the standard library's and dependencies' too, at about
    /// twice the cost.
    All,
    /// The lines of the files the rules say.
    Rules(OwnRules),
}

impl Default for OwnLines {
    /// A guess, for any language: see [`OwnRules::default`].
    fn default() -> Self {
        Self::Rules(OwnRules::default())
    }
}

impl OwnLines {
    /// Only the files whose path starts with one of `paths`.
    #[must_use]
    pub fn under(paths: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Rules(OwnRules {
            include: paths
                .into_iter()
                .map(|path| PathPattern::new(format!("{}**", path.into())))
                .collect(),
            exclude: vec![PathPattern::new("**")],
            anchor: false,
            known: false,
        })
    }

    /// Whether `file` counts.
    #[must_use]
    pub fn covers(&self, file: &SourceFile<'_>) -> bool {
        match self {
            Self::All => true,
            Self::Rules(rules) => rules.covers(file),
        }
    }
}

/// Which files are a guest's own, the first that says deciding: what [`include`] matches is,
/// what [`exclude`] matches is not, a known library of the file's language is not, and with
/// [`anchor`], it is if it is under the guest's project, and is not otherwise. What none of
/// them says anything of is.
///
/// [`include`]: Self::include
/// [`exclude`]: Self::exclude
/// [`anchor`]: Self::anchor
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnRules {
    /// The files that are the guest's own, whatever else says.
    pub include: Vec<PathPattern>,
    /// The files that are not, unless included.
    pub exclude: Vec<PathPattern>,
    /// Whether only the files under the guest's project are its own: the directory its `run`
    /// was compiled in, as the debug info has it, and that of the file it is in. A dependency
    /// is compiled where it is, the standard library where its toolchain was built, so neither
    /// is under it, whatever the language.
    pub anchor: bool,
    /// Whether the libraries witgraph knows of, of the file's language, are left out by their
    /// paths: Rust's standard library and Cargo's dependencies, and clang's and wasi-sdk's
    /// headers. What a guest with no `run` to anchor to falls back on.
    pub known: bool,
}

impl Default for OwnRules {
    /// The guest's project, less the libraries witgraph knows of: no patterns.
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            anchor: true,
            known: true,
        }
    }
}

impl OwnRules {
    /// Whether `file` counts.
    #[must_use]
    pub fn covers(&self, file: &SourceFile<'_>) -> bool {
        if self
            .include
            .iter()
            .any(|pattern| pattern.matches(file.path))
        {
            return true;
        }
        if self
            .exclude
            .iter()
            .any(|pattern| pattern.matches(file.path))
        {
            return false;
        }
        if self.known && known_library(file) {
            return false;
        }
        match (self.anchor, file.root) {
            (true, Some(root)) => under(file.path, root),
            _ => true,
        }
    }
}

/// A source file a guest's debug info names, and what it says of where it is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFile<'a> {
    /// Its path.
    pub path: &'a str,
    /// The language of the code that has its lines, if the debug info says one witgraph knows.
    pub language: Option<SourceLanguage>,
    /// The directory of the guest's project, if it has a `run` compiled with debug info.
    pub root: Option<&'a str>,
}

/// A language of a guest's code, as its debug info names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceLanguage {
    /// Rust.
    Rust,
    /// C, of any standard.
    C,
    /// C++, of any standard.
    Cpp,
}

/// Where the libraries witgraph knows of are, by a part of their path.
fn known_paths(language: SourceLanguage) -> &'static [&'static str] {
    match language {
        // `/rust/deps/` is where the standard library's own dependencies were built.
        SourceLanguage::Rust => &[
            "/rustc/",
            "/rustlib/",
            "/rust/deps/",
            "/.cargo/",
            "/cargo/registry/",
            "/cargo/git/",
        ],
        // Clang's own headers, and those wasi-sdk installs.
        SourceLanguage::C | SourceLanguage::Cpp => {
            &["/lib/clang/", "/share/wasi-sysroot/", "/include/c++/"]
        }
    }
}

/// Whether `file` is in a library witgraph knows of: of any language, if it does not say one.
fn known_library(file: &SourceFile<'_>) -> bool {
    let languages: &[SourceLanguage] = match &file.language {
        Some(language) => std::slice::from_ref(language),
        None => &[SourceLanguage::Rust, SourceLanguage::C],
    };
    languages
        .iter()
        .flat_map(|language| known_paths(*language))
        .any(|part| file.path.contains(part))
}

/// Whether `path` is in the directory `root`, or under it.
fn under(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// A pattern of paths: `**` stands for anything, `*` for anything but a `/`, `?` for one
/// character but a `/`. It matches a whole path, so `**/bindings/**` matches a file in a
/// `bindings` directory anywhere, and `/home/me/project/**` one under that directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern(String);

impl PathPattern {
    /// A pattern, as written.
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    /// The pattern, as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `path` matches it, all of it.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let pattern: Vec<char> = self.0.chars().collect();
        let path: Vec<char> = path.chars().collect();
        // `matched[at]`: whether the pattern so far matches `path[..at]`.
        let mut matched = vec![false; path.len() + 1];
        matched[0] = true;
        let mut at = 0;
        while at < pattern.len() {
            let mut next = vec![false; path.len() + 1];
            if pattern[at] == '*' && pattern.get(at + 1) == Some(&'*') {
                let mut any = false;
                for end in 0..=path.len() {
                    any |= matched[end];
                    next[end] = any;
                }
                at += 2;
            } else if pattern[at] == '*' {
                for end in 0..=path.len() {
                    next[end] = matched[end] || (end > 0 && next[end - 1] && path[end - 1] != '/');
                }
                at += 1;
            } else {
                for end in 1..=path.len() {
                    let fits = pattern[at] == path[end - 1]
                        || (pattern[at] == '?' && path[end - 1] != '/');
                    next[end] = matched[end - 1] && fits;
                }
                at += 1;
            }
            matched = next;
        }
        matched[path.len()]
    }
}

impl From<&str> for PathPattern {
    fn from(pattern: &str) -> Self {
        Self::new(pattern)
    }
}

impl From<String> for PathPattern {
    fn from(pattern: String) -> Self {
        Self::new(pattern)
    }
}

/// A function on a stopped guest's stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// Its name, as the module has it.
    pub function: String,
    /// The source file it is at, if it has lines that count as the guest's own.
    pub file: Option<String>,
    /// The line it is at: the call it is waiting on, unless it is the innermost.
    pub line: Option<u32>,
}

/// What stopped a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// A breakpoint on the line.
    Breakpoint,
    /// [`LineDebugger::pause`].
    Pause,
    /// A step came to the line.
    Step,
}

/// Where a guest has stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineStop {
    /// What stopped it.
    pub reason: StopReason,
    /// The source file, as the debug info names it.
    pub file: String,
    /// The line, from 1.
    pub line: u32,
    /// The column, from 1, or 0 for no particular one.
    pub column: u32,
    /// The node whose guest it is, if it is a node's.
    pub node: Option<NodeId>,
    /// The functions on its stack, innermost first.
    pub frames: Vec<StackFrame>,
    /// The variables in scope where it stopped, innermost scope first, of the innermost
    /// function only.
    pub variables: Vec<SourceVariable>,
}

/// Sets breakpoints in a graph's guests, and steps through them. Only guests built with debug
/// info have lines, and only their own: not the standard library's, or a dependency's.
///
/// While a guest is stopped all of the graph is, and what is asked of the graph waits until it
/// goes on. What is called here does not wait: a host calls it from outside the tick, and it
/// takes effect even on a guest in a loop that never returns.
pub trait LineDebugger {
    /// The source files with lines to stop on, as the debug info names them.
    fn files(&self) -> Vec<String>;

    /// Of [`files`](Self::files), those of `node`'s guest: of the core modules it runs, those
    /// of the links composed into it as well. Two nodes of one component have the same.
    fn files_of(&self, node: &NodeId) -> Vec<String>;

    /// The lines of `file` that have code, in order.
    fn lines(&self, file: &str) -> Vec<u32>;

    /// Where a guest is stopped, if one is.
    fn stopped(&self) -> Option<LineStop>;

    /// The next stop not yet told of.
    fn next_stop(&self) -> impl Future<Output = LineStop> + MaybeSend;

    /// Stops every guest on `line` of `file`, or the next line with code. Returns the line it
    /// came to, or `None` if there is none.
    fn set_breakpoint(&self, file: &str, line: u32) -> Option<u32>;

    /// Clears what [`set_breakpoint`](Self::set_breakpoint) set for this line.
    fn clear_breakpoint(&self, file: &str, line: u32);

    /// Stops at the next line any guest reaches.
    fn pause(&self);

    /// Takes back a [`pause`](Self::pause) no guest has reached yet.
    fn cancel_pause(&self);

    /// Goes on, to the next breakpoint.
    fn resume(&self);

    /// Goes on to another line, in a call this one makes if it makes one.
    fn step_in(&self);

    /// Goes on to another line of this function, or of its caller if it returns.
    fn step_over(&self);

    /// Goes on until this function has returned to one with a line to stop on.
    fn step_out(&self);

    /// Lets go of everything: the guests run on with nothing to stop them.
    fn release(&self);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn rust<'a>(path: &'a str, root: Option<&'a str>) -> SourceFile<'a> {
        SourceFile {
            path,
            language: Some(SourceLanguage::Rust),
            root,
        }
    }

    #[test]
    fn patterns_match_whole_paths() {
        let cases = [
            ("**", "/a/b/c.rs", true),
            ("/a/**", "/a/b/c.rs", true),
            ("/a/**", "/ab/c.rs", false),
            ("**/bindings/**", "/p/x/bindings/world.c", true),
            ("**/bindings/**", "/p/x/bindings.c", false),
            ("/p/*.c", "/p/walk.c", true),
            ("/p/*.c", "/p/src/walk.c", false),
            ("/p/**.c", "/p/src/walk.c", true),
            ("/p/wal?.c", "/p/walk.c", true),
            ("/p/wal?.c", "/p/wal/.c", false),
            ("/p/walk.c", "/p/walk.c", true),
            ("/p/walk.c", "/p/walk.cc", false),
            ("", "", true),
            ("*", "", true),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(
                PathPattern::new(pattern).matches(path),
                expected,
                "{pattern} {path}"
            );
        }
    }

    #[test]
    fn the_guess_is_the_project_less_known_libraries() {
        let own = OwnLines::default();
        let root = Some("/home/me/node");
        assert!(own.covers(&rust("/home/me/node/src/lib.rs", root)));
        assert!(
            !own.covers(&rust("/home/me/other/src/lib.rs", root)),
            "outside it"
        );
        assert!(
            !own.covers(&rust("/home/me/node-tools/lib.rs", root)),
            "a sibling, not under it"
        );
        assert!(!own.covers(&rust("/rustc/abc/library/core/src/option.rs", None)));
        assert!(
            own.covers(&rust("/home/me/other/src/lib.rs", None)),
            "with no anchor, what is not known is its own"
        );
        let c_header = SourceFile {
            path: "/usr/lib/clang/23/include/wasm_simd128.h",
            language: Some(SourceLanguage::C),
            root: None,
        };
        assert!(!own.covers(&c_header));
        assert!(
            own.covers(&SourceFile {
                language: Some(SourceLanguage::Rust),
                ..c_header
            }),
            "known by the language of its code"
        );
    }

    #[test]
    fn include_wins_then_exclude_then_the_guess() {
        let rules = OwnRules {
            include: vec!["/home/me/vendor/**".into()],
            exclude: vec!["**/bindings/**".into(), "/home/me/node/gen/**".into()],
            ..OwnRules::default()
        };
        let own = OwnLines::Rules(rules);
        let root = Some("/home/me/node");
        assert!(
            own.covers(&rust("/home/me/vendor/lib.rs", root)),
            "outside, but included"
        );
        assert!(!own.covers(&rust("/home/me/node/bindings/world.c", root)));
        assert!(!own.covers(&rust("/home/me/node/gen/x.rs", root)));
        assert!(own.covers(&rust("/home/me/node/src/lib.rs", root)));
    }

    #[test]
    fn under_is_only_those_and_all_is_everything() {
        let own = OwnLines::under(["/home/me/node/"]);
        assert!(own.covers(&rust("/home/me/node/src/lib.rs", None)));
        assert!(!own.covers(&rust("/home/me/other.rs", Some("/home/me"))));
        assert!(OwnLines::All.covers(&rust("/rustc/abc/library/core/src/option.rs", None)));
    }
}
