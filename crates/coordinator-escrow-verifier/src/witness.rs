//! No negative or historical cache: the durable witness owns every decision.
use coordinator_escrow::payout_witness::{Occupancy, Reservation, WitnessError};
use std::{future::Future, pin::Pin};

pub type ReservationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Occupancy, WitnessError>> + Send + 'a>>;

pub trait ReservationWitness: Send + Sync {
    fn reserve(&self, reservation: Reservation) -> ReservationFuture<'_>;
}

#[cfg(feature = "lnurl")]
pub struct HttpsWitness {
    transport: crate::lnurl_transport::LnurlPayClient,
    url: url::Url,
    ledger_id: uuid::Uuid,
    key: coordinator_escrow::payout_witness::AuthenticationKey,
}

#[cfg(feature = "lnurl")]
impl HttpsWitness {
    pub fn new(
        transport: crate::lnurl_transport::LnurlPayClient,
        url: url::Url,
        ledger_id: uuid::Uuid,
        key: coordinator_escrow::payout_witness::AuthenticationKey,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !ledger_id.is_nil(),
            "Pinned witness ledger identity must not be nil"
        );
        anyhow::ensure!(
            url.scheme() == "https"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "Witness requires a fixed HTTPS endpoint without credentials or query"
        );
        crate::lnurl_transport::validate_url(&url)?;
        Ok(Self {
            transport,
            url,
            ledger_id,
            key,
        })
    }
}

#[cfg(feature = "lnurl")]
impl ReservationWitness for HttpsWitness {
    fn reserve(&self, reservation: Reservation) -> ReservationFuture<'_> {
        Box::pin(async move {
            use coordinator_escrow::payout_witness::{Authenticated, Receipt, Request};
            let request = Request {
                ledger_id: self.ledger_id,
                nonce: rand::random(),
                reservation,
            };
            let authenticated = self
                .key
                .sign_request(request.clone())
                .map_err(|_| WitnessError::Authentication)?;
            let response = self
                .transport
                .post_json(self.url.clone(), &authenticated)
                .await
                .map_err(|_| WitnessError::Unavailable)?;
            let receipt: Authenticated<Receipt> =
                serde_json::from_value(response).map_err(|_| WitnessError::Authentication)?;
            self.key
                .verify_receipt(&receipt, &request)
                .map_err(|_| WitnessError::Authentication)?;
            receipt.payload.result
        })
    }
}

/// Durable storage for TCP simulations whose host is already trusted with custody.
/// This adapter is excluded from production artifacts and never initializes storage.
#[cfg(feature = "payout-witness-local")]
pub(crate) struct LocalSimulationWitness {
    ledger: coordinator_payout_witness::Ledger,
    ledger_id: uuid::Uuid,
}

#[cfg(feature = "payout-witness-local")]
impl LocalSimulationWitness {
    pub(crate) async fn open(
        path: &std::path::Path,
        ledger_id: uuid::Uuid,
    ) -> anyhow::Result<Self> {
        validate_local_simulation(
            std::env::var("TRANSPORT_MODE").as_deref().ok(),
            std::env::var("KEYMELD_DANGEROUS_TRUST_UNATTESTED_ENCLAVES")
                .as_deref()
                .ok(),
        )?;
        validate_local_database(path, ledger_id)?;
        let ledger = coordinator_payout_witness::Ledger::open(path).await?;
        anyhow::ensure!(
            ledger.id == ledger_id,
            "Local witness database does not match the pinned ledger identity"
        );
        Ok(Self { ledger, ledger_id })
    }
}

#[cfg(feature = "payout-witness-local")]
impl ReservationWitness for LocalSimulationWitness {
    fn reserve(&self, reservation: Reservation) -> ReservationFuture<'_> {
        Box::pin(async move { self.ledger.reserve(self.ledger_id, &reservation).await })
    }
}

#[cfg(any(test, feature = "payout-witness-local"))]
fn validate_local_simulation(
    transport: Option<&str>,
    trust_unattested: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        transport == Some("tcp") && trust_unattested == Some("true"),
        "Local payout witness requires explicit TRANSPORT_MODE=tcp and KEYMELD_DANGEROUS_TRUST_UNATTESTED_ENCLAVES=true"
    );
    Ok(())
}

#[cfg(any(test, feature = "payout-witness-local"))]
fn validate_local_database(path: &std::path::Path, ledger_id: uuid::Uuid) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "Local witness database path must be absolute"
    );
    anyhow::ensure!(
        !ledger_id.is_nil(),
        "Pinned witness ledger identity must not be nil"
    );
    Ok(())
}

/// Explicit fixture only. Production defaults never fall back to process memory.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub(crate) struct MemoryWitness(std::sync::Mutex<MemoryClaims>);

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct MemoryClaims {
    hashes: std::collections::BTreeMap<[u8; 32], coordinator_escrow::payout_witness::Claim>,
    released: std::collections::BTreeMap<(uuid::Uuid, uuid::Uuid), uuid::Uuid>,
}

#[cfg(any(test, feature = "test-support"))]
impl MemoryWitness {
    fn reserve_now(&self, reservation: Reservation) -> Result<Occupancy, WitnessError> {
        let mut ledger = self.0.lock().map_err(|_| WitnessError::Unavailable)?;
        let entry = (reservation.claim.session_id, reservation.claim.user_id);
        if ledger
            .hashes
            .get(&reservation.payment_hash)
            .is_some_and(|owner| owner != &reservation.claim)
            || ledger
                .released
                .get(&entry)
                .is_some_and(|owner| *owner != reservation.claim.claim_id)
        {
            return Err(WitnessError::Conflict);
        }
        if reservation.executing {
            ledger.released.insert(entry, reservation.claim.claim_id);
        }
        ledger
            .hashes
            .insert(reservation.payment_hash, reservation.claim);
        Ok(Occupancy {
            payment_hashes: ledger.hashes.len() as u64,
            released_entries: ledger.released.len() as u64,
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl ReservationWitness for MemoryWitness {
    fn reserve(&self, reservation: Reservation) -> ReservationFuture<'_> {
        Box::pin(async move { self.reserve_now(reservation) })
    }
}

#[cfg(test)]
mod local_guard_tests {
    use super::{validate_local_database, validate_local_simulation};

    #[test]
    fn local_witness_requires_both_explicit_development_guards() {
        assert!(validate_local_simulation(Some("tcp"), Some("true")).is_ok());
        for (transport, trust) in [
            (None, None),
            (None, Some("true")),
            (Some("tcp"), None),
            (Some("tcp"), Some("false")),
            (Some("vsock"), Some("true")),
            (Some("vsock"), Some("false")),
            (Some("invalid"), Some("true")),
            (Some("tcp"), Some("TRUE")),
        ] {
            assert!(validate_local_simulation(transport, trust).is_err());
        }
    }

    #[test]
    fn local_witness_requires_an_absolute_path_and_pinned_identity() {
        let id = uuid::Uuid::from_u128(1);
        let absolute = std::path::Path::new("/var/lib/payout-witness/ledger.sqlite");
        assert!(validate_local_database(absolute, id).is_ok());
        assert!(validate_local_database(absolute, uuid::Uuid::nil()).is_err());
        assert!(validate_local_database(std::path::Path::new("ledger.sqlite"), id).is_err());
        assert!(validate_local_database(std::path::Path::new(""), id).is_err());
    }
}
