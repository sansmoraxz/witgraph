//! What the scheduler itself costs per `run`: a chain of nodes whose `run`
//! only adds, on an executor with no engine, so nothing but scheduling is
//! measured.
//!
//! ```text
//! cargo run --release -p witgraph-sched --example tick_overhead [nodes] [ticks]
//! ```

use std::collections::HashSet;
use std::error::Error;
use std::time::Instant;

use futures::FutureExt;
use futures::executor::block_on;
use witgraph_ir::{
    ComponentContract, ComponentRef, Graph, PortDef, PortDirection, PortKind, PortName, PortRef,
    Type,
};
use witgraph_sched::plan::RunShape;
use witgraph_sched::{
    Canonical, Executor, Generation, GenerationFuture, NodeCaller, NodeFault, OptionPayload, Perf,
    Scheduler, drive,
};

/// Every node is `out = in + add + 1`.
struct Adders {
    ports: HashSet<(PortRef, PortDirection)>,
}

struct Members<'a>(&'a Generation<u32>);

impl NodeCaller<u32> for Members<'_> {
    type Error = String;

    async fn call(
        &self,
        member: usize,
        args: Vec<Option<u32>>,
    ) -> Result<Vec<(String, u32)>, String> {
        let sum = args.iter().flatten().sum::<u32>() + 1;
        let plan = &self.0.plan.members[member];
        Ok(plan
            .outputs
            .keys()
            .map(|port| (port.to_string(), sum))
            .collect())
    }

    fn close(&self, _: u32) -> Result<(), String> {
        Ok(())
    }

    fn missing_input(&self, _: usize, field: &PortName) -> String {
        format!("no value for `{field}`")
    }
}

impl Executor for Adders {
    type Value = u32;
    type Type = ();
    type Island = ();

    fn run(&self, (): (), mut generation: Generation<u32>) -> GenerationFuture<()> {
        async move {
            let external = std::mem::take(&mut generation.external);
            let members = Members(&generation);
            let send = |kind| generation.send(kind);
            drive(
                &members,
                &generation.plan,
                external,
                Self::option_payload,
                &send,
            )
            .await
            .map_err(|message| (NodeFault::WasmTrap { message }, None))
        }
        .boxed()
    }

    fn rebuild_and_run(&self, generation: Generation<u32>) -> GenerationFuture<()> {
        self.run((), generation)
    }

    fn option_payload(_: &u32) -> OptionPayload<'_, u32> {
        OptionPayload::NotOption
    }

    fn port_type(&self, port: &PortRef, direction: PortDirection) -> Option<&()> {
        self.ports
            .contains(&(port.clone(), direction))
            .then_some(&())
    }

    fn check_input(&self, (): &(), value: u32) -> Result<Canonical<u32>, String> {
        Ok(Canonical::assume(value))
    }

    fn parse_wave(&self, (): &(), text: &str) -> Result<Canonical<u32>, String> {
        text.parse()
            .map(Canonical::assume)
            .map_err(|e| format!("{e}"))
    }

    fn to_wave(&self, value: &u32) -> Result<String, String> {
        Ok(value.to_string())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let nodes: usize = args.next().map_or(Ok(32), |a| a.parse())?;
    let ticks: u32 = args.next().map_or(Ok(20_000), |a| a.parse())?;

    let contract = ComponentContract {
        id: "test:nodes/adder@0.1.0".parse::<ComponentRef>()?,
        inputs: vec![
            PortDef::new("in", PortKind::Value, Type::U32).optional(),
            PortDef::new("add", PortKind::Value, Type::U32).optional(),
        ],
        outputs: vec![PortDef::new("out", PortKind::Value, Type::U32)],
        capabilities: vec![],
        docs: None,
    };
    let name = |i: usize| format!("n{i}");
    let mut builder = Graph::builder("chain").add_component(&contract);
    for i in 0..nodes {
        builder = builder.add_node(name(i), contract.id.clone());
    }
    for i in 1..nodes {
        builder = builder.connect(
            format!("c{i}"),
            PortRef::new(name(i - 1), "out"),
            PortRef::new(name(i), "in"),
        );
    }
    let compiled = builder
        .build()
        .compile(std::slice::from_ref(&contract))
        .map_err(|e| format!("{e:?}"))?;

    let mut ports = HashSet::new();
    for i in 0..nodes {
        for input in ["in", "add"] {
            ports.insert((PortRef::new(name(i), input), PortDirection::Input));
        }
        ports.insert((PortRef::new(name(i), "out"), PortDirection::Output));
    }
    let shape = RunShape {
        inputs: Some(vec!["in".into(), "add".into()]),
        has_result: true,
    };
    let islands = compiled
        .islands()
        .iter()
        .map(|members| ((), vec![shape.clone(); members.len()]))
        .collect();
    let mut sched = Scheduler::new(compiled, Perf, Adders { ports }, islands, 100_000)?;
    let (first, add) = (name(0).into(), "add".into());

    let start = Instant::now();
    for value in 0..ticks {
        sched.inject(&first, &add, value)?;
        let _ = block_on(sched.tick());
    }
    let elapsed = start.elapsed();
    let runs = u128::from(ticks) * nodes as u128;
    println!(
        "{nodes} nodes, {ticks} ticks: {:.0} ns per run, {:.1} us per tick",
        elapsed.as_nanos() as f64 / runs as f64,
        elapsed.as_secs_f64() * 1e6 / f64::from(ticks),
    );
    Ok(())
}
