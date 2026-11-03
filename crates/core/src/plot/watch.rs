//! Watches a schematic file and automatically pastes it into the plot whenever it changes.
//! The schematic is centered horizontally in the plot and placed on top of the plot floor.
//! A server file next to the schematic is watched and reloaded with it.
//!
//! The file is parsed on a background thread while the plot keeps running. The paste itself
//! copies block states exactly as they are in the file and doesn't cause any block updates.

use super::probes::server_file_path;
use super::{Plot, PlotWorld, PLOT_BLOCK_HEIGHT, PLOT_BLOCK_WIDTH};
use crate::player::PacketSender;
use anyhow::{anyhow, Result};
use mchprs_blocks::BlockPos;
use mchprs_network::packets::clientbound::ClientBoundPacket;
use mchprs_network::packets::PacketEncoder;
use mchprs_redpiler::CompilerOptions;
use mchprs_schematic::{load_schematic, WorldEditClipboard};
use mchprs_world::storage::Chunk;
use mchprs_world::World;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};
use tracing::error;

/// How often the watched file's modification time is checked
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The lowest y level above the generated plot floor
const FLOOR_Y: i32 = 8;

/// The modification times of the schematic and its server file, if it has one
type WatchedTimes = (SystemTime, Option<SystemTime>);

fn watched_times(path: &Path) -> Option<WatchedTimes> {
    let schematic = fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let probes = fs::metadata(server_file_path(path))
        .and_then(|m| m.modified())
        .ok();
    Some((schematic, probes))
}

pub struct SchematicWatch {
    pub path: PathBuf,
    /// If set, redpiler is compiled with these options after every reload
    pub compile_options: Option<CompilerOptions>,
    /// If false, the file is only loaded once and changes are not picked up
    polling: bool,
    /// If true, the file is parsed and pasted on the plot thread, blocking the plot until it's
    /// done, instead of being parsed in the background
    blocking: bool,
    /// The modification times of the last loaded versions of the files
    loaded_mtime: Option<WatchedTimes>,
    /// Modification times that have been seen but not loaded yet. The files are only loaded once
    /// their modification times have been stable for one poll, so that partially written files
    /// aren't loaded.
    pending_mtime: Option<WatchedTimes>,
    /// The region the last version was pasted into, which is cleared before the next paste
    last_bounds: Option<(BlockPos, BlockPos)>,
    last_check: Instant,
    /// The thread parsing the file, and when it was started
    loading: Option<(JoinHandle<Result<WorldEditClipboard>>, Instant)>,
}

impl SchematicWatch {
    pub fn new(
        path: PathBuf,
        compile_options: Option<CompilerOptions>,
        polling: bool,
        blocking: bool,
    ) -> SchematicWatch {
        let loaded_mtime = watched_times(&path);
        SchematicWatch {
            path,
            compile_options,
            polling,
            blocking,
            loaded_mtime,
            pending_mtime: None,
            last_bounds: None,
            last_check: Instant::now() - POLL_INTERVAL,
            loading: None,
        }
    }
}

impl SchematicWatch {
    /// Forgets the region the last version was pasted into, so it won't be cleared on the next
    /// reload
    pub fn inherit_last_bounds(&mut self, other: &SchematicWatch) {
        self.last_bounds = other.last_bounds;
    }

    pub fn forget_last_bounds(&mut self) {
        self.last_bounds = None;
    }
}

impl Plot {
    /// Checks the watched schematic for changes and reloads it if needed
    pub(super) fn update_schematic_watch(&mut self) {
        let Some(watch) = &mut self.schematic_watch else {
            return;
        };
        if let Some((handle, _)) = &watch.loading {
            if handle.is_finished() {
                let (handle, start_time) = watch.loading.take().unwrap();
                let result = handle
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("schematic loader panicked")));
                self.paste_watched_schematic(result, start_time);
            }
            return;
        }
        if !watch.polling || watch.last_check.elapsed() < POLL_INTERVAL {
            return;
        }
        watch.last_check = Instant::now();

        let Some(mtime) = watched_times(&watch.path) else {
            return;
        };
        if Some(mtime) == watch.loaded_mtime {
            return;
        }
        if watch.pending_mtime != Some(mtime) {
            // Wait for the file to stop changing
            watch.pending_mtime = Some(mtime);
            return;
        }
        watch.loaded_mtime = Some(mtime);
        watch.pending_mtime = None;
        self.reload_watched_schematic();
    }

    /// Starts parsing the watched schematic in the background. It is pasted by
    /// `update_schematic_watch` once it has been parsed. Blocking watches are parsed and pasted
    /// right away.
    pub(super) fn reload_watched_schematic(&mut self) {
        let Some(watch) = &mut self.schematic_watch else {
            return;
        };
        if watch.blocking {
            let start_time = Instant::now();
            let result = load_schematic(&watch.path);
            self.paste_watched_schematic(result, start_time);
            return;
        }
        let path = watch.path.clone();
        let handle = thread::spawn(move || load_schematic(&path));
        watch.loading = Some((handle, Instant::now()));
    }

    fn paste_watched_schematic(&mut self, result: Result<WorldEditClipboard>, start_time: Instant) {
        let Some(watch) = &self.schematic_watch else {
            return;
        };
        let file_name = watch
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let cb = match result {
            Ok(cb) => cb,
            Err(err) => {
                error!(
                    "There was an error loading watched schematic {}:",
                    file_name
                );
                error!("{:#}", err);
                for player in &self.players {
                    player.send_error_message(&format!(
                        "Failed to reload {}. Check console for more details.",
                        file_name
                    ));
                }
                return;
            }
        };

        let last_bounds = watch.last_bounds;
        let compile_options = watch.compile_options.clone();
        let schematic_path = watch.path.clone();

        self.finish_probe_output();
        self.reset_redpiler();

        let (plot_min, _) = self.world.get_corners();
        let first_pos = BlockPos::new(
            plot_min.x + (PLOT_BLOCK_WIDTH - cb.size_x as i32) / 2,
            FLOOR_Y,
            plot_min.z + (PLOT_BLOCK_WIDTH - cb.size_z as i32) / 2,
        );
        let second_pos = first_pos
            + BlockPos::new(
                cb.size_x as i32 - 1,
                cb.size_y as i32 - 1,
                cb.size_z as i32 - 1,
            );
        replace_region(&mut self.world, &cb, first_pos, last_bounds);
        if let Some(watch) = &mut self.schematic_watch {
            watch.last_bounds = Some((first_pos, second_pos));
        }

        let max_height = PLOT_BLOCK_HEIGHT - FLOOR_Y;
        let fits = cb.size_x as i32 <= PLOT_BLOCK_WIDTH
            && cb.size_y as i32 <= max_height
            && cb.size_z as i32 <= PLOT_BLOCK_WIDTH;

        self.load_watched_probes(
            &schematic_path,
            [cb.size_x, cb.size_y, cb.size_z],
            first_pos,
            fits,
        );

        if let Some(options) = compile_options {
            self.start_redpiler(options);
        }

        let msg = format!(
            "Reloaded {} ({}x{}x{} at {}, {}, {}) ({:?})",
            file_name,
            cb.size_x,
            cb.size_y,
            cb.size_z,
            first_pos.x,
            first_pos.y,
            first_pos.z,
            start_time.elapsed()
        );
        let warning = (!fits).then(|| {
            format!(
                "Warning: {} is {}x{}x{}, which is larger than the plot's {}x{}x{} build area. The parts outside the plot were cut off.",
                file_name,
                cb.size_x,
                cb.size_y,
                cb.size_z,
                PLOT_BLOCK_WIDTH,
                max_height,
                PLOT_BLOCK_WIDTH
            )
        });
        for player in &self.players {
            player.send_system_message(&msg);
            if let Some(warning) = &warning {
                player.send_error_message(warning);
            }
        }
    }
}

fn in_bounds(pos: BlockPos, (min, max): (BlockPos, BlockPos)) -> bool {
    (min.x..=max.x).contains(&pos.x)
        && (min.y..=max.y).contains(&pos.y)
        && (min.z..=max.z).contains(&pos.z)
}

/// Clears `old_bounds` and pastes `cb` with its minimum corner at `first_pos`. Blocks are set
/// without any updates, so their states stay exactly as they are in the schematic. Pending ticks
/// in the replaced region are dropped. Chunks don't depend on each other, so they are written in
/// parallel.
fn replace_region(
    world: &mut PlotWorld,
    cb: &WorldEditClipboard,
    first_pos: BlockPos,
    old_bounds: Option<(BlockPos, BlockPos)>,
) {
    let new_bounds = (
        first_pos,
        first_pos
            + BlockPos::new(
                cb.size_x as i32 - 1,
                cb.size_y as i32 - 1,
                cb.size_z as i32 - 1,
            ),
    );
    let replaced =
        |pos| in_bounds(pos, new_bounds) || old_bounds.is_some_and(|b| in_bounds(pos, b));

    let threads = thread::available_parallelism().map_or(1, |n| n.get());
    // Chunks are dealt out round robin so that every thread gets a share of the schematic
    let mut groups: Vec<Vec<&mut Chunk>> = (0..threads).map(|_| Vec::new()).collect();
    for (i, chunk) in world.chunks.iter_mut().enumerate() {
        groups[i % threads].push(chunk);
    }
    // The block change packets are encoded on the same threads
    let packets: Vec<Vec<PacketEncoder>> = thread::scope(|s| {
        let handles: Vec<_> = groups
            .into_iter()
            .map(|group| {
                s.spawn(move || {
                    let mut packets = Vec::new();
                    for chunk in group {
                        replace_in_chunk(chunk, cb, new_bounds, old_bounds);
                        packets.extend(chunk.multi_blocks().map(|packet| packet.encode()));
                        chunk.reset_multi_blocks();
                    }
                    packets
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for packet in packets.iter().flatten() {
        for player in &world.packet_senders {
            player.send_packet(packet);
        }
    }

    world.to_be_ticked.retain(|tick| !replaced(tick.pos));
    // Send block changes before we send block entity data, otherwise it'll be ignored
    world.flush_block_changes();
    for (pos, block_entity) in &cb.block_entities {
        world.set_block_entity(first_pos + *pos, block_entity.clone());
    }
}

fn replace_in_chunk(
    chunk: &mut Chunk,
    cb: &WorldEditClipboard,
    new_bounds: (BlockPos, BlockPos),
    old_bounds: Option<(BlockPos, BlockPos)>,
) {
    let (mut min, mut max) = new_bounds;
    if let Some((old_min, old_max)) = old_bounds {
        min = min.min(old_min);
        max = max.max(old_max);
    }
    let base_x = chunk.x * 16;
    let base_z = chunk.z * 16;
    let min = min.max(BlockPos::new(base_x, 0, base_z));
    let max = max.min(BlockPos::new(
        base_x + 15,
        chunk.sections.len() as i32 * 16 - 1,
        base_z + 15,
    ));
    if min.x > max.x || min.y > max.y || min.z > max.z {
        return;
    }

    let first_pos = new_bounds.0;
    for y in min.y..=max.y {
        for z in min.z..=max.z {
            for x in min.x..=max.x {
                let pos = BlockPos::new(x, y, z);
                let block = if in_bounds(pos, new_bounds) {
                    let rel = pos - first_pos;
                    let idx = (rel.y as u32 * cb.size_z + rel.z as u32) * cb.size_x + rel.x as u32;
                    cb.data.get_entry(idx as usize)
                } else if old_bounds.is_some_and(|b| in_bounds(pos, b)) {
                    0
                } else {
                    continue;
                };
                chunk.set_block((x & 0xF) as u32, y as u32, (z & 0xF) as u32, block);
            }
        }
    }
    chunk.block_entities.retain(|pos, _| {
        let pos = BlockPos::new(pos.x + base_x, pos.y, pos.z + base_z);
        !in_bounds(pos, new_bounds) && !old_bounds.is_some_and(|b| in_bounds(pos, b))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plot::PLOT_WIDTH;
    use mchprs_schematic::paste_clipboard;
    use mchprs_world::storage::PalettedBitBuffer;
    use mchprs_world::{TickEntry, TickPriority};

    fn empty_world() -> PlotWorld {
        let mut chunks = Vec::new();
        for chunk_x in 0..PLOT_WIDTH {
            for chunk_z in 0..PLOT_WIDTH {
                chunks.push(Plot::generate_chunk(8, chunk_x, chunk_z));
            }
        }
        PlotWorld {
            x: 0,
            z: 0,
            chunks,
            to_be_ticked: Vec::new(),
            packet_senders: Vec::new(),
        }
    }

    fn clipboard(size_x: u32, size_y: u32, size_z: u32, seed: u32) -> WorldEditClipboard {
        let len = (size_x * size_y * size_z) as usize;
        let mut data = PalettedBitBuffer::new(len, 9);
        for i in 0..len {
            data.set_entry(i, (i as u32 * 7 + seed) % 40);
        }
        WorldEditClipboard {
            offset_x: 3,
            offset_y: 0,
            offset_z: -2,
            size_x,
            size_y,
            size_z,
            data,
            block_entities: Default::default(),
        }
    }

    #[test]
    fn replace_region_matches_clear_and_paste() {
        let mut expected = empty_world();
        let mut actual = empty_world();
        let mut last_bounds: Option<(BlockPos, BlockPos)> = None;
        for (cb, first_pos) in [
            (clipboard(40, 20, 35, 1), BlockPos::new(10, FLOOR_Y, 20)),
            (clipboard(17, 30, 50, 2), BlockPos::new(30, FLOOR_Y, 5)),
        ] {
            let second_pos = first_pos
                + BlockPos::new(
                    cb.size_x as i32 - 1,
                    cb.size_y as i32 - 1,
                    cb.size_z as i32 - 1,
                );
            if let Some((min, max)) = last_bounds {
                for y in min.y..=max.y {
                    for z in min.z..=max.z {
                        for x in min.x..=max.x {
                            expected.set_block_raw(BlockPos::new(x, y, z), 0);
                        }
                    }
                }
            }
            let paste_pos = first_pos + BlockPos::new(cb.offset_x, cb.offset_y, cb.offset_z);
            paste_clipboard(&mut expected, &cb, paste_pos, false);

            let tick = |pos| TickEntry {
                pos,
                ticks_left: 1,
                tick_priority: TickPriority::Normal,
            };
            actual.to_be_ticked = vec![tick(first_pos), tick(BlockPos::new(500, 50, 500))];
            replace_region(&mut actual, &cb, first_pos, last_bounds);
            assert_eq!(actual.to_be_ticked.len(), 1);
            last_bounds = Some((first_pos, second_pos));
        }

        for (expected, actual) in expected.chunks.iter().zip(&actual.chunks) {
            for y in 0..expected.sections.len() as u32 * 16 {
                for z in 0..16 {
                    for x in 0..16 {
                        assert_eq!(expected.get_block(x, y, z), actual.get_block(x, y, z));
                    }
                }
            }
        }
    }
}
