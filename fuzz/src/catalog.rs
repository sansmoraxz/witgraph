//! The components generated graphs are made of, described twice: as contracts for the
//! compiler, and in the plain terms the rules are written in, so that the rules do not
//! lean on the compiler's own idea of what fits what.

use witgraph_ir::{Carried, ComponentContract, ContractId, HandleType, PortDef, Type};

/// A port's kind, as the rules see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Latched; many may read it.
    Value,
    /// A resource handle, latched.
    Handle,
    /// Moves to one reader.
    Stream,
    /// Moves to one reader.
    Future,
}

impl Kind {
    /// Whether it moves to one reader, and joins its ends in a stream group.
    pub fn moves(self) -> bool {
        matches!(self, Self::Stream | Self::Future)
    }
}

/// What a port carries: what fits is equal payloads, or `OptionU32` into an optional
/// `U32`. A handle's semver track is part of its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// `u32`.
    U32,
    /// `string`.
    Text,
    /// `option<u32>`.
    OptionU32,
    /// `tensor`, of `test:data/tensor` at the given major version.
    Tensor(u32, u32),
    /// `blob`, of `test:data/blob`.
    Blob,
}

/// A port, as the rules see it.
#[derive(Debug, Clone, Copy)]
pub struct Port {
    /// Its name.
    pub name: &'static str,
    /// Its kind.
    pub kind: Kind,
    /// What it carries.
    pub payload: Payload,
    /// Whether it may stay unconnected.
    pub optional: bool,
}

const fn port(name: &'static str, kind: Kind, payload: Payload) -> Port {
    Port {
        name,
        kind,
        payload,
        optional: false,
    }
}

const fn optional(name: &'static str, kind: Kind, payload: Payload) -> Port {
    Port {
        name,
        kind,
        payload,
        optional: true,
    }
}

/// A component, as the rules see it.
#[derive(Debug, Clone, Copy)]
pub struct Component {
    /// Its world.
    pub world: &'static str,
    /// Its inputs.
    pub inputs: &'static [Port],
    /// Its outputs.
    pub outputs: &'static [Port],
}

use Kind::{Future, Handle, Stream, Value};
use Payload::{Blob, OptionU32, Tensor, Text, U32};

/// Every component a generated graph can use.
pub const COMPONENTS: &[Component] = &[
    Component {
        world: "source",
        inputs: &[],
        outputs: &[port("out", Value, U32)],
    },
    Component {
        world: "relay",
        inputs: &[port("in", Value, U32), optional("add", Value, U32)],
        outputs: &[port("out", Value, U32)],
    },
    Component {
        world: "gate",
        inputs: &[optional("in", Value, U32)],
        outputs: &[port("out", Value, OptionU32)],
    },
    Component {
        world: "text",
        inputs: &[port("in", Value, Text)],
        outputs: &[port("out", Value, Text)],
    },
    Component {
        world: "both",
        inputs: &[port("a", Value, U32), optional("b", Value, Text)],
        outputs: &[port("x", Value, U32), port("y", Value, Text)],
    },
    Component {
        world: "stream-source",
        inputs: &[optional("burst", Value, U32)],
        outputs: &[port("items", Stream, U32)],
    },
    Component {
        world: "stream-tee",
        inputs: &[optional("in", Value, U32)],
        outputs: &[port("items", Stream, U32), port("out", Value, U32)],
    },
    Component {
        world: "stream-pass",
        inputs: &[port("in", Stream, U32)],
        outputs: &[port("out", Stream, U32)],
    },
    Component {
        world: "stream-sink",
        inputs: &[port("items", Stream, U32), optional("gate", Value, U32)],
        outputs: &[port("sum", Value, U32)],
    },
    Component {
        world: "stream-waits",
        inputs: &[port("items", Stream, U32), port("gate", Value, U32)],
        outputs: &[port("sum", Value, U32)],
    },
    Component {
        world: "text-stream-source",
        inputs: &[],
        outputs: &[port("items", Stream, Text)],
    },
    Component {
        world: "future-source",
        inputs: &[],
        outputs: &[port("done", Future, U32)],
    },
    Component {
        world: "future-sink",
        inputs: &[port("done", Future, U32)],
        outputs: &[port("value", Value, U32)],
    },
    Component {
        world: "tensor-source",
        inputs: &[],
        outputs: &[port("tensor", Handle, Tensor(1, 0))],
    },
    Component {
        world: "tensor-pass",
        inputs: &[port("tensor", Handle, Tensor(1, 3))],
        outputs: &[port("tensor", Handle, Tensor(1, 3))],
    },
    Component {
        world: "tensor-reader",
        inputs: &[optional("tensor", Handle, Tensor(1, 0))],
        outputs: &[port("sum", Value, U32)],
    },
    Component {
        world: "tensor-two",
        inputs: &[port("tensor", Handle, Tensor(2, 0))],
        outputs: &[port("tensor", Handle, Tensor(2, 0))],
    },
    Component {
        world: "blob-source",
        inputs: &[],
        outputs: &[port("tensor", Handle, Blob)],
    },
];

impl Component {
    /// What a graph calls it.
    pub fn reference(&self) -> ContractId {
        format!("{0}/fuzz:nodes/{0}@0.1.0", self.world)
            .parse()
            .expect("a catalog world is a valid reference")
    }

    /// Its contract.
    pub fn contract(&self) -> ComponentContract {
        ComponentContract {
            id: self.reference(),
            inputs: self.inputs.iter().map(Port::def).collect(),
            outputs: self.outputs.iter().map(Port::def).collect(),
            capabilities: vec![],
            docs: None,
        }
    }

    /// The input of that name.
    pub fn input(&self, name: &str) -> Option<&'static Port> {
        self.inputs.iter().find(|port| port.name == name)
    }

    /// The output of that name.
    pub fn output(&self, name: &str) -> Option<&'static Port> {
        self.outputs.iter().find(|port| port.name == name)
    }
}

impl Payload {
    fn ty(self) -> Type {
        match self {
            U32 => Type::U32,
            Text => Type::STRING,
            OptionU32 => Type::option(Type::U32),
            Tensor(..) | Blob => Type::U32,
        }
    }

    fn handle(self) -> HandleType {
        match self {
            Tensor(major, minor) => HandleType {
                interface: format!("test:data/tensor@{major}.{minor}.0"),
                resource: "tensor".into(),
            },
            _ => HandleType {
                interface: "test:data/blob@1.0.0".into(),
                resource: "blob".into(),
            },
        }
    }

    /// Whether what carries this may feed `input`, by the rules.
    pub fn fits(self, input: &Port) -> bool {
        match (self, input.payload) {
            (Tensor(a, _), Tensor(b, _)) => a == b,
            (from, to) if from == to => true,
            (OptionU32, U32) => input.optional,
            _ => false,
        }
    }
}

impl Port {
    fn def(&self) -> PortDef {
        let def = match self.kind {
            Value => PortDef::value(self.name, self.payload.ty()),
            Handle => PortDef::handle(self.name, self.payload.handle()),
            Stream => PortDef::stream(self.name, Carried::Data(self.payload.ty())),
            Future => PortDef::future(self.name, Carried::Data(self.payload.ty())),
        };
        if self.optional { def.optional() } else { def }
    }
}
