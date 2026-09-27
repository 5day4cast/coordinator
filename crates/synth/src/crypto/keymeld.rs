use anyhow::{ensure, Context, Result};
use coordinator_core::{
    keymeld::{
        prepare_payout_registration, prepare_registration,
        queued::{deposit_scope, EntryConsent},
        PayoutPolicy, PreparedRegistration,
    },
    PayoutRegistrationRequest, RegistrationAssignment,
};
use uuid::Uuid;

/// The entry a ticket's payout policy lets the player make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consent {
    /// The id the entry must be submitted with. A queued entry's is its ticket's.
    pub entry_id: Uuid,
    /// The entry waits in a queued competition until its pool forms.
    pub queued: bool,
}

/// A ticket's sealed Keymeld registration, and what its policy consents to.
pub struct PreparedTicket {
    pub registration: PreparedRegistration,
    pub consent: Consent,
}

/// Verify the assigned enclave before preparing a participant-bound envelope.
pub async fn prepare_for_ticket(
    private_key_hex: &str,
    assignment: &RegistrationAssignment,
    choice: &PayoutRegistrationRequest,
    competition_id: Uuid,
    ticket_id: Uuid,
    ticket_hash: &str,
    payout_preimage_hex: &str,
) -> Result<PreparedTicket> {
    let private_key: [u8; 32] = hex::decode(private_key_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Private key must be 32 bytes"))?;
    if let Some(json) = assignment.payout_policy.as_ref() {
        let policy: PayoutPolicy = serde_json::from_str(json)?;
        let consent = check_ticket_policy(
            &policy,
            assignment,
            choice,
            competition_id,
            ticket_id,
            ticket_hash,
        )?;
        let preimage: [u8; 32] = hex::decode(payout_preimage_hex)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Payout preimage must be 32 bytes"))?;
        let registration = prepare_payout_registration(&private_key, &preimage, assignment)
            .await
            .context("Failed to prepare payout escrow registration")?;
        Ok(PreparedTicket {
            registration,
            consent,
        })
    } else {
        let registration = prepare_registration(&private_key, assignment)
            .await
            .context("Failed to prepare authorized Keymeld registration")?;
        Ok(PreparedTicket {
            registration,
            consent: Consent {
                entry_id: choice.entry_id,
                queued: false,
            },
        })
    }
}

/// Check a ticket's payout policy against what the synth player asked for, as a wallet does
/// before sealing its key: concrete contract terms for a single competition, or a queued
/// competition's template.
///
/// A queued entry is its ticket: its id is the ticket's, and the player's key is deposited under
/// the queue's terms, not a session, so the assignment must name the queue and the terms' digest.
pub fn check_ticket_policy(
    policy: &PayoutPolicy,
    assignment: &RegistrationAssignment,
    choice: &PayoutRegistrationRequest,
    competition_id: Uuid,
    ticket_id: Uuid,
    ticket_hash: &str,
) -> Result<Consent> {
    let consent = EntryConsent::from_policy(policy)?;
    ensure!(
        consent.entry_id() == choice.entry_id
            && consent.competition_id() == competition_id
            && hex::encode(consent.ticket_hash()) == ticket_hash
            && hex::encode(consent.payout_hash()) == choice.payout_hash
            && policy.automatic_lightning_address == choice.lightning_address
            && policy.allow_invoice_fallback == choice.allow_invoice_fallback
            && policy.release_entry_key_after_payment == choice.release_entry_key_after_payment,
        "Ticket payout policy differs from the synth entry authorization"
    );
    let EntryConsent::Queued(entry) = consent else {
        return Ok(Consent {
            entry_id: choice.entry_id,
            queued: false,
        });
    };
    ensure!(
        entry.entry_id == ticket_id && assignment.user_id == ticket_id,
        "A queued entry must be its ticket"
    );
    ensure!(
        entry.terms.number_of_places_win == 1,
        "A queued competition's pools pay one winner"
    );
    let (session, digest) = deposit_scope(&entry.terms)?;
    ensure!(
        Uuid::parse_str(&assignment.session_id).ok() == Some(session.uuid())
            && assignment.manifest_hash == digest,
        "The key deposit is not scoped to the queued competition's terms"
    );
    Ok(Consent {
        entry_id: ticket_id,
        queued: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coordinator_core::keymeld::{
        oracle_statement::{LineTerms, ObservationTerms, ScoringRules},
        pools::PoolRules,
        queued::{QueuedEntryTerms, QueuedTerms},
        ArkEscrowPolicy,
    };
    use dlctix::{
        bitcoin::{FeeRate, Network},
        secp::Scalar,
        MarketMaker,
    };
    use std::collections::BTreeMap;

    const START: i64 = 1_790_000_000;

    fn terms(competition_id: Uuid) -> QueuedTerms {
        QueuedTerms {
            competition_id,
            network: Network::Signet,
            market_maker: MarketMaker {
                pubkey: Scalar::from_slice(&[3; 32]).unwrap().base_point_mul(),
            },
            // The generator's x coordinate: any valid x-only key will do.
            oracle_pubkey: "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
                .into(),
            signing_date: START + 2 * 86_400,
            expiry: (START + 3 * 86_400) as u32,
            observation: ObservationTerms {
                source: "noaa_weather".into(),
                start_observation_date: START,
                end_observation_date: START + 86_400,
                targets: vec!["KORD".into()],
                scoring_fields: vec!["temp_high".into()],
                number_of_values_per_entry: 1,
                scoring_rules: ScoringRules::Lines,
                lines: vec![LineTerms {
                    target: "KORD".into(),
                    metric: "temp_high".into(),
                    lower: -1.5,
                    upper: 1.5,
                    window_hours: 24,
                }],
            },
            number_of_places_win: 1,
            pool_rules: PoolRules::new(2, 25).unwrap(),
            stake_sats: 1_000,
            relative_locktime_block_delta: 72,
            max_fee_rate: FeeRate::from_sat_per_vb_u32(10),
        }
    }

    struct Fixture {
        queue: Uuid,
        ticket: Uuid,
        ticket_hash: [u8; 32],
        choice: PayoutRegistrationRequest,
        entry: QueuedEntryTerms,
        assignment: RegistrationAssignment,
    }

    impl Fixture {
        fn new() -> Self {
            let queue = Uuid::now_v7();
            let ticket = Uuid::now_v7();
            let ticket_hash = [5; 32];
            let choice = PayoutRegistrationRequest {
                entry_id: ticket,
                payout_hash: hex::encode([6; 32]),
                lightning_address: Some("player@example.org".into()),
                allow_invoice_fallback: true,
                release_entry_key_after_payment: true,
            };
            let entry = QueuedEntryTerms {
                terms: terms(queue),
                entry_id: ticket,
                ticket_hash,
                payout_hash: [6; 32],
            };
            let (session, digest) = deposit_scope(&entry.terms).unwrap();
            let assignment = RegistrationAssignment {
                session_id: session.as_string(),
                user_id: ticket,
                manifest_hash: digest.to_vec(),
                enclave_id: 1,
                enclave_key_epoch: 1,
                enclave_public_key: String::new(),
                gateway_url: String::new(),
                trusted_pcrs: BTreeMap::new(),
                dangerous_trust_unattested_enclaves: false,
                payout_policy: None,
            };
            Self {
                queue,
                ticket,
                ticket_hash,
                choice,
                entry,
                assignment,
            }
        }

        fn policy(&self) -> PayoutPolicy {
            PayoutPolicy {
                automatic_lightning_address: self.choice.lightning_address.clone(),
                allow_invoice_fallback: true,
                release_entry_key_after_payment: true,
                contract_terms: String::new(),
                ark_escrow: Some(ArkEscrowPolicy {
                    escrow_tap_tree: "00".into(),
                    max_fee_sats: 150,
                    max_refund_fee_sats: 100,
                    checkpoint_exit_script: "00".into(),
                }),
                queued_entry: Some(self.entry.to_json().unwrap()),
            }
        }

        fn check(&self) -> Result<Consent> {
            check_ticket_policy(
                &self.policy(),
                &self.assignment,
                &self.choice,
                self.queue,
                self.ticket,
                &hex::encode(self.ticket_hash),
            )
        }
    }

    #[test]
    fn a_queued_template_consents_to_an_entry_that_is_its_ticket() {
        let fixture = Fixture::new();
        assert_eq!(
            fixture.check().unwrap(),
            Consent {
                entry_id: fixture.ticket,
                queued: true
            }
        );
    }

    #[test]
    fn a_queued_template_for_another_entry_queue_or_hash_is_refused() {
        let mut other_ticket = Fixture::new();
        other_ticket.ticket = Uuid::now_v7();
        other_ticket.assignment.user_id = other_ticket.ticket;
        assert!(other_ticket.check().is_err(), "entry id must be the ticket");

        let mut other_queue = Fixture::new();
        other_queue.queue = Uuid::now_v7();
        assert!(other_queue.check().is_err());

        let mut other_hash = Fixture::new();
        other_hash.ticket_hash = [9; 32];
        assert!(other_hash.check().is_err());

        let mut other_payout = Fixture::new();
        other_payout.choice.payout_hash = hex::encode([7; 32]);
        assert!(other_payout.check().is_err());

        let fixture = Fixture::new();
        let mut other_address = fixture.policy();
        other_address.automatic_lightning_address = Some("someone@example.org".into());
        assert!(
            check_ticket_policy(
                &other_address,
                &fixture.assignment,
                &fixture.choice,
                fixture.queue,
                fixture.ticket,
                &hex::encode(fixture.ticket_hash),
            )
            .is_err(),
            "the policy must name the player's own refund address"
        );
    }

    #[test]
    fn a_queued_deposit_must_be_scoped_to_the_queue_and_its_terms() {
        let mut other_session = Fixture::new();
        other_session.assignment.session_id = Uuid::now_v7().to_string();
        assert!(other_session.check().is_err());

        let mut other_digest = Fixture::new();
        other_digest.assignment.manifest_hash = vec![0; 32];
        assert!(other_digest.check().is_err());

        let mut other_user = Fixture::new();
        other_user.assignment.user_id = Uuid::now_v7();
        assert!(other_user.check().is_err());
    }
}
