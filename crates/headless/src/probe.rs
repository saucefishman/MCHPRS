//! Probes print sampled signals when a trigger block changes, like Verilog's `$write`. See the
//! README for the file format.

use crate::{read_signal, BASE};
use anyhow::{bail, ensure, Context, Result};
use mchprs_blocks::BlockPos;
use mchprs_schematic::WorldEditClipboard;
use mchprs_world::testing::TestWorld;
use mchprs_world::World;
use serde::Deserialize;
use std::io::Write;

const VERSION: u32 = 1;
/// Fields are read into a `u128`
const MAX_FIELD_BITS: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeFile {
    version: u32,
    schematic_size: [u32; 3],
    probes: Vec<Probe>,
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

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Edge {
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

pub fn parse_probes(text: &str) -> Result<ProbeFile> {
    let file: ProbeFile = serde_json::from_str(text).context("parsing probes")?;
    ensure!(
        file.version == VERSION,
        "probes version {} is not supported, expected {VERSION}",
        file.version
    );
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

fn read(world: &TestWorld, [x, y, z]: [i32; 3]) -> Result<bool> {
    let block = world.get_block(BASE + BlockPos::new(x, y, z));
    read_signal(block).with_context(|| format!("{x} {y} {z} has no signal: {block:?}"))
}

impl ProbeFile {
    pub(crate) fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    /// Checks the file against the schematic it is being run on. `world` must already have the
    /// schematic pasted.
    pub(crate) fn check(&self, cb: &WorldEditClipboard, world: &TestWorld) -> Result<()> {
        let size = [cb.size_x, cb.size_y, cb.size_z];
        ensure!(
            self.schematic_size == size,
            "probes are for a schematic of size {:?}, but this one is {size:?}",
            self.schematic_size
        );
        for probe in &self.probes {
            for pos in probe.positions() {
                read(world, pos).with_context(|| format!("probe {}", probe.name))?;
            }
        }
        Ok(())
    }
}

/// Watches the probes' triggers and writes their output
pub(crate) struct Poller<'a> {
    probes: &'a [Probe],
    last_trigger: Vec<bool>,
    out: &'a mut dyn Write,
}

impl<'a> Poller<'a> {
    /// A trigger that is already at its active level in `world` doesn't fire
    pub(crate) fn new(
        file: &'a ProbeFile,
        world: &TestWorld,
        out: &'a mut dyn Write,
    ) -> Result<Poller<'a>> {
        let last_trigger = file
            .probes
            .iter()
            .map(|probe| read(world, probe.trigger.pos))
            .collect::<Result<_>>()?;
        Ok(Poller {
            probes: &file.probes,
            last_trigger,
            out,
        })
    }

    /// Must run after every change to the world, because a probe samples whatever state it sees
    /// when its trigger changes. Polling any later could show values from after the edge.
    pub(crate) fn poll(&mut self, world: &TestWorld) -> Result<()> {
        let mut wrote = false;
        for (probe, last) in self.probes.iter().zip(&mut self.last_trigger) {
            let now = read(world, probe.trigger.pos)?;
            let fired = match probe.trigger.edge {
                Edge::Rising => now && !*last,
                Edge::Falling => !now && *last,
            };
            *last = now;
            if fired && sample(&probe.enable, world)? {
                let data = probe
                    .data
                    .iter()
                    .map(|source| sample(source, world))
                    .collect::<Result<Vec<_>>>()?;
                self.out.write_all(&render(&probe.format, &data))?;
                wrote = true;
            }
        }
        if wrote {
            self.out.flush()?;
        }
        Ok(())
    }
}

fn sample(source: &Source, world: &TestWorld) -> Result<bool> {
    match source {
        Source::Pos { pos } => read(world, *pos),
        Source::Const { r#const } => Ok(*r#const != 0),
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
        format!(r#"{{"version":1,"schematic_size":[1,1,1],"probes":[{probe}]}}"#)
    }

    #[test]
    fn bad_files_are_rejected() {
        let trigger = r#""trigger":{"pos":[0,0,0],"edge":"rising"}"#;
        let good = format!(
            r#"{{"name":"a","format":[{{"text":"x"}}],{trigger},"enable":{{"const":1}},"data":[]}}"#
        );
        assert!(parse_probes(&probe_file(&good)).is_ok());

        assert!(parse_probes("{}").is_err());
        let wrong_version = probe_file(&good).replace(r#""version":1"#, r#""version":2"#);
        assert!(parse_probes(&wrong_version).is_err());
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
            assert!(parse_probes(&probe_file(&bad)).is_err(), "{bad}");
        }
    }
}
