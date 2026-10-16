//! The wallet's unspent VTXOs, kept between readings.
//!
//! For every listing by address the indexer reads the address's whole history, then filters and
//! pages it. A long-lived wallet's history is thousands of spent VTXOs. So the wallet lists its
//! unspent VTXOs in full only now and then, and otherwise asks for what changed since its last
//! listing: the `after` bound is the one part of a listing the indexer's query itself applies.
//!
//! The indexer stamps a VTXO when it is created, spent, settled or unrolled. It does not stamp a
//! sweep, but a sweep comes at expiry, which the coins' classification reads from the VTXO itself;
//! the next full listing corrects anything else.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ark_core::server::{GetVtxosRequest, VirtualTxOutPoint};
use bitcoin::{OutPoint, ScriptBuf};

/// How often the unspent VTXOs are listed in full.
pub const FULL_EVERY: Duration = Duration::from_secs(30 * 60);
/// How far before the last listing began a listing of changes reaches back, in milliseconds:
/// for clock skew between this host and the server, and for what it recorded during that listing.
pub const OVERLAP_MS: u64 = 5 * 60 * 1000;

/// The unspent VTXOs at the wallet's addresses, as of the last listing.
pub struct VtxoSync {
    /// The addresses' scripts, sorted. Another set of addresses needs a full listing.
    scripts: Vec<ScriptBuf>,
    vtxos: HashMap<OutPoint, VirtualTxOutPoint>,
    /// UNIX milliseconds, by this host's clock, when the last listing began.
    listed_from_ms: u64,
    listed_in_full_at: Instant,
}

/// What the next listing asks the indexer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    /// Every unspent VTXO.
    Full,
    /// Every VTXO stamped at or after `after_ms`, spent ones included.
    Changes { after_ms: u64 },
}

impl Listing {
    /// What to list for `scripts` at `now`, given the last listing.
    pub fn next(sync: Option<&VtxoSync>, scripts: &[ScriptBuf], now: Instant) -> Self {
        match sync {
            Some(sync)
                if sync.scripts == scripts
                    && now.duration_since(sync.listed_in_full_at) < FULL_EVERY =>
            {
                Listing::Changes {
                    after_ms: sync.listed_from_ms.saturating_sub(OVERLAP_MS),
                }
            }
            _ => Listing::Full,
        }
    }

    /// `request`, an unfiltered listing of the wallet's addresses, narrowed to this listing.
    pub fn narrow(self, request: GetVtxosRequest) -> Result<GetVtxosRequest, ark_core::Error> {
        match self {
            Listing::Full => request.renewable_only(),
            Listing::Changes { after_ms } => Ok(request.with_after(after_ms)),
        }
    }
}

impl VtxoSync {
    /// The state after a full listing of `scripts` that began at `started_ms`.
    pub fn full(
        scripts: Vec<ScriptBuf>,
        listed: Vec<VirtualTxOutPoint>,
        started_ms: u64,
        now: Instant,
    ) -> Self {
        let mut sync = Self {
            scripts,
            vtxos: HashMap::new(),
            listed_from_ms: started_ms,
            listed_in_full_at: now,
        };
        sync.apply(listed, started_ms);
        sync
    }

    /// Take a listing of changes that began at `started_ms`. A VTXO listed spent or unrolled
    /// leaves; any other is added or replaced. Changes listed twice, as the overlap lists them,
    /// change nothing the second time.
    pub fn apply(&mut self, listed: Vec<VirtualTxOutPoint>, started_ms: u64) {
        for vtxo in listed {
            if vtxo.is_spent || vtxo.is_unrolled {
                self.vtxos.remove(&vtxo.outpoint);
            } else {
                self.vtxos.insert(vtxo.outpoint, vtxo);
            }
        }
        self.listed_from_ms = started_ms;
    }

    pub fn vtxos(&self) -> Vec<VirtualTxOutPoint> {
        self.vtxos.values().cloned().collect()
    }
}

/// UNIX milliseconds now.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coins::tests::vtxo;
    use ark_core::server::GetVtxosRequestFilter;

    fn spent(mut vtxo: VirtualTxOutPoint) -> VirtualTxOutPoint {
        vtxo.is_spent = true;
        vtxo
    }

    fn outpoints(sync: &VtxoSync) -> Vec<OutPoint> {
        let mut outpoints: Vec<_> = sync.vtxos().iter().map(|vtxo| vtxo.outpoint).collect();
        outpoints.sort();
        outpoints
    }

    #[test]
    fn lists_in_full_first_then_only_changes_until_the_full_listing_is_due() {
        let scripts = vec![ScriptBuf::from_bytes(vec![0x51])];
        let start = Instant::now();
        assert_eq!(Listing::next(None, &scripts, start), Listing::Full);
        let sync = VtxoSync::full(
            scripts.clone(),
            vec![vtxo(1, 5_000, 86_400)],
            1_000_000_000,
            start,
        );
        let later = start + Duration::from_secs(60);
        assert_eq!(
            Listing::next(Some(&sync), &scripts, later),
            Listing::Changes {
                after_ms: 1_000_000_000 - OVERLAP_MS
            }
        );
        assert_eq!(
            Listing::next(Some(&sync), &scripts, start + FULL_EVERY),
            Listing::Full
        );
        let other = vec![ScriptBuf::from_bytes(vec![0x52])];
        assert_eq!(Listing::next(Some(&sync), &other, later), Listing::Full);
    }

    #[test]
    fn a_full_listing_asks_for_unspent_vtxos_and_changes_for_everything_since() {
        let request = || GetVtxosRequest::new_for_outpoints(&[OutPoint::null()]);
        let full = Listing::Full.narrow(request()).unwrap();
        assert!(matches!(
            full.filter(),
            Some(GetVtxosRequestFilter::Renewable)
        ));
        assert_eq!(full.after(), None);
        let changes = Listing::Changes { after_ms: 42 }.narrow(request()).unwrap();
        assert!(
            changes.filter().is_none(),
            "spent VTXOs must be listed to leave"
        );
        assert_eq!(changes.after(), Some(42));
    }

    #[test]
    fn changes_remove_spent_and_unrolled_vtxos_and_add_new_ones() {
        let scripts = vec![ScriptBuf::new()];
        let start = Instant::now();
        // A server that ignored the filter would list spent VTXOs in full too.
        let mut sync = VtxoSync::full(
            scripts,
            vec![
                vtxo(1, 5_000, 86_400),
                vtxo(2, 6_000, 86_400),
                spent(vtxo(9, 1, 1)),
            ],
            10,
            start,
        );
        assert_eq!(
            outpoints(&sync),
            vec![vtxo(1, 0, 0).outpoint, vtxo(2, 0, 0).outpoint]
        );
        let mut unrolled = vtxo(2, 6_000, 86_400);
        unrolled.is_unrolled = true;
        let changes = vec![
            spent(vtxo(1, 5_000, 86_400)),
            unrolled,
            vtxo(3, 7_000, 86_400),
        ];
        sync.apply(changes.clone(), 20);
        assert_eq!(outpoints(&sync), vec![vtxo(3, 0, 0).outpoint]);
        assert_eq!(sync.listed_from_ms, 20);
        // The overlap lists the same changes again.
        sync.apply(changes, 30);
        assert_eq!(outpoints(&sync), vec![vtxo(3, 0, 0).outpoint]);
        let mut renewed = vtxo(3, 7_000, 86_400);
        renewed.expires_at += 86_400;
        sync.apply(vec![renewed.clone()], 40);
        assert_eq!(sync.vtxos(), vec![renewed]);
    }
}
