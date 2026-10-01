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
