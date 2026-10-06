//! Each ticket's share of the Bitcoin network fees: its own line on the ticket's price.
//!
//! A game costs `base_vbytes + vbytes_per_player × players` vbytes on chain. Each entry pays its
//! share of that for a pool of `pool_players` (5: pools start small), at the rate contracts are
//! built at: the current estimate plus a margin (floored at `min_sat_per_vb`), times
//! `multiplier_percent`. The fee is fixed on a ticket's
//! payment hash the first time that hash is priced, before its escrow consent and invoice exist,
//! and never changes after; a ticket whose hash rotates is priced again. The coordinator keeps
//! any surplus and absorbs any shortfall; the kickoff check (`kickoff_check.rs`) is what protects
//! it from fee spikes. While the fee would be more than `pause_above_entry_bps` of the entry fee,
//! entries are paused: no ticket is issued. See `docs/QUEUED_COMPETITIONS.md`.

use super::*;
use crate::config::NetworkFeeSettings;
use log::{info, warn};

/// The message a player sees when the estimate is unavailable; no ticket is issued without it.
pub const FEE_ESTIMATE_UNAVAILABLE: &str =
    "The Bitcoin network fee estimate is unavailable right now, so no ticket was issued; try again in a moment";

/// The message a player sees while entries are paused.
pub const ENTRIES_PAUSED: &str = "Entries are paused while Bitcoin network fees are high";

/// The message a player sees while the Arkade server is failing batch steps (`arkade_health.rs`).
pub const ARKADE_UNAVAILABLE: &str =
    "Entries are paused while the Arkade network recovers; try again in a little while";

/// `ceil((base + per_player × n) / n × rate × multiplier / 100)` sats, `n` the priced pool size,
/// the rate floored at `min_sat_per_vb`. Zero when the fee is off.
///
/// The rate is taken to the nearest thousandth of a sat/vB (LND's estimates are whole sat/kw,
/// multiples of 0.004 sat/vB), so the rest is exact integer arithmetic.
pub fn network_fee_sats(
    settings: &NetworkFeeSettings,
    sat_per_vb: f64,
) -> Result<u64, anyhow::Error> {
    if !settings.enabled {
        return Ok(0);
    }
    if !sat_per_vb.is_finite() || sat_per_vb < 0.0 {
        return Err(anyhow!("invalid fee estimate: {sat_per_vb} sat/vB"));
    }
    let players = u128::from(settings.pool_players);
    if players == 0 {
        return Err(anyhow!(
            "network_fee_settings.pool_players must be at least 1"
        ));
    }
    let floor = u128::from(settings.min_sat_per_vb) * 1_000;
    let rate_milli = ((sat_per_vb * 1_000.0).round() as u128).max(floor);
    let vbytes =
        u128::from(settings.base_vbytes) + u128::from(settings.vbytes_per_player) * players;
    let numerator = vbytes
        .checked_mul(rate_milli)
        .and_then(|v| v.checked_mul(u128::from(settings.multiplier_percent)))
        .ok_or_else(|| anyhow!("network fee overflows"))?;
    let denominator = 1_000 * 100 * players;
    u64::try_from(numerator.div_ceil(denominator)).map_err(|_| anyhow!("network fee overflows"))
}

/// The network fee a ticket issued now would carry, for display before a ticket is requested.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct NetworkFeeQuote {
    pub enabled: bool,
    pub network_fee_sats: u64,
    /// The rate the fee was computed at, after the floor, in sat/vB.
    pub sat_per_vb: f64,
    pub conf_target: u16,
    pub pool_players: u64,
    pub multiplier_percent: u64,
    /// No ticket is issued while the fee is more than this share of the entry fee, in basis
    /// points; 0 never pauses.
    pub pause_above_entry_bps: u64,
    /// No ticket for an Arkade competition is issued while the Arkade server is failing batch
    /// steps, whatever the fee.
    pub arkade_unavailable: bool,
}

impl NetworkFeeQuote {
    /// Whether a ticket for an `entry_fee_sats` entry is refused at this fee.
    pub fn pauses(&self, entry_fee_sats: u64) -> bool {
        self.enabled
            && self.pause_above_entry_bps > 0
            && u128::from(self.network_fee_sats) * 10_000
                > u128::from(entry_fee_sats) * u128::from(self.pause_above_entry_bps)
    }
}

/// What a ticket costs, line by line: `ticket_price_sats` is what its invoice charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TicketPrice {
    pub entry_fee_sats: u64,
    pub coordinator_fee_sats: u64,
    pub network_fee_sats: u64,
    pub ticket_price_sats: u64,
}

impl TicketPrice {
    pub fn new(competition: &Competition, network_fee_sats: u64) -> Result<Self, Error> {
        let entry_fee_sats = competition.event_submission.entry_fee as u64;
        let base = competition.calculate_invoice_amount();
        Ok(Self {
            entry_fee_sats,
            coordinator_fee_sats: base - entry_fee_sats,
            network_fee_sats,
            ticket_price_sats: base
                .checked_add(network_fee_sats)
                .ok_or_else(|| Error::BadRequest("Ticket price overflows".into()))?,
        })
    }

    /// What the ticket's escrow may pay beyond the player's stake: the coordinator fee and the
    /// network fee.
    pub fn escrow_fee_sats(&self) -> u64 {
        self.ticket_price_sats - self.entry_fee_sats
    }
}

impl Coordinator {
    pub fn with_network_fee(mut self, settings: NetworkFeeSettings) -> Result<Self, anyhow::Error> {
        settings.validate()?;
        self.network_fee = settings;
        Ok(self)
    }
}

/// The rate a ticket's network fee is priced at: the rate contracts are built at from LND's
/// `estimate` (`fee_rate_from_estimate`: the estimate plus its margin), and at least
/// `min_sat_per_vb`. The kickoff check costs a pool at the contract rate, so pricing at any lower
/// rate would cancel pools whose fees were never short.
fn priced_sat_per_vb(estimate: f64, min_sat_per_vb: u64) -> Result<f64, anyhow::Error> {
    let rate = crate::infra::bitcoin::fee_rate_from_estimate(estimate)?;
    Ok((rate.to_sat_per_kwu() as f64 / 250.0).max(min_sat_per_vb as f64))
}

impl Coordinator {
    /// The network fee a ticket issued now would carry. Fails if the fee estimate does: a ticket
    /// is never priced without one.
    /// The quote pages show beside a price: at most a minute old, so pages that refresh
    /// themselves don't ask LND for an estimate each time. A ticket's own fee is fixed from a
    /// fresh quote when it is issued (`ticket_network_fee`), and replaces this one on the form.
    pub async fn shown_network_fee_quote(&self) -> Result<NetworkFeeQuote, Error> {
        const FRESH: std::time::Duration = std::time::Duration::from_secs(60);
        let cached = *self
            .shown_network_fee
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, quote)) = cached.filter(|(at, _)| at.elapsed() < FRESH) {
            // The Arkade server's health is decided afresh: it costs no request.
            return Ok(NetworkFeeQuote {
                arkade_unavailable: self.arkade_unavailable(),
                ..quote
            });
        }
        let quote = self.network_fee_quote().await?;
        *self
            .shown_network_fee
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((std::time::Instant::now(), quote));
        Ok(quote)
    }

    pub async fn network_fee_quote(&self) -> Result<NetworkFeeQuote, Error> {
        let settings = &self.network_fee;
        let sat_per_vb = if settings.enabled {
            let estimate = self
                .bitcoin
                .estimate_fee(settings.conf_target)
                .await
                .map_err(|e| {
                    warn!(
                        "No fee estimate for {} blocks to price tickets: {e:#}",
                        settings.conf_target
                    );
                    Error::FeeEstimateUnavailable
                })?;
            priced_sat_per_vb(estimate, settings.min_sat_per_vb).map_err(|e| {
                warn!("Cannot price tickets at a {estimate} sat/vB estimate: {e:#}");
                Error::FeeEstimateUnavailable
            })?
        } else {
            0.0
        };
        let network_fee_sats = network_fee_sats(settings, sat_per_vb).map_err(|e| {
            warn!("Cannot price a ticket's network fee: {e:#}");
            Error::FeeEstimateUnavailable
        })?;
        Ok(NetworkFeeQuote {
            enabled: settings.enabled,
            network_fee_sats,
            sat_per_vb,
            conf_target: settings.conf_target,
            pool_players: settings.pool_players,
            multiplier_percent: settings.multiplier_percent,
            pause_above_entry_bps: settings.pause_above_entry_bps,
            arkade_unavailable: self.arkade_unavailable(),
        })
    }

    /// Whether entries to Arkade competitions are paused because the Arkade server is failing
    /// batch steps.
    pub fn arkade_unavailable(&self) -> bool {
        self.arkade_health
            .unavailable(time::OffsetDateTime::now_utc())
    }

    /// The ticket's network fee for its current hash: fixed now at the current estimate unless it
    /// already is, and refused while entries are paused. A ticket invoiced before network fees
    /// existed has none. No ticket for an Arkade competition gets an invoice while the Arkade
    /// server is failing batch steps: its escrow could neither kick off nor be refunded.
    pub(in crate::domain::competitions) async fn ticket_network_fee(
        &self,
        competition: &Competition,
        ticket: &Ticket,
    ) -> Result<u64, Error> {
        if ticket.payment_request.is_none()
            && self.arkade_unavailable()
            && self.competition_store.is_ark_funded(competition.id).await?
        {
            info!(
                "Ticket {} refused: the Arkade server is failing batch steps",
                ticket.id
            );
            return Err(Error::ArkadeUnavailable);
        }
        if let Some(fee) = self
            .competition_store
            .fixed_ticket_network_fee(ticket.id, &ticket.hash)
            .await?
        {
            return Ok(fee);
        }
        if ticket.payment_request.is_some() {
            return Ok(0);
        }
        let quote = self.network_fee_quote().await?;
        if quote.pauses(competition.event_submission.entry_fee as u64) {
            info!(
                "Ticket {} refused: its {} sat network fee is above {} bps of the entry fee",
                ticket.id, quote.network_fee_sats, quote.pause_above_entry_bps
            );
            return Err(Error::EntriesPaused);
        }
        self.competition_store
            .fix_ticket_network_fee(ticket, quote.network_fee_sats)
            .await?
            .ok_or_else(|| {
                Error::BadRequest("Ticket reservation changed; request a new ticket".into())
            })
    }

    /// The ticket's price, its network fee fixed now if it isn't yet.
    pub(super) async fn ticket_price(
        &self,
        competition: &Competition,
        ticket: &Ticket,
    ) -> Result<TicketPrice, Error> {
        TicketPrice::new(
            competition,
            self.ticket_network_fee(competition, ticket).await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fee(settings: &NetworkFeeSettings, rate: f64) -> u64 {
        network_fee_sats(settings, rate).unwrap()
    }

    #[test]
    fn a_five_player_share_at_one_and_a_half_times_the_rate() {
        let settings = NetworkFeeSettings::default();
        // (368 + 26 × 5) / 5 = 99.6 vB, × 1.5 = 149.4 sats at 1 sat/vB.
        assert_eq!(fee(&settings, 1.0), 150);
        assert_eq!(fee(&settings, 2.0), 299); // 298.8
        assert_eq!(fee(&settings, 3.0), 449); // 448.2
    }

    #[test]
    fn rounds_up_to_the_next_sat() {
        let settings = NetworkFeeSettings::default();
        // 253 sat/kw, LND's floor, is 1.012 sat/vB: 151.2 sats.
        assert_eq!(fee(&settings, 1.012), 152);
        assert_eq!(fee(&settings, 10.0), 1_494);
        let exact = NetworkFeeSettings {
            multiplier_percent: 125,
            ..NetworkFeeSettings::default()
        };
        assert_eq!(fee(&exact, 2.0), 249); // 249.0 exactly: no rounding
    }

    /// Tickets are priced at the rate contracts are built at, so a kickoff at the same estimate
    /// costs exactly what was priced.
    #[test]
    fn priced_at_the_contract_rate() {
        // LND's floor, 253 sat/kWU, is 1.012 sat/vB; contracts are built at it plus 0.25.
        assert_eq!(priced_sat_per_vb(1.012, 1).unwrap(), 1.264);
        assert_eq!(fee(&NetworkFeeSettings::default(), 1.264), 189);
        assert_eq!(priced_sat_per_vb(2.2, 1).unwrap(), 2.452);
        assert_eq!(priced_sat_per_vb(3.0, 1).unwrap(), 3.3);
        assert_eq!(priced_sat_per_vb(0.25, 1).unwrap(), 1.012);
        assert_eq!(priced_sat_per_vb(1.0, 4).unwrap(), 4.0);
        assert!(priced_sat_per_vb(f64::NAN, 1).is_err());
    }

    #[test]
    fn the_rate_is_floored() {
        let settings = NetworkFeeSettings::default();
        assert_eq!(fee(&settings, 0.0), 150);
        assert_eq!(fee(&settings, 0.25), 150);
        let floor = NetworkFeeSettings {
            min_sat_per_vb: 4,
            ..NetworkFeeSettings::default()
        };
        assert_eq!(fee(&floor, 1.0), fee(&settings, 4.0));
        assert_eq!(fee(&floor, 10.0), 1_494);
    }

    #[test]
    fn multiplier_pool_size_and_weights_come_from_the_settings() {
        let full_pool = NetworkFeeSettings {
            pool_players: 25,
            multiplier_percent: 125,
            ..NetworkFeeSettings::default()
        };
        // (368 + 26 × 25) / 25 × 1.25 = 50.9 and 509 sats.
        assert_eq!(fee(&full_pool, 1.0), 51);
        assert_eq!(fee(&full_pool, 10.0), 509);
        let at_rate = NetworkFeeSettings {
            multiplier_percent: 100,
            ..NetworkFeeSettings::default()
        };
        assert_eq!(fee(&at_rate, 1.0), 100); // 99.6
        let weights = NetworkFeeSettings {
            base_vbytes: 100,
            vbytes_per_player: 0,
            pool_players: 10,
            multiplier_percent: 200,
            ..NetworkFeeSettings::default()
        };
        assert_eq!(fee(&weights, 2.0), 40); // 100 / 10 × 2 × 2
    }

    #[test]
    fn off_is_zero_and_bad_estimates_are_refused() {
        let off = NetworkFeeSettings {
            enabled: false,
            ..NetworkFeeSettings::default()
        };
        assert_eq!(fee(&off, 50.0), 0);
        let settings = NetworkFeeSettings::default();
        assert!(network_fee_sats(&settings, f64::NAN).is_err());
        assert!(network_fee_sats(&settings, f64::INFINITY).is_err());
        assert!(network_fee_sats(&settings, -1.0).is_err());
    }

    /// A $5 entry is about 5,914 sats (BTC at $84,547), so entries pause once its network fee is
    /// more than 591.4 sats: from about 3.96 sat/vB.
    #[test]
    fn entries_pause_above_ten_percent_of_the_entry() {
        let settings = NetworkFeeSettings::default();
        const ENTRY: u64 = 5_914;
        assert!(!settings.pauses(591, ENTRY));
        assert!(settings.pauses(592, ENTRY));
        assert_eq!(fee(&settings, 3.95), 591);
        assert_eq!(fee(&settings, 3.96), 592);
        assert!(!settings.pauses(fee(&settings, 3.0), ENTRY));
        assert!(settings.pauses(fee(&settings, 5.0), ENTRY));
        // Exactly 10% is not above it.
        assert!(!settings.pauses(500, 5_000));
        assert!(settings.pauses(501, 5_000));
        let never = NetworkFeeSettings {
            pause_above_entry_bps: 0,
            ..NetworkFeeSettings::default()
        };
        assert!(!never.pauses(1_000_000, ENTRY));
        let quote = NetworkFeeQuote {
            enabled: true,
            network_fee_sats: 592,
            sat_per_vb: 3.96,
            conf_target: 2,
            pool_players: 5,
            multiplier_percent: 150,
            pause_above_entry_bps: 1_000,
            arkade_unavailable: false,
        };
        assert!(quote.pauses(ENTRY));
        assert!(!quote.pauses(5_920));
    }

    #[test]
    fn settings_default_and_validate() {
        let settings = NetworkFeeSettings::default();
        assert_eq!(
            settings,
            NetworkFeeSettings {
                enabled: true,
                pool_players: 5,
                multiplier_percent: 150,
                base_vbytes: 368,
                vbytes_per_player: 26,
                conf_target: 2,
                min_sat_per_vb: 1,
                pause_above_entry_bps: 1_000,
            }
        );
        settings.validate().unwrap();
        // A config without the sections gets the defaults.
        let config = toml::to_string(&crate::config::Settings::default()).unwrap();
        let parsed: crate::config::Settings =
            toml::from_str(config.split("[network_fee_settings]").next().unwrap()).unwrap();
        assert_eq!(parsed.network_fee_settings, settings);
        assert_eq!(
            parsed.kickoff_check_settings,
            crate::config::KickoffCheckSettings::default()
        );
        let partial: NetworkFeeSettings = toml::from_str("multiplier_percent = 200").unwrap();
        assert_eq!(partial.multiplier_percent, 200);
        assert_eq!(partial.pool_players, 5);
        for bad in [
            NetworkFeeSettings {
                pool_players: 0,
                ..settings.clone()
            },
            NetworkFeeSettings {
                multiplier_percent: 0,
                ..settings.clone()
            },
            NetworkFeeSettings {
                conf_target: 0,
                ..settings.clone()
            },
            NetworkFeeSettings {
                min_sat_per_vb: 0,
                ..settings.clone()
            },
            NetworkFeeSettings {
                base_vbytes: 0,
                vbytes_per_player: 0,
                ..settings.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
