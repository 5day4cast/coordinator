//! Read-only process and allocator accounting, plus counts of the trims
//! [`crate::infra::allocator`] makes. RSS includes live allocations and retained pages; glibc's
//! free arena bytes are reusable, not necessarily resident or reclaimable.
use prometheus::{
    Gauge, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry,
};
use std::{sync::LazyLock, time::Duration};
use tokio::{sync::Mutex, time::Instant};

use super::unknown;

/// How long one allocator read serves scrapes: see [`AllocatorSample`].
const ALLOCATOR_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// glibc's in-use, free arena and system bytes, as the allocator gauges report them.
type AllocatorBytes = (usize, usize, usize);

static RESIDENT: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_process_resident_bytes",
        "Coordinator process RSS, excluding other container processes",
    ))
});
static RESIDENT_PEAK: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_process_resident_peak_bytes",
        "Coordinator process RSS high-water mark since start",
    ))
});
static ANONYMOUS: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_process_anonymous_bytes",
        "Coordinator process resident anonymous pages",
    ))
});
static SWAP: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_process_swap_bytes",
        "Coordinator process anonymous pages in swap",
    ))
});
static ALLOCATED: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
    "coordinator_allocator_in_use_bytes", "glibc in-use arena chunks plus separately mapped allocations; not a Rust live-object count",
))
});
static FREE: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_allocator_free_arena_bytes",
        "glibc free arena space; includes nonresident pages and is not a reclaimable-RSS estimate",
    ))
});
static SYSTEM: LazyLock<Gauge> = LazyLock::new(|| {
    unknown(Gauge::new(
        "coordinator_allocator_system_bytes",
        "glibc arena space plus separately mapped allocations; not RSS",
    ))
});
static TRIMS: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "coordinator_allocator_trims_total",
        "Times the coordinator returned freed heap memory to the system after its memory grew",
    )
    .expect("valid metric")
});
static TRIMMED: LazyLock<IntCounter> = LazyLock::new(|| {
    IntCounter::new(
        "coordinator_allocator_trimmed_bytes_total",
        "Resident anonymous plus swapped bytes those trims released, measured around each trim",
    )
    .expect("valid metric")
});
static CHECKPOINT: LazyLock<HistogramVec> = LazyLock::new(|| {
    HistogramVec::new(
    HistogramOpts::new("coordinator_checkpoint_plaintext_bytes", "Confidential checkpoint plaintext bytes per write or authenticated read. Format-1 writes and all reads count the whole document; format-2 writes count only the fields and journal entries serialized for that write")
        .buckets(vec![65_536., 262_144., 1_048_576., 4_194_304., 16_777_216., 67_108_864., 268_435_456., 536_870_912.]),
    &["format", "operation"],
).expect("valid metric")
});

static CHECKPOINT_BUFFER: LazyLock<Histogram> = LazyLock::new(|| {
    Histogram::with_opts(
        HistogramOpts::new(
            "coordinator_checkpoint_encode_buffer_bytes",
            "Largest serialized plaintext buffer capacity in one partitioned checkpoint write",
        )
        .buckets(vec![
            65_536.,
            262_144.,
            1_048_576.,
            4_194_304.,
            16_777_216.,
            67_108_864.,
            268_435_456.,
            536_870_912.,
        ]),
    )
    .expect("valid metric")
});

pub(crate) fn checkpoint_encode_buffer_bytes(bytes: usize) {
    CHECKPOINT_BUFFER.observe(bytes as f64);
}

/// What each committed write stores, by kind. `row` is the sealed document in format 1, the
/// manifest in format 2 and the root in format 3.
const CHECKPOINT_WRITE_KINDS: [&str; 3] = ["row", "part", "node"];

static CHECKPOINT_WRITTEN: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "coordinator_checkpoint_written_bytes_total",
            "Bytes committed confidential checkpoint writes stored: the sealed row, new parts, and new format-3 manifest nodes",
        ),
        &["kind"],
    )
    .expect("valid metric")
});

/// Count `bytes` of one kind that a committed checkpoint write stored.
pub(crate) fn checkpoint_written_bytes(kind: &'static str, bytes: usize) {
    CHECKPOINT_WRITTEN
        .with_label_values(&[kind])
        .inc_by(bytes as u64);
}

/// What signing-state retention deletes, by kind: `row` is a checkpoint row, `part` a part or
/// format-3 manifest node.
const CHECKPOINT_DELETE_KINDS: [&str; 2] = ["row", "part"];

static CHECKPOINT_DELETED_ROWS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "coordinator_checkpoint_deleted_rows_total",
            "Checkpoint rows, and their parts and manifest nodes, deleted once their competition was paid out",
        ),
        &["kind"],
    )
    .expect("valid metric")
});

static CHECKPOINT_DELETED_BYTES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        Opts::new(
            "coordinator_checkpoint_deleted_bytes_total",
            "Sealed bytes of the checkpoint rows, parts and manifest nodes deleted once their competition was paid out",
        ),
        &["kind"],
    )
    .expect("valid metric")
});

/// Paid-out competitions whose signing state is still stored, by `state`: `waiting` out the
/// retention period, or `due` for deletion.
const CHECKPOINT_RETENTION_STATES: [&str; 2] = ["waiting", "due"];

static CHECKPOINT_RETENTION: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    IntGaugeVec::new(
        Opts::new(
            "coordinator_checkpoint_retention_competitions",
            "Fully paid-out competitions whose signing state is still stored: waiting out the retention period, or due for deletion",
        ),
        &["state"],
    )
    .expect("valid metric")
});

static DATABASE_FREE: LazyLock<IntGauge> = LazyLock::new(|| {
    IntGauge::new(
        "coordinator_competitions_db_free_bytes",
        "Bytes of free pages in the competitions database, which new writes reuse before the file grows",
    )
    .expect("valid metric")
});

/// Count `rows` of one kind, with their sealed `bytes`, that signing-state retention deleted.
pub(crate) fn checkpoint_deleted(kind: &'static str, rows: u64, bytes: u64) {
    CHECKPOINT_DELETED_ROWS
        .with_label_values(&[kind])
        .inc_by(rows);
    CHECKPOINT_DELETED_BYTES
        .with_label_values(&[kind])
        .inc_by(bytes);
}

/// Record how many paid-out competitions' signing state waits out its retention period, and
/// how many are due for deletion.
pub(crate) fn checkpoint_retention(waiting: usize, due: usize) {
    for (state, competitions) in CHECKPOINT_RETENTION_STATES.into_iter().zip([waiting, due]) {
        CHECKPOINT_RETENTION
            .with_label_values(&[state])
            .set(i64::try_from(competitions).unwrap_or(i64::MAX));
    }
}

/// Record the bytes of the competitions database's free pages.
pub(crate) fn competitions_db_free_bytes(bytes: i64) {
    DATABASE_FREE.set(bytes);
}

pub(super) fn register(registry: &Registry) -> prometheus::Result<()> {
    for gauge in [
        &*RESIDENT,
        &*RESIDENT_PEAK,
        &*ANONYMOUS,
        &*SWAP,
        &*ALLOCATED,
        &*FREE,
        &*SYSTEM,
    ] {
        registry.register(Box::new(gauge.clone()))?;
    }
    registry.register(Box::new(TRIMS.clone()))?;
    registry.register(Box::new(TRIMMED.clone()))?;
    registry.register(Box::new(CHECKPOINT.clone()))?;
    registry.register(Box::new(CHECKPOINT_BUFFER.clone()))?;
    for kind in CHECKPOINT_WRITE_KINDS {
        CHECKPOINT_WRITTEN.with_label_values(&[kind]);
    }
    registry.register(Box::new(CHECKPOINT_WRITTEN.clone()))?;
    for kind in CHECKPOINT_DELETE_KINDS {
        CHECKPOINT_DELETED_ROWS.with_label_values(&[kind]);
        CHECKPOINT_DELETED_BYTES.with_label_values(&[kind]);
    }
    registry.register(Box::new(CHECKPOINT_DELETED_ROWS.clone()))?;
    registry.register(Box::new(CHECKPOINT_DELETED_BYTES.clone()))?;
    for state in CHECKPOINT_RETENTION_STATES {
        CHECKPOINT_RETENTION.with_label_values(&[state]);
    }
    registry.register(Box::new(CHECKPOINT_RETENTION.clone()))?;
    registry.register(Box::new(DATABASE_FREE.clone()))?;
    Ok(())
}

/// Count one trim, and the drop in [`anonymous_footprint_bytes`] across it.
pub(crate) fn allocator_trimmed(released: u64) {
    TRIMS.inc();
    TRIMMED.inc_by(released);
}

/// Resident anonymous pages plus swapped ones: what the process holds whether or not the
/// kernel has moved it to swap. `None` where `/proc/self/status` cannot be read.
pub(crate) fn anonymous_footprint_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status_bytes(&status, "RssAnon")?.checked_add(status_bytes(&status, "VmSwap")?)
}

/// Fixed labels only; checkpoint contents and session identifiers never become metrics.
pub(crate) fn checkpoint_bytes(format: &'static str, operation: &'static str, bytes: usize) {
    CHECKPOINT
        .with_label_values(&[format, operation])
        .observe(bytes as f64);
}

fn status_bytes(status: &str, field: &str) -> Option<u64> {
    let value = status.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name == field).then_some(value)
    })?;
    let mut words = value.split_whitespace();
    let kib: u64 = words.next()?.parse().ok()?;
    (words.next()? == "kB").then_some(())?;
    kib.checked_mul(1024)
}

/// The last allocator read, and when it was taken.
///
/// `mallinfo2` walks the free lists of every arena while holding that arena's lock. A read
/// costs most, and stalls allocating threads longest, when the heap is fragmented: just when
/// these gauges matter. Scrapes therefore share one read for [`ALLOCATOR_REFRESH_INTERVAL`],
/// taken on the blocking pool instead of an async worker.
#[derive(Default)]
pub(super) struct AllocatorSample(Mutex<Option<(Instant, Option<AllocatorBytes>)>>);

impl AllocatorSample {
    /// What `read` returns, or the last read while it is younger than
    /// [`ALLOCATOR_REFRESH_INTERVAL`]. Concurrent scrapes wait for one read.
    async fn bytes(
        &self,
        read: impl FnOnce() -> Option<AllocatorBytes> + Send + 'static,
    ) -> Option<AllocatorBytes> {
        let mut last = self.0.lock().await;
        if let Some((at, bytes)) = *last {
            if at.elapsed() < ALLOCATOR_REFRESH_INTERVAL {
                return bytes;
            }
        }
        // A read that panicked keeps the gauges as they were; the next scrape reads again.
        let bytes = tokio::task::spawn_blocking(read).await.ok()?;
        *last = Some((Instant::now(), bytes));
        bytes
    }
}

/// Refresh the process gauges, and the allocator gauges from `allocator`.
pub(super) async fn refresh(allocator: &AllocatorSample) {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for (gauge, field) in [
            (&*RESIDENT, "VmRSS"),
            (&*RESIDENT_PEAK, "VmHWM"),
            (&*ANONYMOUS, "RssAnon"),
            (&*SWAP, "VmSwap"),
        ] {
            gauge.set(status_bytes(&status, field).map_or(f64::NAN, |bytes| bytes as f64));
        }
    }
    if let Some((allocated, free, system)) = allocator.bytes(allocator_bytes).await {
        ALLOCATED.set(allocated as f64);
        FREE.set(free as f64);
        SYSTEM.set(system as f64);
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_bytes() -> Option<AllocatorBytes> {
    // glibc mallinfo2 uses ten size_t fields, in this ABI order. It takes its own locks.
    // This reads accounting only: no malloc_trim, allocator tuning, or process attachment.
    #[repr(C)]
    struct Mallinfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }
    unsafe extern "C" {
        fn mallinfo2() -> Mallinfo2;
    }
    // SAFETY: mallinfo2 has no arguments, is thread-safe, and returns the C layout above.
    let info = unsafe { mallinfo2() };
    Some((
        info.uordblks.checked_add(info.hblkhd)?,
        info.fordblks,
        info.arena.checked_add(info.hblkhd)?,
    ))
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_bytes() -> Option<AllocatorBytes> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn process_status_distinguishes_resident_anonymous_swap_and_high_water() {
        let status = "Name:\tcoordinator\nVmHWM:\t8192 kB\nVmRSS:\t4096 kB\nRssAnon:\t3072 kB\nVmSwap:\t512 kB\n";
        assert_eq!(status_bytes(status, "VmRSS"), Some(4 * 1024 * 1024));
        assert_eq!(status_bytes(status, "VmHWM"), Some(8 * 1024 * 1024));
        assert_eq!(status_bytes(status, "RssAnon"), Some(3 * 1024 * 1024));
        assert_eq!(status_bytes(status, "VmSwap"), Some(512 * 1024));
        assert_eq!(status_bytes(status, "unknown"), None);
        assert_eq!(status_bytes("VmRSS: 8 MB", "VmRSS"), None);
        assert_eq!(
            status_bytes("VmRSS: 18446744073709551615 kB", "VmRSS"),
            None
        );
    }

    #[test]
    fn checkpoint_metrics_describe_size_without_session_labels() {
        let registry = Registry::new();
        register(&registry).unwrap();
        checkpoint_bytes("parts", "decode", 70_000);
        let families = registry.gather();
        let family = families
            .iter()
            .find(|m| m.get_name() == "coordinator_checkpoint_plaintext_bytes")
            .unwrap();
        let metric = family
            .get_metric()
            .iter()
            .find(|m| {
                m.get_label()
                    .iter()
                    .any(|l| l.get_name() == "operation" && l.get_value() == "decode")
            })
            .unwrap();
        assert_eq!(metric.get_label().len(), 2);
        assert!(metric.get_histogram().get_sample_count() >= 1);
        assert!(metric.get_histogram().get_sample_sum() >= 70_000.);
    }

    #[test]
    fn checkpoint_writes_count_stored_bytes_by_kind_from_startup() {
        let registry = Registry::new();
        register(&registry).unwrap();
        let written = |kind: &str| {
            registry
                .gather()
                .iter()
                .find(|m| m.get_name() == "coordinator_checkpoint_written_bytes_total")
                .unwrap()
                .get_metric()
                .iter()
                .find(|m| m.get_label()[0].get_value() == kind)
                .map(|m| m.get_counter().get_value())
        };
        for kind in CHECKPOINT_WRITE_KINDS {
            assert!(
                written(kind).is_some(),
                "{kind} is exported before any write"
            );
        }
        let before = written("node").unwrap();
        checkpoint_written_bytes("node", 4096);
        assert!(written("node").unwrap() >= before + 4096.);
    }

    #[test]
    fn checkpoint_deletions_count_rows_and_bytes_by_kind_from_startup() {
        let registry = Registry::new();
        register(&registry).unwrap();
        let counted = |name: &str, kind: &str| {
            registry
                .gather()
                .iter()
                .find(|m| m.get_name() == name)
                .unwrap()
                .get_metric()
                .iter()
                .find(|m| m.get_label()[0].get_value() == kind)
                .map(|m| m.get_counter().get_value())
        };
        for kind in CHECKPOINT_DELETE_KINDS {
            for name in [
                "coordinator_checkpoint_deleted_rows_total",
                "coordinator_checkpoint_deleted_bytes_total",
            ] {
                assert!(
                    counted(name, kind).is_some(),
                    "{name}{{kind={kind}}} is exported before any deletion"
                );
            }
        }
        let rows = counted("coordinator_checkpoint_deleted_rows_total", "part").unwrap();
        let bytes = counted("coordinator_checkpoint_deleted_bytes_total", "part").unwrap();
        checkpoint_deleted("part", 3, 12_000);
        assert!(counted("coordinator_checkpoint_deleted_rows_total", "part").unwrap() >= rows + 3.);
        assert!(
            counted("coordinator_checkpoint_deleted_bytes_total", "part").unwrap()
                >= bytes + 12_000.
        );

        let gauge = |name: &str, state: Option<&str>| {
            registry
                .gather()
                .iter()
                .find(|m| m.get_name() == name)
                .and_then(|m| {
                    m.get_metric()
                        .iter()
                        .find(|m| state.is_none_or(|state| m.get_label()[0].get_value() == state))
                        .map(|m| m.get_gauge().get_value())
                })
        };
        for state in CHECKPOINT_RETENTION_STATES {
            assert!(
                gauge("coordinator_checkpoint_retention_competitions", Some(state)).is_some(),
                "{state} is exported before any pass"
            );
        }
        checkpoint_retention(4, 1);
        competitions_db_free_bytes(8192);
        let waiting = gauge(
            "coordinator_checkpoint_retention_competitions",
            Some("waiting"),
        );
        assert!(waiting.is_some());
        assert!(gauge("coordinator_competitions_db_free_bytes", None).is_some());
    }

    #[test]
    fn trims_are_counted_with_the_bytes_they_released() {
        let registry = Registry::new();
        register(&registry).unwrap();
        allocator_trimmed(3 * 1024 * 1024);
        let families = registry.gather();
        let counter = |name: &str| {
            families
                .iter()
                .find(|m| m.get_name() == name)
                .unwrap()
                .get_metric()[0]
                .get_counter()
                .get_value()
        };
        assert!(counter("coordinator_allocator_trims_total") >= 1.);
        assert!(counter("coordinator_allocator_trimmed_bytes_total") >= 3. * 1024. * 1024.);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_anonymous_footprint_is_readable_on_the_deployment_target() {
        assert!(anonymous_footprint_bytes().is_some_and(|bytes| bytes > 0));
    }

    #[tokio::test(start_paused = true)]
    async fn allocator_reads_are_reused_until_the_refresh_interval_passes() {
        let sample = AllocatorSample::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let read = |bytes: AllocatorBytes| {
            let reads = Arc::clone(&reads);
            move || {
                reads.fetch_add(1, Ordering::SeqCst);
                Some(bytes)
            }
        };
        assert_eq!(sample.bytes(read((1, 2, 3))).await, Some((1, 2, 3)));
        tokio::time::advance(ALLOCATOR_REFRESH_INTERVAL - Duration::from_millis(1)).await;
        assert_eq!(
            sample.bytes(read((4, 5, 6))).await,
            Some((1, 2, 3)),
            "a scrape within the interval reuses the last read"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(sample.bytes(read((4, 5, 6))).await, Some((4, 5, 6)));
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn glibc_accounting_is_available_on_the_deployment_target() {
        let (allocated, free, system) = allocator_bytes().unwrap();
        assert!(allocated > 0);
        assert_eq!(allocated.checked_add(free), Some(system));
    }
}
