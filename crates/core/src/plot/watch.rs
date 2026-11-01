//! Watches a schematic file and automatically pastes it into the plot whenever it changes.
//! The schematic is centered horizontally in the plot and placed on top of the plot floor.
//! A server file next to the schematic is watched and reloaded with it.

use super::probes::server_file_path;
use super::{worldedit, Plot, PLOT_BLOCK_HEIGHT, PLOT_BLOCK_WIDTH};
use crate::player::PacketSender;
use mchprs_blocks::BlockPos;
use mchprs_redpiler::CompilerOptions;
use mchprs_schematic::{load_schematic, paste_clipboard};
use mchprs_world::World;
use std::fs;
use std::path::{Path, PathBuf};
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
    /// The modification times of the last loaded versions of the files
    loaded_mtime: Option<WatchedTimes>,
    /// Modification times that have been seen but not loaded yet. The files are only loaded once
    /// their modification times have been stable for one poll, so that partially written files
    /// aren't loaded.
    pending_mtime: Option<WatchedTimes>,
    /// The region the last version was pasted into, which is cleared before the next paste
    last_bounds: Option<(BlockPos, BlockPos)>,
    last_check: Instant,
}

impl SchematicWatch {
    pub fn new(
        path: PathBuf,
        compile_options: Option<CompilerOptions>,
        polling: bool,
    ) -> SchematicWatch {
        let loaded_mtime = watched_times(&path);
        SchematicWatch {
            path,
            compile_options,
            polling,
            loaded_mtime,
            pending_mtime: None,
            last_bounds: None,
            last_check: Instant::now() - POLL_INTERVAL,
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

    pub(super) fn reload_watched_schematic(&mut self) {
        let Some(watch) = &self.schematic_watch else {
            return;
        };
        let start_time = Instant::now();
        let file_name = watch
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let cb = match load_schematic(&watch.path) {
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

        let (plot_min, plot_max) = self.world.get_corners();
        if let Some((first_pos, second_pos)) = last_bounds {
            let first_pos = first_pos.max(plot_min);
            let second_pos = second_pos.min(plot_max);
            for y in first_pos.y..=second_pos.y {
                for z in first_pos.z..=second_pos.z {
                    for x in first_pos.x..=second_pos.x {
                        let pos = BlockPos::new(x, y, z);
                        self.world.set_block_raw(pos, 0);
                        self.world.delete_block_entity(pos);
                    }
                }
            }
        }

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
        let paste_pos = first_pos + BlockPos::new(cb.offset_x, cb.offset_y, cb.offset_z);
        paste_clipboard(&mut self.world, &cb, paste_pos, false);
        worldedit::update(&mut self.world, first_pos, second_pos);
        self.world.flush_block_changes();
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
