use super::node::{Node, NodeId, Nodes};
use mchprs_blocks::BlockPos;
use mchprs_probes::{Edge, ProbeHit, ProbeSource, ProbeSpec};
use rustc_hash::FxHashMap;
use tracing::warn;

#[derive(Clone, Copy)]
enum Source {
    Node(NodeId),
    Const(bool),
}

struct Trigger {
    node: NodeId,
    last: bool,
    changed: bool,
}

struct Probe {
    trigger: usize,
    edge: Edge,
    enable: Source,
    data: Vec<Source>,
}

/// Samples probes from node state. Triggers are shared because most probes are on the same clock,
/// which keeps the check that runs every tick small.
#[derive(Default)]
pub struct Probes {
    triggers: Vec<Trigger>,
    probes: Vec<Probe>,
    pub hits: Vec<ProbeHit>,
}

/// Lamps and trapdoors only set `powered`, and wires and constants only set `output_power`
fn signal(node: &Node) -> bool {
    node.powered || node.output_power > 0
}

impl Probes {
    /// A probe with a position that has no node reads it as low. Its trigger never fires.
    pub fn new(
        specs: &[ProbeSpec],
        pos_map: &FxHashMap<BlockPos, NodeId>,
        nodes: &Nodes,
    ) -> Probes {
        let lookup = |pos: BlockPos| {
            let node = pos_map.get(&pos).copied();
            if node.is_none() {
                warn!("Probe position {} has no redpiler node", pos);
            }
            node
        };
        let source = |source: &ProbeSource| match source {
            ProbeSource::Pos(pos) => lookup(*pos).map_or(Source::Const(false), Source::Node),
            ProbeSource::Const(value) => Source::Const(*value),
        };

        let mut result = Probes::default();
        for spec in specs {
            let Some(node) = lookup(spec.trigger) else {
                // Hits are numbered by position in `specs`, so it still needs an entry
                result.probes.push(Probe {
                    trigger: usize::MAX,
                    edge: spec.edge,
                    enable: Source::Const(false),
                    data: Vec::new(),
                });
                continue;
            };
            let trigger = match result.triggers.iter().position(|t| t.node == node) {
                Some(trigger) => trigger,
                None => {
                    result.triggers.push(Trigger {
                        node,
                        last: signal(&nodes[node]),
                        changed: false,
                    });
                    result.triggers.len() - 1
                }
            };
            result.probes.push(Probe {
                trigger,
                edge: spec.edge,
                enable: source(&spec.enable),
                data: spec.data.iter().map(source).collect(),
            });
        }
        result
    }

    /// Runs at the end of every tick and after every input change. Nothing downstream of a trigger
    /// with a delay has reacted to it yet, so values are from before the edge.
    #[inline]
    pub fn poll(&mut self, nodes: &Nodes) {
        let mut any_changed = false;
        for trigger in &mut self.triggers {
            let now = signal(&nodes[trigger.node]);
            trigger.changed = now != trigger.last;
            trigger.last = now;
            any_changed |= trigger.changed;
        }
        if any_changed {
            self.fire(nodes);
        }
    }

    #[inline(never)]
    fn fire(&mut self, nodes: &Nodes) {
        let sample = |source: &Source| match source {
            Source::Node(node) => signal(&nodes[*node]),
            Source::Const(value) => *value,
        };
        for (i, probe) in self.probes.iter().enumerate() {
            let Some(trigger) = self.triggers.get(probe.trigger) else {
                continue;
            };
            let fired = trigger.changed
                && match probe.edge {
                    Edge::Rising => trigger.last,
                    Edge::Falling => !trigger.last,
                };
            if fired && sample(&probe.enable) {
                self.hits.push(ProbeHit {
                    probe: i as u32,
                    data: probe.data.iter().map(sample).collect(),
                });
            }
        }
    }
}
