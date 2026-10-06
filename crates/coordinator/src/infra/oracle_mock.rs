use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use blake2::{Blake2s256, Digest};
use dlctix::{
    attestation_locking_point, attestation_secret,
    secp::{MaybeScalar, Scalar},
    EventLockingConditions,
};
use itertools::Itertools;
use uuid::Uuid;

use super::oracle::{AddEventEntries, Error, Event, Oracle, OracleEventTerms, OracleLine};
use crate::domain::CreateEvent;

#[derive(Debug, Clone)]
pub struct Outcome {
    pub winners: Vec<usize>,
}

impl Outcome {
    pub fn new(winners: Vec<usize>) -> Self {
        Self { winners }
    }

    pub fn single_winner(index: usize) -> Self {
        Self {
            winners: vec![index],
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.winners
            .iter()
            .flat_map(|&idx| (idx as u64).to_be_bytes())
            .collect()
    }
}

struct MockEvent {
    config: CreateEvent,
    nonce: Scalar,
    locking_conditions: EventLockingConditions,
    entries: Vec<AddEventEntries>,
    attestation: Option<MaybeScalar>,
    /// For a `lines` event, the lines frozen when it was created.
    lines: Vec<OracleLine>,
}

/// The metrics the mock scores, as NOAA's oracle does.
const SCORING_FIELDS: [&str; 3] = ["temp_high", "temp_low", "wind_speed"];

pub struct MockOracle {
    seed: [u8; 32],
    events: Arc<RwLock<HashMap<Uuid, MockEvent>>>,
    pending_attestations: Arc<RwLock<HashMap<Uuid, Outcome>>>,
}

impl MockOracle {
    pub fn new(seed: [u8; 32]) -> Self {
        Self {
            seed,
            events: Arc::new(RwLock::new(HashMap::new())),
            pending_attestations: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn queue_attestation(&self, event_id: Uuid, outcome: Outcome) {
        self.pending_attestations
            .write()
            .unwrap()
            .insert(event_id, outcome);
    }

    pub fn has_pending_attestation(&self, event_id: &Uuid) -> bool {
        self.pending_attestations
            .read()
            .unwrap()
            .contains_key(event_id)
    }

    pub fn get_locking_conditions(&self, event_id: &Uuid) -> Option<EventLockingConditions> {
        self.events
            .read()
            .unwrap()
            .get(event_id)
            .map(|e| e.locking_conditions.clone())
    }

    pub fn event_count(&self) -> usize {
        self.events.read().unwrap().len()
    }

    pub fn reset(&self) {
        self.events.write().unwrap().clear();
        self.pending_attestations.write().unwrap().clear();
    }

    fn hash_with_context(&self, context: &[u8]) -> [u8; 32] {
        let mut hasher = Blake2s256::new();
        hasher.update(self.seed);
        hasher.update(context);
        hasher.finalize().into()
    }

    fn generate_scalar(&self, context: &[u8]) -> Scalar {
        let mut hash = self.hash_with_context(context);
        // Ensure valid scalar by clearing high bit if needed
        hash[0] &= 0x7f;
        Scalar::from_slice(&hash).unwrap_or_else(|_| {
            hash[0] = 0;
            Scalar::from_slice(&hash).expect("fallback scalar")
        })
    }

    fn generate_nonce(&self, event_id: &Uuid) -> Scalar {
        self.generate_scalar(event_id.as_bytes())
    }

    /// The oracle's key, with an even Y, as a BIP340 key: the locking points a signed
    /// statement gives under the x-only key are the ones the mock announces.
    fn generate_oracle_key(&self) -> Scalar {
        let key = self.generate_scalar(b"oracle_key");
        let (_, parity): (
            dlctix::musig2::secp256k1::XOnlyPublicKey,
            dlctix::musig2::secp256k1::Parity,
        ) = key.base_point_mul().into();
        match parity {
            dlctix::musig2::secp256k1::Parity::Even => key,
            dlctix::musig2::secp256k1::Parity::Odd => -key,
        }
    }

    /// The lines a `lines` event of the mock freezes: one per location and metric.
    fn generate_lines(&self, config: &CreateEvent) -> Vec<OracleLine> {
        let window_hours =
            (config.end_observation_date - config.start_observation_date).whole_hours();
        config
            .locations
            .iter()
            .flat_map(|location| {
                SCORING_FIELDS.iter().map(move |metric| {
                    let spread = self.hash_with_context(format!("{location}/{metric}").as_bytes())
                        [0] as f64
                        / 64.0;
                    OracleLine {
                        target: location.clone(),
                        metric: (*metric).to_string(),
                        lower: -1.0 - spread,
                        upper: 1.0 + spread,
                        window_hours,
                    }
                })
            })
            .collect()
    }

    /// The oracle's signed statement of an event, once every entry is in.
    fn statement(
        &self,
        event_id: &Uuid,
        event: &MockEvent,
        terms: &OracleEventTerms,
    ) -> Result<Option<coordinator_escrow::oracle_statement::SignedStatement>, Error> {
        use coordinator_escrow::oracle_statement::{
            Outcomes, RankingOutcomes, SignedStatement, Statement, Terms,
        };
        use dlctix::musig2::secp256k1::{Keypair, Secp256k1, SecretKey};
        let mut entry_ids: Vec<Uuid> = event
            .entries
            .iter()
            .flat_map(|submission| submission.entries.iter().map(|entry| entry.id))
            .collect();
        if entry_ids.len() != event.config.total_allowed_entries {
            return Ok(None);
        }
        entry_ids.sort_unstable();
        let statement = Statement {
            event_id: *event_id,
            signing_date: event.config.signing_date.unix_timestamp(),
            expiry: event
                .locking_conditions
                .expiry
                .ok_or_else(|| Error::Request("mock event has no expiry".into()))?,
            nonce_point: event.nonce.base_point_mul(),
            outcomes: Outcomes::Ranking(RankingOutcomes {
                number_of_places_win: event.config.number_of_places_win as u32,
                entry_ids,
            }),
            terms: Terms::Observation(terms.observation()?),
        };
        let digest = statement
            .digest()
            .map_err(|e| Error::Request(e.to_string()))?;
        let secret = SecretKey::from_byte_array(self.generate_oracle_key().serialize())
            .map_err(|e| Error::Request(e.to_string()))?;
        let keypair = Keypair::from_secret_key(&Secp256k1::new(), &secret);
        let signature = Secp256k1::new().sign_schnorr_no_aux_rand(&digest, &keypair);
        Ok(Some(SignedStatement {
            statement,
            signature: signature.to_string(),
        }))
    }

    fn event_terms(&self, event_id: &Uuid, event: &MockEvent) -> Result<OracleEventTerms, Error> {
        let config = &event.config;
        let mut terms = OracleEventTerms {
            event: Event {
                id: *event_id,
                nonce_point: event.nonce.base_point_mul(),
                event_announcement: event.locking_conditions.clone(),
                attestation: event.attestation,
            },
            signing_date: config.signing_date,
            start_observation_date: config.start_observation_date,
            end_observation_date: config.end_observation_date,
            locations: config.locations.clone(),
            number_of_values_per_entry: config.number_of_values_per_entry as u32,
            total_allowed_entries: config.total_allowed_entries,
            number_of_places_win: config.number_of_places_win as u32,
            source: "noaa_weather".into(),
            scoring_fields: SCORING_FIELDS
                .iter()
                .map(|field| field.to_string())
                .collect(),
            scoring_rules: Some(config.scoring_rules()),
            lines: event.lines.clone(),
            statement: None,
        };
        terms.statement = self.statement(event_id, event, &terms)?;
        Ok(terms)
    }

    fn add_event(&self, config: CreateEvent, lines: Vec<OracleLine>) -> Result<Event, Error> {
        config
            .validate_oracle_settings()
            .map_err(|reason| Error::BadRequest(reason.into()))?;
        let nonce = self.generate_nonce(&config.id);
        let locking_conditions = self.generate_locking_conditions(&config, &nonce);
        let id = config.id;
        let event = MockEvent {
            config,
            nonce,
            locking_conditions: locking_conditions.clone(),
            entries: vec![],
            attestation: None,
            lines,
        };
        self.events.write().unwrap().insert(id, event);
        Ok(Event {
            id,
            nonce_point: nonce.base_point_mul(),
            event_announcement: locking_conditions,
            attestation: None,
        })
    }

    fn generate_locking_conditions(
        &self,
        config: &CreateEvent,
        nonce: &Scalar,
    ) -> EventLockingConditions {
        let oracle_seckey = self.generate_oracle_key();
        let oracle_pubkey = oracle_seckey.base_point_mul();
        let nonce_point = nonce.base_point_mul();

        let entries = config.total_allowed_entries;
        let locking_points: Vec<_> = (0..entries)
            .permutations(config.number_of_places_win)
            .chain(std::iter::once((0..entries).collect()))
            .map(|winners| {
                let message = Outcome::new(winners).to_bytes();
                attestation_locking_point(oracle_pubkey, nonce_point, &message)
            })
            .collect();

        let expiry = config.signing_date.unix_timestamp() as u32 + 86400;

        EventLockingConditions {
            locking_points,
            expiry: Some(expiry),
        }
    }

    fn generate_attestation(&self, event_id: &Uuid, outcome: &Outcome) -> MaybeScalar {
        attestation_secret(
            self.generate_oracle_key(),
            self.generate_nonce(event_id),
            outcome.to_bytes(),
        )
    }
}

#[async_trait]
impl Oracle for MockOracle {
    async fn create_event(&self, config: CreateEvent) -> Result<Event, Error> {
        let lines = match config.scoring_rules() {
            crate::infra::oracle::ScoringRules::Lines => self.generate_lines(&config),
            crate::infra::oracle::ScoringRules::Fixed => vec![],
        };
        self.add_event(config, lines)
    }

    async fn create_event_from_lines(
        &self,
        config: CreateEvent,
        lines_from_event: Uuid,
    ) -> Result<Event, Error> {
        if config.scoring_rules() != crate::infra::oracle::ScoringRules::Lines {
            return Err(Error::BadRequest(
                "lines_from_event needs lines scoring rules".into(),
            ));
        }
        let lines = self
            .events
            .read()
            .unwrap()
            .get(&lines_from_event)
            .map(|earlier| earlier.lines.clone())
            .ok_or_else(|| {
                Error::BadRequest(format!(
                    "lines_from_event {lines_from_event} is not an event on this oracle"
                ))
            })?;
        self.add_event(config, lines)
    }

    async fn get_event_terms(&self, event_id: &Uuid) -> Result<OracleEventTerms, Error> {
        let events = self.events.read().unwrap();
        let event = events
            .get(event_id)
            .ok_or_else(|| Error::NotFound(format!("Event {} not found", event_id)))?;
        self.event_terms(event_id, event)
    }

    async fn public_key(&self) -> Result<dlctix::musig2::secp256k1::PublicKey, Error> {
        Ok(self.generate_oracle_key().base_point_mul().into())
    }

    async fn get_event(&self, event_id: &Uuid) -> Result<Event, Error> {
        let mut events = self.events.write().unwrap();
        let event = events
            .get_mut(event_id)
            .ok_or_else(|| Error::NotFound(format!("Event {} not found", event_id)))?;

        if event.attestation.is_none() {
            if let Some(outcome) = self.pending_attestations.write().unwrap().remove(event_id) {
                let attestation = self.generate_attestation(event_id, &outcome);
                if !event
                    .locking_conditions
                    .locking_points
                    .contains(&attestation.base_point_mul())
                {
                    return Err(Error::BadRequest(
                        "outcome is not in the event announcement".into(),
                    ));
                }
                event.attestation = Some(attestation);
            }
        }

        Ok(Event {
            id: *event_id,
            nonce_point: event.nonce.base_point_mul(),
            event_announcement: event.locking_conditions.clone(),
            attestation: event.attestation,
        })
    }

    async fn submit_entries(&self, event_entries: AddEventEntries) -> Result<(), Error> {
        let mut events = self.events.write().unwrap();
        let event = events.get_mut(&event_entries.event_id).ok_or_else(|| {
            Error::NotFound(format!("Event {} not found", event_entries.event_id))
        })?;

        event.entries.push(event_entries);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn test_config() -> CreateEvent {
        CreateEvent {
            id: Uuid::now_v7(),
            signing_date: OffsetDateTime::now_utc() + time::Duration::days(1),
            start_observation_date: OffsetDateTime::now_utc(),
            end_observation_date: OffsetDateTime::now_utc() + time::Duration::hours(12),
            locations: vec!["KLAX".to_string()],
            number_of_values_per_entry: 3,
            number_of_places_win: 1,
            total_allowed_entries: 10,
            entry_fee: 1000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(10),
            total_competition_pool: 9000,
            relative_locktime_block_delta: None,
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
            contract_options: None,
        }
    }

    #[tokio::test]
    async fn mock_rejects_unbounded_announcements_without_creating_an_event() {
        let oracle = MockOracle::new([0u8; 32]);
        for (entries, places) in [(usize::MAX, 1), (25, 5)] {
            let mut config = test_config();
            config.total_allowed_entries = entries;
            config.number_of_places_win = places;
            let id = config.id;
            assert!(matches!(
                oracle.create_event(config).await,
                Err(Error::BadRequest(_))
            ));
            assert!(matches!(
                oracle.get_event(&id).await,
                Err(Error::NotFound(_))
            ));
        }
    }

    #[tokio::test]
    async fn test_create_and_get_event() {
        let oracle = MockOracle::new([0u8; 32]);
        let config = test_config();

        let event = oracle.create_event(config.clone()).await.unwrap();
        assert_eq!(event.id, config.id);
        assert!(event.attestation.is_none());

        let fetched = oracle.get_event(&config.id).await.unwrap();
        assert_eq!(fetched.id, config.id);
    }

    #[tokio::test]
    async fn test_queue_attestation() {
        let oracle = MockOracle::new([0u8; 32]);
        let config = test_config();

        oracle.create_event(config.clone()).await.unwrap();
        oracle.queue_attestation(config.id, Outcome::single_winner(0));

        let event = oracle.get_event(&config.id).await.unwrap();
        assert_eq!(
            event.attestation.unwrap().base_point_mul(),
            event.event_announcement.locking_points[0]
        );
    }

    #[tokio::test]
    async fn test_deterministic() {
        let config = test_config();

        let oracle1 = MockOracle::new([42u8; 32]);
        let oracle2 = MockOracle::new([42u8; 32]);

        let event1 = oracle1.create_event(config.clone()).await.unwrap();
        let event2 = oracle2.create_event(config).await.unwrap();

        assert_eq!(event1.nonce_point, event2.nonce_point);
    }

    #[tokio::test]
    async fn multi_place_and_refund_attestations_match_the_announced_order() {
        let oracle = MockOracle::new([42u8; 32]);
        let mut config = test_config();
        config.total_allowed_entries = 3;
        config.number_of_places_win = 2;
        let outcomes = (0..3).permutations(2).chain(std::iter::once(vec![0, 1, 2]));
        for (index, winners) in outcomes.enumerate() {
            config.id = Uuid::now_v7();
            oracle.create_event(config.clone()).await.unwrap();
            oracle.queue_attestation(config.id, Outcome::new(winners));
            let event = oracle.get_event(&config.id).await.unwrap();
            assert_eq!(event.event_announcement.locking_points.len(), 7);
            assert_eq!(
                event.attestation.unwrap().base_point_mul(),
                event.event_announcement.locking_points[index]
            );
        }
    }

    #[tokio::test]
    async fn unannounced_outcomes_are_refused() {
        let oracle = MockOracle::new([0u8; 32]);
        let config = test_config();
        oracle.create_event(config.clone()).await.unwrap();
        oracle.queue_attestation(
            config.id,
            Outcome::single_winner(config.total_allowed_entries),
        );
        assert!(matches!(
            oracle.get_event(&config.id).await,
            Err(Error::BadRequest(_))
        ));
    }
}
