//! Whether the Arkade server is working, judged from the coordinator's own batch steps.
//!
//! A kickoff that forfeits its escrows, a recovery the server takes in a batch, and a refund it
//! takes offchain are successes. A batch the server gave up on, or a request it answered with an
//! internal error, is a failure (`coordinator_ark::Error::is_server_fault`); a refusal of what was
//! sent, or a failure on this side, is neither. While the last failure is newer than the last
//! success and less than `arkade_outage_secs` old, the server is unavailable and no ticket for an
//! Arkade competition is issued: its payment would wait in an escrow that cannot kick off, and
//! whose refund needs a batch too. The next success lifts the pause, and so does a quiet window
//! with no failure at all.

use std::sync::Mutex;

use log::{info, warn};
use time::{Duration, OffsetDateTime};

/// How long a server failure pauses entries when nothing succeeds after it.
pub const DEFAULT_ARKADE_OUTAGE_SECS: u64 = 15 * 60;

/// Whether the Arkade server is unavailable at `now`: its last failure is newer than its last
/// success, and less than `window` old.
pub fn arkade_unavailable(
    last_success: Option<OffsetDateTime>,
    last_failure: Option<OffsetDateTime>,
    window: Duration,
    now: OffsetDateTime,
) -> bool {
    let Some(failure) = last_failure else {
        return false;
    };
    if last_success.is_some_and(|success| success >= failure) {
        return false;
    }
    now - failure < window
}

/// What the batch steps last saw of the Arkade server.
#[derive(Debug, Default)]
struct Seen {
    last_success: Option<OffsetDateTime>,
    last_failure: Option<(OffsetDateTime, String)>,
    /// Whether entries were paused when last decided, so each change is logged once.
    paused: bool,
}

/// The Arkade server's health, kept in memory: a restart starts with it available.
#[derive(Debug)]
pub struct ArkadeHealth {
    window: Duration,
    seen: Mutex<Seen>,
}

impl Default for ArkadeHealth {
    fn default() -> Self {
        Self::new(DEFAULT_ARKADE_OUTAGE_SECS)
    }
}

impl ArkadeHealth {
    pub fn new(outage_secs: u64) -> Self {
        Self {
            window: Duration::seconds(i64::try_from(outage_secs).unwrap_or(i64::MAX)),
            seen: Mutex::default(),
        }
    }

    /// A batch step the server carried out.
    pub fn succeeded(&self, now: OffsetDateTime) {
        let mut seen = self.lock();
        seen.last_success = Some(now);
        self.decide(&mut seen, now);
    }

    /// A batch step the server failed.
    pub fn failed(&self, message: &str, now: OffsetDateTime) {
        let mut seen = self.lock();
        seen.last_failure = Some((now, message.to_owned()));
        self.decide(&mut seen, now);
    }

    /// Count a batch step's result: a success, a server failure, or neither.
    pub fn record<T>(&self, result: &Result<T, coordinator_ark::Error>) {
        let now = OffsetDateTime::now_utc();
        match result {
            Ok(_) => self.succeeded(now),
            Err(error) if error.is_server_fault() => self.failed(&error.to_string(), now),
            Err(_) => {}
        }
    }

    /// Whether entries to Arkade competitions are paused at `now`.
    pub fn unavailable(&self, now: OffsetDateTime) -> bool {
        let mut seen = self.lock();
        self.decide(&mut seen, now)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Seen> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Decide whether entries are paused, logging and reporting a change once.
    fn decide(&self, seen: &mut Seen, now: OffsetDateTime) -> bool {
        let paused = arkade_unavailable(
            seen.last_success,
            seen.last_failure.as_ref().map(|(at, _)| *at),
            self.window,
            now,
        );
        if paused != seen.paused {
            seen.paused = paused;
            crate::metrics::ARKADE_UNAVAILABLE.set(i64::from(paused));
            let message = seen
                .last_failure
                .as_ref()
                .map_or("", |(_, message)| message.as_str());
            if paused {
                warn!(
                    "Entries to Arkade competitions paused: the Arkade server failed a batch \
                     step ({message})"
                );
            } else if seen
                .last_success
                .zip(seen.last_failure.as_ref())
                .is_some_and(|(success, (failure, _))| success >= *failure)
            {
                info!(
                    "Entries to Arkade competitions resumed: the Arkade server carried out a \
                     batch step"
                );
            } else {
                info!(
                    "Entries to Arkade competitions resumed: no Arkade server failure for {} minutes",
                    self.window.whole_minutes()
                );
            }
        }
        paused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::seconds(900);

    fn at(minutes: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::minutes(minutes)
    }

    #[test]
    fn a_fresh_failure_makes_the_server_unavailable() {
        assert!(arkade_unavailable(None, Some(at(0)), WINDOW, at(0)));
        assert!(arkade_unavailable(None, Some(at(0)), WINDOW, at(14)));
        // An older success does not outweigh it.
        assert!(arkade_unavailable(Some(at(-5)), Some(at(0)), WINDOW, at(1)));
    }

    #[test]
    fn a_success_after_the_failure_makes_it_available() {
        assert!(!arkade_unavailable(Some(at(3)), Some(at(0)), WINDOW, at(4)));
        assert!(!arkade_unavailable(Some(at(0)), Some(at(0)), WINDOW, at(0)));
    }

    #[test]
    fn a_failure_older_than_the_window_makes_it_available() {
        assert!(!arkade_unavailable(None, Some(at(0)), WINDOW, at(15)));
        assert!(!arkade_unavailable(
            Some(at(-5)),
            Some(at(0)),
            WINDOW,
            at(60)
        ));
    }

    #[test]
    fn nothing_seen_is_available() {
        assert!(!arkade_unavailable(None, None, WINDOW, at(0)));
        assert!(!arkade_unavailable(Some(at(0)), None, WINDOW, at(0)));
    }

    #[test]
    fn failures_pause_and_successes_or_quiet_lift_the_pause() {
        let health = ArkadeHealth::new(900);
        assert!(!health.unavailable(at(0)));
        health.failed("batch failed: failed to create commitment tx", at(0));
        assert!(health.unavailable(at(1)));
        health.succeeded(at(2));
        assert!(!health.unavailable(at(2)));
        health.failed("failed to rescan boarding utxos", at(3));
        assert!(health.unavailable(at(10)));
        assert!(!health.unavailable(at(18)), "a quiet window lifts it");
    }

    #[test]
    fn only_server_faults_count_as_failures() {
        let health = ArkadeHealth::new(900);
        health.record::<()>(&Err(coordinator_ark::Error::Timeout(
            "waiting for the batch",
        )));
        assert!(!health.unavailable(OffsetDateTime::now_utc()));
        health.record::<()>(&Err(coordinator_ark::Error::BatchFailed {
            id: "batch".into(),
            reason: "failed to create commitment tx".into(),
        }));
        assert!(health.unavailable(OffsetDateTime::now_utc()));
        health.record(&Ok(()));
        assert!(!health.unavailable(OffsetDateTime::now_utc()));
    }
}
