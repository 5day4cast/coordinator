//! Following each awaited hold invoice: its own LND stream first, a slow poll as the fallback.
//!
//! A swap awaiting payment has a stream on its invoice from its first tick until it leaves
//! `AwaitingPayment`. While the stream is up the invoice is looked up at most every
//! `POLL_WHILE_STREAMING`, in case an update was missed. Once it ends or fails, the swap falls
//! back to a lookup every `POLL_WITHOUT_STREAM`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::lnd::InvoiceState;

/// How often an invoice whose stream is up is looked up as well.
pub const POLL_WHILE_STREAMING: Duration = Duration::from_secs(30);
/// How often an invoice whose stream has ended or failed is looked up.
pub const POLL_WITHOUT_STREAM: Duration = Duration::from_secs(5);

/// What a swap asks LND about its hold invoice. `Lnd` answers; the tests stand in for it.
pub trait InvoiceSource: Clone + Send + Sync + 'static {
    fn invoice_state(
        &self,
        payment_hash: &[u8; 32],
    ) -> impl Future<Output = anyhow::Result<InvoiceState>> + Send;

    /// Record each state the invoice's stream shows in `seen`, until the stream ends.
    fn follow_invoice(
        &self,
        payment_hash: [u8; 32],
        seen: SeenState,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// The furthest state an invoice was seen in, by its stream or a lookup.
#[derive(Clone, Default)]
pub struct SeenState(Arc<Mutex<Option<InvoiceState>>>);

impl SeenState {
    /// Record `state`, unless the invoice was already seen further along. A lookup that
    /// started before the stream showed `Accepted` must not take the swap back to `Open`.
    pub fn record(&self, state: InvoiceState) {
        let mut seen = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if seen.is_none_or(|seen| progress(state) > progress(seen)) {
            *seen = Some(state);
        }
    }

    fn get(&self) -> Option<InvoiceState> {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

fn progress(state: InvoiceState) -> u8 {
    match state {
        InvoiceState::Open => 0,
        InvoiceState::Accepted => 1,
        InvoiceState::Settled | InvoiceState::Canceled => 2,
    }
}

/// The invoices of the swaps awaiting payment, each followed on its own stream.
#[derive(Default)]
pub struct InvoiceWatches(Mutex<HashMap<Uuid, Watch>>);

struct Watch {
    seen: SeenState,
    streaming: Arc<AtomicBool>,
    polled_at: Option<Instant>,
    stream: tokio::task::JoinHandle<()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stream.abort();
    }
}

impl InvoiceWatches {
    /// The furthest state `swap`'s invoice has been seen in at `now`, starting its stream the
    /// first time. The invoice is looked up only when a lookup is due, so this is `None` only
    /// until the stream or the first lookup has shown a state.
    pub async fn state<L: InvoiceSource>(
        &self,
        lnd: &L,
        swap: Uuid,
        payment_hash: [u8; 32],
        now: Instant,
    ) -> anyhow::Result<Option<InvoiceState>> {
        let (seen, poll) = {
            let mut watches = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
            let watch = watches
                .entry(swap)
                .or_insert_with(|| start(lnd, swap, payment_hash));
            let every = if watch.streaming.load(Ordering::SeqCst) {
                POLL_WHILE_STREAMING
            } else {
                POLL_WITHOUT_STREAM
            };
            let poll = watch
                .polled_at
                .is_none_or(|at| now.saturating_duration_since(at) >= every);
            if poll {
                watch.polled_at = Some(now);
            }
            (watch.seen.clone(), poll)
        };
        if poll {
            seen.record(lnd.invoice_state(&payment_hash).await?);
        }
        Ok(seen.get())
    }

    /// Drop the streams of swaps no longer awaiting payment.
    pub fn keep_only(&self, awaiting: &HashSet<Uuid>) {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|swap, _| awaiting.contains(swap));
    }
}

fn start<L: InvoiceSource>(lnd: &L, swap: Uuid, payment_hash: [u8; 32]) -> Watch {
    let seen = SeenState::default();
    let streaming = Arc::new(AtomicBool::new(true));
    let stream = {
        let lnd = lnd.clone();
        let seen = seen.clone();
        let streaming = streaming.clone();
        tokio::spawn(async move {
            match lnd.follow_invoice(payment_hash, seen).await {
                Ok(()) => log::debug!("swap {swap}: its invoice stream ended; polling instead"),
                Err(error) => {
                    log::debug!(
                        "swap {swap}: its invoice stream failed: {error:#}; polling instead"
                    )
                }
            }
            streaming.store(false, Ordering::SeqCst);
        })
    };
    Watch {
        seen,
        streaming,
        polled_at: None,
        stream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A fake LND: lookups answer `polled` and are counted; the stream shows what the test
    /// sends, and ends when the test drops its sender.
    #[derive(Clone)]
    struct FakeLnd {
        polled: Arc<Mutex<InvoiceState>>,
        polls: Arc<AtomicUsize>,
        stream: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<InvoiceState>>>>,
    }

    impl FakeLnd {
        fn new(polled: InvoiceState) -> (Self, tokio::sync::mpsc::UnboundedSender<InvoiceState>) {
            let (send, receive) = tokio::sync::mpsc::unbounded_channel();
            let lnd = Self {
                polled: Arc::new(Mutex::new(polled)),
                polls: Default::default(),
                stream: Arc::new(Mutex::new(Some(receive))),
            };
            (lnd, send)
        }

        fn polls(&self) -> usize {
            self.polls.load(Ordering::SeqCst)
        }
    }

    impl InvoiceSource for FakeLnd {
        async fn invoice_state(&self, _: &[u8; 32]) -> anyhow::Result<InvoiceState> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(*self.polled.lock().unwrap())
        }

        async fn follow_invoice(&self, _: [u8; 32], seen: SeenState) -> anyhow::Result<()> {
            let receive = self.stream.lock().unwrap().take();
            let Some(mut receive) = receive else {
                anyhow::bail!("already subscribed");
            };
            while let Some(state) = receive.recv().await {
                seen.record(state);
            }
            Ok(())
        }
    }

    const HASH: [u8; 32] = [7; 32];

    /// Let the stream task take what was sent.
    async fn settle_down() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn an_invoice_accepted_on_the_stream_advances_the_swap_without_a_lookup() {
        let (lnd, stream) = FakeLnd::new(InvoiceState::Open);
        let watches = InvoiceWatches::default();
        let swap = Uuid::now_v7();
        let start = Instant::now();

        assert_eq!(
            watches.state(&lnd, swap, HASH, start).await.unwrap(),
            Some(InvoiceState::Open)
        );
        assert_eq!(lnd.polls(), 1, "the first tick looks the invoice up once");

        stream.send(InvoiceState::Accepted).unwrap();
        settle_down().await;
        assert_eq!(
            watches
                .state(&lnd, swap, HASH, start + Duration::from_secs(1))
                .await
                .unwrap(),
            Some(InvoiceState::Accepted),
            "the next tick pays the escrow"
        );
        assert_eq!(lnd.polls(), 1);
    }

    #[tokio::test]
    async fn with_the_stream_down_the_lookup_advances_the_swap() {
        let (lnd, stream) = FakeLnd::new(InvoiceState::Open);
        let watches = InvoiceWatches::default();
        let swap = Uuid::now_v7();
        let start = Instant::now();

        watches.state(&lnd, swap, HASH, start).await.unwrap();
        drop(stream);
        settle_down().await;
        *lnd.polled.lock().unwrap() = InvoiceState::Accepted;

        assert_eq!(
            watches
                .state(&lnd, swap, HASH, start + Duration::from_secs(4))
                .await
                .unwrap(),
            Some(InvoiceState::Open),
            "not due yet"
        );
        assert_eq!(
            watches
                .state(&lnd, swap, HASH, start + POLL_WITHOUT_STREAM)
                .await
                .unwrap(),
            Some(InvoiceState::Accepted)
        );
        assert_eq!(lnd.polls(), 2);
    }

    #[tokio::test]
    async fn while_the_stream_is_up_an_invoice_is_looked_up_at_most_every_30_seconds() {
        let (lnd, _stream) = FakeLnd::new(InvoiceState::Open);
        let watches = InvoiceWatches::default();
        let swap = Uuid::now_v7();
        let start = Instant::now();

        // Ten minutes of one-second ticks.
        for second in 0..600 {
            watches
                .state(&lnd, swap, HASH, start + Duration::from_secs(second))
                .await
                .unwrap();
        }
        assert_eq!(lnd.polls(), 20);
    }

    #[tokio::test]
    async fn a_stale_lookup_never_takes_an_accepted_invoice_back_to_open() {
        let seen = SeenState::default();
        seen.record(InvoiceState::Accepted);
        seen.record(InvoiceState::Open);
        assert_eq!(seen.get(), Some(InvoiceState::Accepted));
        seen.record(InvoiceState::Canceled);
        assert_eq!(seen.get(), Some(InvoiceState::Canceled));
    }

    #[tokio::test]
    async fn a_swap_that_left_awaiting_payment_drops_its_stream() {
        let (lnd, stream) = FakeLnd::new(InvoiceState::Open);
        let watches = InvoiceWatches::default();
        let swap = Uuid::now_v7();
        watches
            .state(&lnd, swap, HASH, Instant::now())
            .await
            .unwrap();
        settle_down().await;

        watches.keep_only(&HashSet::new());
        settle_down().await;
        assert!(stream.is_closed(), "the stream task was aborted");
    }
}
