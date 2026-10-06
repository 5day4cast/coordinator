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
    /// How eligible stations are ranked; weather selection is the default.
    #[serde(default)]
    pub picker: Option<crate::picker::PickerConfig>,
    /// Set false to put a stress run's competitions on the oracle's public list. Every other
    /// scenario's competitions stay unlisted.
    #[serde(default)]
    pub unlisted: Option<bool>,
    /// How the lane's stress runs push their competition; the defaults otherwise.
    #[serde(default)]
    pub stress: Option<crate::scenarios::stress::StressSettings>,
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
    /// Picks each entry makes, instead of the defaults'; at most every station and metric the
    /// window scores, which is what an unset value takes.
    #[serde(default)]
    pub picks_per_entry: Option<usize>,
    /// The most entries its queued runs take, instead of the scenario's own: for
    /// `queued_one_pool`, its pool's seats.
    #[serde(default)]
    pub max_entries: Option<u32>,
    /// The places its queued runs' pools of ten or more pay, instead of the scenario's own: two
    /// (70% and 30%) for `queued_one_pool`, one for the others.
    #[serde(default)]
    pub places: Option<u32>,
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
        let stations = self.configured_stations(base);
        anyhow::ensure!(!stations.is_empty(), "lane {name}: needs stations");
        if let Some(count) = self.stations_per_run {
            anyhow::ensure!(
                (1..=stations.len()).contains(&count),
                "lane {name}: stations_per_run must be 1 to the {} stations",
                stations.len()
            );
        }
        if let Some(picker) = &self.picker {
            picker
                .validate()
                .map_err(|error| anyhow::anyhow!("lane {name}: {error:#}"))?;
        }
        anyhow::ensure!(
            self.picks_per_entry != Some(0),
            "lane {name}: picks_per_entry must be at least 1"
        );
        let entry_window = self.entry_window_secs.unwrap_or(base.entry_window_secs);
        base.entry_timing
            .validate(entry_window)
            .map_err(|error| anyhow::anyhow!("lane {name}: {error:#}"))?;
        let stress = crate::scenarios::stress::STRESS_FULL_POOL;
        if self.scenarios.iter().any(|scenario| scenario == stress) {
            self.stress
                .clone()
                .unwrap_or_default()
                .validate(entry_window)
                .map_err(|error| anyhow::anyhow!("lane {name}: {error:#}"))?;
        }
        anyhow::ensure!(
            self.unlisted != Some(false)
                || self.scenarios.iter().all(|scenario| scenario == stress),
            "lane {name}: only {stress} runs can list their competitions; set unlisted = false \
             on a lane of those alone"
        );
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

    /// The stations the lane lists, or the defaults'.
    pub fn configured_stations<'a>(&'a self, base: &'a ScenarioConfig) -> &'a [String] {
        self.stations.as_ref().unwrap_or(&base.stations)
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
        config.listed = self.unlisted == Some(false);
        config.stress = self.stress.clone();
        if let Some(entry_window) = self.entry_window_secs {
            config.entry_window_secs = entry_window;
        }
        if self.picks_per_entry.is_some() {
            config.picks_per_entry = self.picks_per_entry;
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
            // The default competition's one pool of 20 seats, which takes anyone who comes until
            // it is full.
            config.max_pool_players = None;
        }
        if self.max_entries.is_some() {
            config.queue_max_entries = self.max_entries;
        }
        if self.places.is_some() {
            config.places = self.places;
        }
        (scenario, config)
    }
}

/// Between the first runs of two lanes after synth starts. Lanes started together sent the
/// oracle more requests at once than it takes.
pub const LANE_STAGGER: std::time::Duration = std::time::Duration::from_secs(45);

/// How long the `index`th lane waits after synth starts before its first run.
pub fn start_offset(index: usize) -> std::time::Duration {
    LANE_STAGGER * index as u32
}

/// Tries a lane makes again at a run that created no competition.
pub const START_RETRIES: u32 = 3;
/// Between those tries.
pub const START_RETRY: Duration = Duration::minutes(5);

/// When a lane tries again at a run that created no competition, after `failed` tries at it
/// failed, at `now`: None once it has tried [`START_RETRIES`] more times, or for a run whose
/// entries close at `close`, when trying again would leave less than half its entry window of
/// `entry_window_secs` to enter.
pub fn retry_start(
    failed: u32,
    now: OffsetDateTime,
    close: Option<OffsetDateTime>,
    entry_window_secs: u64,
) -> Option<OffsetDateTime> {
    let at = now + START_RETRY;
    let half_window = Duration::seconds((entry_window_secs / 2) as i64);
    (failed <= START_RETRIES && close.is_none_or(|close| close - at >= half_window)).then_some(at)
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
            picker: None,
            unlisted: None,
            stress: None,
            fill: Fill::Immediate,
            early_players: 1,
            backfill_before_close_secs: 1800,
            backfill_margin: 1,
            picks_per_entry: None,
            max_entries: None,
            places: None,
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

        // A lane may take fewer picks; never more than the window holds, nor none.
        let mut fewer = lane.clone();
        fewer.picks_per_entry = Some(3);
        fewer.validate(&base).unwrap();
        let (_, config) = fewer.run_config(&base, 3, Some(datetime!(2026-09-29 12:00 UTC)));
        assert_eq!(config.values_per_entry(), 3);
        fewer.picks_per_entry = Some(40);
        let (_, config) = fewer.run_config(&base, 3, Some(datetime!(2026-09-29 12:00 UTC)));
        assert_eq!(config.values_per_entry(), 6);
        fewer.picks_per_entry = Some(0);
        assert!(fewer.validate(&base).is_err());
    }

    #[test]
    fn lanes_start_apart_and_retry_a_failed_start_a_few_times() {
        assert_eq!(start_offset(0), std::time::Duration::ZERO);
        assert_eq!(start_offset(2), std::time::Duration::from_secs(90));
        let now = datetime!(2026-10-03 15:42 UTC);
        // An interval lane's run takes its entries from when it is created.
        assert_eq!(retry_start(1, now, None, 7200), Some(now + START_RETRY));
        assert_eq!(
            retry_start(START_RETRIES, now, None, 7200),
            Some(now + START_RETRY)
        );
        assert_eq!(retry_start(START_RETRIES + 1, now, None, 7200), None);
        // An aligned lane's run closes entries at the half, whatever time it starts.
        let close = datetime!(2026-10-04 00:00 UTC);
        assert!(retry_start(1, datetime!(2026-10-03 23:00 UTC), Some(close), 3600).is_some());
        assert!(
            retry_start(1, datetime!(2026-10-03 23:30 UTC), Some(close), 3600).is_none(),
            "a retry at 23:35 leaves less than half the hour to enter"
        );
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

    /// A stress lane's runs are unlisted unless it says otherwise, and only stress runs may be
    /// listed.
    #[test]
    fn only_a_stress_lane_lists_its_competitions() {
        let base = ScenarioConfig::default();
        let mut stress = lane(Align::Interval, &[DAY]);
        stress.scenarios = vec![crate::scenarios::stress::STRESS_FULL_POOL.into()];
        stress.validate(&base).unwrap();
        let (_, config) = stress.run_config(&base, 0, None);
        assert!(!config.listed);
        stress.unlisted = Some(false);
        stress.validate(&base).unwrap();
        let (scenario, config) = stress.run_config(&base, 0, None);
        assert_eq!(scenario, "stress_full_pool");
        assert!(config.listed);
        let mut mixed = lane(Align::Interval, &[DAY]);
        mixed.unlisted = Some(false);
        assert!(mixed.validate(&base).is_err());
        // The burst must fit the lane's entry window.
        stress.entry_window_secs = Some(120);
        assert!(stress.validate(&base).is_err());
    }

    /// The open lane's backfilled runs make the default competition: one pool of 20 seats that
    /// pays two places from ten players. A lane may set its own cap and places.
    #[test]
    fn a_backfilled_lane_makes_the_default_competition() {
        use crate::scenarios::queued::QueueShape;
        let base = ScenarioConfig {
            queue_players: Some(5),
            ..Default::default()
        };
        let mut open = lane(Align::Interval, &[DAY]);
        open.scenarios = vec![QUEUED_ONE_POOL.into()];
        open.fill = Fill::Backfill;
        let (scenario, config) = open.run_config(&base, 0, None);
        let shape = QueueShape::of(&scenario, &config).unwrap().unwrap();
        assert_eq!(
            (shape.rules.max_players(), shape.max_entries, shape.places),
            (20, Some(20), 2)
        );
        open.places = Some(1);
        open.max_entries = Some(12);
        let (scenario, config) = open.run_config(&base, 0, None);
        let shape = QueueShape::of(&scenario, &config).unwrap().unwrap();
        assert_eq!((shape.max_entries, shape.places), (Some(12), 1));
    }
}
