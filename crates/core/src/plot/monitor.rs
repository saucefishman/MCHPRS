use mchprs_save_data::plot_data::Tps;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Default)]
struct AtomicTps {
    tps: AtomicU32,
    unlimited: AtomicBool,
}

impl AtomicTps {
    fn from_tps(tps: Tps) -> Self {
        match tps {
            Tps::Limited(tps) => AtomicTps {
                tps: AtomicU32::new(tps),
                unlimited: AtomicBool::new(false),
            },
            Tps::Unlimited => AtomicTps {
                tps: AtomicU32::new(0),
                unlimited: AtomicBool::new(true),
            },
        }
    }

    fn update(&self, tps: Tps) {
        match tps {
            Tps::Limited(tps) => {
                self.tps.store(tps, Ordering::Relaxed);
                self.unlimited.store(false, Ordering::Relaxed);
            }
            Tps::Unlimited => self.unlimited.store(true, Ordering::Relaxed),
        }
    }
}

struct MonitorData {
    tps: AtomicTps,
    ticks_passed: Arc<AtomicU64>,
    reset_timings: AtomicU32,
    too_slow: AtomicBool,
    ticking: AtomicBool,
    running: AtomicBool,
    timings_record: Mutex<VecDeque<u32>>,
    lag_events: Mutex<Vec<LagEvent>>,
}

/// How many 500ms intervals the measured rate must stay below, or back at, the lag threshold before
/// the plot says so
const LAG_INTERVALS: usize = 10;
/// The share of the target rtps, in percent, under which a plot counts as lagging
const LAG_PERCENT: u64 = 90;

/// A change in whether a plot keeps up with its target rtps, for the plot to report with a likely
/// cause
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LagEvent {
    /// The rate over the last 5 seconds stayed below 90% of the target
    Behind { measured: u32, target: u32 },
    /// The rate stayed at 90% of the target or more for 5 seconds after being behind
    Recovered { measured: u32, target: u32 },
}

#[derive(Debug)]
pub struct TimingsReport {
    pub five_s: f32,
    pub ten_s: f32,
    pub one_m: f32,
    pub five_m: f32,
    pub fifteen_m: f32,
}

pub struct TimingsMonitor {
    data: Arc<MonitorData>,
    monitor_thread: Option<JoinHandle<()>>,
}

impl TimingsMonitor {
    /// `label` identifies the plot in log messages.
    pub fn new(tps: Tps, label: String) -> TimingsMonitor {
        let data = Arc::new(MonitorData {
            ticks_passed: Default::default(),
            reset_timings: Default::default(),
            running: AtomicBool::new(true),
            too_slow: Default::default(),
            ticking: Default::default(),
            timings_record: Default::default(),
            lag_events: Default::default(),
            tps: AtomicTps::from_tps(tps),
        });
        let monitor_thread = Some(Self::run_thread(data.clone(), label));
        TimingsMonitor {
            data,
            monitor_thread,
        }
    }

    pub fn stop(&mut self) {
        self.data.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.monitor_thread.take()
            && handle.join().is_err()
        {
            warn!("Failed to join monitor thread handle");
        }
    }

    pub fn generate_report(&self) -> Option<TimingsReport> {
        let records = self.data.timings_record.lock().unwrap();
        if records.is_empty() {
            return None;
        }

        let mut ticks_5s = 0;
        let mut ticks_10s = 0;
        let mut ticks_1m = 0;
        let mut ticks_5m = 0;
        let mut ticks_15m = 0;
        // TODO: https://github.com/rust-lang/rust-clippy/issues/8987
        #[allow(clippy::significant_drop_in_scrutinee)]
        for (i, ticks) in records.iter().enumerate() {
            if i < LAG_INTERVALS {
                ticks_5s += *ticks;
            }
            if i < 20 {
                ticks_10s += *ticks;
            }
            if i < 120 {
                ticks_1m += *ticks;
            }
            if i < 600 {
                ticks_5m += *ticks;
            }
            ticks_15m += *ticks;
        }

        Some(TimingsReport {
            five_s: ticks_5s as f32 / records.len().min(LAG_INTERVALS) as f32 * 2.0,
            ten_s: ticks_10s as f32 / records.len().min(20) as f32 * 2.0,
            one_m: ticks_1m as f32 / records.len().min(120) as f32 * 2.0,
            five_m: ticks_5m as f32 / records.len().min(600) as f32 * 2.0,
            fifteen_m: ticks_15m as f32 / records.len() as f32 * 2.0,
        })
    }

    pub fn set_tps(&self, new_tps: Tps) {
        self.data.tps.update(new_tps);
        self.data.too_slow.store(false, Ordering::Relaxed);
    }

    pub fn tick(&self) {
        self.data.ticks_passed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn tickn(&self, ticks: u64) {
        self.data.ticks_passed.fetch_add(ticks, Ordering::Relaxed);
    }

    /// Changes in whether the plot keeps up with its target since the last call
    pub fn drain_lag_events(&self) -> Vec<LagEvent> {
        std::mem::take(&mut *self.data.lag_events.lock().unwrap())
    }

    pub fn is_running_behind(&self) -> bool {
        self.data.too_slow.load(Ordering::Relaxed)
    }

    pub fn set_ticking(&self, ticking: bool) {
        self.data.ticking.store(ticking, Ordering::Relaxed);
    }

    pub fn reset_timings(&self) {
        self.data.reset_timings.store(4, Ordering::Relaxed);
    }

    fn run_thread(data: Arc<MonitorData>, label: String) -> JoinHandle<()> {
        thread::spawn(move || {
            let mut last_tps = data.tps.tps.load(Ordering::Relaxed);
            let mut last_ticks_count = data.ticks_passed.load(Ordering::Relaxed);
            let mut was_ticking_before = data.ticking.load(Ordering::Relaxed);

            let mut behind_for = 0;
            // The last `LAG_INTERVALS` intervals, newest first, and whether the plot was last
            // reported as behind
            let mut recent: VecDeque<u32> = VecDeque::new();
            let mut lagging = false;
            // Ticks and intervals sampled for the one-off unlimited rtps report
            let mut was_unlimited = data.tps.unlimited.load(Ordering::Relaxed);
            let mut unlimited_ticks = 0u64;
            let mut unlimited_intervals = 0u64;
            loop {
                thread::sleep(Duration::from_millis(500));
                if !data.running.load(Ordering::Relaxed) {
                    return;
                }

                let ticks_count = data.ticks_passed.load(Ordering::Relaxed);
                if ticks_count == 0 {
                    continue;
                }
                let ticks_passed = (ticks_count - last_ticks_count) as u32;
                last_ticks_count = ticks_count;

                let tps = data.tps.tps.load(Ordering::Relaxed);
                let ticking = data.ticking.load(Ordering::Relaxed);
                if !(ticking && was_ticking_before)
                    || tps != last_tps
                    || data.reset_timings.load(Ordering::Relaxed) > 0
                {
                    let _ = data.reset_timings.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |n| n.checked_sub(1),
                    );
                    was_ticking_before = ticking;
                    last_tps = tps;
                    // A new target, or a plot that wasn't ticking, starts measuring again
                    recent.clear();
                    lagging = false;
                    continue;
                }

                let unlimited = data.tps.unlimited.load(Ordering::Relaxed);
                // 5% threshold
                if unlimited || ticks_passed < (tps / 2) * 95 / 100 {
                    behind_for += 1;
                } else {
                    behind_for = 0;
                    data.too_slow.store(false, Ordering::Relaxed);
                }

                if !unlimited {
                    recent.push_front(ticks_passed);
                    recent.truncate(LAG_INTERVALS);
                    if let Some(event) = lag_event(&recent, lagging, tps) {
                        lagging = matches!(event, LagEvent::Behind { .. });
                        data.lag_events.lock().unwrap().push(event);
                    }
                }

                if unlimited != was_unlimited {
                    was_unlimited = unlimited;
                    recent.clear();
                    lagging = false;
                    unlimited_ticks = 0;
                    unlimited_intervals = 0;
                }

                if behind_for >= 3 {
                    data.too_slow.store(true, Ordering::Relaxed);
                    // Unlimited rtps has no target to miss, so instead log the average
                    // rtps actually achieved, sampled once over the first 5 seconds.
                    if unlimited && unlimited_intervals < 10 {
                        unlimited_ticks += ticks_passed as u64;
                        unlimited_intervals += 1;
                        if unlimited_intervals == 10 {
                            info!(
                                "Plot {} is running at {} rtps (unlimited)",
                                label,
                                unlimited_ticks * 2 / unlimited_intervals
                            );
                        }
                    }
                }

                // The timings record will only go back 15 minutes.
                // This means that, with the 500ms interval, the timings record will
                // have a max size of 1800 entries.
                let mut timings_record = data.timings_record.lock().unwrap();
                if timings_record.len() == 1800 {
                    timings_record.pop_back();
                }
                timings_record.push_front(ticks_passed);
            }
        })
    }
}

/// Whether `recent`, the ticks run in each of the last 500ms intervals with the newest first, show
/// a plot falling behind `tps` or catching up again. `lagging` is whether it was last reported as
/// behind.
fn lag_event(recent: &VecDeque<u32>, lagging: bool, tps: u32) -> Option<LagEvent> {
    if recent.len() < LAG_INTERVALS {
        return None;
    }
    let threshold = tps as u64 * LAG_PERCENT / 100;
    let below = |ticks: &u32| (*ticks as u64 * 2) < threshold;
    let measured =
        (recent.iter().map(|&t| t as u64).sum::<u64>() * 2 / LAG_INTERVALS as u64) as u32;
    let target = tps;
    if !lagging && recent.iter().all(below) {
        Some(LagEvent::Behind { measured, target })
    } else if lagging && !recent.iter().any(below) {
        Some(LagEvent::Recovered { measured, target })
    } else {
        None
    }
}

impl Drop for TimingsMonitor {
    fn drop(&mut self) {
        // Joining the thread in drop is a bad idea so we just let it detach
        self.data.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intervals(ticks: &[u32]) -> VecDeque<u32> {
        ticks.iter().copied().collect()
    }

    #[test]
    fn lag_is_reported_after_five_seconds_below_ninety_percent() {
        // 400 ticks per 500ms is 800 rtps, under 90% of 1000
        assert_eq!(lag_event(&intervals(&[400; 9]), false, 1000), None);
        assert_eq!(
            lag_event(&intervals(&[400; 10]), false, 1000),
            Some(LagEvent::Behind {
                measured: 800,
                target: 1000
            })
        );
        // One interval at 90% is enough to hold off the warning
        let mut mixed = intervals(&[400; 10]);
        mixed[3] = 450;
        assert_eq!(lag_event(&mixed, false, 1000), None);
        // Already reported
        assert_eq!(lag_event(&intervals(&[400; 10]), true, 1000), None);
    }

    #[test]
    fn recovery_needs_five_seconds_at_ninety_percent() {
        let mut recovering = intervals(&[500; 10]);
        recovering[9] = 400;
        assert_eq!(lag_event(&recovering, true, 1000), None);
        assert_eq!(
            lag_event(&intervals(&[450; 10]), true, 1000),
            Some(LagEvent::Recovered {
                measured: 900,
                target: 1000
            })
        );
        assert_eq!(lag_event(&intervals(&[500; 10]), false, 1000), None);
    }
}
