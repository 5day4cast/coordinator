//! Read-only process and allocator accounting. RSS includes live allocations and retained
//! pages; glibc's free arena bytes are reusable, not necessarily resident or reclaimable.
use prometheus::{Gauge, Histogram, HistogramOpts, HistogramVec, Registry};
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
    registry.register(Box::new(CHECKPOINT.clone()))?;
    registry.register(Box::new(CHECKPOINT_BUFFER.clone()))?;
    Ok(())
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
