use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mchprs_headless::{
    parse_cuts, parse_pins, parse_script, parse_server_file, run_test_with_probes, settle, Mode,
};
use mchprs_redpiler::CompilerOptions;
use mchprs_schematic::load_schematic;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(about = "Run schematics on MCHPRS's redstone and redpiler code without a server")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Drive a schematic's pins from a script and check its outputs
    Test {
        schem: PathBuf,
        pins: PathBuf,
        script: PathBuf,
        /// Comma-separated: noupdate, redstone, redpiler
        #[arg(long, value_delimiter = ',', default_value = "redstone,redpiler")]
        modes: Vec<Mode>,
        /// Place a stone and lever at this pin before running
        #[arg(long = "add-lever", value_name = "NAME")]
        add_levers: Vec<String>,
        /// Redpiler compile flags, such as "-oi"
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        redpiler_flags: String,
        /// Server file (<name>.server.json) of signals to print when their triggers change
        #[arg(long)]
        probes: Option<PathBuf>,
        /// Toggle the lever of the clock in the --probes file as the script ticks
        #[arg(long, requires = "probes")]
        run_clock: bool,
    },
    /// Run a schematic until nothing is pending and write its signal block states
    Settle {
        schem: PathBuf,
        cuts: PathBuf,
        states: PathBuf,
        #[arg(long, default_value_t = 20000)]
        max_ticks: u32,
        /// Write the states and succeed even if ticks are still pending
        #[arg(long)]
        allow_pending: bool,
    },
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn run(args: Args) -> Result<ExitCode> {
    match args.command {
        Command::Test {
            schem,
            pins,
            script,
            modes,
            add_levers,
            redpiler_flags,
            probes,
            run_clock,
        } => {
            let cb =
                load_schematic(&schem).with_context(|| format!("loading {}", schem.display()))?;
            let pins = parse_pins(&read(&pins)?)?;
            let script = parse_script(&read(&script)?)?;
            let probes = probes
                .map(|path| {
                    parse_server_file(&read(&path)?).with_context(|| path.display().to_string())
                })
                .transpose()?;
            let mut fails = 0;
            for mode in modes {
                fails += run_test_with_probes(
                    &cb,
                    &pins,
                    &script,
                    mode,
                    &add_levers,
                    CompilerOptions::parse(&redpiler_flags),
                    probes.as_ref(),
                    run_clock,
                    &mut std::io::stdout(),
                )?;
            }
            Ok(ExitCode::from((fails > 0) as u8))
        }
        Command::Settle {
            schem,
            cuts,
            states,
            max_ticks,
            allow_pending,
        } => {
            let cb =
                load_schematic(&schem).with_context(|| format!("loading {}", schem.display()))?;
            let result = settle(&cb, &parse_cuts(&read(&cuts)?)?, max_ticks);
            println!(
                "settled in {} ticks, {} still pending",
                result.ticks, result.pending
            );
            if result.pending > 0 && !allow_pending {
                eprintln!("error: ticks still pending after {max_ticks} ticks");
                return Ok(ExitCode::from(1));
            }
            std::fs::write(&states, result.states)
                .with_context(|| format!("writing {}", states.display()))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn main() -> ExitCode {
    run(Args::parse()).unwrap_or_else(|err| {
        eprintln!("error: {err:#}");
        ExitCode::from(2)
    })
}
