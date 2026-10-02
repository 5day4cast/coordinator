//! Read-only payout coverage for the operations list. Contract cleanup alone does not prove
//! player payment. Only successful Lightning records for each outcome beneficiary count.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use dlctix::{secp::Point, Player};
use serde::Deserialize;
use uuid::Uuid;

use super::{Competition, CompetitionStore, Coordinator};
use crate::domain::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperatorPayoutProgress {
    pub paid: usize,
    pub expected: usize,
}

impl OperatorPayoutProgress {
    pub fn all_paid(self) -> bool {
        self.expected > 0 && self.paid == self.expected
    }
}

#[derive(Deserialize)]
struct Terms {
    players: Vec<Player>,
    funding_value: u64,
    weights: BTreeMap<usize, u64>,
}

fn coverage(terms: &Terms, payments: &[(Point, u64)]) -> Option<OperatorPayoutProgress> {
    let roster: BTreeSet<_> = terms.players.iter().map(|player| player.pubkey).collect();
    if roster.len() != terms.players.len() {
        return None;
    }
    let total = terms
        .weights
        .values()
        .try_fold(0u64, |sum, weight| {
            (*weight > 0).then(|| sum.checked_add(*weight)).flatten()
        })
        .filter(|sum| *sum > 0)?;
    let mut beneficiaries = BTreeSet::new();
    let mut paid = 0;
    for (index, weight) in &terms.weights {
        let player = terms.players.get(*index)?;
        if !beneficiaries.insert(player.pubkey) {
            return None;
        }
        let owed = u64::try_from(
            u128::from(terms.funding_value) * u128::from(*weight) / u128::from(total),
        )
        .ok()?;
        if owed == 0 {
            return None;
        }
        if payments
            .iter()
            .any(|(key, amount)| *key == player.pubkey && *amount == owed)
        {
            paid += 1;
        }
    }
    Some(OperatorPayoutProgress {
        paid,
        expected: beneficiaries.len(),
    })
}

impl CompetitionStore {
    /// Read only the selected outcome's weights, player keys and funding value. Loading a
    /// signed contract would reconstruct every possible outcome transaction on each page.
    pub async fn operator_payout_progress(
        &self,
        competitions: &[Competition],
    ) -> Result<HashMap<Uuid, OperatorPayoutProgress>, sqlx::Error> {
        let mut progress = HashMap::new();
        for competition in competitions.iter().filter(|c| {
            c.attestation.is_some() || c.expiry_broadcasted_at.is_some() || c.completed_at.is_some()
        }) {
            let point = competition
                .attestation
                .map(|value| value.base_point_mul().to_string());
            let json: Option<String> = sqlx::query_scalar(r#"
                WITH terms AS (
                    SELECT CASE WHEN json_valid(contract_parameters) THEN contract_parameters ELSE '{}' END AS params,
                           expiry_broadcasted_at, outcome_transaction, attestation
                    FROM competitions WHERE id = ?1
                ), selected AS (
                    SELECT params, CASE
                        WHEN expiry_broadcasted_at IS NULL AND attestation IS NOT NULL AND json_type(params, '$.event.locking_points') = 'array' THEN (
                            SELECT CASE WHEN COUNT(*) = 1 THEN 'att' || MIN(key) END
                            FROM json_each(params, '$.event.locking_points') WHERE value = ?2
                        )
                        WHEN expiry_broadcasted_at IS NOT NULL AND outcome_transaction IS NULL AND attestation IS NULL AND json_type(params, '$.event.expiry') = 'integer' THEN 'exp'
                    END AS outcome FROM terms
                )
                SELECT json_object('players', json_extract(params, '$.players'),
                                   'funding_value', json_extract(params, '$.funding_value'),
                                   'weights', json_extract(params, '$.outcome_payouts.' || outcome))
                FROM selected WHERE outcome IS NOT NULL
            "#)
            .bind(competition.id.to_string())
            .bind(point)
            .fetch_optional(self.db_connection.read()).await?;
            let Some(terms) = json.and_then(|value| serde_json::from_str::<Terms>(&value).ok())
            else {
                // An ambiguous expiry/attestation or malformed terms remains unknown.
                // The full entry trace retains the transactions for investigation.
                continue;
            };
            let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as("SELECT e.id, e.ephemeral_pubkey, p.payout_amount_sats FROM entries e LEFT JOIN payouts p ON p.entry_id = e.id AND p.succeed_at IS NOT NULL AND p.failed_at IS NULL WHERE e.event_id = ?")
                .bind(competition.id.to_string()).fetch_all(self.db_connection.read()).await?;
            let mut roster: BTreeMap<Point, BTreeSet<String>> = BTreeMap::new();
            let mut payments = Vec::new();
            for (entry, key, amount) in rows {
                if let Ok(key) = Point::from_hex(&key) {
                    roster.entry(key).or_default().insert(entry);
                    if let Some(amount) = amount.and_then(|value| u64::try_from(value).ok()) {
                        payments.push((key, amount));
                    }
                }
            }
            if terms.players.iter().any(|player| {
                roster
                    .get(&player.pubkey)
                    .is_none_or(|entries| entries.len() != 1)
            }) {
                continue;
            }
            if let Some(value) = coverage(&terms, &payments) {
                progress.insert(competition.id, value);
            }
        }
        Ok(progress)
    }
}

impl Coordinator {
    pub async fn operator_payout_progress(
        &self,
        competitions: &[Competition],
    ) -> Result<HashMap<Uuid, OperatorPayoutProgress>, Error> {
        Ok(self
            .competition_store
            .operator_payout_progress(competitions)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dlctix::{hashlock, secp::Scalar};

    fn sample_terms(weights: &[(usize, u64)]) -> Terms {
        Terms {
            players: (1..=3)
                .map(|key| Player {
                    pubkey: Scalar::from_slice(&[key; 32]).unwrap().base_point_mul(),
                    ticket_hash: hashlock::sha256(&[key + 10; 32]),
                    payout_hash: hashlock::sha256(&[key + 20; 32]),
                })
                .collect(),
            funding_value: 3_000,
            weights: weights.iter().copied().collect(),
        }
    }

    #[test]
    fn only_the_right_beneficiary_and_full_amount_count() {
        let terms = sample_terms(&[(0, 1)]);
        let winner = terms.players[0].pubkey;
        let loser = terms.players[1].pubkey;
        assert_eq!(coverage(&terms, &[]).unwrap().paid, 0);
        assert_eq!(coverage(&terms, &[(loser, 3000)]).unwrap().paid, 0);
        assert_eq!(coverage(&terms, &[(winner, 2999)]).unwrap().paid, 0);
        assert!(coverage(&terms, &[(winner, 3000)]).unwrap().all_paid());
    }

    #[test]
    fn equal_returns_require_every_recipient_without_counting_duplicates_twice() {
        let terms = sample_terms(&[(0, 1), (1, 1), (2, 1)]);
        let payments: Vec<_> = terms
            .players
            .iter()
            .map(|player| (player.pubkey, 1000))
            .collect();
        assert_eq!(
            coverage(&terms, &[payments[0], payments[0]]),
            Some(OperatorPayoutProgress {
                paid: 1,
                expected: 3
            })
        );
        assert!(!coverage(&terms, &payments[..2]).unwrap().all_paid());
        assert!(coverage(&terms, &payments).unwrap().all_paid());
        let invalid = sample_terms(&[(3, 1)]);
        assert!(coverage(&invalid, &payments).is_none());
    }
}
