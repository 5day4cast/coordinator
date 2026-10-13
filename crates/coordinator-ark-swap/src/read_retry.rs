//! Bounded retries for read-only Arkade requests. Never use this around a spend or a batch.

use std::future::Future;
use std::io;
use std::time::Duration;

use anyhow::Context;
use rand08::Rng;
use tonic::{Code, Status};

/// The original wallet indexer page budget, shared by all attempts and their backoff.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ATTEMPTS: u32 = 3;

/// Equal jitter on exponential backoff: 1–2s, then 2–4s. The nonzero floor avoids
/// immediate retries; independent jitter spreads readers that fail together.
fn retry_delay(failed_attempt: u32, rng: &mut impl Rng) -> Duration {
    // Only two retries are allowed. Keep the ceiling at four seconds if that ever changes.
    let ceiling_ms = if failed_attempt == 1 { 2_000 } else { 4_000 };
    Duration::from_millis(rng.gen_range(ceiling_ms / 2..=ceiling_ms))
}

/// Repeat a transiently failed read on the client's reconnecting channel.
///
/// Tonic reconnects the channel after a transport failure, but does not repeat the failed RPC.
/// Validation errors and changes to the server's rules must still reach the caller. A hung
/// request uses the remaining budget; retries cannot extend that budget to sixty seconds.
pub async fn read<T, F, Fut, E>(operation: &str, mut request: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Into<anyhow::Error>,
{
    tokio::time::timeout(READ_TIMEOUT, async {
        let mut attempt = 1;
        loop {
            match request().await {
                Ok(value) => {
                    if attempt > 1 {
                        log::info!("{operation} recovered on attempt {attempt}");
                    }
                    return Ok(value);
                }
                Err(error) => {
                    let error = error.into();
                    if attempt == MAX_ATTEMPTS || !retryable(&error) {
                        return Err(error).with_context(|| {
                            format!("{operation} failed after {attempt} attempt(s)")
                        });
                    }
                    let delay = retry_delay(attempt, &mut rand08::thread_rng());
                    log::warn!(
                        "{operation} failed on attempt {attempt}; retrying in {}ms: {error:#}",
                        delay.as_millis(),
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    })
    .await
    .with_context(|| {
        format!(
            "{operation} exceeded its {}s read budget",
            READ_TIMEOUT.as_secs()
        )
    })?
}

fn retryable(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(status) = cause.downcast_ref::<Status>() {
            return match status.code() {
                Code::Unavailable | Code::DeadlineExceeded => true,
                // The observed HTTP/2 connection resets arrive as Unknown, with a transport
                // error and an io::Error underneath. Do not retry arbitrary Unknown replies.
                Code::Unknown => error.chain().any(|cause| {
                    cause.downcast_ref::<io::Error>().is_some_and(|error| {
                        matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset
                                | io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::ConnectionRefused
                                | io::ErrorKind::BrokenPipe
                                | io::ErrorKind::NotConnected
                                | io::ErrorKind::TimedOut
                                | io::ErrorKind::UnexpectedEof
                        )
                    })
                }),
                _ => false,
            };
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand08::SeedableRng;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn reset() -> anyhow::Error {
        let mut status = Status::unknown("transport error");
        status.set_source(Arc::new(io::Error::from(io::ErrorKind::ConnectionReset)));
        anyhow::Error::new(status).context("request failed")
    }

    #[test]
    fn jitter_varies_within_nonzero_exponential_backoff_windows() {
        let mut rng = rand08::rngs::StdRng::seed_from_u64(7);
        for (attempt, minimum, maximum) in [(1, 1, 2), (2, 2, 4), (3, 2, 4)] {
            let window = Duration::from_secs(minimum)..=Duration::from_secs(maximum);
            let delays: BTreeSet<_> = (0..32).map(|_| retry_delay(attempt, &mut rng)).collect();
            assert!(delays.iter().all(|delay| window.contains(delay)));
            assert!(
                delays.len() > 1,
                "concurrent readers must not use one fixed delay"
            );
        }
    }

    #[tokio::test]
    async fn real_grpc_transport_resets_reconnect_and_remain_bounded() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicU32::new(0));
        let accepted = connections.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                accepted.fetch_add(1, Ordering::SeqCst);
                // Closing with the remaining HTTP/2 preface unread resets the connection.
            }
        });
        let channel = tonic::transport::Endpoint::from_shared(url)
            .unwrap()
            .connect_lazy();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            read("test RPC", || {
                let mut grpc = tonic::client::Grpc::new(channel.clone());
                async move {
                    grpc.ready()
                        .await
                        .map_err(|error| Status::from_error(error.into()))?;
                    grpc.unary(
                        tonic::Request::new(()),
                        tonic::codegen::http::uri::PathAndQuery::from_static("/test.Reader/Page"),
                        tonic_prost::ProstCodec::<(), ()>::default(),
                    )
                    .await
                }
            }),
        )
        .await;
        server.abort();

        let error = result.unwrap().unwrap_err();
        assert!(retryable(&error), "{error:#}");
        assert_eq!(connections.load(Ordering::SeqCst), MAX_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_reset_recovers_within_the_same_read() {
        let calls = Cell::new(0);
        let started = tokio::time::Instant::now();
        let value = read("list test page", || {
            calls.set(calls.get() + 1);
            std::future::ready(if calls.get() == 1 {
                Err(reset())
            } else {
                Ok(42)
            })
        })
        .await
        .unwrap();

        assert_eq!(value, 42);
        assert_eq!(calls.get(), 2);
        assert!((Duration::from_secs(1)..=Duration::from_secs(2)).contains(&started.elapsed()));
    }

    #[tokio::test(start_paused = true)]
    async fn transient_statuses_retry_but_stop_after_three_attempts() {
        for code in [Code::Unavailable, Code::DeadlineExceeded] {
            let calls = Cell::new(0);
            let times = RefCell::new(Vec::new());
            let error = read("list test page", || {
                calls.set(calls.get() + 1);
                times.borrow_mut().push(tokio::time::Instant::now());
                std::future::ready(Err::<(), _>(Status::new(code, "temporary failure")))
            })
            .await
            .unwrap_err();

            assert_eq!(calls.get(), MAX_ATTEMPTS);
            assert_eq!(error.downcast_ref::<Status>().unwrap().code(), code);
            let times = times.borrow();
            assert!(
                (Duration::from_secs(1)..=Duration::from_secs(2)).contains(&(times[1] - times[0]))
            );
            assert!(
                (Duration::from_secs(2)..=Duration::from_secs(4)).contains(&(times[2] - times[1]))
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_and_unclassified_errors_are_not_retried() {
        for code in [
            Code::InvalidArgument,
            Code::PermissionDenied,
            Code::Unauthenticated,
            Code::FailedPrecondition,
            Code::Internal,
            Code::Unknown,
        ] {
            let calls = Cell::new(0);
            let error = read("list test page", || {
                calls.set(calls.get() + 1);
                // Even text mentioning a reset is not proof of a transport failure.
                std::future::ready(Err::<(), _>(Status::new(code, "connection reset")))
            })
            .await
            .unwrap_err();
            assert_eq!(calls.get(), 1, "{code:?}");
            assert_eq!(error.downcast_ref::<Status>().unwrap().code(), code);
        }
        let calls = Cell::new(0);
        read("decode test page", || {
            calls.set(calls.get() + 1);
            std::future::ready(Err::<(), _>(anyhow::anyhow!("invalid VTXO")))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_request_keeps_the_original_twenty_second_limit() {
        let calls = Cell::new(0);
        let started = tokio::time::Instant::now();
        let error = read("list test page", || {
            calls.set(calls.get() + 1);
            std::future::pending::<Result<(), Status>>()
        })
        .await
        .unwrap_err();

        assert_eq!(calls.get(), 1);
        assert_eq!(started.elapsed(), READ_TIMEOUT);
        assert!(error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_and_retries_share_the_twenty_second_limit() {
        let calls = Cell::new(0);
        let started = tokio::time::Instant::now();
        let error = read("list test page", || {
            calls.set(calls.get() + 1);
            async {
                tokio::time::sleep(Duration::from_secs(12)).await;
                Err::<(), _>(Status::unavailable("connection lost"))
            }
        })
        .await
        .unwrap_err();

        assert_eq!(calls.get(), 2);
        assert_eq!(started.elapsed(), READ_TIMEOUT);
        assert!(error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn expiring_during_backoff_does_not_send_another_request() {
        let calls = Cell::new(0);
        let started = tokio::time::Instant::now();
        let error = read("list test page", || {
            calls.set(calls.get() + 1);
            async {
                tokio::time::sleep(Duration::from_millis(19_500)).await;
                Err::<(), _>(Status::unavailable("connection lost"))
            }
        })
        .await
        .unwrap_err();

        assert_eq!(calls.get(), 1);
        assert_eq!(started.elapsed(), READ_TIMEOUT);
        assert!(error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some());
    }
}
