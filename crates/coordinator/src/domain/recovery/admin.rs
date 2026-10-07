//! What operators see of the recovery records and the one thing they can do to them: offer
//! every live record again, to a relay added to `[recovery].relays` or one that lost its data.
//! See docs/RECOVERY.md, "Adding a relay".

use super::{
    publisher::{REPUBLISH_PER_TICK, TICK_SECS},
    Recovery,
};
use crate::{
    domain::{CompetitionStore, Error, RecoveryKindCount},
    metrics::RECOVERY_REPUBLISHED,
};
use log::info;
use serde::{Deserialize, Serialize};

/// One configured relay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayStatus {
    pub url: String,
    /// Live records this relay has not taken: everything, for a relay just added.
    pub missing: i64,
}

/// Where the recovery records stand, as `coordinator admin recovery status` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStatus {
    /// Recovery records are on (`[recovery].enabled`). Nothing else is filled in while off.
    pub enabled: bool,
    pub coordinator_pubkey: String,
    pub network: String,
    pub relays: Vec<RelayStatus>,
    /// Events not yet taken by every relay: records' versions and deletions.
    pub outbox_depth: i64,
    /// Records by kind: live, settled and waiting out the grace period, and retired.
    pub records: Vec<RecoveryKindCount>,
}

/// What a republish queued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepublishReport {
    /// The relays the records go to again.
    pub relays: Vec<String>,
    /// Live records queued.
    pub queued: u64,
    /// About how many seconds until the last of them is offered.
    pub seconds: i64,
}

/// The recovery records' status; `recovery` is `None` while they are off.
pub async fn recovery_status(
    recovery: Option<&Recovery>,
    store: &CompetitionStore,
) -> Result<RecoveryStatus, Error> {
    let Some(recovery) = recovery else {
        return Ok(RecoveryStatus::default());
    };
    let mut relays = Vec::with_capacity(recovery.relays().len());
    for relay in recovery.relays() {
        relays.push(RelayStatus {
            url: relay.clone(),
            missing: store.recovery_relay_missing(relay).await?,
        });
    }
    Ok(RecoveryStatus {
        enabled: true,
        coordinator_pubkey: recovery.public_key().to_hex(),
        network: recovery.network(),
        relays,
        outbox_depth: store.recovery_outbox_depth().await?,
        records: store.recovery_counts().await?,
    })
}

/// Offer every live record again to `relays`, or to every configured relay when it is empty:
/// each forgets those relays took it and any retry delay, and they come due a few per tick so
/// new versions keep going out. Each relay must be one of `[recovery].relays`; records whose
/// money settled and that were retired are not sent again.
pub async fn republish(
    recovery: Option<&Recovery>,
    store: &CompetitionStore,
    relays: &[String],
    now: i64,
) -> Result<RepublishReport, Error> {
    let Some(recovery) = recovery else {
        return Err(Error::BadRequest(
            "recovery records are off ([recovery].enabled)".into(),
        ));
    };
    if recovery.relays().is_empty() {
        return Err(Error::BadRequest(
            "[recovery].relays names no relay to publish to".into(),
        ));
    }
    let mut chosen: Vec<String> = Vec::new();
    for relay in relays.iter().map(|relay| relay.trim()) {
        if relay.is_empty() || chosen.iter().any(|known| known == relay) {
            continue;
        }
        if !recovery.relays().iter().any(|known| known == relay) {
            return Err(Error::BadRequest(format!(
                "{relay} is not one of [recovery].relays ({}); add it there and restart first",
                recovery.relays().join(", ")
            )));
        }
        chosen.push(relay.to_owned());
    }
    let queued = store
        .requeue_recovery_records(chosen.clone(), now, REPUBLISH_PER_TICK, TICK_SECS)
        .await?;
    RECOVERY_REPUBLISHED.inc_by(queued);
    let relays = if chosen.is_empty() {
        recovery.relays().to_vec()
    } else {
        chosen
    };
    info!(
        "Republishing {queued} recovery records to {}",
        relays.join(", ")
    );
    Ok(RepublishReport {
        relays,
        queued,
        seconds: queued.div_ceil(REPUBLISH_PER_TICK as u64) as i64 * TICK_SECS,
    })
}
