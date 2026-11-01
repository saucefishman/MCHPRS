//! Loads the server file next to a watched schematic, prints probe output to the plot's players
//! and the console, and runs the file's clock.

use super::Plot;
use crate::player::PacketSender;
use anyhow::{Context, Result};
use mchprs_blocks::blocks::Block;
use mchprs_blocks::BlockPos;
use mchprs_probes::{parse_server_file, ProbeHit, ProbeSpec, ServerClock, ServerFile, WorldPoller};
use mchprs_world::World;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{error, info};

/// Messages over this limit are dropped and counted, because a fast clock could otherwise flood
/// clients
const MAX_MESSAGES_PER_SECOND: u32 = 200;

/// `claude.litematic` has its server file in `claude.server.json`
pub(super) fn server_file_path(schematic: &Path) -> PathBuf {
    schematic.with_extension("server.json")
}

pub(super) struct PlotProbes {
    file: ServerFile,
    specs: Vec<ProbeSpec>,
    clock: Option<ServerClock>,
    /// Created when needed while redpiler isn't compiled
    poller: Option<WorldPoller>,
    hits: Vec<ProbeHit>,
    output: MessageOutput,
}

impl PlotProbes {
    pub(super) fn load(
        path: &Path,
        size: [u32; 3],
        world: &impl mchprs_world::World,
        origin: BlockPos,
    ) -> Result<PlotProbes> {
        let text = fs::read_to_string(path).context("reading file")?;
        let file = parse_server_file(&text)?;
        file.check(size, world, origin)?;
        let specs = file.specs(origin);
        let clock = file.clock(origin);
        Ok(PlotProbes {
            file,
            specs,
            clock,
            poller: None,
            hits: Vec::new(),
            output: MessageOutput::new(Instant::now()),
        })
    }

    pub(super) fn len(&self) -> usize {
        self.file.len()
    }
}

impl Plot {
    /// Checks triggers by reading blocks, which is only needed while redpiler isn't compiled. This
    /// has to run after every tick and before the next one, including after a player uses a lever.
    pub(super) fn poll_world_probes(&mut self) {
        let Some(probes) = &mut self.probes else {
            return;
        };
        if self.redpiler.is_active() {
            return;
        }
        let world = &self.world;
        probes
            .poller
            .get_or_insert_with(|| WorldPoller::new(probes.specs.clone(), world))
            .poll(world, &mut probes.hits);
    }

    /// Prints what the probes have collected
    pub(super) fn update_probes(&mut self) {
        self.poll_world_probes();
        let Some(probes) = &mut self.probes else {
            return;
        };
        if self.redpiler.is_active() {
            self.redpiler.drain_probe_hits(&mut probes.hits);
        }
        let now = Instant::now();
        let mut lines = Vec::new();
        for hit in probes.hits.drain(..) {
            probes
                .output
                .write(&probes.file.render(&hit), now, &mut lines);
        }
        probes.output.update(now, &mut lines);
        self.send_probe_lines(&lines);
    }

    /// Must run while redpiler is still compiled, so that its last output isn't lost
    pub(super) fn finish_probe_output(&mut self) {
        self.update_probes();
        if let Some(probes) = &mut self.probes {
            let mut lines = Vec::new();
            probes.output.finish(&mut lines);
            self.send_probe_lines(&lines);
        }
    }

    pub(super) fn clear_probes(&mut self) {
        self.finish_probe_output();
        self.probes = None;
    }

    pub(super) fn probe_specs(&self) -> Vec<ProbeSpec> {
        self.probes
            .as_ref()
            .map_or_else(Vec::new, |probes| probes.specs.clone())
    }

    pub(super) fn clock_ticks_until_toggle(&self) -> Option<u32> {
        let clock = self.probes.as_ref()?.clock.as_ref()?;
        Some(clock.ticks_until_toggle())
    }

    pub(super) fn clock_lever(&self) -> Option<BlockPos> {
        let clock = self.probes.as_ref()?.clock.as_ref()?;
        Some(clock.pos())
    }

    /// Counts `ticks` that have just run, and toggles the clock's lever if it's time to. They must
    /// not go past the next toggle. Redpiler is flushed after its batch of ticks, not here.
    pub(super) fn advance_clock(&mut self, ticks: u32) {
        let Some(clock) = self
            .probes
            .as_mut()
            .and_then(|probes| probes.clock.as_mut())
        else {
            return;
        };
        if !clock.advance(ticks) {
            return;
        }
        let pos = clock.pos();
        let block = self.world.get_block(pos);
        // A player could have replaced the lever since the file was checked
        if !matches!(block, Block::Lever { .. }) {
            return;
        }
        if self.redpiler.is_active() {
            self.redpiler.on_use_block(pos);
        } else {
            mchprs_redstone::on_use(block, &mut self.world, pos);
            self.poll_world_probes();
        }
    }

    /// Redpiler's state replaces the world's while it's compiled, so the poller starts again
    pub(super) fn forget_probe_poller(&mut self) {
        if let Some(probes) = &mut self.probes {
            probes.poller = None;
        }
    }

    fn send_probe_lines(&self, lines: &[String]) {
        for line in lines {
            info!("[probe] {}", line);
            for player in &self.players {
                player.send_system_message(line);
            }
        }
    }

    /// Replaces the probes and clock with the file for a schematic that was just pasted. `fits` is whether
    /// all of the schematic made it into the plot.
    pub(super) fn load_watched_probes(
        &mut self,
        schematic: &Path,
        size: [u32; 3],
        origin: BlockPos,
        fits: bool,
    ) {
        self.probes = None;
        let path = server_file_path(schematic);
        let Ok(probes_meta) = fs::metadata(&path) else {
            return;
        };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let result = if fits {
            PlotProbes::load(&path, size, &self.world, origin)
        } else {
            Err(anyhow::anyhow!("the schematic doesn't fit in the plot"))
        };
        match result {
            Ok(probes) => {
                let clock = if probes.clock.is_some() {
                    " and a clock"
                } else {
                    ""
                };
                let msg = format!("Loaded {} probes{} from {}", probes.len(), clock, name);
                self.probes = Some(probes);
                for player in &self.players {
                    player.send_system_message(&msg);
                }
                let stale = fs::metadata(schematic)
                    .and_then(|schem| Ok(schem.modified()?.duration_since(probes_meta.modified()?)))
                    .is_ok_and(|age| age.is_ok_and(|age| age > STALE_AGE));
                if stale {
                    let msg = format!("Warning: {} is older than the schematic", name);
                    for player in &self.players {
                        player.send_error_message(&msg);
                    }
                }
            }
            Err(err) => {
                error!("Could not load {}: {:#}", name, err);
                let msg = format!("Probes and clock not loaded from {}: {:#}", name, err);
                for player in &self.players {
                    player.send_error_message(&msg);
                }
            }
        }
    }
}

/// A build writes both files, so only a larger gap suggests they're from different builds
const STALE_AGE: Duration = Duration::from_secs(10);

/// Turns each probe's output into chat messages straight away. Chat can't add to a message that
/// was already sent, so output without a newline still gets its own message.
struct MessageOutput {
    window_start: Instant,
    messages_in_window: u32,
    dropped: u32,
}

impl MessageOutput {
    fn new(now: Instant) -> MessageOutput {
        MessageOutput {
            window_start: now,
            messages_in_window: 0,
            dropped: 0,
        }
    }

    fn write(&mut self, bytes: &[u8], now: Instant, messages: &mut Vec<String>) {
        self.update(now, messages);
        // A message is already its own line, so a final newline doesn't need one more
        let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        for line in bytes.split(|&byte| byte == b'\n') {
            if self.messages_in_window < MAX_MESSAGES_PER_SECOND {
                self.messages_in_window += 1;
                messages.push(clean_line(line));
            } else {
                self.dropped += 1;
            }
        }
    }

    fn update(&mut self, now: Instant, messages: &mut Vec<String>) {
        if now - self.window_start < Duration::from_secs(1) {
            return;
        }
        self.window_start = now;
        self.messages_in_window = 0;
        self.finish(messages);
    }

    fn finish(&mut self, messages: &mut Vec<String>) {
        if self.dropped > 0 {
            messages.push(format!(
                "[{} messages of probe output dropped]",
                self.dropped
            ));
            self.dropped = 0;
        }
    }
}

/// Designs can print any byte, and clients show `§` and control characters badly
fn clean_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| if c.is_control() || c == '§' { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(output: &mut MessageOutput, text: &[u8], now: Instant) -> Vec<String> {
        let mut messages = Vec::new();
        output.write(text, now, &mut messages);
        messages
    }

    #[test]
    fn each_write_prints_immediately() {
        let now = Instant::now();
        let mut output = MessageOutput::new(now);
        assert_eq!(write(&mut output, b"H", now), ["H"]);
        assert_eq!(write(&mut output, b"i", now), ["i"]);
        assert_eq!(write(&mut output, b"r=1\n", now), ["r=1"]);
        assert_eq!(write(&mut output, b"a\nb", now), ["a", "b"]);
        assert_eq!(write(&mut output, b"\n", now), [""]);
    }

    #[test]
    fn long_output_is_not_split() {
        let now = Instant::now();
        let mut output = MessageOutput::new(now);
        assert_eq!(write(&mut output, &[b'a'; 1000], now), ["a".repeat(1000)]);
    }

    #[test]
    fn floods_are_dropped_and_counted() {
        let now = Instant::now();
        let mut output = MessageOutput::new(now);
        let extra = 5;
        for _ in 0..MAX_MESSAGES_PER_SECOND {
            assert_eq!(write(&mut output, b"x", now).len(), 1);
        }
        for _ in 0..extra {
            assert!(write(&mut output, b"x", now).is_empty());
        }

        let later = now + Duration::from_secs(1);
        let mut messages = Vec::new();
        output.update(later, &mut messages);
        assert_eq!(messages, ["[5 messages of probe output dropped]"]);
        assert_eq!(write(&mut output, b"y", later), ["y"]);
    }

    #[test]
    fn bad_bytes_are_replaced() {
        assert_eq!(clean_line(b"a\x07b\xc2\xa7c"), "a?b?c");
        assert_eq!(clean_line(&[b'a', 0xff]), "a\u{fffd}");
    }

    #[test]
    fn sibling_file_name() {
        assert_eq!(
            server_file_path(Path::new("schems/claude.litematic")),
            Path::new("schems/claude.server.json")
        );
    }
}
