//! Returns the memory glibc keeps after bursts of work.
//!
//! glibc keeps what a thread frees in that thread's arena, and gives pages back to the kernel
//! only from the top of a heap. Kickoffs, payouts and refunds read confidential checkpoints of
//! up to 256 MiB on the blocking pool, so each burst raises arenas to their peak, and anything
//! still allocated above the freed space keeps that space there. Over a weekend of production
//! traffic the process held about 1.2 GiB of free arena space, resident or swapped, against
//! about 35 MiB in use. `malloc_trim` returns every whole free page in every arena, swapped
//! pages included.
//!
//! The trimmer reads the process's anonymous footprint every [`CHECK_INTERVAL`]. Once that
//! footprint has grown by [`TRIM_AFTER_GROWTH`] since the last trim and has stopped growing,
//! after a burst rather than during it, it trims. Growth that turns out to be live data is
//! trimmed for once and becomes the new baseline.
use crate::metrics::{allocator_trimmed, anonymous_footprint_bytes};
use log::{info, warn};
use std::time::Duration;
use tokio::time::MissedTickBehavior;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// How often the trimmer reads the process's anonymous footprint.
const CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Growth since the last trim, or since start, after which the trimmer acts once growth stops.
const TRIM_AFTER_GROWTH: u64 = 64 * 1024 * 1024;
/// Growth between two reads below which the work that grew the footprint counts as finished.
const SETTLED_WITHIN: u64 = 8 * 1024 * 1024;

/// When to trim, decided from successive reads of the anonymous footprint.
#[derive(Debug)]
struct TrimPolicy {
    /// The footprint just after the last trim, or the smallest read since, if lower.
    baseline: u64,
    /// The previous read.
    last: u64,
}

impl TrimPolicy {
    fn new(footprint: u64) -> Self {
        Self {
            baseline: footprint,
            last: footprint,
        }
    }

    /// Whether to trim now, given the footprint just read.
    fn should_trim(&mut self, footprint: u64) -> bool {
        let settled = footprint <= self.last.saturating_add(SETTLED_WITHIN);
        self.last = footprint;
        self.baseline = self.baseline.min(footprint);
        settled && footprint - self.baseline >= TRIM_AFTER_GROWTH
    }

    /// Measure growth again from the footprint read just after a trim.
    fn trimmed(&mut self, footprint: u64) {
        self.baseline = footprint;
        self.last = footprint;
    }
}

/// Return every whole free page in glibc's arenas to the kernel. It walks each arena's free
/// lists under that arena's lock, so run it off the async runtime.
fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: malloc_trim takes each arena's lock itself and only releases free memory.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Trim after bursts of work, until `cancel`. Nothing runs without glibc or `/proc`.
pub fn spawn_trimmer(tracker: &TaskTracker, cancel: CancellationToken) {
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return;
    }
    let Some(start) = anonymous_footprint_bytes() else {
        warn!("Cannot read this process's memory; freed memory stays with the allocator");
        return;
    };
    tracker.spawn(async move {
        let mut policy = TrimPolicy::new(start);
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            let Some(before) = anonymous_footprint_bytes() else {
                continue;
            };
            if !policy.should_trim(before) {
                continue;
            }
            let trim = tokio::task::spawn_blocking(|| {
                release_freed_memory();
                anonymous_footprint_bytes()
            });
            let Ok(after) = trim.await else {
                continue;
            };
            let after = after.unwrap_or(before);
            policy.trimmed(after);
            let released = before.saturating_sub(after);
            allocator_trimmed(released);
            info!(
                "Returned {} MiB of freed memory to the system; {} MiB remain",
                released / (1024 * 1024),
                after / (1024 * 1024)
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn growth_below_the_threshold_is_left_alone() {
        let mut policy = TrimPolicy::new(100 * MIB);
        assert!(!policy.should_trim(130 * MIB));
        assert!(!policy.should_trim(160 * MIB));
        assert!(
            !policy.should_trim(163 * MIB),
            "settled, but 63 MiB above the baseline"
        );
    }

    #[test]
    fn a_burst_is_trimmed_once_it_stops_growing() {
        let mut policy = TrimPolicy::new(100 * MIB);
        assert!(!policy.should_trim(400 * MIB), "still growing");
        assert!(!policy.should_trim(900 * MIB), "still growing");
        assert!(
            policy.should_trim(904 * MIB),
            "settled 804 MiB above the baseline"
        );
        policy.trimmed(150 * MIB);
        assert!(
            !policy.should_trim(150 * MIB),
            "nothing grew since the trim"
        );
        assert!(!policy.should_trim(200 * MIB));
        assert!(
            !policy.should_trim(204 * MIB),
            "54 MiB above the post-trim baseline"
        );
    }

    #[test]
    fn growth_a_trim_cannot_release_becomes_the_baseline() {
        let mut policy = TrimPolicy::new(100 * MIB);
        assert!(!policy.should_trim(300 * MIB));
        assert!(policy.should_trim(300 * MIB));
        policy.trimmed(290 * MIB);
        assert!(
            !policy.should_trim(290 * MIB),
            "live data is not trimmed again"
        );
        assert!(!policy.should_trim(300 * MIB));
        assert!(!policy.should_trim(350 * MIB), "still growing");
        assert!(policy.should_trim(354 * MIB), "64 MiB above 290 MiB");
    }

    #[test]
    fn the_baseline_follows_the_footprint_down() {
        let mut policy = TrimPolicy::new(500 * MIB);
        assert!(!policy.should_trim(200 * MIB));
        assert!(!policy.should_trim(260 * MIB), "still growing");
        assert!(
            policy.should_trim(264 * MIB),
            "64 MiB above the low of 200 MiB"
        );
    }

    /// glibc keeps freed blocks below a live one; a trim hands their pages back.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn a_trim_returns_freed_blocks_that_glibc_keeps() {
        const BLOCKS: usize = 192 * 1024;
        let mut blocks = Vec::with_capacity(BLOCKS);
        for i in 0..BLOCKS {
            // Written rather than zeroed, so every page is resident.
            blocks.push(vec![(i % 251) as u8 | 1; 1024]);
        }
        // Allocated after the blocks, so their space is not at the top of the heap.
        let pin = vec![1u8; 1024];
        std::hint::black_box(&blocks);
        let held = anonymous_footprint_bytes().unwrap();
        drop(blocks);
        release_freed_memory();
        let after = anonymous_footprint_bytes().unwrap();
        std::hint::black_box(&pin);
        assert!(
            held.saturating_sub(after) >= 96 * MIB,
            "held {held} bytes, {after} after the trim"
        );
    }
}
