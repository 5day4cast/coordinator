use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Inclusive, per-player wait bounds. Zero preserves the original immediate entry flow.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DelayRange {
    pub min_secs: u64,
    pub max_secs: u64,
}

/// When players arrive in the entry window.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArrivalPattern {
    /// Uniformly within `arrival`.
    #[default]
    Range,
    /// Over the whole window, as real players come: some early, some along the way, and a bunch
    /// near the deadline. From `arrival.min_secs` to the latest arrival that still leaves each
    /// player's payment and submission waits, the deadline margin and [`SPREAD_SLACK_SECS`] for
    /// the requests themselves. A player planned to abandon a ticket still arrives within
    /// `arrival`, so a replacement has time to follow.
    Spread,
}

/// What a spread arrival leaves for requesting, registering and paying for a ticket, beyond the
/// planned waits.
pub const SPREAD_SLACK_SECS: u64 = 90;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EntryTiming {
    pub arrival: DelayRange,
    pub arrival_pattern: ArrivalPattern,
    pub before_payment: DelayRange,
    pub before_submit: DelayRange,
    pub deadline_margin_secs: u64,
}

impl EntryTiming {
    /// The latest a spread arrival may come in an `entry_window_secs` window, or None if the
    /// window leaves no room after the waits.
    pub fn latest_spread_arrival(&self, entry_window_secs: u64) -> Option<u64> {
        let payment = self
            .before_payment
            .max_secs
            .checked_add(self.deadline_margin_secs.max(60))?;
        let submit = self
            .before_payment
            .max_secs
            .checked_add(self.before_submit.max_secs)?
            .checked_add(self.deadline_margin_secs)?;
        entry_window_secs
            .checked_sub(payment.max(submit))?
            .checked_sub(SPREAD_SLACK_SECS + 1)
            .filter(|latest| *latest > self.arrival.min_secs)
    }

    /// One spread arrival: a fifth early, two fifths anywhere, and two fifths near the deadline.
    fn spread_arrival(&self, latest: u64, rng: &mut impl rand::Rng) -> u64 {
        let earliest = self.arrival.min_secs;
        let span = latest - earliest;
        match rng.random_range(0..5u8) {
            0 => rng.random_range(earliest..=earliest + span * 15 / 100),
            1 | 2 => rng.random_range(earliest..=latest),
            _ => rng.random_range(latest - span * 25 / 100..=latest),
        }
    }

    pub fn validate(&self, entry_window_secs: u64) -> anyhow::Result<()> {
        if self.arrival_pattern == ArrivalPattern::Spread {
            anyhow::ensure!(
                self.latest_spread_arrival(entry_window_secs).is_some(),
                "entry_window_secs leaves no time to spread arrivals over after the entry waits"
            );
        }
        anyhow::ensure!(entry_window_secs > 60 && entry_window_secs <= 86_400,
            "entry_window_secs must be between 61 and 86400 seconds (invoices close 60 seconds before observations)");
        for range in [self.arrival, self.before_payment, self.before_submit] {
            anyhow::ensure!(
                range.min_secs <= range.max_secs,
                "entry timing minimum exceeds maximum"
            );
        }
        let payment = self
            .arrival
            .max_secs
            .checked_add(self.before_payment.max_secs)
            .ok_or_else(|| anyhow::anyhow!("entry timing overflow"))?;
        let submit = payment
            .checked_add(self.before_submit.max_secs)
            .ok_or_else(|| anyhow::anyhow!("entry timing overflow"))?;
        anyhow::ensure!(payment.checked_add(self.deadline_margin_secs.max(60)).is_some_and(|end| end < entry_window_secs),
            "arrival and payment waits must finish before the invoice deadline, including the safety margin");
        anyhow::ensure!(
            submit
                .checked_add(self.deadline_margin_secs)
                .is_some_and(|end| end < entry_window_secs),
            "entry waits and deadline margin must fit inside the entry window"
        );
        Ok(())
    }
}

/// How a run fills its competition.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    /// Every drawn player enters, as the arrival pattern plans.
    #[default]
    Immediate,
    /// The competition stays open to anyone: a few players enter early, so its page is not
    /// empty, and the rest wait until shortly before entries close, when synth enters only as
    /// many as the competition still needs to start. See [`Backfill`].
    Backfill,
}

/// A run that fills its competition late, with only the players it still needs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Backfill {
    /// Drawn players who enter over the window before the backfill.
    pub early_players: usize,
    /// How long before entries close synth works out how many more players it needs.
    pub before_close_secs: u64,
    /// Players above the competition's minimum synth makes sure of.
    pub margin: u64,
}

impl Backfill {
    /// Check the backfill leaves its players time to pay and submit, and the early players a
    /// window to arrive in, in an `entry_window_secs` window.
    pub fn validate(&self, timing: &EntryTiming, entry_window_secs: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.before_close_secs < entry_window_secs,
            "backfill_before_close_secs must be shorter than the entry window"
        );
        let waits = timing
            .before_payment
            .max_secs
            .saturating_add(timing.before_submit.max_secs)
            .saturating_add(timing.deadline_margin_secs.max(60))
            .saturating_add(SPREAD_SLACK_SECS);
        anyhow::ensure!(
            self.before_close_secs > waits,
            "backfill_before_close_secs must leave the late players their payment and submission \
             waits, the deadline margin and {SPREAD_SLACK_SECS} seconds ({waits} seconds)"
        );
        timing
            .validate(self.early_window_secs(entry_window_secs))
            .map_err(|error| anyhow::anyhow!("the window before the backfill: {error:#}"))
    }

    /// The part of an `entry_window_secs` window the early players arrive in: up to the backfill.
    pub fn early_window_secs(&self, entry_window_secs: u64) -> u64 {
        entry_window_secs.saturating_sub(self.before_close_secs)
    }
}

/// What a backfill found and does.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct BackfillCount {
    /// Everyone's paid entries, other people's included.
    pub paid_entries: u64,
    pub min_players: u64,
    pub margin: u64,
    /// Players the competition needs to reach its minimum and the margin.
    pub needed: u64,
    /// Drawn players still waiting to enter.
    pub waiting: usize,
    /// Of them, those who enter now.
    pub entering: usize,
}

impl BackfillCount {
    pub fn new(min_players: u64, margin: u64, paid_entries: u64, waiting: usize) -> Self {
        let needed = (min_players + margin).saturating_sub(paid_entries);
        Self {
            paid_entries,
            min_players,
            margin,
            needed,
            waiting,
            entering: usize::try_from(needed).unwrap_or(usize::MAX).min(waiting),
        }
    }

    /// The run drew too few players to make up what the competition needs.
    pub fn short(&self) -> bool {
        self.needed > self.entering as u64
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryBehavior {
    #[default]
    Complete,
    AbandonUnpaid,
    AbandonPaid,
    DuplicateSubmission,
    LateSubmission,
}

/// Resolved before any competition is created. Arrival is measured from its creation;
/// stage waits must still respect the actual invoice and competition deadlines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryPlan {
    pub user_index: usize,
    pub arrival_secs: u64,
    pub before_payment_secs: u64,
    pub before_submit_secs: u64,
    pub behavior: EntryBehavior,
}

/// An inclusive range of players.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlayerRange {
    pub min: usize,
    pub max: usize,
}

/// Players in a band of a [`PlayerMix`], drawn with `weight` against the other bands.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlayerBand {
    pub min: usize,
    pub max: usize,
    pub weight: u32,
}

/// How many players a run draws, like the crowds real competitions get.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PlayerMix {
    /// Single competitions, and queues meant to form one pool: mostly 5 to 10 players,
    /// sometimes 2 to 4.
    pub bands: Vec<PlayerBand>,
    /// A queue meant to split: more players than a pool holds.
    pub split: PlayerRange,
    /// The fewest players while network fees are above `small_pools_max_sat_per_vb`. The
    /// coordinator refuses a smaller competition then, and its kickoff check cancels a smaller
    /// pool.
    pub min_players_high_fees: usize,
    pub small_pools_max_sat_per_vb: u64,
}

impl Default for PlayerMix {
    fn default() -> Self {
        Self {
            bands: vec![
                PlayerBand {
                    min: 5,
                    max: 10,
                    weight: 7,
                },
                PlayerBand {
                    min: 2,
                    max: 4,
                    weight: 3,
                },
            ],
            split: PlayerRange { min: 26, max: 30 },
            min_players_high_fees: 5,
            small_pools_max_sat_per_vb: 2,
        }
    }
}

impl PlayerMix {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.bands.is_empty()
                && self.bands.iter().all(|band| 1 <= band.min
                    && band.min <= band.max
                    && band.max <= 100
                    && band.weight > 0),
            "players.bands must hold bands of 1 to 100 players with positive weights"
        );
        anyhow::ensure!(
            1 <= self.split.min && self.split.min <= self.split.max && self.split.max <= 100,
            "players.split must be a range of 1 to 100 players"
        );
        anyhow::ensure!(
            (1..=100).contains(&self.min_players_high_fees),
            "players.min_players_high_fees must be 1 to 100"
        );
        Ok(())
    }

    /// The fewest players a run may draw at `sat_per_vb`, or None when fees allow any.
    pub fn floor_at(&self, sat_per_vb: Option<f64>) -> Option<usize> {
        match sat_per_vb {
            Some(rate) if rate <= self.small_pools_max_sat_per_vb as f64 => None,
            _ => Some(self.min_players_high_fees),
        }
    }

    /// Draw from the bands, within `min..=max`: a band is drawn by weight and then a count in
    /// it, clamped to the bounds.
    fn draw(&self, rng: &mut impl rand::Rng, min: usize, max: usize) -> usize {
        let total: u32 = self.bands.iter().map(|band| band.weight).sum();
        let mut pick = rng.random_range(0..total);
        let band = self
            .bands
            .iter()
            .find(|band| {
                let found = pick < band.weight;
                pick = pick.saturating_sub(band.weight);
                found
            })
            .expect("a band below the total weight");
        rng.random_range(band.min..=band.max)
            .clamp(min, max.max(min))
    }

    /// How many players enter `scenario`, with at least `floor`. For a queued scenario this is
    /// the players who enter completely, `queue_players`; None for a scenario whose count is
    /// part of what it tests.
    pub fn players_for(
        &self,
        scenario: &str,
        seed: u64,
        floor: Option<usize>,
        max_pool: usize,
    ) -> Option<usize> {
        use rand::{Rng, SeedableRng};
        // Its own stream, so a fixed count replays the same player plan.
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(seed ^ 0x504c_4159_4552);
        let floor = floor.unwrap_or(1);
        Some(match scenario {
            super::queued::QUEUED_TOO_FEW => return None,
            super::queued::QUEUED_SPLIT => rng
                .random_range(self.split.min..=self.split.max)
                .max(max_pool + 1),
            super::queued::QUEUED_ONE_POOL => self.draw(&mut rng, floor.max(2), max_pool),
            super::queued::QUEUED_LEFTOVER_REFUND => self.draw(&mut rng, floor.max(2), 100),
            _ => self.draw(&mut rng, floor, 100),
        })
    }
}

/// Longer weather windows plus a deliberate short case for refund-path coverage.
pub fn default_observation_windows() -> Vec<u64> {
    vec![7200, 10800, 14400, 600]
}

/// Choose once per manual run; its saved ScenarioConfig keeps the resolved duration.
pub fn choose_observation_window(windows: &[u64]) -> u64 {
    use rand::Rng;
    windows[rand::rng().random_range(0..windows.len())]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioConfig {
    #[serde(default)]
    pub planned_scenario: Option<String>,
    /// Reproduce a run's selected timings and observation window. Chosen before recording if absent.
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub entry_timing: EntryTiming,
    #[serde(default)]
    pub entry_plan: Vec<EntryPlan>,
    /// Optional choices for a manual run; the resolved duration is saved below.
    #[serde(default)]
    pub observation_window_choices: Vec<u64>,
    /// Number of synthetic users
    pub users: usize,
    /// NOAA stations to use
    pub stations: Vec<String>,
    /// Entry fee in sats
    pub entry_fee: usize,
    #[serde(default = "default_max_ticket_fees_sats")]
    pub max_ticket_fees_sats: u64,
    /// Time before observation starts (entry window) in seconds
    pub entry_window_secs: u64,
    /// Observation window duration in seconds
    pub observation_window_secs: u64,
    /// Delay after observation ends before signing deadline, in seconds
    pub signing_delay_secs: u64,
    /// Max time to wait for each state transition (seconds)
    pub state_timeout_secs: u64,
    /// Poll interval for state transitions (seconds)
    pub poll_interval_secs: u64,
    /// The players' Lightning Address, where payouts and refunds go. The refund scenario needs
    /// one, since the enclave resolves a refund's address before signing anything; without one a
    /// cancelled entry's money stays in its escrow.
    #[serde(default)]
    pub lightning_address: Option<String>,
    /// The node a scenario pays entries from. Escrow scenarios need one, because ark-swapd
    /// funds an escrow from a real payment.
    #[serde(default)]
    pub lnd: Option<crate::lnd::LndConfig>,
    /// Max time to wait for a refund to settle (seconds), which includes its escrow's locktime.
    #[serde(default = "default_refund_timeout_secs")]
    pub refund_timeout_secs: u64,
    /// Players who enter a queued scenario completely, instead of the scenario's own number; see
    /// [`super::queued::QueueShape`]. `users` follows from it.
    #[serde(default)]
    pub queue_players: Option<usize>,
    /// The largest pool of a queued scenario, instead of 20 for `queued_one_pool`, the default
    /// competition, and 25 for the others.
    #[serde(default)]
    pub max_pool_players: Option<usize>,
    /// The most entries a queued scenario's queue takes, instead of its pool's seats for
    /// `queued_one_pool` and the coordinator's default for the others.
    #[serde(default)]
    pub queue_max_entries: Option<u32>,
    /// The places a queued scenario's pools of ten or more pay, instead of 2 for
    /// `queued_one_pool` (70% and 30%) and 1 for the others. Smaller pools pay one.
    #[serde(default)]
    pub places: Option<u32>,
    /// Draw the player count from this mix instead of using `users` (or the queued scenario's
    /// own number).
    #[serde(default)]
    pub player_mix: Option<PlayerMix>,
    /// The fewest players the mix may draw, while network fees are too high for small
    /// competitions. Set before the run is recorded.
    #[serde(default)]
    pub min_players: Option<usize>,
    /// The competition's id, chosen when the run is planned, so a run is known by its
    /// competition from its first step.
    #[serde(default)]
    pub competition_id: Option<uuid::Uuid>,
    /// When observations start and entries close. Unset, `entry_window_secs` after the
    /// competition is created; set for a window that must start on the hour, such as a day or
    /// night half.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub observation_start: Option<OffsetDateTime>,
    /// Seats in the competition, instead of one per player: for a competition left open for
    /// people as well as synth's players.
    #[serde(default)]
    pub seats: Option<usize>,
    /// Put the competition on the oracle's public list. Only the scenarios that take it set it;
    /// the others' competitions are always unlisted.
    #[serde(default)]
    pub listed: bool,
    /// How a stress run pushes its competition; the defaults when unset.
    #[serde(default)]
    pub stress: Option<super::stress::StressSettings>,
    /// Fill the competition late, with only the players it needs; every drawn player enters
    /// otherwise.
    #[serde(default)]
    pub backfill: Option<Backfill>,
}

/// What an observation window can score, as the oracle attests it: a window of 24 hours or more
/// holds every station's daytime high and overnight low; the day half, 12:00-24:00 UTC, only
/// highs; the night half, 00:00-12:00 UTC, only lows. Wind counts in all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowShape {
    FullDay,
    Day,
    Night,
}

impl WindowShape {
    pub fn of(start: OffsetDateTime, seconds: u64) -> Self {
        let start = start.to_offset(time::UtcOffset::UTC);
        let on_the_hour = start.minute() == 0 && start.second() == 0 && start.nanosecond() == 0;
        match (seconds, start.hour()) {
            (43_200, 12) if on_the_hour => Self::Day,
            (43_200, 0) if on_the_hour => Self::Night,
            _ => Self::FullDay,
        }
    }

    /// Whether it scores the daytime high, the overnight low and wind.
    pub fn scores(self) -> [bool; 3] {
        match self {
            Self::FullDay => [true, true, true],
            Self::Day => [true, false, true],
            Self::Night => [false, true, true],
        }
    }

    pub fn metrics(self) -> usize {
        self.scores().into_iter().filter(|scored| *scored).count()
    }
}

/// When a competition's entries close, its observations end and its contract must be signed.
#[derive(Debug, Clone, Copy)]
pub struct CompetitionTimes {
    pub id: uuid::Uuid,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    pub signing: OffsetDateTime,
}

pub fn default_max_ticket_fees_sats() -> u64 {
    1_000
}

fn default_refund_timeout_secs() -> u64 {
    30 * 60
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            planned_scenario: None,
            seed: None,
            entry_timing: EntryTiming::default(),
            entry_plan: vec![],
            observation_window_choices: vec![],
            users: 3,
            stations: vec!["KDEN".to_string(), "KJFK".to_string(), "KORD".to_string()],
            entry_fee: 1000,
            max_ticket_fees_sats: default_max_ticket_fees_sats(),
            entry_window_secs: 120,
            observation_window_secs: default_observation_windows()[0],
            signing_delay_secs: 60,
            state_timeout_secs: 600,
            poll_interval_secs: 5,
            lightning_address: None,
            lnd: None,
            refund_timeout_secs: default_refund_timeout_secs(),
            queue_players: None,
            max_pool_players: None,
            queue_max_entries: None,
            places: None,
            player_mix: None,
            min_players: None,
            competition_id: None,
            observation_start: None,
            seats: None,
            listed: false,
            stress: None,
            backfill: None,
        }
    }
}

impl ScenarioConfig {
    /// The planned competition's id and times, for a competition created `now`.
    pub fn competition_times(&self, now: OffsetDateTime) -> CompetitionTimes {
        let start = self
            .observation_start
            .unwrap_or(now + time::Duration::seconds(self.entry_window_secs as i64));
        let end = start + time::Duration::seconds(self.observation_window_secs as i64);
        CompetitionTimes {
            id: self.competition_id.unwrap_or_else(uuid::Uuid::now_v7),
            start,
            end,
            signing: end + time::Duration::seconds(self.signing_delay_secs as i64),
        }
    }

    /// What the planned window scores.
    pub fn window_shape(&self) -> WindowShape {
        match self.observation_start {
            Some(start) => WindowShape::of(start, self.observation_window_secs),
            None => WindowShape::FullDay,
        }
    }

    /// Picks each entry makes: one per station and metric the window scores.
    pub fn values_per_entry(&self) -> usize {
        self.stations.len() * self.window_shape().metrics()
    }

    pub fn resolve_plan(&self, scenario: &str) -> anyhow::Result<Self> {
        use rand::{Rng, SeedableRng};
        // A manual competition may be left to people alone.
        let fewest = usize::from(scenario != super::manual::MANUAL_COMPETITION);
        anyhow::ensure!(
            (fewest..=100).contains(&self.users),
            "users must be between {fewest} and 100"
        );
        let seed = self.seed.unwrap_or_else(rand::random);
        let mut config = self.clone();
        config.competition_id.get_or_insert_with(uuid::Uuid::now_v7);
        if let Some(mix) = &self.player_mix {
            mix.validate()?;
            let max_pool = super::queued::QueueShape::max_pool(scenario, self);
            match mix.players_for(scenario, seed, self.min_players, max_pool) {
                Some(players) if super::queued::is_queued(scenario) => {
                    config.queue_players = self.queue_players.or(Some(players));
                }
                Some(players) => config.users = players,
                None => {}
            }
        }
        if scenario == super::stress::STRESS_FULL_POOL {
            // A stress run's player count is part of what it tests.
            let stress = self.stress.clone().unwrap_or_default();
            stress.validate(config.entry_window_secs)?;
            config.users = stress.users;
        }
        if let Some(shape) = super::queued::QueueShape::of(scenario, &config)? {
            // A queued scenario's player count is part of what it tests.
            config.users = shape.users();
            config.entry_window_secs = shape.entry_window_secs(scenario, config.entry_window_secs);
        }
        if matches!(scenario, "abandoned_unpaid" | "paid_abandonment") {
            // The replacement cannot reserve this seat until the unpaid ticket's 10-minute
            // reservation expires; paid abandonment probes that the paid seat still cannot
            // be replaced at that point. Save this deadline before creating the competition.
            let required = [
                600,
                self.entry_timing.arrival.max_secs,
                self.entry_timing.before_payment.max_secs,
                self.entry_timing.before_submit.max_secs,
                self.entry_timing.deadline_margin_secs.max(60),
                120,
            ]
            .into_iter()
            .try_fold(0u64, u64::checked_add)
            .ok_or_else(|| anyhow::anyhow!("entry timing overflow"))?;
            config.entry_window_secs = config.entry_window_secs.max(required);
        }
        self.entry_timing.validate(config.entry_window_secs)?;
        if let Some(backfill) = &self.backfill {
            anyhow::ensure!(
                scenario == super::queued::QUEUED_ONE_POOL,
                "{scenario} cannot be backfilled: only {} is, since a single competition that is \
                 not full when entries close is cancelled",
                super::queued::QUEUED_ONE_POOL
            );
            backfill.validate(&self.entry_timing, config.entry_window_secs)?;
        }
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(seed);
        anyhow::ensure!(
            self.observation_window_choices
                .iter()
                .all(|seconds| (1..=604_800).contains(seconds)),
            "observation window choices must be between 1 second and 7 days"
        );
        if !self.observation_window_choices.is_empty() {
            // A fixed-duration replay must keep the same player plan. Select the weather
            // window from a separate stream so changing its choices consumes no player draws.
            let mut window_rng = rand_chacha::ChaCha20Rng::seed_from_u64(seed ^ 0x5749_4e44_4f57);
            config.observation_window_secs = self.observation_window_choices
                [window_rng.random_range(0..self.observation_window_choices.len())];
        }
        anyhow::ensure!(
            (1..=604_800).contains(&config.observation_window_secs),
            "observation window must be between 1 second and 7 days"
        );
        let behavior = match scenario {
            "full_lifecycle" | "escrow_refund" => EntryBehavior::Complete,
            super::stress::STRESS_FULL_POOL | super::manual::MANUAL_COMPETITION => {
                EntryBehavior::Complete
            }
            super::queued::QUEUED_SPLIT
            | super::queued::QUEUED_ONE_POOL
            | super::queued::QUEUED_TOO_FEW => EntryBehavior::Complete,
            "abandoned_unpaid" => EntryBehavior::AbandonUnpaid,
            "paid_abandonment" | super::queued::QUEUED_LEFTOVER_REFUND => {
                EntryBehavior::AbandonPaid
            }
            "duplicate_submission" => EntryBehavior::DuplicateSubmission,
            "late_submission" => EntryBehavior::LateSubmission,
            _ => anyhow::bail!("Unknown scenario: {scenario}"),
        };
        let exceptional_user = match config.users {
            0 => 0,
            users => rng.random_range(0..users),
        };
        config.seed = Some(seed);
        config.planned_scenario = Some(scenario.to_string());
        // A backfilled run's early players arrive before the backfill, and the others when it is
        // due, if they are needed then.
        let early_window = self.backfill.map_or(config.entry_window_secs, |backfill| {
            backfill.early_window_secs(config.entry_window_secs)
        });
        let early_players = self
            .backfill
            .map_or(config.users, |backfill| backfill.early_players);
        let spread = match self.entry_timing.arrival_pattern {
            ArrivalPattern::Range => None,
            ArrivalPattern::Spread => self.entry_timing.latest_spread_arrival(early_window),
        };
        let abandons = matches!(
            behavior,
            EntryBehavior::AbandonUnpaid | EntryBehavior::AbandonPaid
        );
        config.entry_plan = (0..config.users)
            .map(|user_index| EntryPlan {
                user_index,
                arrival_secs: match spread {
                    _ if user_index >= early_players => early_window,
                    // A replacement follows an abandoned ticket, so it is left early.
                    Some(latest) if !(abandons && user_index == exceptional_user) => {
                        self.entry_timing.spread_arrival(latest, &mut rng)
                    }
                    _ => rng.random_range(
                        self.entry_timing.arrival.min_secs..=self.entry_timing.arrival.max_secs,
                    ),
                },
                before_payment_secs: rng.random_range(
                    self.entry_timing.before_payment.min_secs
                        ..=self.entry_timing.before_payment.max_secs,
                ),
                before_submit_secs: rng.random_range(
                    self.entry_timing.before_submit.min_secs
                        ..=self.entry_timing.before_submit.max_secs,
                ),
                behavior: if user_index == exceptional_user {
                    behavior
                } else {
                    EntryBehavior::Complete
                },
            })
            .collect();
        if scenario == super::stress::STRESS_FULL_POOL {
            super::stress::burst(&mut config, &mut rng);
        }
        Ok(config)
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn timed_config() -> ScenarioConfig {
        ScenarioConfig {
            seed: Some(42),
            entry_window_secs: 600,
            observation_window_choices: default_observation_windows(),
            entry_timing: EntryTiming {
                arrival: DelayRange {
                    min_secs: 0,
                    max_secs: 90,
                },
                arrival_pattern: ArrivalPattern::Range,
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
            ..Default::default()
        }
    }

    #[test]
    fn seed_replays_every_player_wait_and_manual_observation_window() {
        let config = timed_config();
        let first = config.resolve_plan("duplicate_submission").unwrap();
        let second = config.resolve_plan("duplicate_submission").unwrap();
        assert_eq!(first.entry_plan, second.entry_plan);
        assert_eq!(
            first.observation_window_secs,
            second.observation_window_secs
        );
        assert_eq!(first.seed, Some(42));
        assert_eq!(
            first.planned_scenario.as_deref(),
            Some("duplicate_submission")
        );
        assert_eq!(
            first
                .entry_plan
                .iter()
                .filter(|plan| plan.behavior == EntryBehavior::DuplicateSubmission)
                .count(),
            1
        );
        for (index, plan) in first.entry_plan.iter().enumerate() {
            assert_eq!(plan.user_index, index);
            assert!(plan.arrival_secs <= 90);
            assert!((5..=60).contains(&plan.before_payment_secs));
            assert!((10..=120).contains(&plan.before_submit_secs));
        }
        let restored: ScenarioConfig =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(restored.entry_plan, first.entry_plan);
        let mut fixed_window = config.clone();
        fixed_window.observation_window_choices.clear();
        fixed_window.observation_window_secs = first.observation_window_secs;
        assert_eq!(fixed_window.resolve_plan("duplicate_submission").unwrap().entry_plan, first.entry_plan,
            "replaying the recorded duration must not consume a different number of player RNG draws");
        let mut different = config;
        different.seed = Some(43);
        assert_ne!(
            different
                .resolve_plan("duplicate_submission")
                .unwrap()
                .entry_plan,
            first.entry_plan
        );
    }

    #[test]
    fn legacy_defaults_have_no_delays_and_abandonment_records_a_usable_window() {
        let config = ScenarioConfig::default();
        let normal = config.resolve_plan("full_lifecycle").unwrap();
        assert_eq!(normal.entry_window_secs, 120);
        assert!(normal.entry_plan.iter().all(|plan| plan.arrival_secs == 0
            && plan.before_payment_secs == 0
            && plan.before_submit_secs == 0
            && plan.behavior == EntryBehavior::Complete));
        let abandoned = timed_config().resolve_plan("abandoned_unpaid").unwrap();
        assert_eq!(abandoned.entry_window_secs, 1050);
        assert_eq!(
            timed_config()
                .resolve_plan("paid_abandonment")
                .unwrap()
                .entry_window_secs,
            1050
        );
        assert_eq!(
            abandoned
                .entry_plan
                .iter()
                .filter(|plan| plan.behavior == EntryBehavior::AbandonUnpaid)
                .count(),
            1
        );
    }

    #[test]
    fn queued_scenarios_plan_their_own_players_and_a_longer_entry_window() {
        let config = ScenarioConfig::default();
        let split = config.resolve_plan("queued_split").unwrap();
        assert_eq!((split.users, split.entry_plan.len()), (27, 27));
        assert_eq!(split.entry_window_secs, 600);
        assert!(split
            .entry_plan
            .iter()
            .all(|plan| plan.behavior == EntryBehavior::Complete));
        assert_eq!(
            split.resolve_plan("queued_split").unwrap().entry_plan,
            split.entry_plan,
            "a recorded plan resolves to itself"
        );

        let leftover = config.resolve_plan("queued_leftover_refund").unwrap();
        assert_eq!((leftover.users, leftover.entry_window_secs), (4, 300));
        assert_eq!(
            leftover
                .entry_plan
                .iter()
                .filter(|plan| plan.behavior == EntryBehavior::AbandonPaid)
                .count(),
            1
        );

        let smaller = ScenarioConfig {
            queue_players: Some(5),
            max_pool_players: Some(3),
            ..Default::default()
        };
        assert_eq!(smaller.resolve_plan("queued_split").unwrap().users, 5);
        assert!(smaller.resolve_plan("queued_one_pool").is_err());
        assert_eq!(
            smaller.resolve_plan("full_lifecycle").unwrap().users,
            3,
            "queue overrides leave single competitions alone"
        );
    }

    fn mixed(seed: u64, floor: Option<usize>) -> ScenarioConfig {
        ScenarioConfig {
            seed: Some(seed),
            player_mix: Some(PlayerMix::default()),
            min_players: floor,
            ..Default::default()
        }
    }

    #[test]
    fn drawn_player_counts_follow_the_mix_and_the_fee_floor() {
        let counts: Vec<usize> = (0..400)
            .map(|seed| {
                mixed(seed, None)
                    .resolve_plan("full_lifecycle")
                    .unwrap()
                    .users
            })
            .collect();
        assert!(counts.iter().all(|users| (2..=10).contains(users)));
        let small = counts.iter().filter(|users| **users < 5).count();
        assert!(
            (60..=180).contains(&small),
            "about three in ten are small: {small}"
        );
        for users in 2..=10 {
            assert!(counts.contains(&users), "{users} players never drawn");
        }
        // While fees are high, no competition is drawn under five players.
        assert!((0..400).all(|seed| {
            let plan = mixed(seed, Some(5))
                .resolve_plan("duplicate_submission")
                .unwrap();
            (5..=10).contains(&plan.users) && plan.entry_plan.len() == plan.users
        }));
        // The count is part of the seed's replay.
        let plan = mixed(7, None).resolve_plan("full_lifecycle").unwrap();
        let again = plan.resolve_plan("full_lifecycle").unwrap();
        assert_eq!(
            (again.users, again.entry_plan),
            (plan.users, plan.entry_plan.clone())
        );
        assert_eq!(
            mixed(7, None).resolve_plan("full_lifecycle").unwrap().users,
            plan.users
        );
        // A fixed count stays fixed.
        assert_eq!(
            ScenarioConfig::default()
                .resolve_plan("full_lifecycle")
                .unwrap()
                .users,
            3
        );
    }

    #[test]
    fn queued_runs_draw_their_own_players() {
        for seed in 0..100 {
            let split = mixed(seed, None).resolve_plan("queued_split").unwrap();
            assert!((26..=30).contains(&split.users), "{}", split.users);
            let one = mixed(seed, Some(5))
                .resolve_plan("queued_one_pool")
                .unwrap();
            assert!((5..=10).contains(&one.users));
            let leftover = mixed(seed, None)
                .resolve_plan("queued_leftover_refund")
                .unwrap();
            assert!(
                (3..=11).contains(&leftover.users),
                "players and the abandoner"
            );
            let too_few = mixed(seed, Some(5)).resolve_plan("queued_too_few").unwrap();
            assert_eq!(too_few.users, 2, "too few is what it tests");
        }
        let smaller = ScenarioConfig {
            max_pool_players: Some(4),
            ..mixed(1, None)
        };
        assert!((2..=4).contains(&smaller.resolve_plan("queued_one_pool").unwrap().users));
        let split = mixed(3, None).resolve_plan("queued_split").unwrap();
        let again = split.resolve_plan("queued_split").unwrap();
        assert_eq!(
            (again.users, again.entry_plan),
            (split.users, split.entry_plan)
        );
    }

    fn spread_config(seed: u64) -> ScenarioConfig {
        let mut config = timed_config();
        config.seed = Some(seed);
        config.entry_window_secs = 3600;
        config.entry_timing.arrival_pattern = ArrivalPattern::Spread;
        config
    }

    #[test]
    fn spread_arrivals_cover_the_window_and_crowd_the_deadline() {
        let timing = spread_config(0).entry_timing;
        // 3600 less the payment and submission waits (180) and margin (60), less the slack.
        let latest = timing.latest_spread_arrival(3600).unwrap();
        assert_eq!(latest, 3600 - 240 - SPREAD_SLACK_SECS - 1);
        let arrivals: Vec<u64> = (0..200)
            .flat_map(|seed| {
                let mut config = spread_config(seed);
                config.users = 10;
                config.resolve_plan("full_lifecycle").unwrap().entry_plan
            })
            .map(|plan| plan.arrival_secs)
            .collect();
        assert!(arrivals.iter().all(|arrival| *arrival <= latest));
        let share = |range: std::ops::RangeInclusive<u64>| {
            arrivals
                .iter()
                .filter(|arrival| range.contains(arrival))
                .count()
                * 100
                / arrivals.len()
        };
        assert!(share(0..=latest / 10) >= 15, "some come early");
        assert!(
            share(latest * 3 / 4..=latest) >= 40,
            "a bunch come near the deadline"
        );
        assert!(
            share(latest / 4..=latest / 2) >= 8,
            "some come along the way"
        );

        // The abandoning player still comes within `arrival`, leaving time for a replacement.
        for seed in 0..50 {
            let plan = spread_config(seed)
                .resolve_plan("abandoned_unpaid")
                .unwrap();
            let abandoner = plan
                .entry_plan
                .iter()
                .find(|plan| plan.behavior == EntryBehavior::AbandonUnpaid)
                .unwrap();
            assert!(abandoner.arrival_secs <= 90);
        }
        // A window too short to spread over is refused before planning.
        let mut short = spread_config(0);
        short.entry_window_secs = 300;
        assert!(short.resolve_plan("full_lifecycle").is_err());
    }

    #[test]
    fn a_backfill_enters_only_the_players_the_competition_still_needs() {
        // Fees ask for five players, and synth keeps one more. Synth's early player and two
        // strangers have paid, so three of the five waiting players enter.
        let count = BackfillCount::new(5, 1, 3, 5);
        assert_eq!((count.needed, count.entering, count.short()), (3, 3, false));
        // Strangers already cover the minimum and the margin: nobody else enters.
        let count = BackfillCount::new(5, 1, 7, 5);
        assert_eq!((count.needed, count.entering, count.short()), (0, 0, false));
        // The run drew too few: every waiting player enters, and the pool may still be short.
        let count = BackfillCount::new(5, 1, 1, 2);
        assert_eq!((count.needed, count.entering, count.short()), (5, 2, true));
    }

    #[test]
    fn a_backfilled_plan_spreads_its_early_players_before_the_backfill() {
        let mut config = spread_config(3);
        config.backfill = Some(Backfill {
            early_players: 1,
            before_close_secs: 1800,
            margin: 1,
        });
        let plan = config.resolve_plan("queued_one_pool").unwrap();
        assert_eq!(plan.users, 5);
        let latest = config.entry_timing.latest_spread_arrival(1800).unwrap();
        assert!(plan.entry_plan[0].arrival_secs <= latest);
        assert!(plan.entry_plan[1..]
            .iter()
            .all(|player| player.arrival_secs == 1800));
        assert_eq!(
            plan.resolve_plan("queued_one_pool").unwrap().entry_plan,
            plan.entry_plan,
            "a recorded plan resolves to itself"
        );
        // A single competition that is not full when entries close is cancelled.
        assert!(config.resolve_plan("full_lifecycle").is_err());
        // The late players need time to pay and submit before entries close.
        config.backfill = Some(Backfill {
            early_players: 1,
            before_close_secs: 300,
            margin: 1,
        });
        assert!(config.resolve_plan("queued_one_pool").is_err());
    }

    #[test]
    fn invalid_ranges_and_invoice_deadline_overruns_fail_before_planning() {
        let mut config = ScenarioConfig::default();
        config.entry_timing.arrival = DelayRange {
            min_secs: 10,
            max_secs: 9,
        };
        assert!(config.resolve_plan("full_lifecycle").is_err());
        config.entry_timing.arrival = DelayRange {
            min_secs: 0,
            max_secs: 60,
        };
        assert!(
            config.resolve_plan("full_lifecycle").is_err(),
            "invoice cutoff is observation start minus 60 seconds"
        );
        config.entry_timing.arrival.max_secs = 10;
        config.entry_timing.before_submit.max_secs = 110;
        assert!(config.resolve_plan("full_lifecycle").is_err());
        config.entry_timing.before_submit.max_secs = u64::MAX;
        assert!(config.resolve_plan("abandoned_unpaid").is_err());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub scenario: String,
    pub status: ScenarioStatus,
    pub steps: Vec<StepResult>,
    pub total_duration_ms: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed_at: Option<OffsetDateTime>,
    pub error: Option<String>,
}

impl ScenarioResult {
    /// The coordinator would not create a competition this small at the network fees then, so
    /// the run stopped before anyone entered.
    pub fn refused_as_small(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.name == REFUSED_AS_SMALL_STEP && step.status == StepStatus::Skipped)
    }

    /// Why the coordinator did not create the run's competition, if it failed to: a lane tries
    /// such a run again later.
    pub fn creation_error(&self) -> Option<String> {
        self.steps
            .iter()
            .find(|step| step.name == REFUSED_AS_SMALL_STEP && step.status == StepStatus::Failed)
            .map(|step| step.error.clone().unwrap_or_default())
    }
}

/// The step a run records when the coordinator refuses its competition as too small for the
/// network fees.
pub const REFUSED_AS_SMALL_STEP: &str = "create_competition";

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStatus {
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub name: String,
    pub status: StepStatus,
    pub duration_ms: i64,
    pub details: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Passed,
    Failed,
    Skipped,
}
