//! The file a build writes next to its schematic for the server to use. Probes print sampled
//! signals when a trigger block changes, like Verilog's `$write`, and the clock toggles a lever
//! on a fixed period. See the headless crate's README for the file format.

use anyhow::{bail, ensure, Context, Result};
use mchprs_blocks::blocks::Block;
use mchprs_blocks::BlockPos;
use mchprs_world::World;
use serde::Deserialize;

const VERSION: u32 = 2;
/// Fields are read into a `u128`
const MAX_FIELD_BITS: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFile {
    version: u32,
    schematic_size: [u32; 3],
    probes: Vec<Probe>,
    clock: Option<Clock>,
    #[serde(default)]
    levers: Vec<Lever>,
}

/// An input pin that a player can set by name with `/lever`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lever {
    name: String,
    pos: [i32; 3],
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Clock {
    /// Only used in messages
    pin: String,
    pos: [i32; 3],
    /// In plot ticks, where a 1-delay repeater takes one tick
    half_period_ticks: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Probe {
    name: String,
    format: Vec<Token>,
    trigger: Trigger,
    enable: Source,
    data: Vec<Source>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trigger {
    pos: [i32; 3],
    edge: Edge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Rising,
    Falling,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Source {
    Pos { pos: [i32; 3] },
    Const { r#const: u8 },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Token {
    Text { text: String },
    Field(Field),
}

#[derive(Debug, Deserialize)]
struct Field {
    /// Indices into `Probe::data`, LSB first
    bits: Vec<usize>,
    radix: Radix,
    #[serde(default)]
    signed: bool,
    /// The minimum width in characters. 0 means no padding.
    #[serde(default)]
    width: usize,
    #[serde(default = "default_pad")]
    pad: char,
    #[serde(default)]
    justify: Justify,
}

fn default_pad() -> char {
    ' '
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
enum Justify {
    #[default]
    #[serde(rename = ">")]
    Right,
    #[serde(rename = "<")]
    Left,
    /// Padding goes between a number's sign and its digits
    #[serde(rename = "=")]
    AfterSign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
enum Radix {
    #[serde(rename = "d")]
    Decimal,
    #[serde(rename = "x")]
    Hex,
    #[serde(rename = "o")]
    Octal,
    #[serde(rename = "b")]
    Binary,
    #[serde(rename = "c")]
    Char,
}

pub fn parse_server_file(text: &str) -> Result<ServerFile> {
    let file: ServerFile = serde_json::from_str(text).context("parsing server file")?;
    ensure!(
        file.version == VERSION,
        "server file version {} is not supported, expected {VERSION}",
        file.version
    );
    if let Some(clock) = &file.clock {
        ensure!(
            clock.half_period_ticks >= 1,
            "clock {}: half_period_ticks must be at least 1",
            clock.pin
        );
    }
    for (i, lever) in file.levers.iter().enumerate() {
        ensure!(
            !file.levers[..i]
                .iter()
                .any(|other| other.name == lever.name),
            "lever {} is listed twice",
            lever.name
        );
    }
    for probe in &file.probes {
        probe
            .check_format()
            .with_context(|| format!("probe {}", probe.name))?;
    }
    Ok(file)
}

impl Probe {
    fn check_format(&self) -> Result<()> {
        for source in self.sources() {
            if let Source::Const { r#const } = source {
                ensure!(*r#const <= 1, "const must be 0 or 1, got {}", r#const);
            }
        }
        for token in &self.format {
            if let Token::Field(Field { bits, .. }) = token {
                ensure!(!bits.is_empty(), "format field has no bits");
                ensure!(
                    bits.len() <= MAX_FIELD_BITS,
                    "format field has {} bits, the limit is {MAX_FIELD_BITS}",
                    bits.len()
                );
                if let Some(bit) = bits.iter().find(|&&bit| bit >= self.data.len()) {
                    bail!("format bit {bit} is outside data of {}", self.data.len());
                }
            }
        }
        Ok(())
    }

    fn sources(&self) -> impl Iterator<Item = &Source> {
        std::iter::once(&self.enable).chain(&self.data)
    }

    fn positions(&self) -> impl Iterator<Item = [i32; 3]> + '_ {
        let sources = self.sources().filter_map(|source| match source {
            Source::Pos { pos } => Some(*pos),
            Source::Const { .. } => None,
        });
        std::iter::once(self.trigger.pos).chain(sources)
    }
}

/// The signal a block outputs, or `None` if it isn't a block that probes can read
pub fn read_signal(block: Block) -> Option<bool> {
    Some(match block {
        Block::Repeater(repeater) => repeater.powered,
        Block::Comparator(comparator) => comparator.powered,
        Block::RedstoneWire(wire) => wire.power > 0,
        Block::RedstoneTorch { lit }
        | Block::RedstoneWallTorch { lit, .. }
        | Block::RedstoneLamp { lit } => lit,
        Block::Lever { powered, .. } => powered,
        _ => return None,
    })
}

/// A probe with world positions, which is what redpiler and [`WorldPoller`] run
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeSpec {
    pub trigger: BlockPos,
    pub edge: Edge,
    pub enable: ProbeSource,
    pub data: Vec<ProbeSource>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeSource {
    Pos(BlockPos),
    Const(bool),
}

/// A probe that fired, with its data captured at that moment
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeHit {
    pub probe: u32,
    pub data: Vec<bool>,
}

fn world_pos(origin: BlockPos, [x, y, z]: [i32; 3]) -> BlockPos {
    origin + BlockPos::new(x, y, z)
}

fn inside(pos: [i32; 3], size: [u32; 3]) -> bool {
    pos.iter().zip(size).all(|(&n, s)| n >= 0 && (n as u32) < s)
}

/// Counts ticks and says when to toggle the clock's lever
#[derive(Clone, Debug)]
pub struct ServerClock {
    pos: BlockPos,
    half_period_ticks: u32,
    ticks_until_toggle: u32,
}

impl ServerClock {
    pub fn pos(&self) -> BlockPos {
        self.pos
    }

    pub fn ticks_until_toggle(&self) -> u32 {
        self.ticks_until_toggle
    }

    /// Records `ticks` that have run, which must not be more than [`Self::ticks_until_toggle`].
    /// Returns whether the lever should be toggled now.
    pub fn advance(&mut self, ticks: u32) -> bool {
        self.ticks_until_toggle -= ticks;
        if self.ticks_until_toggle == 0 {
            self.ticks_until_toggle = self.half_period_ticks;
            return true;
        }
        false
    }
}

impl ServerFile {
    pub fn len(&self) -> usize {
        self.probes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    /// Checks the file against the schematic it is being run on, which must already be pasted into
    /// `world` with its minimum corner at `origin`.
    pub fn check(&self, size: [u32; 3], world: &impl World, origin: BlockPos) -> Result<()> {
        ensure!(
            self.schematic_size == size,
            "probes are for a schematic of size {:?}, but this one is {size:?}",
            self.schematic_size
        );
        for probe in &self.probes {
            for [x, y, z] in probe.positions() {
                ensure!(
                    inside([x, y, z], size),
                    "probe {}: {x} {y} {z} is outside the schematic",
                    probe.name
                );
                let block = world.get_block(world_pos(origin, [x, y, z]));
                ensure!(
                    read_signal(block).is_some(),
                    "probe {}: {x} {y} {z} has no signal: {block:?}",
                    probe.name
                );
            }
        }
        if let Some(clock) = &self.clock {
            let [x, y, z] = clock.pos;
            ensure!(
                inside(clock.pos, size),
                "clock {}: {x} {y} {z} is outside the schematic",
                clock.pin
            );
            let block = world.get_block(world_pos(origin, clock.pos));
            ensure!(
                matches!(block, Block::Lever { .. }),
                "clock {}: {x} {y} {z} is not a lever: {block:?}",
                clock.pin
            );
        }
        for lever in &self.levers {
            let [x, y, z] = lever.pos;
            ensure!(
                inside(lever.pos, size),
                "lever {}: {x} {y} {z} is outside the schematic",
                lever.name
            );
            let block = world.get_block(world_pos(origin, lever.pos));
            ensure!(
                matches!(block, Block::Lever { .. }),
                "lever {}: {x} {y} {z} is not a lever: {block:?}",
                lever.name
            );
        }
        Ok(())
    }

    /// The named levers' world positions, in file order
    pub fn levers(&self, origin: BlockPos) -> Vec<(String, BlockPos)> {
        self.levers
            .iter()
            .map(|lever| (lever.name.clone(), world_pos(origin, lever.pos)))
            .collect()
    }

    pub fn specs(&self, origin: BlockPos) -> Vec<ProbeSpec> {
        let source = |source: &Source| match source {
            Source::Pos { pos } => ProbeSource::Pos(world_pos(origin, *pos)),
            Source::Const { r#const } => ProbeSource::Const(*r#const != 0),
        };
        self.probes
            .iter()
            .map(|probe| ProbeSpec {
                trigger: world_pos(origin, probe.trigger.pos),
                edge: probe.trigger.edge,
                enable: source(&probe.enable),
                data: probe.data.iter().map(source).collect(),
            })
            .collect()
    }

    /// The clock counting from now, so its first toggle is `half_period_ticks` away
    pub fn clock(&self, origin: BlockPos) -> Option<ServerClock> {
        self.clock.as_ref().map(|clock| ServerClock {
            pos: world_pos(origin, clock.pos),
            half_period_ticks: clock.half_period_ticks,
            ticks_until_toggle: clock.half_period_ticks,
        })
    }

    /// The text a hit prints. This isn't always UTF-8, because `c` fields write raw bytes.
    pub fn render(&self, hit: &ProbeHit) -> Vec<u8> {
        render(&self.probes[hit.probe as usize].format, &hit.data)
    }
}

/// Watches triggers by reading blocks from the world, for when redpiler isn't compiled
pub struct WorldPoller {
    specs: Vec<ProbeSpec>,
    last_trigger: Vec<bool>,
}

fn read_or_low(world: &impl World, pos: BlockPos) -> bool {
    // The block could have been replaced since the file was checked
    read_signal(world.get_block(pos)).unwrap_or(false)
}

impl WorldPoller {
    /// A trigger that is already at its active level in `world` doesn't fire
    pub fn new(specs: Vec<ProbeSpec>, world: &impl World) -> WorldPoller {
        let last_trigger = specs
            .iter()
            .map(|spec| read_or_low(world, spec.trigger))
            .collect();
        WorldPoller {
            specs,
            last_trigger,
        }
    }

    /// Must run after every change to the world, because a probe samples whatever state it sees
    /// when its trigger changes. Polling any later could show values from after the edge.
    pub fn poll(&mut self, world: &impl World, hits: &mut Vec<ProbeHit>) {
        for (i, (spec, last)) in self.specs.iter().zip(&mut self.last_trigger).enumerate() {
            let now = read_or_low(world, spec.trigger);
            let fired = match spec.edge {
                Edge::Rising => now && !*last,
                Edge::Falling => !now && *last,
            };
            *last = now;
            let sample = |source: &ProbeSource| match source {
                ProbeSource::Pos(pos) => read_or_low(world, *pos),
                ProbeSource::Const(value) => *value,
            };
            if fired && sample(&spec.enable) {
                hits.push(ProbeHit {
                    probe: i as u32,
                    data: spec.data.iter().map(sample).collect(),
                });
            }
        }
    }
}

fn render(format: &[Token], data: &[bool]) -> Vec<u8> {
    let mut out = Vec::new();
    for token in format {
        match token {
            Token::Text { text } => out.extend_from_slice(text.as_bytes()),
            Token::Field(field) => render_field(&mut out, field, data),
        }
    }
    out
}

fn render_field(out: &mut Vec<u8>, field: &Field, data: &[bool]) {
    let Field {
        bits,
        radix,
        signed,
        ..
    } = field;
    let value = bits
        .iter()
        .enumerate()
        .fold(0u128, |value, (i, &bit)| value | (data[bit] as u128) << i);
    let text = match radix {
        Radix::Decimal if *signed && data[bits[bits.len() - 1]] && bits.len() < MAX_FIELD_BITS => {
            ((value | u128::MAX << bits.len()) as i128).to_string()
        }
        Radix::Decimal if *signed => (value as i128).to_string(),
        Radix::Decimal => value.to_string(),
        Radix::Hex => format!("{value:x}"),
        Radix::Octal => format!("{value:o}"),
        Radix::Binary => format!("{value:b}"),
        // Not text, because the byte might not be valid UTF-8
        Radix::Char => String::new(),
    };
    let body: &[u8] = if *radix == Radix::Char {
        &[value as u8]
    } else {
        text.as_bytes()
    };

    let padding = field
        .pad
        .to_string()
        .repeat(field.width.saturating_sub(body.len()));
    let padding = padding.as_bytes();
    match field.justify {
        Justify::Right => {
            out.extend_from_slice(padding);
            out.extend_from_slice(body);
        }
        Justify::Left => {
            out.extend_from_slice(body);
            out.extend_from_slice(padding);
        }
        Justify::AfterSign => {
            let sign_len = text.starts_with('-') as usize;
            out.extend_from_slice(&body[..sign_len]);
            out.extend_from_slice(padding);
            out.extend_from_slice(&body[sign_len..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(radix: &str, signed: bool, data: &[bool]) -> String {
        let bits: Vec<usize> = (0..data.len()).collect();
        let token: Token = serde_json::from_value(serde_json::json!({
            "bits": bits, "radix": radix, "signed": signed
        }))
        .unwrap();
        String::from_utf8(render(&[token], data)).unwrap()
    }

    #[test]
    fn fields_render_lsb_first() {
        let bits = [true, false, true, true];
        assert_eq!(field("d", false, &bits), "13");
        assert_eq!(field("x", false, &bits), "d");
        assert_eq!(field("o", false, &bits), "15");
        assert_eq!(field("b", false, &bits), "1101");
    }

    #[test]
    fn signed_decimal_extends_the_top_bit() {
        assert_eq!(field("d", true, &[true, false, true, true]), "-3");
        assert_eq!(field("d", true, &[true, false, true, false]), "5");
        assert_eq!(field("d", true, &[true; 128]), "-1");
    }

    fn padded(radix: &str, width: usize, pad: &str, justify: &str, data: &[bool]) -> String {
        let bits: Vec<usize> = (0..data.len()).collect();
        let token: Token = serde_json::from_value(serde_json::json!({
            "bits": bits, "radix": radix, "signed": true,
            "width": width, "pad": pad, "justify": justify
        }))
        .unwrap();
        String::from_utf8(render(&[token], data)).unwrap()
    }

    #[test]
    fn fields_are_padded_and_justified() {
        let minus_three = [true, false, true, true];
        assert_eq!(padded("d", 5, " ", ">", &minus_three), "   -3");
        assert_eq!(padded("d", 5, " ", "<", &minus_three), "-3   ");
        assert_eq!(padded("d", 5, "0", "=", &minus_three), "-0003");
        assert_eq!(padded("x", 2, "0", ">", &minus_three), "0d");
        assert_eq!(padded("x", 2, "0", "=", &minus_three), "0d");
        // Too long for the width, and no width at all
        assert_eq!(padded("b", 2, "0", ">", &minus_three), "1101");
        assert_eq!(padded("x", 0, "0", ">", &minus_three), "d");
    }

    #[test]
    fn char_fields_pad_around_a_raw_byte() {
        let dash = [true, false, true, true, false, true, false, false];
        assert_eq!(padded("c", 3, " ", ">", &dash), "  -");
        assert_eq!(padded("c", 3, "_", "=", &dash), "__-");

        let token: Token = serde_json::from_value(serde_json::json!({
            "bits": [0, 1, 2, 3, 4, 5, 6, 7], "radix": "c", "width": 2
        }))
        .unwrap();
        assert_eq!(render(&[token], &[true; 8]), [b' ', 0xff]);
    }

    #[test]
    fn char_uses_the_low_byte() {
        let mut bits = [false; 9];
        for bit in [0, 3, 6, 8] {
            bits[bit] = true;
        }
        assert_eq!(field("c", false, &bits), "I");
    }

    fn probe_file(probe: &str) -> String {
        format!(r#"{{"version":2,"schematic_size":[1,1,1],"probes":[{probe}]}}"#)
    }

    #[test]
    fn clock_toggles_every_half_period() {
        let file = parse_server_file(
            r#"{"version":2,"schematic_size":[1,1,1],"probes":[],"clock":{"pin":"clk","pos":[0,0,0],"half_period_ticks":3}}"#,
        )
        .unwrap();
        let mut clock = file.clock(BlockPos::new(0, 0, 0)).unwrap();
        assert_eq!(clock.ticks_until_toggle(), 3);
        assert!(!clock.advance(2));
        assert_eq!(clock.ticks_until_toggle(), 1);
        assert!(clock.advance(1));
        assert_eq!(clock.ticks_until_toggle(), 3);
        assert!(clock.advance(3));
    }

    #[test]
    fn bad_clocks_are_rejected() {
        let clock = |half_period: &str| {
            format!(
                r#"{{"version":2,"schematic_size":[1,1,1],"probes":[],"clock":{{"pin":"clk","pos":[0,0,0],"half_period_ticks":{half_period}}}}}"#
            )
        };
        assert!(parse_server_file(&clock("1")).is_ok());
        assert!(parse_server_file(&clock("0")).is_err());
        assert!(parse_server_file(&clock("-1")).is_err());
    }

    #[test]
    fn levers_are_named_inputs() {
        let file = |levers: &str| {
            format!(r#"{{"version":2,"schematic_size":[1,1,1],"probes":[],"levers":[{levers}]}}"#)
        };
        let rst = r#"{"name":"rst","pos":[0,0,0]}"#;
        let parsed = parse_server_file(&file(rst)).unwrap();
        assert_eq!(
            parsed.levers(BlockPos::new(8, 8, 8)),
            [("rst".to_string(), BlockPos::new(8, 8, 8))]
        );
        assert!(parse_server_file(&probe_file(""))
            .unwrap()
            .levers
            .is_empty());
        assert!(parse_server_file(&file(&format!("{rst},{rst}"))).is_err());
        assert!(parse_server_file(&file(r#"{"name":"rst"}"#)).is_err());
    }

    #[test]
    fn bad_files_are_rejected() {
        let trigger = r#""trigger":{"pos":[0,0,0],"edge":"rising"}"#;
        let good = format!(
            r#"{{"name":"a","format":[{{"text":"x"}}],{trigger},"enable":{{"const":1}},"data":[]}}"#
        );
        assert!(parse_server_file(&probe_file(&good)).is_ok());

        assert!(parse_server_file("{}").is_err());
        let wrong_version = probe_file(&good).replace(r#""version":2"#, r#""version":1"#);
        assert!(parse_server_file(&wrong_version).is_err());
        for bad in [
            good.replace(r#""const":1"#, r#""const":2"#),
            good.replace("rising", "sideways"),
            good.replace(r#"{"text":"x"}"#, r#"{"bits":[0],"radix":"d"}"#),
            good.replace(r#"{"text":"x"}"#, r#"{"bits":[],"radix":"d"}"#),
            good.replace(r#"{"text":"x"}"#, r#"{"bits":[0],"radix":"z"}"#),
            good.replace(
                r#"{"text":"x"}"#,
                r#"{"bits":[0],"radix":"d","justify":"^"}"#,
            ),
            good.replace(r#"{"text":"x"}"#, r#"{"bits":[0],"radix":"d","pad":"ab"}"#),
        ] {
            assert!(parse_server_file(&probe_file(&bad)).is_err(), "{bad}");
        }
    }
}
