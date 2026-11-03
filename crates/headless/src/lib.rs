//! Runs schematics on MCHPRS's redstone and redpiler code without a server. See the README for the
//! file formats.

use anyhow::{bail, ensure, Context, Result};
use mchprs_blocks::blocks::{Block, LeverFace};
use mchprs_blocks::{BlockDirection, BlockPos};
use mchprs_probes::{read_signal, ProbeHit, ServerClock, WorldPoller};
use mchprs_redpiler::{BackendVariant, Compiler, CompilerOptions};
use mchprs_schematic::{paste_clipboard, WorldEditClipboard};
use mchprs_world::testing::TestWorld;
use mchprs_world::World;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::str::FromStr;

pub use mchprs_probes::{parse_server_file, ServerFile};

/// Where schematic coordinate (0, 0, 0) lands in the world.
const BASE: BlockPos = BlockPos::new(8, 8, 8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    NoUpdate,
    Redstone,
    Redpiler,
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Mode, String> {
        match s {
            "noupdate" => Ok(Mode::NoUpdate),
            "redstone" => Ok(Mode::Redstone),
            "redpiler" => Ok(Mode::Redpiler),
            _ => Err(format!(
                "unknown mode {s}, expected noupdate, redstone or redpiler"
            )),
        }
    }
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::NoUpdate => "noupdate",
            Mode::Redstone => "redstone",
            Mode::Redpiler => "redpiler",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub name: String,
    pub pos: BlockPos,
    pub normal_x: i32,
    pub normal_z: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptCommand {
    Settle(u32),
    Set(String, bool),
    Pulse(String, u32),
    Expect(String, bool),
    Print(String),
}

fn content_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
}

fn parse_ints(fields: &[&str]) -> Result<Vec<i32>> {
    fields
        .iter()
        .map(|v| v.parse().with_context(|| format!("bad integer {v}")))
        .collect()
}

pub fn parse_pins(text: &str) -> Result<Vec<Pin>> {
    content_lines(text)
        .map(|(num, line)| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let parsed = match fields.as_slice() {
                [name, rest @ ..] if rest.len() == 5 => parse_ints(rest).map(|n| Pin {
                    name: name.to_string(),
                    pos: BlockPos::new(n[0], n[1], n[2]),
                    normal_x: n[3],
                    normal_z: n[4],
                }),
                _ => Err(anyhow::anyhow!("expected <name> <x> <y> <z> <nx> <nz>")),
            };
            parsed.with_context(|| format!("pins line {num}: {line}"))
        })
        .collect()
}

pub fn parse_cuts(text: &str) -> Result<Vec<BlockPos>> {
    content_lines(text)
        .map(|(num, line)| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            ensure!(fields.len() == 3, "cuts line {num}: expected <x> <y> <z>");
            let n = parse_ints(&fields).with_context(|| format!("cuts line {num}: {line}"))?;
            Ok(BlockPos::new(n[0], n[1], n[2]))
        })
        .collect()
}

fn parse_bit(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => bail!("expected 0 or 1, got {value}"),
    }
}

fn parse_command(line: &str) -> Result<ScriptCommand> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    Ok(match fields.as_slice() {
        ["settle", n] => ScriptCommand::Settle(n.parse()?),
        ["set", name, v] => ScriptCommand::Set(name.to_string(), parse_bit(v)?),
        ["pulse", name, n] => ScriptCommand::Pulse(name.to_string(), n.parse()?),
        ["expect", name, v] => ScriptCommand::Expect(name.to_string(), parse_bit(v)?),
        ["print", rest @ ..] => ScriptCommand::Print(rest.join(" ")),
        _ => bail!("unknown command"),
    })
}

pub fn parse_script(text: &str) -> Result<Vec<ScriptCommand>> {
    content_lines(text)
        .map(|(num, line)| {
            parse_command(line).with_context(|| format!("script line {num}: {line}"))
        })
        .collect()
}

fn paste(cb: &WorldEditClipboard) -> TestWorld {
    // Leaves room for BASE and for levers added just outside the schematic
    let chunks = |n: u32| (n as i32 + 24) / 16 + 1;
    let mut world = TestWorld::new(chunks(cb.size_x), chunks(cb.size_y), chunks(cb.size_z));
    let origin = BASE + BlockPos::new(cb.offset_x, cb.offset_y, cb.offset_z);
    paste_clipboard(&mut world, cb, origin, false);
    world
}

fn schematic_positions(cb: &WorldEditClipboard) -> impl Iterator<Item = BlockPos> + use<> {
    let (sx, sy, sz) = (cb.size_x as i32, cb.size_y as i32, cb.size_z as i32);
    (0..sx)
        .flat_map(move |x| (0..sy).flat_map(move |y| (0..sz).map(move |z| BlockPos::new(x, y, z))))
}

fn update_all(world: &mut TestWorld, cb: &WorldEditClipboard) {
    for pos in schematic_positions(cb) {
        let pos = BASE + pos;
        mchprs_redstone::update(world.get_block(pos), world, pos);
    }
}

fn tick_redstone(world: &mut TestWorld) {
    world.run_due_ticks(|world, pos| mchprs_redstone::tick(world.get_block(pos), world, pos));
}

struct ProbeOutput<'a> {
    file: &'a ServerFile,
    /// Only used when redpiler isn't compiled
    poller: Option<WorldPoller>,
    hits: Vec<ProbeHit>,
    out: &'a mut dyn std::io::Write,
}

struct Runner<'a> {
    world: TestWorld,
    compiler: Option<Compiler>,
    probes: Option<ProbeOutput<'a>>,
    clock: Option<ServerClock>,
}

impl Runner<'_> {
    fn poll(&mut self) -> Result<()> {
        let Some(probes) = &mut self.probes else {
            return Ok(());
        };
        if let Some(compiler) = &mut self.compiler {
            compiler.drain_probe_hits(&mut probes.hits);
        } else if let Some(poller) = &mut probes.poller {
            poller.poll(&self.world, &mut probes.hits);
        }
        for hit in probes.hits.drain(..) {
            probes.out.write_all(&probes.file.render(&hit))?;
            probes.out.flush()?;
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if let Some(compiler) = &mut self.compiler {
            compiler.tick();
        } else {
            tick_redstone(&mut self.world);
        }
        self.poll()?;
        if let Some(clock) = &mut self.clock
            && clock.advance(1)
        {
            let lever = clock.pos();
            self.toggle(lever)?;
        }
        Ok(())
    }

    fn toggle(&mut self, pos: BlockPos) -> Result<()> {
        if let Some(compiler) = &mut self.compiler {
            compiler.on_use_block(pos);
        } else {
            mchprs_redstone::on_use(self.world.get_block(pos), &mut self.world, pos);
        }
        self.poll()
    }

    fn run_ticks(&mut self, ticks: u32) -> Result<()> {
        for _ in 0..ticks {
            self.tick()?;
        }
        Ok(())
    }

    /// Writes redpiler's state to the world. This scans every node, so it's only done before the
    /// world is read rather than on every tick.
    fn sync_world(&mut self) {
        if let Some(compiler) = &mut self.compiler {
            compiler.flush(&mut self.world);
        }
    }

    fn read(&self, name: &str, pos: BlockPos) -> Result<bool> {
        let block = self.world.get_block(pos);
        read_signal(block).with_context(|| format!("output {name} has no signal: {block:?}"))
    }
}

/// Runs a script and returns how many `expect`s failed.
pub fn run_test(
    cb: &WorldEditClipboard,
    pins: &[Pin],
    script: &[ScriptCommand],
    mode: Mode,
    add_levers: &[String],
) -> Result<usize> {
    run_test_with_probes(
        cb,
        pins,
        script,
        mode,
        add_levers,
        Default::default(),
        None,
        false,
        &mut std::io::stdout(),
    )
}

/// Like [`run_test`], compiling redpiler with `options` and writing the output of `probes` to
/// `probe_out`. `run_clock` toggles the lever of the clock in `probes` as the script ticks.
#[allow(clippy::too_many_arguments)]
pub fn run_test_with_probes(
    cb: &WorldEditClipboard,
    pins: &[Pin],
    script: &[ScriptCommand],
    mode: Mode,
    add_levers: &[String],
    mut options: CompilerOptions,
    probes: Option<&ServerFile>,
    run_clock: bool,
    probe_out: &mut dyn std::io::Write,
) -> Result<usize> {
    for name in add_levers {
        ensure!(
            pins.iter().any(|pin| pin.name == *name),
            "--add-lever {name}: no such pin"
        );
    }

    let clock = probes.and_then(|file| file.clock(BASE));
    ensure!(
        !run_clock || clock.is_some(),
        "--run-clock: the file has no clock"
    );
    let clock = clock.filter(|_| run_clock);

    let mut world = paste(cb);
    let mut inputs = BTreeMap::new();
    let mut outputs = BTreeMap::new();
    for pin in pins {
        let cell = BASE + pin.pos;
        let lever = cell + BlockPos::new(pin.normal_x, 1, pin.normal_z);
        if add_levers.contains(&pin.name) {
            world.set_block(
                cell + BlockPos::new(pin.normal_x, 0, pin.normal_z),
                Block::Stone {},
            );
            world.set_block(
                lever,
                Block::Lever {
                    face: LeverFace::Floor,
                    facing: BlockDirection::North,
                    powered: false,
                },
            );
        }
        if matches!(world.get_block(lever), Block::Lever { .. }) {
            inputs.insert(pin.name.as_str(), lever);
        } else {
            outputs.insert(pin.name.as_str(), cell);
        }
    }
    // Compiling can take a long time, so report mistakes in the script first
    for command in script {
        match command {
            ScriptCommand::Set(name, _) | ScriptCommand::Pulse(name, _) => {
                ensure!(inputs.contains_key(name.as_str()), "{name} is not an input")
            }
            ScriptCommand::Expect(name, _) => {
                ensure!(
                    outputs.contains_key(name.as_str()),
                    "{name} is not an output"
                )
            }
            _ => {}
        }
    }
    if let Some(probes) = probes {
        probes.check([cb.size_x, cb.size_y, cb.size_z], &world, BASE)?;
    }

    if mode != Mode::NoUpdate {
        update_all(&mut world, cb);
    }
    let compiler = (mode == Mode::Redpiler).then(|| {
        let mut compiler = Compiler::default();
        options.backend_variant = BackendVariant::Direct;
        if let Some(probes) = probes {
            options.probes = probes.specs(BASE);
        }
        options.keep.extend(clock.as_ref().map(ServerClock::pos));
        let max = BlockPos::new(
            world.x_size * 16 - 1,
            world.y_size * 16 - 1,
            world.z_size * 16 - 1,
        );
        let bounds = (BlockPos::new(0, 0, 0), max);
        let ticks = world.to_be_ticked().to_vec();
        compiler.compile(&world, bounds, options, ticks, Default::default());
        compiler
    });

    let probes = probes
        .filter(|file| !file.is_empty())
        .map(|file| ProbeOutput {
            file,
            poller: compiler
                .is_none()
                .then(|| WorldPoller::new(file.specs(BASE), &world)),
            hits: Vec::new(),
            out: probe_out,
        });
    let mut runner = Runner {
        world,
        compiler,
        probes,
        clock,
    };
    let label = mode.name();
    let mut fails = 0;
    for command in script {
        match command {
            ScriptCommand::Settle(ticks) => runner.run_ticks(*ticks)?,
            ScriptCommand::Set(name, value) => {
                let lever = inputs[name.as_str()];
                runner.sync_world();
                let on = matches!(
                    runner.world.get_block(lever),
                    Block::Lever { powered: true, .. }
                );
                if on != *value {
                    runner.toggle(lever)?;
                }
            }
            ScriptCommand::Pulse(name, ticks) => {
                let lever = inputs[name.as_str()];
                runner.toggle(lever)?;
                runner.run_ticks(*ticks)?;
                runner.toggle(lever)?;
            }
            ScriptCommand::Expect(name, want) => {
                runner.sync_world();
                let got = runner.read(name, outputs[name.as_str()])?;
                if got != *want {
                    println!(
                        "  [{label}] FAIL {name}: want {} got {}",
                        *want as u8, got as u8
                    );
                    fails += 1;
                }
            }
            ScriptCommand::Print(text) => {
                runner.sync_world();
                let mut values = Vec::new();
                for (name, pos) in &outputs {
                    values.push(format!("{name}={}", runner.read(name, *pos)? as u8));
                }
                println!("  [{label}] {text:<22} {}", values.join(" "));
            }
        }
    }
    println!("[{label}] {fails} failures");
    Ok(fails)
}

pub struct SettleResult {
    pub ticks: u32,
    /// How many ticks were still scheduled when it stopped
    pub pending: usize,
    pub states: String,
}

/// Removes `cuts`, updates every block and ticks until nothing is pending or `max_ticks` is reached.
pub fn settle(cb: &WorldEditClipboard, cuts: &[BlockPos], max_ticks: u32) -> SettleResult {
    let mut world = paste(cb);
    for cut in cuts {
        world.set_block(BASE + *cut, Block::Air {});
    }
    update_all(&mut world, cb);
    let mut ticks = 0;
    while !world.to_be_ticked().is_empty() && ticks < max_ticks {
        tick_redstone(&mut world);
        ticks += 1;
    }

    let mut states = String::new();
    for pos in schematic_positions(cb) {
        let BlockPos { x, y, z } = pos;
        let _ = match world.get_block(BASE + pos) {
            Block::Repeater(r) => {
                writeln!(
                    states,
                    "{x} {y} {z} powered {} {}",
                    r.powered as u8, r.locked as u8
                )
            }
            Block::RedstoneWire(wire) => writeln!(states, "{x} {y} {z} power {}", wire.power),
            Block::RedstoneTorch { lit }
            | Block::RedstoneWallTorch { lit, .. }
            | Block::RedstoneLamp { lit } => writeln!(states, "{x} {y} {z} lit {}", lit as u8),
            _ => Ok(()),
        };
    }
    SettleResult {
        ticks,
        pending: world.to_be_ticked().len(),
        states,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mchprs_blocks::blocks::Repeater;
    use mchprs_redstone::wire::make_cross;
    use mchprs_schematic::create_clipboard;

    const PINS: &str = "# input then output\n\na 2 1 1 -1 0\ny 3 1 1 1 0\n";

    /// Stone floor, then lever on stone -> repeater -> dust along X at y=1.
    fn circuit(delay: u8) -> WorldEditClipboard {
        let mut world = TestWorld::new(1, 1, 1);
        for x in 0..6 {
            world.set_block(BlockPos::new(x, 0, 1), Block::Stone {});
        }
        world.set_block(BlockPos::new(1, 1, 1), Block::Stone {});
        world.set_block(
            BlockPos::new(1, 2, 1),
            Block::Lever {
                face: LeverFace::Floor,
                facing: BlockDirection::North,
                powered: false,
            },
        );
        world.set_block(
            BlockPos::new(2, 1, 1),
            Block::Repeater(Repeater {
                delay,
                facing: BlockDirection::West,
                ..Default::default()
            }),
        );
        world.set_block(BlockPos::new(3, 1, 1), Block::RedstoneWire(make_cross(0)));
        let origin = BlockPos::new(0, 0, 0);
        create_clipboard(&mut world, origin, origin, BlockPos::new(5, 3, 2))
    }

    fn run(script: &str, mode: Mode) -> Result<usize> {
        let pins = parse_pins(PINS)?;
        run_test(&circuit(1), &pins, &parse_script(script)?, mode, &[])
    }

    const SCRIPT: &str = "settle 2\nexpect y 0\nset a 1\nsettle 4\nexpect y 1\nset a 0\nsettle 4\nexpect y 0\npulse a 4\nexpect y 1\nsettle 4\nexpect y 0\n";

    #[test]
    fn script_passes_on_redstone() {
        assert_eq!(run(SCRIPT, Mode::Redstone).unwrap(), 0);
    }

    #[test]
    fn script_passes_on_redpiler() {
        assert_eq!(run(SCRIPT, Mode::Redpiler).unwrap(), 0);
    }

    #[test]
    fn failed_expects_are_counted() {
        for mode in [Mode::Redstone, Mode::Redpiler] {
            assert_eq!(run("settle 2\nexpect y 1\nexpect y 1\n", mode).unwrap(), 2);
        }
    }

    #[test]
    fn pin_kinds_are_checked() {
        assert!(run("set y 1\n", Mode::Redstone).is_err());
        assert!(run("expect a 1\n", Mode::Redstone).is_err());
        assert!(run("expect missing 1\n", Mode::Redstone).is_err());
    }

    #[test]
    fn add_lever_makes_an_input() {
        // The output pin's own normal is free, so it can take a lever
        let pins = parse_pins("y 3 1 1 1 0\n").unwrap();
        let script = parse_script("set y 1\n").unwrap();
        let cb = circuit(1);
        assert!(run_test(&cb, &pins, &script, Mode::Redstone, &[]).is_err());
        let levers = ["y".to_string()];
        assert_eq!(
            run_test(&cb, &pins, &script, Mode::Redstone, &levers).unwrap(),
            0
        );
        assert!(run_test(&cb, &pins, &script, Mode::Redstone, &["z".to_string()]).is_err());
    }

    #[test]
    fn bad_input_is_rejected() {
        assert!(parse_pins("a 1 2\n").is_err());
        assert!(parse_pins("a 1 2 x 0 0\n").is_err());
        assert!(parse_cuts("1 2\n").is_err());
        assert!(parse_script("frobnicate\n").is_err());
        assert!(parse_script("expect a 2\n").is_err());
        assert!("fast".parse::<Mode>().is_err());
    }

    /// The lever's position and the repeater's output in `circuit`, printed on the lever's edge
    fn repeater_probe(edge: &str, enable: &str) -> ServerFile {
        parse_server_file(&format!(
            r#"{{"version":2,"schematic_size":[6,4,3],"probes":[{{
                "name":"t",
                "format":[{{"text":"r="}},{{"bits":[0],"radix":"d"}},{{"text":"\n"}}],
                "trigger":{{"pos":[1,2,1],"edge":"{edge}"}},
                "enable":{enable},
                "data":[{{"pos":[2,1,1]}}]
            }}]}}"#
        ))
        .unwrap()
    }

    fn probe_output(delay: u8, script: &str, probes: &ServerFile, mode: Mode) -> Result<String> {
        probe_output_with(delay, script, probes, mode, Default::default())
    }

    fn probe_output_with(
        delay: u8,
        script: &str,
        probes: &ServerFile,
        mode: Mode,
        options: CompilerOptions,
    ) -> Result<String> {
        let pins = parse_pins(PINS)?;
        let mut out = Vec::new();
        run_test_with_probes(
            &circuit(delay),
            &pins,
            &parse_script(script)?,
            mode,
            &[],
            options,
            Some(probes),
            false,
            &mut out,
        )?;
        Ok(String::from_utf8(out)?)
    }

    const CONST_ON: &str = r#"{"const":1}"#;

    #[test]
    fn probe_samples_before_the_edge_reaches_a_one_tick_repeater() {
        let probes = repeater_probe("rising", CONST_ON);
        for mode in [Mode::Redstone, Mode::Redpiler] {
            let output = probe_output(1, "settle 2\nset a 1\nsettle 4\n", &probes, mode).unwrap();
            assert_eq!(output, "r=0\n", "{mode:?}");
        }
    }

    #[test]
    fn probe_reads_state_left_by_the_previous_edge() {
        let probes = repeater_probe("rising", CONST_ON);
        let script = "settle 2\nset a 1\nsettle 8\nset a 0\nsettle 1\nset a 1\nsettle 8\n";
        for mode in [Mode::Redstone, Mode::Redpiler] {
            let output = probe_output(4, script, &probes, mode).unwrap();
            assert_eq!(output, "r=0\nr=1\n", "{mode:?}");
        }
    }

    #[test]
    fn probe_falling_edge_fires() {
        let probes = repeater_probe("falling", CONST_ON);
        for mode in [Mode::Redstone, Mode::Redpiler] {
            let output =
                probe_output(1, "settle 2\nset a 1\nsettle 4\nset a 0\n", &probes, mode).unwrap();
            assert_eq!(output, "r=1\n", "{mode:?}");
        }
    }

    #[test]
    fn probe_enable_gates_output() {
        let script = "set a 1\nsettle 4\nset a 0\nsettle 4\nset a 1\nsettle 4\n";
        let never = repeater_probe("rising", r#"{"const":0}"#);
        assert_eq!(probe_output(1, script, &never, Mode::Redstone).unwrap(), "");
        // The repeater is low at both rising edges
        let low = repeater_probe("rising", r#"{"pos":[2,1,1]}"#);
        assert_eq!(probe_output(1, script, &low, Mode::Redpiler).unwrap(), "");
        // The lever has just gone high, so it enables its own edge
        let high = repeater_probe("rising", r#"{"pos":[1,2,1]}"#);
        assert_eq!(
            probe_output(1, script, &high, Mode::Redpiler).unwrap(),
            "r=0\nr=0\n"
        );
    }

    #[test]
    fn probes_are_checked_against_the_schematic() {
        let wrong_size =
            parse_server_file(r#"{"version":2,"schematic_size":[1,1,1],"probes":[]}"#).unwrap();
        assert!(probe_output(1, "settle 1\n", &wrong_size, Mode::Redstone).is_err());
        // Stone floor has no signal
        let stone = repeater_probe("rising", r#"{"pos":[0,0,1]}"#);
        assert!(probe_output(1, "settle 1\n", &stone, Mode::Redpiler).is_err());
    }

    const FLAGS: [&str; 4] = ["", "-o", "-i", "-oi"];

    /// `circuit`'s lever as a clock, with a probe of the repeater on its rising edge
    fn clocked_file() -> ServerFile {
        parse_server_file(
            r#"{"version":2,"schematic_size":[6,4,3],"probes":[{
                "name":"t",
                "format":[{"bits":[0],"radix":"d"},{"text":"\n"}],
                "trigger":{"pos":[1,2,1],"edge":"rising"},
                "enable":{"const":1},
                "data":[{"pos":[2,1,1]}]
            }],"clock":{"pin":"clk","pos":[1,2,1],"half_period_ticks":3}}"#,
        )
        .unwrap()
    }

    fn clocked_run(
        cb: &WorldEditClipboard,
        pins: &str,
        script: &str,
        file: &ServerFile,
        mode: Mode,
        options: CompilerOptions,
        run_clock: bool,
    ) -> Result<(usize, String)> {
        let mut out = Vec::new();
        let fails = run_test_with_probes(
            cb,
            &parse_pins(pins)?,
            &parse_script(script)?,
            mode,
            &[],
            options,
            Some(file),
            run_clock,
            &mut out,
        )?;
        Ok((fails, String::from_utf8(out)?))
    }

    #[test]
    fn clock_toggles_the_lever_every_half_period() {
        // The lever toggles after ticks 3, 6 and 9, and the repeater follows one tick later
        let script = "settle 3\nexpect y 0\nsettle 1\nexpect y 1\nsettle 2\nexpect y 1\nsettle 1\nexpect y 0\nsettle 3\nexpect y 1\n";
        let file = clocked_file();
        for mode in [Mode::Redstone, Mode::Redpiler] {
            let (fails, output) = clocked_run(
                &circuit(1),
                PINS,
                script,
                &file,
                mode,
                Default::default(),
                true,
            )
            .unwrap();
            assert_eq!((fails, output.as_str()), (0, "0\n0\n"), "{mode:?}");
        }
        // Optimizing drops the dust that `expect` reads, so only the probe can be checked
        for flags in FLAGS {
            let (_, output) = clocked_run(
                &circuit(1),
                PINS,
                script,
                &file,
                Mode::Redpiler,
                CompilerOptions::parse(flags),
                true,
            )
            .unwrap();
            assert_eq!(output, "0\n0\n", "flags {flags:?}");
        }
    }

    #[test]
    fn clock_is_opt_in() {
        let script = "settle 10\nexpect y 0\n";
        let (fails, output) = clocked_run(
            &circuit(1),
            PINS,
            script,
            &clocked_file(),
            Mode::Redstone,
            Default::default(),
            false,
        )
        .unwrap();
        assert_eq!((fails, output.as_str()), (0, ""));

        let no_clock =
            parse_server_file(r#"{"version":2,"schematic_size":[6,4,3],"probes":[]}"#).unwrap();
        let result = clocked_run(
            &circuit(1),
            PINS,
            script,
            &no_clock,
            Mode::Redstone,
            Default::default(),
            true,
        );
        assert!(result.is_err());
    }

    #[test]
    fn clock_must_be_a_lever_inside_the_schematic() {
        for pos in ["[0,0,1]", "[6,0,0]"] {
            let file = parse_server_file(&format!(
                r#"{{"version":2,"schematic_size":[6,4,3],"probes":[],"clock":{{"pin":"clk","pos":{pos},"half_period_ticks":3}}}}"#
            ))
            .unwrap();
            let result = clocked_run(
                &circuit(1),
                PINS,
                "settle 1\n",
                &file,
                Mode::Redstone,
                Default::default(),
                true,
            );
            assert!(result.is_err(), "{pos}");
        }
    }

    #[test]
    fn clock_lever_that_drives_nothing_survives_optimization() {
        let mut world = TestWorld::new(1, 1, 1);
        world.set_block(BlockPos::new(0, 0, 0), Block::Stone {});
        world.set_block(
            BlockPos::new(0, 1, 0),
            Block::Lever {
                face: LeverFace::Floor,
                facing: BlockDirection::North,
                powered: false,
            },
        );
        let origin = BlockPos::new(0, 0, 0);
        let cb = create_clipboard(&mut world, origin, origin, BlockPos::new(0, 1, 0));
        let file = parse_server_file(
            r#"{"version":2,"schematic_size":[1,2,1],"probes":[],"clock":{"pin":"clk","pos":[0,1,0],"half_period_ticks":1}}"#,
        )
        .unwrap();
        let (fails, _) = clocked_run(
            &cb,
            "",
            "settle 4\n",
            &file,
            Mode::Redpiler,
            CompilerOptions::parse("-oi"),
            true,
        )
        .unwrap();
        assert_eq!(fails, 0);
    }

    #[test]
    fn compiled_probes_hold_under_every_flag() {
        let probes = repeater_probe("rising", CONST_ON);
        let script = "settle 2\nset a 1\nsettle 8\nset a 0\nsettle 1\nset a 1\nsettle 8\n";
        for flags in FLAGS {
            let options = CompilerOptions::parse(flags);
            let output = probe_output_with(4, script, &probes, Mode::Redpiler, options).unwrap();
            assert_eq!(output, "r=0\nr=1\n", "flags {flags:?}");
        }
    }

    #[test]
    fn compiled_probes_read_dust_under_every_flag() {
        // The dust after the repeater has no sinks and is dropped by -o unless it is kept
        let probes = parse_server_file(
            r#"{"version":2,"schematic_size":[6,4,3],"probes":[{
                "name":"t",
                "format":[{"bits":[0],"radix":"d"},{"text":"\n"}],
                "trigger":{"pos":[3,1,1],"edge":"rising"},
                "enable":{"const":1},
                "data":[{"pos":[2,1,1]}]
            }]}"#,
        )
        .unwrap();
        for flags in FLAGS {
            let options = CompilerOptions::parse(flags);
            let output = probe_output_with(
                1,
                "settle 2\nset a 1\nsettle 4\n",
                &probes,
                Mode::Redpiler,
                options,
            )
            .unwrap();
            assert_eq!(output, "1\n", "flags {flags:?}");
        }
    }

    /// A torch that folds to on and a repeater it feeds through a lit torch's inverse, tapped with
    /// no sinks. Neither has any input that can change, so `-o` makes both constants.
    #[test]
    fn compiled_probes_read_constant_nodes_under_every_flag() {
        let mut world = TestWorld::new(1, 1, 1);
        for x in 0..6 {
            world.set_block(BlockPos::new(x, 0, 1), Block::Stone {});
        }
        // Saved with the wrong states, so reading the blocks instead of the nodes would be wrong
        world.set_block(BlockPos::new(1, 1, 1), Block::RedstoneTorch { lit: false });
        world.set_block(
            BlockPos::new(2, 1, 1),
            Block::Repeater(Repeater {
                delay: 1,
                facing: BlockDirection::West,
                powered: false,
                ..Default::default()
            }),
        );
        world.set_block(BlockPos::new(4, 1, 1), Block::Stone {});
        world.set_block(
            BlockPos::new(4, 2, 1),
            Block::Lever {
                face: LeverFace::Floor,
                facing: BlockDirection::North,
                powered: false,
            },
        );
        let origin = BlockPos::new(0, 0, 0);
        let cb = create_clipboard(&mut world, origin, origin, BlockPos::new(5, 3, 2));
        let probes = parse_server_file(
            r#"{"version":2,"schematic_size":[6,4,3],"probes":[{
                "name":"t",
                "format":[{"bits":[0,1],"radix":"b"},{"text":"\n"}],
                "trigger":{"pos":[4,2,1],"edge":"rising"},
                "enable":{"pos":[1,1,1]},
                "data":[{"pos":[1,1,1]},{"pos":[2,1,1]}]
            }]}"#,
        )
        .unwrap();
        let pins = parse_pins("a 3 1 1 1 0\n").unwrap();
        let script = parse_script("settle 4\nset a 1\nsettle 4\n").unwrap();
        for flags in FLAGS {
            let mut out = Vec::new();
            run_test_with_probes(
                &cb,
                &pins,
                &script,
                Mode::Redpiler,
                &[],
                CompilerOptions::parse(flags),
                Some(&probes),
                false,
                &mut out,
            )
            .unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), "11\n", "flags {flags:?}");
        }
    }

    /// A wall torch that is turned off through a repeater, with dust looping back around.
    fn oscillator() -> (WorldEditClipboard, ServerFile) {
        let mut world = TestWorld::new(1, 1, 1);
        for x in 0..4 {
            for z in 0..5 {
                world.set_block(BlockPos::new(x, 0, z), Block::Stone {});
            }
        }
        world.set_block(BlockPos::new(1, 1, 1), Block::Stone {});
        world.set_block(
            BlockPos::new(2, 1, 1),
            Block::RedstoneWallTorch {
                lit: true,
                facing: BlockDirection::East,
            },
        );
        for (x, z) in [(2, 2), (2, 3), (1, 3)] {
            world.set_block(BlockPos::new(x, 1, z), Block::RedstoneWire(make_cross(0)));
        }
        world.set_block(
            BlockPos::new(1, 1, 2),
            Block::Repeater(Repeater {
                delay: 3,
                facing: BlockDirection::South,
                ..Default::default()
            }),
        );
        let origin = BlockPos::new(0, 0, 0);
        let cb = create_clipboard(&mut world, origin, origin, BlockPos::new(3, 3, 4));
        let probes = parse_server_file(
            r#"{"version":2,"schematic_size":[4,4,5],"probes":[{
                "name":"t",
                "format":[],
                "trigger":{"pos":[1,1,2],"edge":"rising"},
                "enable":{"const":1},
                "data":[{"pos":[2,1,1]},{"pos":[1,1,2]}]
            }]}"#,
        )
        .unwrap();
        (cb, probes)
    }

    fn compile_oscillator(flags: &str) -> (TestWorld, Compiler) {
        let (cb, probes) = oscillator();
        let mut world = paste(&cb);
        update_all(&mut world, &cb);
        let mut options = CompilerOptions::parse(flags);
        options.probes = probes.specs(BASE);
        let mut compiler = Compiler::default();
        let max = BlockPos::new(
            world.x_size * 16 - 1,
            world.y_size * 16 - 1,
            world.z_size * 16 - 1,
        );
        let ticks = world.to_be_ticked().to_vec();
        compiler.compile(
            &world,
            (BlockPos::new(0, 0, 0), max),
            options,
            ticks,
            Default::default(),
        );
        (world, compiler)
    }

    #[test]
    fn hits_are_captured_at_tickn_speed() {
        for flags in FLAGS {
            let (_, mut one_at_a_time) = compile_oscillator(flags);
            let mut expected = Vec::new();
            for _ in 0..1000 {
                one_at_a_time.tick();
                one_at_a_time.drain_probe_hits(&mut expected);
            }

            let (_, mut batched) = compile_oscillator(flags);
            batched.tickn(1000);
            let mut hits = Vec::new();
            batched.drain_probe_hits(&mut hits);

            assert!(hits.len() > 50, "flags {flags:?} got {} hits", hits.len());
            assert_eq!(hits, expected, "flags {flags:?}");
            // The repeater has just turned on, and the torch hasn't reacted to it
            assert!(hits.iter().all(|hit| hit.data == [true, true]), "{hits:?}");
        }
    }

    #[test]
    fn compiled_probes_match_polled_redstone() {
        let (cb, probes) = oscillator();
        let mut world = paste(&cb);
        update_all(&mut world, &cb);
        let mut poller = WorldPoller::new(probes.specs(BASE), &world);
        let mut polled = Vec::new();
        for _ in 0..1000 {
            tick_redstone(&mut world);
            poller.poll(&world, &mut polled);
        }

        let (_, mut compiler) = compile_oscillator("");
        compiler.tickn(1000);
        let mut hits = Vec::new();
        compiler.drain_probe_hits(&mut hits);
        assert_eq!(hits, polled);
    }

    #[test]
    fn marker_blocks_conduct_like_stone() {
        // Lever on the marker block, with a wall torch on the marker block's side
        for marker in [Block::Stone {}, Block::EmeraldBlock {}, Block::GoldBlock {}] {
            let mut world = TestWorld::new(1, 1, 1);
            world.set_block(BlockPos::new(1, 1, 1), marker);
            world.set_block(
                BlockPos::new(1, 2, 1),
                Block::Lever {
                    face: LeverFace::Floor,
                    facing: BlockDirection::North,
                    powered: false,
                },
            );
            world.set_block(
                BlockPos::new(2, 1, 1),
                Block::RedstoneWallTorch {
                    lit: true,
                    facing: BlockDirection::East,
                },
            );
            let origin = BlockPos::new(0, 0, 0);
            let cb = create_clipboard(&mut world, origin, origin, BlockPos::new(3, 3, 2));
            let pins = parse_pins("a 0 1 1 1 0\ny 2 1 1 1 0\n").unwrap();
            let script =
                parse_script("settle 4\nexpect y 1\nset a 1\nsettle 4\nexpect y 0\n").unwrap();
            for mode in [Mode::Redstone, Mode::Redpiler] {
                let fails = run_test(&cb, &pins, &script, mode, &[]).unwrap();
                assert_eq!(fails, 0, "{marker:?} {mode:?}");
            }
        }
    }

    #[test]
    fn settle_writes_states() {
        let result = settle(&circuit(1), &[], 100);
        assert_eq!(result.pending, 0);
        assert!(result.states.contains("2 1 1 powered 0 0\n"));
        assert!(result.states.contains("3 1 1 power 0\n"));
    }

    #[test]
    fn settle_applies_cuts() {
        let result = settle(&circuit(1), &[BlockPos::new(3, 1, 1)], 100);
        assert!(!result.states.contains("3 1 1 power"));
    }

    #[test]
    fn settle_reports_pending_at_max_ticks() {
        // A repeater saved powered with no input schedules its turn-off when updated
        let mut cb_world = TestWorld::new(1, 1, 1);
        cb_world.set_block(BlockPos::new(1, 0, 1), Block::Stone {});
        cb_world.set_block(
            BlockPos::new(1, 1, 1),
            Block::Repeater(Repeater {
                delay: 4,
                facing: BlockDirection::West,
                powered: true,
                ..Default::default()
            }),
        );
        let origin = BlockPos::new(0, 0, 0);
        let cb = create_clipboard(&mut cb_world, origin, origin, BlockPos::new(2, 2, 2));
        assert_eq!(settle(&cb, &[], 1).pending, 1);
        let done = settle(&cb, &[], 100);
        assert_eq!(done.pending, 0);
        assert!(done.states.contains("1 1 1 powered 0 0\n"));
    }
}
