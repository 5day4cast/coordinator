//! The wallet's coins, by how long they have left.
//!
//! A VTXO lives until the batch it descends from expires, a week after that batch. A payment
//! inherits the expiry of the coins it spends, so an escrow paid from a coin with a day left has
//! a day left itself. Once a VTXO expires the Arkade server sweeps it, and only settling it in a
//! batch brings it back.
//!
//! So the service renews coins that are running out before they expire, recovers those that
//! already have, and never pays an escrow from a coin that is about to.

use std::collections::HashSet;

use ark_core::server::VirtualTxOutPoint;
use bitcoin::{Amount, OutPoint};

pub const DAY_SECS: i64 = 86_400;

/// How much life a coin needs, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Margins {
    /// A spendable coin with less than this left is settled into a fresh one.
    pub renew_secs: i64,
    /// A coin with less than this left never pays an escrow.
    pub pay_secs: i64,
}

impl Margins {
    pub fn from_days(renew_days: u32, pay_days: u32) -> Self {
        Self {
            renew_secs: i64::from(renew_days) * DAY_SECS,
            pay_secs: i64::from(pay_days) * DAY_SECS,
        }
    }
}

/// One unspent VTXO of the wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coin {
    pub outpoint: OutPoint,
    pub sat: u64,
    /// UNIX seconds.
    pub created_at: i64,
    /// UNIX seconds.
    pub expires_at: i64,
    /// It came from an Arkade transaction, not straight from a batch.
    pub preconfirmed: bool,
}

/// The wallet's unspent VTXOs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coins {
    /// Can be sent in an Arkade transaction.
    pub spendable: Vec<Coin>,
    /// Swept, expired or below dust: only settling them in a batch makes them spendable again.
    pub recoverable: Vec<Coin>,
}

/// What a renewal settles, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renewal {
    pub outpoints: Vec<OutPoint>,
    pub recoverable: usize,
    pub recoverable_sat: u64,
    pub expiring: usize,
    pub expiring_sat: u64,
    /// UNIX seconds. When the first of the expiring coins expires.
    pub earliest_expiry: Option<i64>,
}

/// Whether `vtxo` is unspent but can no longer be sent at `now`: the same rule as ark-core's
/// `is_recoverable`, with the clock passed in.
pub fn recoverable_at(vtxo: &VirtualTxOutPoint, dust: Amount, now: i64) -> bool {
    !vtxo.is_spent && (vtxo.amount < dust || vtxo.is_swept || now > vtxo.expires_at)
}

impl Coins {
    /// Sort `vtxos`, as the indexer lists them, into what can be sent and what must be recovered
    /// at `now`. Spent and unrolled VTXOs are left out, and one listed twice counts once.
    pub fn classify(vtxos: &[VirtualTxOutPoint], dust: Amount, now: i64) -> Self {
        let mut coins = Self::default();
        let mut seen = HashSet::new();
        for vtxo in vtxos {
            if vtxo.is_spent || !seen.insert(vtxo.outpoint) {
                continue;
            }
            let coin = Coin {
                outpoint: vtxo.outpoint,
                sat: vtxo.amount.to_sat(),
                created_at: vtxo.created_at,
                expires_at: vtxo.expires_at,
                preconfirmed: vtxo.is_preconfirmed,
            };
            if recoverable_at(vtxo, dust, now) {
                coins.recoverable.push(coin);
            } else if !vtxo.is_unrolled {
                coins.spendable.push(coin);
            }
        }
        coins
    }

    /// The coins that may pay an escrow at `now`: spendable, with `pay_secs` of life or more.
    pub fn payable(&self, now: i64, pay_secs: i64) -> impl Iterator<Item = &Coin> {
        self.spendable
            .iter()
            .filter(move |coin| coin.expires_at - now >= pay_secs)
    }

    pub fn payable_sat(&self, now: i64, pay_secs: i64) -> u64 {
        self.payable(now, pay_secs).map(|coin| coin.sat).sum()
    }

    pub fn spendable_sat(&self) -> u64 {
        self.spendable.iter().map(|coin| coin.sat).sum()
    }

    pub fn recoverable_sat(&self) -> u64 {
        self.recoverable.iter().map(|coin| coin.sat).sum()
    }

    /// What the wallet holds but cannot pay an escrow with until a batch settles it: recoverable
    /// coins, and spendable ones with less than `pay_secs` left.
    pub fn awaiting_renewal_sat(&self, now: i64, pay_secs: i64) -> u64 {
        self.recoverable_sat() + self.spendable_sat() - self.payable_sat(now, pay_secs)
    }

    /// UNIX seconds. When the first spendable coin expires.
    pub fn earliest_expiry(&self) -> Option<i64> {
        self.spendable.iter().map(|coin| coin.expires_at).min()
    }

    /// The coins that pay `amount_sat` at `now`, or `None` if those with `pay_secs` of life
    /// or more do not cover it.
    ///
    /// The longest-lived go first: the escrow expires when the first of its inputs would have,
    /// and so does the change. A coin is added to keep the change from falling below dust.
    pub fn select(
        &self,
        amount_sat: u64,
        dust_sat: u64,
        now: i64,
        pay_secs: i64,
    ) -> Option<Vec<OutPoint>> {
        let mut payable: Vec<&Coin> = self.payable(now, pay_secs).collect();
        payable.sort_by_key(|coin| {
            (
                std::cmp::Reverse(coin.expires_at),
                std::cmp::Reverse(coin.sat),
            )
        });
        let mut selected = Vec::new();
        let mut selected_sat = 0u64;
        for coin in payable {
            let change_sat = selected_sat.checked_sub(amount_sat);
            if change_sat.is_some_and(|change| change == 0 || change >= dust_sat) {
                break;
            }
            selected.push(coin.outpoint);
            selected_sat += coin.sat;
        }
        (selected_sat >= amount_sat).then_some(selected)
    }

    /// What to settle in the next batch at `now`: every recoverable coin, and every spendable
    /// one with less than `renew_secs` left. `None` when there is nothing to settle, or when it
    /// adds up to less than dust, which no batch accepts.
    ///
    /// A coin straight from a batch that never had `renew_secs` of life is left to expire: a
    /// margin longer than the server's VTXO lifetime would otherwise settle it in every batch.
    pub fn renewal(&self, now: i64, renew_secs: i64, dust_sat: u64) -> Option<Renewal> {
        let expiring: Vec<&Coin> = self
            .spendable
            .iter()
            .filter(|coin| coin.expires_at - now < renew_secs)
            .filter(|coin| coin.preconfirmed || coin.expires_at - coin.created_at > renew_secs)
            .collect();
        let renewal = Renewal {
            outpoints: self
                .recoverable
                .iter()
                .chain(expiring.iter().copied())
                .map(|coin| coin.outpoint)
                .collect(),
            recoverable: self.recoverable.len(),
            recoverable_sat: self.recoverable_sat(),
            expiring: expiring.len(),
            expiring_sat: expiring.iter().map(|coin| coin.sat).sum(),
            earliest_expiry: expiring.iter().map(|coin| coin.expires_at).min(),
        };
        let settles = !renewal.outpoints.is_empty()
            && renewal.recoverable_sat + renewal.expiring_sat >= dust_sat;
        settles.then_some(renewal)
    }
}

/// `sat` with thousands separators, as operators read amounts: `251,006`.
pub fn grouped(sat: u64) -> String {
    let digits = sat.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::Txid;

    pub(crate) const NOW: i64 = 1_790_000_000;
    const DUST: Amount = Amount::from_sat(330);
    const MARGINS: Margins = Margins {
        renew_secs: 3 * DAY_SECS,
        pay_secs: 2 * DAY_SECS,
    };

    /// An unspent, preconfirmed VTXO of `sat` with `life` seconds left at `NOW`.
    pub(crate) fn vtxo(id: u8, sat: u64, life: i64) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            outpoint: OutPoint::new(Txid::from_byte_array([id; 32]), 0),
            created_at: NOW - DAY_SECS,
            expires_at: NOW + life,
            amount: Amount::from_sat(sat),
            script: bitcoin::ScriptBuf::new(),
            is_preconfirmed: true,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
            depth: 1,
        }
    }

    fn swept(id: u8, sat: u64) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            is_swept: true,
            ..vtxo(id, sat, -3_600)
        }
    }

    fn coins(vtxos: &[VirtualTxOutPoint]) -> Coins {
        Coins::classify(vtxos, DUST, NOW)
    }

    fn outpoints(vtxos: &[&VirtualTxOutPoint]) -> Vec<OutPoint> {
        vtxos.iter().map(|vtxo| vtxo.outpoint).collect()
    }

    #[test]
    fn coins_are_spendable_until_they_are_swept_expired_or_below_dust() {
        let healthy = vtxo(1, 50_000, 5 * DAY_SECS);
        let expired = vtxo(2, 40_000, -1);
        let swept = swept(3, 30_000);
        let sub_dust = vtxo(4, 200, 5 * DAY_SECS);
        let spent = VirtualTxOutPoint {
            is_spent: true,
            ..vtxo(5, 20_000, 5 * DAY_SECS)
        };
        let unrolled = VirtualTxOutPoint {
            is_unrolled: true,
            ..vtxo(6, 10_000, 5 * DAY_SECS)
        };
        let listed = [
            healthy.clone(),
            expired.clone(),
            swept.clone(),
            sub_dust.clone(),
            spent,
            unrolled,
            // The two listings the wallet reads can name one VTXO twice.
            healthy.clone(),
        ];
        let coins = coins(&listed);
        assert_eq!(
            coins
                .spendable
                .iter()
                .map(|coin| coin.outpoint)
                .collect::<Vec<_>>(),
            outpoints(&[&healthy])
        );
        assert_eq!(
            coins
                .recoverable
                .iter()
                .map(|coin| coin.outpoint)
                .collect::<Vec<_>>(),
            outpoints(&[&expired, &swept, &sub_dust])
        );
        assert_eq!(coins.spendable_sat(), 50_000);
        assert_eq!(coins.recoverable_sat(), 70_200);
    }

    #[test]
    fn a_renewal_runs_when_a_coin_is_recoverable_or_near_expiry_and_not_otherwise() {
        let healthy = vtxo(1, 50_000, 5 * DAY_SECS);
        let at_the_margin = vtxo(2, 9_000, MARGINS.renew_secs);
        let renew = |listed: &[VirtualTxOutPoint]| {
            coins(listed).renewal(NOW, MARGINS.renew_secs, DUST.to_sat())
        };
        assert_eq!(renew(&[]), None);
        assert_eq!(renew(&[healthy.clone(), at_the_margin.clone()]), None);

        // The production case: the wallet's one large coin, a day from expiry.
        let expiring = vtxo(3, 102_957, DAY_SECS);
        assert_eq!(
            renew(&[healthy.clone(), expiring.clone()]),
            Some(Renewal {
                outpoints: outpoints(&[&expiring]),
                recoverable: 0,
                recoverable_sat: 0,
                expiring: 1,
                expiring_sat: 102_957,
                earliest_expiry: Some(NOW + DAY_SECS),
            })
        );

        // Swept coins are recovered along with those about to expire; healthy ones are left.
        let swept = swept(4, 251_006);
        let just_inside = vtxo(5, 7_000, MARGINS.renew_secs - 1);
        assert_eq!(
            renew(&[
                healthy.clone(),
                expiring.clone(),
                swept.clone(),
                just_inside.clone()
            ]),
            Some(Renewal {
                outpoints: outpoints(&[&swept, &expiring, &just_inside]),
                recoverable: 1,
                recoverable_sat: 251_006,
                expiring: 2,
                expiring_sat: 109_957,
                earliest_expiry: Some(NOW + DAY_SECS),
            })
        );
        assert_eq!(
            renew(&[healthy.clone(), swept.clone()]).unwrap().outpoints,
            outpoints(&[&swept])
        );
    }

    #[test]
    fn a_renewal_no_batch_would_accept_is_not_tried() {
        let renew = |listed: &[VirtualTxOutPoint]| {
            coins(listed).renewal(NOW, MARGINS.renew_secs, DUST.to_sat())
        };
        // Less than dust in all cannot be settled, however many healthy coins there are.
        let crumbs = [vtxo(1, 100, 5 * DAY_SECS), vtxo(2, 200, 5 * DAY_SECS)];
        assert_eq!(renew(&crumbs), None);
        assert_eq!(
            renew(&[
                crumbs[0].clone(),
                crumbs[1].clone(),
                vtxo(3, 50_000, 5 * DAY_SECS)
            ]),
            None
        );
        // Enough of them can.
        assert_eq!(
            renew(&[
                crumbs[0].clone(),
                crumbs[1].clone(),
                vtxo(4, 100, 5 * DAY_SECS)
            ])
            .unwrap()
            .recoverable_sat,
            400
        );

        // A coin fresh from a batch whose whole life is shorter than the margin is not
        // settled again and again; it is recovered once it expires.
        let short_lived_batch = VirtualTxOutPoint {
            is_preconfirmed: false,
            created_at: NOW - 60,
            ..vtxo(5, 80_000, 2 * DAY_SECS)
        };
        assert_eq!(renew(std::slice::from_ref(&short_lived_batch)), None);
        let long_lived_batch = VirtualTxOutPoint {
            created_at: NOW - 5 * DAY_SECS,
            ..short_lived_batch.clone()
        };
        assert!(renew(&[long_lived_batch]).is_some());
    }

    #[test]
    fn an_escrow_is_paid_from_the_longest_lived_coins_and_never_from_one_about_to_expire() {
        let select = |listed: &[VirtualTxOutPoint], amount_sat| {
            coins(listed).select(amount_sat, DUST.to_sat(), NOW, MARGINS.pay_secs)
        };
        let day_left = vtxo(1, 102_957, DAY_SECS);
        let four_days = vtxo(2, 20_000, 4 * DAY_SECS);
        let six_days = vtxo(3, 10_000, 6 * DAY_SECS);
        let at_the_margin = vtxo(4, 5_000, MARGINS.pay_secs);
        let swept = swept(5, 251_006);

        // The large coin is a day from expiry, so it pays nothing, whatever it could cover.
        assert_eq!(select(std::slice::from_ref(&day_left), 6_000), None);
        assert_eq!(select(&[day_left.clone(), swept.clone()], 6_000), None);

        let listed = [
            day_left.clone(),
            four_days.clone(),
            six_days.clone(),
            at_the_margin.clone(),
            swept,
        ];
        assert_eq!(select(&listed, 6_000), Some(outpoints(&[&six_days])));
        assert_eq!(
            select(&listed, 25_000),
            Some(outpoints(&[&six_days, &four_days]))
        );
        assert_eq!(
            select(&listed, 35_000),
            Some(outpoints(&[&six_days, &four_days, &at_the_margin]))
        );
        assert_eq!(select(&listed, 35_001), None);

        // Change below dust takes another coin along, when there is one.
        assert_eq!(
            select(&listed, 9_900),
            Some(outpoints(&[&six_days, &four_days]))
        );
        assert_eq!(select(&listed, 10_000), Some(outpoints(&[&six_days])));
        assert_eq!(
            select(std::slice::from_ref(&six_days), 9_900),
            Some(outpoints(&[&six_days]))
        );

        let coins = coins(&listed);
        assert_eq!(coins.payable_sat(NOW, MARGINS.pay_secs), 35_000);
        assert_eq!(
            coins.awaiting_renewal_sat(NOW, MARGINS.pay_secs),
            102_957 + 251_006
        );
        assert_eq!(coins.earliest_expiry(), Some(NOW + DAY_SECS));
    }

    #[test]
    fn amounts_read_with_thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(712), "712");
        assert_eq!(grouped(6_848), "6,848");
        assert_eq!(grouped(251_006), "251,006");
        assert_eq!(grouped(1_000_000), "1,000,000");
        assert_eq!(grouped(12_345_678), "12,345,678");
    }
}
