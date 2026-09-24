//! One-time use of NIP-98 auth events.
//!
//! The `NostrAuth` extractor accepts an event whose `created_at` is within
//! [`MAX_EVENT_SKEW_SECS`] of now, before or after. An event dated that far
//! ahead is therefore accepted for up to twice that, 120 seconds from its
//! first use. Without this guard the same `Authorization` header could be
//! replayed for that whole time. Each event id is claimed once, after its
//! signature verifies, and is remembered until the extractor would reject it
//! as expired anyway.
//!
//! The guard is per process, which is enough while one process serves the
//! public API at a time. Blue/green runs two coordinator slots on one
//! database, but nix-rollout writes one upstream to `upstream.caddy` and stops
//! the previous slot after a switch (nixos_setup `apps/forecast/default.nix`,
//! nix-rollout `runtime::route_to`). What remains is a switch or restart: a
//! header first used up to 2 × [`MAX_EVENT_SKEW_SECS`] before it can be
//! replayed once afterwards. Serving from both slots at once, or closing that
//! gap, needs the claims in the shared database.

use nostr::EventId;
use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

/// Largest accepted difference between an event's `created_at` and now.
pub const MAX_EVENT_SKEW_SECS: i64 = 60;

/// Default bound on remembered events: 100k ids over a two-minute window is
/// ~830 authenticated requests per second before new requests are refused.
pub const DEFAULT_REPLAY_CAPACITY: usize = 100_000;

pub struct Nip98ReplayGuard {
    /// Event id to the last unix second at which it could still be accepted.
    seen: Mutex<HashMap<EventId, i64>>,
    capacity: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayRejection {
    /// The event was already used.
    Replayed,
    /// Every remembered event is still live; refuse rather than forget one.
    Full,
}

impl Nip98ReplayGuard {
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Claim `id` for a single use. Admission also prunes a full guard,
    /// keeping the common path to one hash lookup.
    pub fn claim(&self, id: EventId, created_at: i64, now: i64) -> Result<(), ReplayRejection> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.contains_key(&id) {
            return Err(ReplayRejection::Replayed);
        }
        if seen.len() >= self.capacity {
            seen.retain(|_, last_valid| *last_valid >= now);
        }
        if seen.len() >= self.capacity {
            return Err(ReplayRejection::Full);
        }
        seen.insert(id, created_at + MAX_EVENT_SKEW_SECS);
        Ok(())
    }

    /// Forget expired events during idle periods without waiting for capacity.
    pub fn prune(&self, now: i64) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, last_valid| *last_valid >= now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    #[test]
    fn an_event_is_accepted_once() {
        let guard = Nip98ReplayGuard::new(8);
        assert_eq!(guard.claim(id(1), 1_000, 1_000), Ok(()));
        assert_eq!(
            guard.claim(id(1), 1_000, 1_030),
            Err(ReplayRejection::Replayed)
        );
        assert_eq!(guard.claim(id(2), 1_000, 1_030), Ok(()));
    }

    #[test]
    fn full_guard_forgets_only_expired_events() {
        let guard = Nip98ReplayGuard::new(2);
        guard.claim(id(1), 1_000, 1_000).unwrap();
        guard.claim(id(2), 1_050, 1_050).unwrap();

        // id(1) can be accepted until 1_060, so nothing may be forgotten yet.
        assert_eq!(guard.claim(id(3), 1_055, 1_055), Err(ReplayRejection::Full));

        // After 1_060, id(1) has expired and its slot is reused; id(2) is kept.
        assert_eq!(guard.claim(id(3), 1_061, 1_061), Ok(()));
        assert_eq!(
            guard.claim(id(2), 1_050, 1_061),
            Err(ReplayRejection::Replayed)
        );
    }

    #[test]
    fn maintenance_prunes_idle_entries_without_forgetting_live_events() {
        let guard = Nip98ReplayGuard::new(10);
        guard.claim(id(1), 1_000, 1_000).unwrap();
        guard.claim(id(2), 1_050, 1_050).unwrap();
        guard.prune(1_060);
        assert_eq!(guard.seen.lock().unwrap().len(), 2);
        guard.prune(1_061);
        assert_eq!(guard.seen.lock().unwrap().len(), 1);
        assert_eq!(
            guard.claim(id(2), 1_050, 1_061),
            Err(ReplayRejection::Replayed)
        );
    }
}
