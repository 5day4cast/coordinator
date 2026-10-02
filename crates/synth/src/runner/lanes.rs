//! Lanes: streams of competitions that run side by side, like the games real players find.
//!
//! Each lane starts a run on its own cadence and does not wait for its earlier runs, so its
//! competitions overlap: one taking entries while others are observed, signed and paid out. A
//! lane with an hourly cadence and an hour-long entry window always has a competition open. A lane
//! aligned to the UTC halves times its runs so entries close at 00:00 and 12:00 UTC, the only
//! starts the oracle takes for a 12-hour window.

use rand::seq::IndexedRandom;
use serde::Deserialize;
use time::{Duration, OffsetDateTime, Time};

use crate::scenarios::{queued::QUEUED_ONE_POOL, Backfill, Fill, ScenarioConfig};

/// When a lane's runs close their entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    /// `entry_window_secs` after each run starts, a run every `interval_secs`.
    #[default]
    Interval,
    /// At 00:00 and 12:00 UTC: a run starts `entry_window_secs` before each.
    UtcHalf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneConfig {
    pub name: String,
    /// Between the starts of two runs. Unused by a lane aligned to the UTC halves.
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    #[serde(default)]
    pub align: Align,
    /// The scenarios its runs take turns at.
    pub scenarios: Vec<String>,
    /// The observation windows its runs take turns at, once through the scenarios each; the
    /// defaults' otherwise. 24 hours or more, or 12 hours in a lane aligned to the UTC halves.
    #[serde(default)]
    pub observation_windows_secs: Option<Vec<u64>>,
    /// Instead of the defaults'.
    #[serde(default)]
    pub entry_window_secs: Option<u64>,
    /// The stations its competitions draw from; the defaults' otherwise.
    #[serde(default)]
    pub stations: Option<Vec<String>>,
    /// How many of `stations` each competition takes, drawn per run; all of them otherwise.
    #[serde(default)]
    pub stations_per_run: Option<usize>,
    /// How its runs fill their competitions: every drawn player enters, or the competition stays
    /// open to anyone and synth fills it late with the players it still needs.
    #[serde(default)]
    pub fill: Fill,
    /// When backfilling: drawn players who enter over the window, so the page is not empty.
    #[serde(default = "default_early_players")]
    pub early_players: usize,
    /// When backfilling: how long before entries close synth enters the players still needed.
    #[serde(default = "default_backfill_before_close_secs")]
    pub backfill_before_close_secs: u64,
    /// When backfilling: players above the competition's minimum synth makes sure of.
    #[serde(default = "default_backfill_margin")]
    pub backfill_margin: u64,
}

fn default_interval_secs() -> u64 {
    3600
}

fn default_early_players() -> usize {
    1
}

fn default_backfill_before_close_secs() -> u64 {
    1800
}

fn default_backfill_margin() -> u64 {
    1
}

const HALF_DAY: u64 = 43_200;
const DAY: u64 = 86_400;

impl LaneConfig {
    /// Check the lane against the oracle's windows and `base`, the defaults its runs start from.
    pub fn validate(&self, base: &ScenarioConfig) -> anyhow::Result<()> {
        let name = &self.name;
        anyhow::ensure!(
            self.interval_secs > 0,
            "lane {name}: interval_secs must be positive"
        );
        anyhow::ensure!(
            !self.scenarios.is_empty()
                && self
                    .scenarios
                    .iter()
                    .all(|scenario| super::SCENARIOS.contains(&scenario.as_str())),
            "lane {name}: scenarios must name supported scenarios"
        );
        let windows = self.windows(base);
        anyhow::ensure!(
            !windows.is_empty(),
            "lane {name}: needs an observation window"
        );
        for window in windows {
            let fits = matches!(
                (window, self.align),
                (DAY..=604_800, _) | (HALF_DAY, Align::UtcHalf)
            );
            anyhow::ensure!(
                fits,
                "lane {name}: an observation window of {window} s is not one the oracle attests \
                 (24 hours to 7 days, or 12 hours in a lane aligned to the UTC halves)"
            );
        }
        let stations = self.stations.as_ref().unwrap_or(&base.stations);
        anyhow::ensure!(!stations.is_empty(), "lane {name}: needs stations");
        if let Some(count) = self.stations_per_run {
            anyhow::ensure!(
                (1..=stations.len()).contains(&count),
                "lane {name}: stations_per_run must be 1 to the {} stations",
                stations.len()
            );
        }
        let entry_window = self.entry_window_secs.unwrap_or(base.entry_window_secs);
        base.entry_timing
            .validate(entry_window)
            .map_err(|error| anyhow::anyhow!("lane {name}: {error:#}"))?;
        if self.align == Align::UtcHalf {
            anyhow::ensure!(
                entry_window < HALF_DAY,
                "lane {name}: the entry window must be shorter than the 12 hours between runs"
            );
        }
        if let Some(backfill) = self.backfill() {
            // A single competition that is not full when entries close is cancelled; a queue
            // forms a pool of whoever entered, if they are enough.
            anyhow::ensure!(
                self.scenarios
                    .iter()
                    .all(|scenario| scenario == QUEUED_ONE_POOL),
                "lane {name}: a backfilled lane runs only {QUEUED_ONE_POOL}, whose queue starts \
                 with whoever entered; a single competition that is not full when entries close \
                 is cancelled"
            );
            backfill
                .validate(&base.entry_timing, entry_window)
                .map_err(|error| anyhow::anyhow!("lane {name}: {error:#}"))?;
        }
        Ok(())
    }

    /// How its runs backfill, if they do.
    pub fn backfill(&self) -> Option<Backfill> {
        (self.fill == Fill::Backfill).then_some(Backfill {
            early_players: self.early_players,
            before_close_secs: self.backfill_before_close_secs,
            margin: self.backfill_margin,
        })
    }

    fn windows(&self, base: &ScenarioConfig) -> Vec<u64> {
        self.observation_windows_secs
            .clone()
            .unwrap_or_else(|| base.observation_window_choices.clone())
    }

    /// When the lane's next run after `after` starts, and for an aligned lane when its entries
    /// close.
    pub fn next_start(
        &self,
        base: &ScenarioConfig,
        after: OffsetDateTime,
    ) -> (OffsetDateTime, Option<OffsetDateTime>) {
        match self.align {
            Align::Interval => (after, None),
            Align::UtcHalf => {
                let entry_window = Duration::seconds(
                    self.entry_window_secs.unwrap_or(base.entry_window_secs) as i64,
                );
                let close = next_half(after + entry_window);
                (close - entry_window, Some(close))
            }
        }
    }

    /// The plan for the lane's `cycle`th run, whose entries close at `close` if it is aligned.
    pub fn run_config(
        &self,
        base: &ScenarioConfig,
        cycle: usize,
        close: Option<OffsetDateTime>,
    ) -> (String, ScenarioConfig) {
        let scenario = self.scenarios[cycle % self.scenarios.len()].clone();
        let windows = self.windows(base);
        let mut config = base.clone();
        config.observation_window_choices.clear();
        config.observation_window_secs = windows[(cycle / self.scenarios.len()) % windows.len()];
        config.seed = base.seed.map(|seed| seed.wrapping_add(cycle as u64));
        config.competition_id = None;
        config.observation_start = close;
        if let Some(entry_window) = self.entry_window_secs {
            config.entry_window_secs = entry_window;
        }
        let stations = self.stations.as_ref().unwrap_or(&base.stations);
        config.stations = match self.stations_per_run {
            Some(count) => stations
                .choose_multiple(&mut rand::rng(), count)
                .cloned()
                .collect(),
            None => stations.clone(),
        };
        config.backfill = self.backfill();
        if config.backfill.is_some() {
            // Pools as large as the coordinator allows, so the queue takes anyone who comes.
            config.max_pool_players = None;
        }
        (scenario, config)
    }
}

/// The first 00:00 or 12:00 UTC at or after `at`.
pub fn next_half(at: OffsetDateTime) -> OffsetDateTime {
    let at = at.to_offset(time::UtcOffset::UTC);
    let midnight = at.replace_time(Time::MIDNIGHT);
    [
        midnight,
        midnight + Duration::hours(12),
        midnight + Duration::DAY,
    ]
    .into_iter()
    .find(|half| *half >= at)
    .expect("the next midnight is later")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::WindowShape;
    use time::macros::datetime;

    fn lane(align: Align, windows: &[u64]) -> LaneConfig {
        LaneConfig {
            name: "test".into(),
            interval_secs: 3600,
            align,
            scenarios: vec!["full_lifecycle".into(), "queued_one_pool".into()],
            observation_windows_secs: Some(windows.to_vec()),
            entry_window_secs: Some(3600),
            stations: Some(vec![
                "KDEN".into(),
                "KJFK".into(),
                "KORD".into(),
                "KSEA".into(),
            ]),
            stations_per_run: Some(2),
            fill: Fill::Immediate,
            early_players: 1,
            backfill_before_close_secs: 1800,
            backfill_margin: 1,
        }
    }

    #[test]
    fn halves_are_the_next_midnight_or_noon_utc() {
        assert_eq!(
            next_half(datetime!(2026-09-28 11:00:01 UTC)),
            datetime!(2026-09-28 12:00 UTC)
        );
        assert_eq!(
            next_half(datetime!(2026-09-28 12:00 UTC)),
            datetime!(2026-09-28 12:00 UTC)
        );
        assert_eq!(
            next_half(datetime!(2026-09-28 18:30 UTC)),
            datetime!(2026-09-29 00:00 UTC)
        );
        // In another offset, the same instant.
        assert_eq!(
            next_half(datetime!(2026-09-28 07:30 -4)),
            datetime!(2026-09-28 12:00 UTC)
        );
    }

    #[test]
    fn an_aligned_lane_closes_entries_on_the_half_with_the_windows_metrics() {
        let base = ScenarioConfig::default();
        let lane = lane(Align::UtcHalf, &[HALF_DAY, DAY]);
        lane.validate(&base).unwrap();
        // At 10:30 the next close an hour's entries can reach is noon; the run starts at 11:00.
        let (start, close) = lane.next_start(&base, datetime!(2026-09-28 10:30 UTC));
        assert_eq!(start, datetime!(2026-09-28 11:00 UTC));
        assert_eq!(close, Some(datetime!(2026-09-28 12:00 UTC)));
        // At 11:30 it is too late for noon.
        let (start, close) = lane.next_start(&base, datetime!(2026-09-28 11:30 UTC));
        assert_eq!(start, datetime!(2026-09-28 23:00 UTC));
        assert_eq!(close, Some(datetime!(2026-09-29 00:00 UTC)));

        let (scenario, config) = lane.run_config(&base, 0, close);
        assert_eq!(scenario, "full_lifecycle");
        assert_eq!(config.observation_window_secs, HALF_DAY);
        assert_eq!(config.window_shape(), WindowShape::Night);
        assert_eq!(config.stations.len(), 2);
        assert_eq!(config.values_per_entry(), 4, "two stations, low and wind");
        let times = config.competition_times(datetime!(2026-09-28 23:00:05 UTC));
        assert_eq!(times.start, datetime!(2026-09-29 00:00 UTC));
        assert_eq!(times.end, datetime!(2026-09-29 12:00 UTC));
        // Through the scenarios once, then the next window.
        let (scenario, config) = lane.run_config(&base, 3, Some(datetime!(2026-09-29 12:00 UTC)));
        assert_eq!(scenario, "queued_one_pool");
        assert_eq!(config.observation_window_secs, DAY);
        assert_eq!(config.window_shape(), WindowShape::FullDay);
        assert_eq!(config.values_per_entry(), 6);
    }

    #[test]
    fn windows_the_oracle_cannot_attest_are_refused() {
        let base = ScenarioConfig::default();
        assert!(lane(Align::Interval, &[DAY, 3 * DAY])
            .validate(&base)
            .is_ok());
        for (align, windows) in [
            (Align::Interval, &[HALF_DAY][..]),
            (Align::UtcHalf, &[7200][..]),
            (Align::Interval, &[][..]),
            (Align::Interval, &[8 * DAY][..]),
        ] {
            assert!(lane(align, windows).validate(&base).is_err(), "{windows:?}");
        }
        let mut unknown = lane(Align::Interval, &[DAY]);
        unknown.scenarios = vec!["not_a_scenario".into()];
        assert!(unknown.validate(&base).is_err());
        let mut too_many = lane(Align::Interval, &[DAY]);
        too_many.stations_per_run = Some(5);
        assert!(too_many.validate(&base).is_err());
    }
}
