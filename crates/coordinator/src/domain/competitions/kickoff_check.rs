//! The kickoff check: an Arkade competition or pool builds its contract only if what its entries
//! paid beyond the pot covers what the game costs the coordinator.
//!
//! At kickoff the coordinator knows the pool's size and the fee rate it will pay, so unlike the
//! network fee this check does not guess. The cost is the game's chain cost, `base_vbytes +
//! vbytes_per_player × players` vbytes (`network_fee_settings`) at the kickoff rate, plus the
//! Lightning allowance for paying the winner and keeping channels balanced. The rate is the one
//! the contract is then built at, and it must be within the fee ceiling the players consented
//! to: a contract is never built above it. A pool that fails is cancelled, and every entry is
//! refunded through the usual escrow refunds, network fee included.
//!
//! A pool is checked when it forms, before its Keymeld session and oracle event, and every
//! Arkade competition again just before its contract is built. See
//! `docs/QUEUED_COMPETITIONS.md`.

use super::*;
use crate::config::{KickoffCheckSettings, NetworkFeeSettings};
use crate::domain::competitions::CompetitionKind;
use crate::infra::bitcoin::fee_rate_for_target;
use bitcoin::{Amount, FeeRate};
use log::{info, warn};
use serde::{Deserialize, Serialize};

/// What the check found, kept for the competition's API so operators can see why a pool was
/// cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KickoffCheck {
    pub players: u64,
    pub paid_places: u64,
    /// The kickoff fee rate, which the contract is built at.
    pub sat_per_vb: u64,
    /// The fee ceiling the players consented to, in whole sat/vB, rounded down.
    pub max_sat_per_vb: u64,
    /// What the entries paid beyond the pot, as the kickoff can collect it: service and network
    /// fees.
    pub paid_sats: u64,
    pub chain_vbytes: u64,
    pub chain_cost_sats: u64,
    /// Paying the winner over Lightning and keeping channels balanced: a share of the pot.
    pub routing_and_liquidity_sats: u64,
    /// `paid_sats` less every cost; the check passes at zero.
    pub margin_sats: i64,
    /// The fewest players a pool may start with at this rate.
    pub min_players: u64,
    /// The rate is within the ceiling.
    pub within_ceiling: bool,
    pub passed: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub checked_at: OffsetDateTime,
    /// A failed check waits for fees to fall until then, checking again, before the pool is
    /// cancelled. None once it passed, or when the wait is over.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub retry_until: Option<OffsetDateTime>,
}

/// What a lifecycle step does after the kickoff check.
pub(super) enum KickoffGate {
    /// No check due, or it passed.
    Proceed,
    /// The check could not run yet; try the step again later.
    Wait,
    /// The check failed: fail the competition so every entry is refunded.
    Fail(CompetitionError),
}

/// What a pool looks like at kickoff.
#[derive(Debug, Clone, Copy)]
pub struct KickoffPool {
    pub players: u64,
    pub paid_places: u64,
    pub pot_sats: u64,
    pub paid_sats: u64,
    /// The fewest players its terms allow; more are needed while fees are not low.
    pub template_min_players: u64,
}

impl KickoffCheck {
    pub fn evaluate(
        weights: &NetworkFeeSettings,
        settings: &KickoffCheckSettings,
        pool: KickoffPool,
        rate: FeeRate,
        ceiling: FeeRate,
        checked_at: OffsetDateTime,
    ) -> Result<Self, anyhow::Error> {
        let overflow = || anyhow!("kickoff check overflows");
        let chain_vbytes = pool
            .players
            .checked_mul(weights.vbytes_per_player)
            .and_then(|v| v.checked_add(weights.base_vbytes))
            .ok_or_else(overflow)?;
        let chain_cost_sats = rate.fee_vb(chain_vbytes).ok_or_else(overflow)?.to_sat();
        let routing_and_liquidity_sats = u64::try_from(
            (u128::from(pool.pot_sats) * u128::from(settings.routing_and_liquidity_bps))
                .div_ceil(10_000),
        )
        .map_err(|_| overflow())?;
        let cost = chain_cost_sats
            .checked_add(routing_and_liquidity_sats)
            .ok_or_else(overflow)?;
        let margin_sats =
            i64::try_from(i128::from(pool.paid_sats) - i128::from(cost)).map_err(|_| overflow())?;
        let sat_per_vb = rate.to_sat_per_vb_ceil();
        let min_players = settings.min_players_at(pool.template_min_players, sat_per_vb);
        let within_ceiling = rate <= ceiling;
        Ok(Self {
            players: pool.players,
            paid_places: pool.paid_places,
            sat_per_vb,
            max_sat_per_vb: ceiling.to_sat_per_vb_floor(),
            paid_sats: pool.paid_sats,
            chain_vbytes,
            chain_cost_sats,
            routing_and_liquidity_sats,
            margin_sats,
            min_players,
            within_ceiling,
            passed: within_ceiling && margin_sats >= 0 && pool.players >= min_players,
            checked_at,
            retry_until: None,
        })
    }

    /// Why the check failed, for the competition's errors and the players' refund notice.
    pub fn reason(&self) -> String {
        if !self.within_ceiling {
            format!(
                "the network fee rate, {} sat/vB, is above the {} sat/vB ceiling in the terms",
                self.sat_per_vb, self.max_sat_per_vb
            )
        } else if self.players < self.min_players {
            format!(
                "{} players, but a pool needs at least {} while network fees are {} sat/vB",
                self.players, self.min_players, self.sat_per_vb
            )
        } else {
            format!(
                "its fees ({} sats) do not cover its costs ({} sats) at {} sat/vB",
                self.paid_sats,
                self.chain_cost_sats + self.routing_and_liquidity_sats,
                self.sat_per_vb
            )
        }
    }

    /// Until when a failed check waits for fees to fall: `fee_wait_secs` after registration
    /// closed at `closed`, if that is still ahead. None for a check that passed.
    pub fn fee_wait(
        &self,
        settings: &KickoffCheckSettings,
        closed: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Option<OffsetDateTime> {
        let until = closed + time::Duration::seconds(settings.fee_wait_secs as i64);
        (!self.passed && now < until).then_some(until)
    }

    /// The rate the contract is built at once the check passed.
    pub fn fee_rate(&self) -> Result<FeeRate, anyhow::Error> {
        FeeRate::from_sat_per_vb(self.sat_per_vb)
            .ok_or_else(|| anyhow!("kickoff rate {} sat/vB is out of range", self.sat_per_vb))
    }
}

impl Coordinator {
    pub fn with_kickoff_check(
        mut self,
        settings: KickoffCheckSettings,
    ) -> Result<Self, anyhow::Error> {
        settings.validate()?;
        self.kickoff_check = settings;
        Ok(self)
    }

    /// The rate a contract is built at: LND's next-block estimate, in whole sat/vB.
    pub(super) async fn contract_fee_rate(&self) -> Result<FeeRate, anyhow::Error> {
        let fee_rates = self.bitcoin.get_estimated_fee_rates().await?;
        info!("Fee rates: {:?}", fee_rates);
        fee_rate_for_target(&fee_rates, 1)
    }

    /// Whether `competition` is checked at this step: an Arkade competition with the check on,
    /// whose contract is not built yet, either a pool without its oracle event, or any whose
    /// entries are with the oracle, so the contract is built next.
    pub(super) async fn needs_kickoff_check(
        &self,
        competition: &Competition,
    ) -> Result<bool, anyhow::Error> {
        if !self.kickoff_check.enabled
            || competition.contract_parameters.is_some()
            || competition.is_cancelled()
            || competition.is_failed()
            || competition.kind == CompetitionKind::Queued
        {
            return Ok(false);
        }
        let due = (competition.kind == CompetitionKind::Pool
            && competition.event_created_at.is_none())
            || competition.entries_submitted_at.is_some();
        Ok(due && self.competition_store.is_ark_funded(competition.id).await?)
    }

    /// Check `competition` at the current kickoff rate, log it and keep it.
    pub(super) async fn run_kickoff_check(
        &self,
        competition: &Competition,
    ) -> Result<KickoffCheck, anyhow::Error> {
        let ark = self
            .ark()
            .ok_or_else(|| anyhow!("Arkade is not configured"))?;
        let mut entries = self
            .competition_store
            .get_competition_entries(competition.id, vec![EntryStatus::Paid])
            .await?;
        entries.sort_by_key(|entry| entry.ticket_id);
        let escrows = self
            .competition_store
            .funded_ark_escrows(competition.id)
            .await?;
        if escrows.len() != entries.len() {
            return Err(anyhow!(
                "{} entries but {} funded escrows",
                entries.len(),
                escrows.len()
            ));
        }
        let pot_sats = competition.event_submission.total_competition_pool as u64;
        let escrowed = escrows.iter().try_fold(0u64, |total, escrow| {
            escrow
                .vtxo_sats
                .and_then(|sats| total.checked_add(sats))
                .ok_or_else(|| anyhow!("Escrow for ticket {} has no amount", escrow.ticket_id))
        })?;
        // What the kickoff pays the coordinator: the escrows beyond the pot, up to what every
        // escrow consented to, and nothing below dust, which the Arkade server keeps.
        let beyond_pot = escrowed.saturating_sub(pot_sats);
        let paid_sats = beyond_pot.min(self.pool_fee_cap(&entries).await?.to_sat());
        let paid_sats = if Amount::from_sat(paid_sats) < ark.server.info().dust {
            0
        } else {
            paid_sats
        };
        let (ceiling, template_min_players) = self.kickoff_terms(competition).await?;
        let pool = KickoffPool {
            players: entries.len() as u64,
            paid_places: competition.event_submission.number_of_places_win as u64,
            pot_sats,
            paid_sats,
            template_min_players,
        };
        let rate = self.contract_fee_rate().await?;
        let now = OffsetDateTime::now_utc();
        let mut check = KickoffCheck::evaluate(
            &self.network_fee,
            &self.kickoff_check,
            pool,
            rate,
            ceiling,
            now,
        )?;
        check.retry_until = check.fee_wait(
            &self.kickoff_check,
            competition.event_submission.start_observation_date,
            now,
        );
        let summary = format!(
            "{} players (at least {} at this rate) at {} sat/vB (ceiling {}): paid {} sats \
             beyond the pot, chain {} sats for {} vB, routing and liquidity {}, margin {}",
            check.players,
            check.min_players,
            check.sat_per_vb,
            check.max_sat_per_vb,
            check.paid_sats,
            check.chain_cost_sats,
            check.chain_vbytes,
            check.routing_and_liquidity_sats,
            check.margin_sats
        );
        if check.passed {
            info!(
                "Competition {} passes its kickoff check: {summary}",
                competition.id
            );
        } else if let Some(until) = check.retry_until {
            info!(
                "Competition {} fails its kickoff check for now, and waits for fees to fall until \
                 {until}: {summary}",
                competition.id
            );
        } else {
            warn!(
                "Competition {} fails its kickoff check: {summary}",
                competition.id
            );
        }
        self.competition_store
            .store_kickoff_check(competition.id, &check)
            .await?;
        Ok(check)
    }

    /// The fee ceiling the competition's players consented to, and the fewest players its
    /// terms allow: a pool's terms, or for a single competition the coordinator's ceiling and
    /// two players.
    async fn kickoff_terms(
        &self,
        competition: &Competition,
    ) -> Result<(FeeRate, u64), anyhow::Error> {
        match self.pool_of(competition).await? {
            Some((settings, _)) => Ok((
                settings.terms.max_fee_rate,
                settings.terms.pool_rules.min_players() as u64,
            )),
            None => Ok((self.automatic_payout_max_fee_rate, 2)),
        }
    }

    /// A competition of fewer than `min_players` is created only while network fees are low
    /// enough for small pools; its kickoff check would cancel it otherwise.
    pub(super) async fn require_small_competitions_allowed(
        &self,
        players: u64,
    ) -> Result<(), Error> {
        let settings = &self.kickoff_check;
        if !settings.enabled || self.ark().is_none() || players >= settings.min_players {
            return Ok(());
        }
        let rate = self.contract_fee_rate().await.map_err(|e| {
            warn!("No fee rate to check a {players}-player competition: {e:#}");
            Error::FeeEstimateUnavailable
        })?;
        let sat_per_vb = rate.to_sat_per_vb_ceil();
        if sat_per_vb > settings.small_pools_max_sat_per_vb {
            return Err(Error::BadRequest(format!(
                "A competition needs at least {} players while Bitcoin network fees are above {} \
                 sat/vB (now {sat_per_vb} sat/vB)",
                settings.min_players, settings.small_pools_max_sat_per_vb
            )));
        }
        Ok(())
    }

    /// Run the kickoff check when `competition` is due one. A check that cannot run yet (no fee
    /// estimate, an escrow not listed yet) is not a failure: the step waits and runs it again,
    /// and nothing is built without a passed check.
    pub(super) async fn kickoff_gate(&self, competition: &Competition) -> KickoffGate {
        let due = match self.needs_kickoff_check(competition).await {
            Ok(due) => due,
            Err(e) => {
                warn!(
                    "Competition {}: cannot tell if a kickoff check is due: {e:#}",
                    competition.id
                );
                return KickoffGate::Wait;
            }
        };
        if !due {
            return KickoffGate::Proceed;
        }
        match self.run_kickoff_check(competition).await {
            Ok(check) if check.passed => KickoffGate::Proceed,
            // Fees may still fall: check again on the next pass.
            Ok(check) if check.retry_until.is_some() => KickoffGate::Wait,
            Ok(check) => KickoffGate::Fail(CompetitionError::KickoffCheckFailed(check.reason())),
            Err(e) => {
                warn!(
                    "Competition {}: kickoff check could not run: {e:#}",
                    competition.id
                );
                KickoffGate::Wait
            }
        }
    }

    /// The rate an Arkade competition's contract is built at: the one its kickoff check passed
    /// at, with the check on; otherwise the current one.
    pub(super) async fn checked_contract_fee_rate(
        &self,
        competition: &Competition,
    ) -> Result<FeeRate, anyhow::Error> {
        if !self.kickoff_check.enabled
            || !self.competition_store.is_ark_funded(competition.id).await?
        {
            return self.contract_fee_rate().await;
        }
        let check = self
            .competition_store
            .kickoff_check(competition.id)
            .await?
            .ok_or_else(|| anyhow!("Competition {} has no kickoff check", competition.id))?;
        if !check.passed {
            return Err(anyhow!(
                "Competition {} failed its kickoff check",
                competition.id
            ));
        }
        check.fee_rate()
    }

    /// Fill in each competition's latest kickoff check, for the API, in one query.
    pub(super) async fn attach_kickoff_checks(
        &self,
        competitions: &mut [Competition],
    ) -> Result<(), Error> {
        if let [competition] = competitions {
            competition.kickoff_check =
                self.competition_store.kickoff_check(competition.id).await?;
            return Ok(());
        }
        let mut checks = self.competition_store.kickoff_checks().await?;
        for competition in competitions.iter_mut() {
            competition.kickoff_check = checks.remove(&competition.id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: OffsetDateTime = time::macros::datetime!(2026-09-27 12:00 UTC);

    fn rate(sat_per_vb: u64) -> FeeRate {
        FeeRate::from_sat_per_vb(sat_per_vb).unwrap()
    }

    /// 25 players paying 5,000 sat entries, 150 sats of service fee (3%) and `network` sats of
    /// network fee each, one paid place.
    fn full_pool(network: u64) -> KickoffPool {
        KickoffPool {
            players: 25,
            paid_places: 1,
            pot_sats: 125_000,
            paid_sats: 25 * (150 + network),
            template_min_players: 2,
        }
    }

    fn check(pool: KickoffPool, sat_per_vb: u64, ceiling: u64) -> KickoffCheck {
        KickoffCheck::evaluate(
            &NetworkFeeSettings::default(),
            &KickoffCheckSettings::default(),
            pool,
            rate(sat_per_vb),
            rate(ceiling),
            NOW,
        )
        .unwrap()
    }

    #[test]
    fn a_full_pool_with_the_network_fee_passes_at_1_sat_per_vb() {
        let check = check(full_pool(142), 1, 100);
        assert_eq!(check.chain_vbytes, 992);
        assert_eq!(check.chain_cost_sats, 992);
        // 0.5% of the 125,000 sat pot.
        assert_eq!(check.routing_and_liquidity_sats, 625);
        assert_eq!(check.paid_sats, 25 * 292);
        assert_eq!(check.margin_sats, 25 * 292 - 992 - 625);
        assert!(check.passed && check.within_ceiling);
        assert_eq!(check.fee_rate().unwrap(), rate(1));
    }

    #[test]
    fn a_spiked_rate_fails() {
        let check = check(full_pool(50), 10, 100);
        assert_eq!(check.chain_cost_sats, 9_920);
        assert!(check.margin_sats < 0);
        assert!(!check.passed);
        // Without a network fee, the service fee alone covers less.
        assert!(check_at(full_pool(0), 3).passed);
        assert!(!check_at(full_pool(0), 4).passed);
    }

    fn check_at(pool: KickoffPool, sat_per_vb: u64) -> KickoffCheck {
        check(pool, sat_per_vb, 100)
    }

    #[test]
    fn the_boundary_is_exact() {
        // At 4 sat/vB a full pool costs 3,968 + 625 = 4,593 sats.
        let mut pool = full_pool(50);
        pool.paid_sats = 4_593;
        let at = check_at(pool, 4);
        assert_eq!(at.margin_sats, 0);
        assert!(at.passed);
        pool.paid_sats = 4_592;
        let below = check_at(pool, 4);
        assert_eq!(below.margin_sats, -1);
        assert!(!below.passed);
    }

    #[test]
    fn a_rate_above_the_ceiling_fails_whatever_was_paid() {
        let mut pool = full_pool(50);
        pool.paid_sats = 1_000_000;
        let above = check(pool, 11, 10);
        assert!(!above.within_ceiling && !above.passed);
        assert!(above.margin_sats > 0);
        let at = check(pool, 10, 10);
        assert!(at.within_ceiling && at.passed);
    }

    #[test]
    fn the_allowance_comes_from_the_settings() {
        let settings = KickoffCheckSettings {
            routing_and_liquidity_bps: 100,
            ..KickoffCheckSettings::default()
        };
        let pool = KickoffPool {
            players: 5,
            paid_places: 1,
            pot_sats: 25_001,
            paid_sats: 0,
            template_min_players: 2,
        };
        let check = KickoffCheck::evaluate(
            &NetworkFeeSettings::default(),
            &settings,
            pool,
            rate(2),
            rate(10),
            NOW,
        )
        .unwrap();
        assert_eq!(check.chain_vbytes, 342 + 26 * 5);
        assert_eq!(check.chain_cost_sats, 944);
        // 1% of 25,001 rounds up.
        assert_eq!(check.routing_and_liquidity_sats, 251);
        assert_eq!(check.margin_sats, -(944 + 251));
    }

    /// Priced and checked at the same rounded-up rate, a small pool passes at LND's floor, and a
    /// full five still passes when the estimate more than doubles before kickoff.
    #[test]
    fn pools_priced_at_the_contract_rate_pass_their_check() {
        let settings = NetworkFeeSettings::default();
        let fee_at = |sat_per_vb: f64| network_fee_sats(&settings, sat_per_vb).unwrap();
        let pool = |players: u64, network: u64| KickoffPool {
            players,
            paid_places: 1,
            pot_sats: players * 5_000,
            paid_sats: players * (150 + network),
            template_min_players: 2,
        };
        // LND's floor, 1.012 sat/vB: priced and built at 2.
        assert!(check_at(pool(2, fee_at(2.0)), 2).passed);
        assert!(check_at(pool(3, fee_at(2.0)), 2).passed);
        // Priced at 2; by kickoff the estimate is 2.2, so the contract is built at 3.
        assert!(check_at(pool(5, fee_at(2.0)), 3).passed);
    }

    /// Two or three players start a pool only while fees are extremely low; above
    /// `small_pools_max_sat_per_vb` a pool needs five, however much its entries paid.
    #[test]
    fn small_pools_start_only_while_fees_are_low() {
        let small = |players: u64| KickoffPool {
            players,
            paid_places: 1,
            pot_sats: players * 5_000,
            paid_sats: 1_000_000,
            template_min_players: 2,
        };
        let low = check_at(small(3), 1);
        assert_eq!(low.min_players, 2);
        assert!(low.passed);
        assert!(check_at(small(2), 2).passed);
        let high = check_at(small(3), 5);
        assert_eq!(high.min_players, 5);
        assert!(!high.passed);
        assert!(check_at(small(5), 5).passed);
        assert!(!check_at(small(4), 3).passed);
        // A pool's own minimum above five still holds.
        let mut strict = small(5);
        strict.template_min_players = 6;
        assert!(!check_at(strict, 1).passed);
    }

    /// A failing pool waits an hour after registration closes for fees to fall, then is
    /// cancelled; a passing one never waits.
    #[test]
    fn a_failed_check_waits_for_fees_to_fall_until_the_hour_is_up() {
        let settings = KickoffCheckSettings::default();
        let closed = NOW - time::Duration::minutes(10);
        let failed = check_at(full_pool(50), 10);
        assert_eq!(
            failed.fee_wait(&settings, closed, NOW),
            Some(closed + time::Duration::hours(1))
        );
        assert_eq!(
            failed.fee_wait(&settings, closed, closed + time::Duration::hours(1)),
            None,
            "the wait is over"
        );
        assert_eq!(
            check_at(full_pool(142), 1).fee_wait(&settings, closed, NOW),
            None
        );
        let no_wait = KickoffCheckSettings {
            fee_wait_secs: 0,
            ..settings
        };
        assert_eq!(failed.fee_wait(&no_wait, closed, NOW), None);
        // Checks stored before the wait existed still read.
        let mut old = serde_json::to_value(&failed).unwrap();
        old.as_object_mut().unwrap().remove("retry_until");
        assert_eq!(
            serde_json::from_value::<KickoffCheck>(old)
                .unwrap()
                .retry_until,
            None
        );
    }
}
