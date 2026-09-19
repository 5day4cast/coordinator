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

use super::oracle::{AddEventEntries, Error, Event, Oracle};
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
    nonce: Scalar,
    locking_conditions: EventLockingConditions,
    entries: Vec<AddEventEntries>,
    attestation: Option<MaybeScalar>,
}

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

    fn generate_oracle_key(&self) -> Scalar {
        self.generate_scalar(b"oracle_key")
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
            &outcome.to_bytes(),
        )
    }
}

#[async_trait]
impl Oracle for MockOracle {
    async fn create_event(&self, config: CreateEvent) -> Result<Event, Error> {
        let nonce = self.generate_nonce(&config.id);
        let locking_conditions = self.generate_locking_conditions(&config, &nonce);

        let event = MockEvent {
            nonce,
            locking_conditions: locking_conditions.clone(),
            entries: vec![],
            attestation: None,
        };

        self.events.write().unwrap().insert(config.id, event);

        Ok(Event {
            id: config.id,
            nonce_point: nonce.base_point_mul(),
            event_announcement: locking_conditions,
            attestation: None,
        })
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
            coordinator_fee_percentage: 10,
            total_competition_pool: 9000,
            relative_locktime_block_delta: None,
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
