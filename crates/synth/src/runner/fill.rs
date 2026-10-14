//! Fill the competitions synth did not make before their entries close, so the people in them
//! get to play. See [`crate::config::FillConfig`] for when, and [`crate::scenarios::fill`] for
//! what a fill run does.

use std::collections::HashSet;

use anyhow::{ensure, Context, Result};
use log::{info, warn};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};
use uuid::Uuid;

use crate::client::competitions::CompetitionResponse;
use crate::config::FillConfig;
use crate::scenarios::{
    fill::{empty_seats, FILL_COMPETITION},
    ScenarioConfig, WindowShape,
};

/// Most players a run enters, as any run.
const MAX_PLAYERS: u64 = 100;

/// The competitions in `listed` to fill at `now`, each with the players it needs: still taking
/// entries, closing within `fill.before_close_secs` yet late enough for synth's players to pay
/// and submit, at most `fill.max_entry_fee_sats` an entry, with `fill.min_entries` paid, and
/// seats empty beyond `fill.leave_open`. Whether synth made one is for the caller to check.
pub fn wanted<'a>(
    listed: &'a [CompetitionResponse],
    now: OffsetDateTime,
    fill: &FillConfig,
    base: &ScenarioConfig,
) -> Vec<(&'a CompetitionResponse, usize)> {
    listed
        .iter()
        .filter(|competition| competition.open_to_enter(now))
        .filter(|competition| {
            competition.entries_close().is_some_and(|close| {
                let left = close - now;
                left <= Duration::seconds(fill.before_close_secs as i64)
                    && left.is_positive()
                    && base
                        .entry_timing
                        .validate(left.whole_seconds() as u64)
                        .is_ok()
            })
        })
        .filter(|competition| {
            entry_fee(competition).is_some_and(|fee| fee <= fill.max_entry_fee_sats)
        })
        .filter(|competition| competition.paid_entries() >= fill.min_entries)
        .filter_map(|competition| {
            let players = empty_seats(competition)?
                .saturating_sub(fill.leave_open)
                .min(MAX_PLAYERS);
            (players > 0).then_some((competition, players as usize))
        })
        .collect()
}

fn entry_fee(competition: &CompetitionResponse) -> Option<u64> {
    competition
        .event_submission
        .get("entry_fee")
        .and_then(serde_json::Value::as_u64)
}

/// The plan of a run that fills `competition` with `players` of synth's, from `base`, synth's
/// defaults: the competition's stations, fee, window and picks, with the time left before its
/// entries close as the window synth's players arrive in.
pub fn plan(
    base: &ScenarioConfig,
    competition: &CompetitionResponse,
    players: usize,
    now: OffsetDateTime,
) -> Result<ScenarioConfig> {
    let close = competition
        .entries_close()
        .context("it gives no entry deadline")?;
    let end = competition
        .event_submission
        .get("end_observation_date")
        .and_then(serde_json::Value::as_str)
        .and_then(|end| OffsetDateTime::parse(end, &Rfc3339).ok())
        .context("it gives no end to its observations")?;
    let picks = competition
        .event_submission
        .get("number_of_values_per_entry")
        .and_then(serde_json::Value::as_u64)
        .context("it does not say how many picks an entry makes")?;
    let stations = competition.stations();
    ensure!(!stations.is_empty(), "it names no stations");
    let window = (end - close).whole_seconds();
    ensure!(window > 0, "its observations end before they start");
    let shape = WindowShape::of(close, window as u64);
    // Synth's players pick the metrics the window scores; a competition scoring others would
    // refuse their entries.
    if let Some(fields) = competition.event_submission.get("scoring_fields") {
        let scored: HashSet<&str> = fields
            .as_array()
            .context("its scoring fields are not a list")?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        let [high, low, wind] = shape.scores();
        let picked: HashSet<&str> = [(high, "temp_high"), (low, "temp_low"), (wind, "wind_speed")]
            .into_iter()
            .filter_map(|(scores, field)| scores.then_some(field))
            .collect();
        ensure!(
            scored == picked,
            "it scores {scored:?}, but synth's players pick {picked:?} for its window"
        );
    }
    ensure!(
        (1..=(stations.len() * shape.metrics()) as u64).contains(&picks),
        "its entries make {picks} picks, more than its stations and metrics offer"
    );
    Ok(ScenarioConfig {
        planned_scenario: None,
        seed: None,
        entry_plan: Vec::new(),
        observation_window_choices: Vec::new(),
        users: players,
        stations,
        entry_fee: entry_fee(competition).context("it gives no entry fee")? as usize,
        entry_window_secs: (close - now).whole_seconds().max(0) as u64,
        observation_window_secs: window as u64,
        queue_players: None,
        max_pool_players: None,
        queue_max_entries: None,
        places: None,
        player_mix: None,
        min_players: None,
        competition_id: Some(competition.id),
        observation_start: Some(close),
        seats: None,
        listed: false,
        stress: None,
        backfill: None,
        picks_per_entry: Some(picks as usize),
        lane: None,
        ..base.clone()
    })
}

impl super::Runner {
    /// Every `fill.check_interval_secs`, start a fill run for each competition [`wanted`] that
    /// no run of synth's made or took up. While the fill scenario is paused, none starts.
    pub async fn run_fill(&self, fill: FillConfig, base: ScenarioConfig) -> Result<()> {
        fill.validate(&base.entry_timing)?;
        info!(
            "Filling other people's competitions from {} s before their entries close",
            fill.before_close_secs
        );
        // Competitions already filled, or found not to be fillable.
        let mut seen: HashSet<Uuid> = HashSet::new();
        let mut checks =
            tokio::time::interval(std::time::Duration::from_secs(fill.check_interval_secs));
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            checks.tick().await;
            if !fill.enabled
                || !self
                    .db
                    .scenario_enabled(FILL_COMPETITION)
                    .await
                    .unwrap_or(false)
            {
                continue;
            }
            let listed = match self.client.list_open_competitions().await {
                Ok(listed) => listed,
                Err(error) => {
                    warn!("Cannot list the competitions to fill: {error:#}");
                    continue;
                }
            };
            let now = OffsetDateTime::now_utc();
            for (competition, players) in wanted(&listed, now, &fill, &base) {
                let id = competition.id;
                if seen.contains(&id) || self.live.contains_key(&id) {
                    continue;
                }
                match self.db.has_run_for(&id).await {
                    Ok(false) => {}
                    Ok(true) => {
                        seen.insert(id);
                        continue;
                    }
                    Err(error) => {
                        warn!("Cannot tell whether synth made competition {id}: {error:#}");
                        continue;
                    }
                }
                seen.insert(id);
                let config = match plan(&base, competition, players, now) {
                    Ok(config) => config,
                    Err(error) => {
                        warn!("Competition {id} cannot be filled: {error:#}");
                        continue;
                    }
                };
                info!(
                    "Filling competition {id}: {players} of synth's players enter before {}",
                    competition
                        .entries_close()
                        .map_or_else(|| "entries close".into(), |close| close.to_string())
                );
                crate::server::metrics::record_fill(players);
                let runner = self.clone();
                tokio::spawn(async move {
                    if let Err(why) = runner.attempt(FILL_COMPETITION, config).await {
                        warn!("Filling competition {id} failed: {why}");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::{ArrivalPattern, DelayRange, EntryTiming};

    fn at(text: &str) -> OffsetDateTime {
        OffsetDateTime::parse(text, &Rfc3339).unwrap()
    }

    fn queue(entries: u64, held: u64, close: &str) -> CompetitionResponse {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::now_v7(),
            "created_at": "2026-10-14T12:34:00Z",
            "event_submission": {
                "start_observation_date": close,
                "end_observation_date": "2026-10-16T00:00:00Z",
                "locations": ["KFTY", "KATL", "KAIO"],
                "number_of_values_per_entry": 9,
                "scoring_fields": ["temp_high", "temp_low", "wind_speed"],
                "entry_fee": 5000,
                "total_allowed_entries": 20,
            },
            "total_entries": 0,
            "total_paid_entries": 0,
            "total_paid_out_entries": 0,
            "kind": "queued",
            "state": "created",
            "max_entries": 20,
            "entries": entries,
            "held": held,
        }))
        .unwrap()
    }

    fn fill() -> FillConfig {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    fn base() -> ScenarioConfig {
        ScenarioConfig {
            entry_timing: EntryTiming {
                arrival: DelayRange {
                    min_secs: 0,
                    max_secs: 90,
                },
                arrival_pattern: ArrivalPattern::Spread,
                before_payment: DelayRange {
                    min_secs: 5,
                    max_secs: 60,
                },
                before_submit: DelayRange {
                    min_secs: 10,
                    max_secs: 120,
                },
                deadline_margin_secs: 60,
            },
            lightning_address: Some("freya@lnurl.5day4cast.com".into()),
            player_mix: Some(Default::default()),
            ..Default::default()
        }
    }

    #[test]
    fn only_competitions_closing_soon_with_empty_seats_are_filled() {
        let close = "2026-10-15T00:00:00Z";
        let listed = vec![
            queue(1, 1, close),
            queue(0, 0, close),
            queue(20, 20, close),
            queue(3, 5, "2026-10-15T06:00:00Z"),
        ];
        let base = base();
        let fill = fill();
        // Hours ahead: nothing yet.
        assert!(wanted(&listed, at("2026-10-14T20:00:00Z"), &fill, &base).is_empty());
        // Twenty minutes ahead: the two with empty seats, to their caps.
        let now = at("2026-10-14T23:40:00Z");
        let found: Vec<_> = wanted(&listed, now, &fill, &base)
            .into_iter()
            .map(|(competition, players)| (competition.id, players))
            .collect();
        assert_eq!(found, [(listed[0].id, 19), (listed[1].id, 20)]);
        // Too late for synth's players to pay and submit.
        assert!(wanted(&listed, at("2026-10-14T23:57:00Z"), &fill, &base).is_empty());
        // Seats left for people, a minimum of entries, and a fee cap.
        let fussy: FillConfig = serde_json::from_value(serde_json::json!({
            "leave_open": 2, "min_entries": 1, "max_entry_fee_sats": 5000
        }))
        .unwrap();
        let found: Vec<_> = wanted(&listed, now, &fussy, &base)
            .into_iter()
            .map(|(competition, players)| (competition.id, players))
            .collect();
        assert_eq!(found, [(listed[0].id, 17)]);
        let cheap: FillConfig =
            serde_json::from_value(serde_json::json!({ "max_entry_fee_sats": 4999 })).unwrap();
        assert!(wanted(&listed, now, &cheap, &base).is_empty());
    }

    #[test]
    fn a_fill_plan_takes_the_competitions_terms() {
        let competition = queue(1, 1, "2026-10-15T00:00:00Z");
        let now = at("2026-10-14T23:30:00Z");
        let config = plan(&base(), &competition, 19, now).unwrap();
        assert_eq!(config.competition_id, Some(competition.id));
        assert_eq!(config.users, 19);
        assert_eq!(config.stations, ["KFTY", "KATL", "KAIO"]);
        assert_eq!(config.entry_fee, 5000);
        assert_eq!(config.entry_window_secs, 1800);
        assert_eq!(config.observation_window_secs, 86_400);
        assert_eq!(config.values_per_entry(), 9);
        assert!(config.player_mix.is_none());
        let resolved = config.resolve_plan(FILL_COMPETITION).unwrap();
        assert_eq!(resolved.entry_plan.len(), 19);
        assert_eq!(resolved.competition_id, Some(competition.id));
        assert!(resolved
            .entry_plan
            .iter()
            .all(|entry| entry.arrival_secs < 1800));

        // A competition scoring only the high cannot take synth's picks.
        let mut highs = competition.clone();
        highs.event_submission["scoring_fields"] = serde_json::json!(["temp_high"]);
        assert!(plan(&base(), &highs, 19, now).is_err());
        // A fill run must name its competition.
        let mut unnamed = config;
        unnamed.competition_id = None;
        assert!(unnamed.resolve_plan(FILL_COMPETITION).is_err());
    }
}
