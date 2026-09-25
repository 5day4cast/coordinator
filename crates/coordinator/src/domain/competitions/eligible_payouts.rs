//! The payouts a player's account page shows: each of their entries that won
//! a competition whose outcome is on chain, with how its payout stands.

use dlctix::secp::Point;
use log::debug;
use uuid::Uuid;

use super::{winner_payout_sats, Coordinator, SearchBy};
use crate::domain::Error;

/// A winning entry's payout.
#[derive(Debug, Clone)]
pub struct EligiblePayout {
    pub competition_id: Uuid,
    pub entry_id: Uuid,
    /// The payout job's status, or else where the payout stands: "On-chain
    /// settlement", "Queued automatically" or "Awaiting invoice".
    pub status: String,
    pub amount_sats: u64,
    /// The Lightning Address the entry authorized for automatic payout.
    pub automatic_lightning_address: Option<String>,
    /// The winner may still collect with an invoice instead.
    pub allow_invoice_fallback: bool,
    /// The entry's payout is held in the Keymeld escrow.
    pub escrow_enabled: bool,
}

impl Coordinator {
    /// The payouts owed to the player with `pubkey` (hex), in entry order.
    pub async fn eligible_payouts(&self, pubkey: &str) -> Result<Vec<EligiblePayout>, Error> {
        let entries = self
            .get_entries(pubkey.to_owned(), SearchBy { event_ids: None })
            .await?;
        let competitions = self.get_competitions().await?;
        let store = &self.competition_store;

        let mut payouts = Vec::new();
        for entry in &entries {
            let policy = store
                .entry_payout_policy(entry.id)
                .await
                .ok()
                .flatten()
                .and_then(|json| {
                    serde_json::from_str::<coordinator_escrow::authorization::PayoutPolicy>(&json)
                        .ok()
                });
            if entry.paid_out_at.is_some() && policy.is_none() {
                continue;
            }
            let Some(competition) = competitions.iter().find(|c| c.id == entry.event_id) else {
                debug!("no competition {} for entry {}", entry.event_id, entry.id);
                continue;
            };
            if competition.attestation.is_none() || competition.outcome_broadcasted_at.is_none() {
                continue;
            }
            let (Some(params), Ok(outcome), Ok(entry_pubkey)) = (
                competition.contract_parameters.as_ref(),
                competition.get_current_outcome(),
                Point::from_hex(&entry.ephemeral_pubkey),
            ) else {
                continue;
            };
            let Ok(amount_sats) = winner_payout_sats(params, &outcome, &entry_pubkey) else {
                continue;
            };

            let window_closed = store
                .payout_window_is_closed(competition.id)
                .await
                .unwrap_or(true)
                || competition.delta_broadcasted_at.is_some()
                || competition.expiry_broadcasted_at.is_some()
                || competition.completed_at.is_some()
                || competition.cancelled_at.is_some();
            let automatic_lightning_address = policy
                .as_ref()
                .and_then(|policy| policy.automatic_lightning_address.clone());
            let status = match store.payout_job_status(entry.id).await.ok().flatten() {
                Some(status) => status,
                None if window_closed => "On-chain settlement".into(),
                None if automatic_lightning_address.is_some() => "Queued automatically".into(),
                None => "Awaiting invoice".into(),
            };
            payouts.push(EligiblePayout {
                competition_id: competition.id,
                entry_id: entry.id,
                status,
                amount_sats,
                automatic_lightning_address,
                allow_invoice_fallback: policy
                    .as_ref()
                    .is_none_or(|policy| policy.allow_invoice_fallback)
                    && !window_closed
                    && entry.paid_out_at.is_none(),
                escrow_enabled: policy.is_some(),
            });
        }
        Ok(payouts)
    }
}
