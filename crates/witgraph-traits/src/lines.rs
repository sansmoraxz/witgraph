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

/// Which of a guest's source lines count as its own, to stop and step on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OwnLines {
    /// All but what looks like the Rust standard library's or a dependency's, by its path.
    /// Another language's libraries look like the guest's own to this: say where it is instead.
    #[default]
    Guess,
    /// Only the files whose path starts with one of these.
    Under(Vec<String>),
    /// Every line the debug info has, at about twice the cost.
    All,
}

impl OwnLines {
    /// Whether `file` counts, by the path the debug info gives it.
    #[must_use]
    pub fn covers(&self, file: &str) -> bool {
        /// Paths a guest's own code does not live under.
        const NOT_OWN: [&str; 6] = [
            "/rustc/",
            "/rustlib/",
            "/rust/deps/",
            "/.cargo/",
            "/cargo/registry/",
            "/cargo/git/",
        ];
        match self {
            Self::Guess => !NOT_OWN.iter().any(|path| file.contains(path)),
            Self::Under(paths) => paths.iter().any(|path| file.starts_with(path.as_str())),
            Self::All => true,
        }
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
