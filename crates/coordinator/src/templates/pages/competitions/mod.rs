//! The home page: what the game is, the competition to enter now, and every
//! competition grouped by where it stands.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::{
    get_percentage_weights,
    leaderboard::{Metric, Phase},
    winner_payout_sats, Competition, RefundProgress, WindowShape,
};
use crate::infra::oracle::ScoringRules;
use crate::templates::format::{self, sats, TimeStyle};

mod queue;
pub use queue::{PoolLink, PoolOf, Queue, QueueView};

/// Finished competitions shown per page.
pub const PAGE_SIZE: usize = 10;

/// View data for a competition
#[derive(Debug, Clone)]
pub struct CompetitionView {
    pub id: String,
    pub phase: Phase,
    pub start: OffsetDateTime,
    pub end: OffsetDateTime,
    /// When the oracle is due to sign the result.
    pub signing: OffsetDateTime,
    /// When the contract expires without a signed result and shares the pot back; `None` for
    /// a competition read without its oracle event, as the lists read them.
    pub expiry: Option<OffsetDateTime>,
    pub entry_fee: u64,
    /// What the entrant pays before the network fee: the entry fee plus the coordinator fee.
    pub ticket_price: u64,
    /// The network fee a ticket issued now would add, for a competition taking entries; `None`
    /// until the page adds it, or while there is no estimate.
    pub network_fee: Option<u64>,
    /// The coordinator fee as a percentage of the entry fee: `5%`, `2.5%`.
    pub service_fee_percent: String,
    pub total_pool: u64,
    pub total_entries: u64,
    pub total_allowed_entries: u64,
    pub paid_places: u64,
    pub can_enter: bool,
    pub number_of_values_per_entry: usize,
    /// How many entries one player may make.
    pub max_entries_per_player: u32,
    pub locations: Vec<String>,
    /// How the oracle scores the picks.
    pub scoring_rules: ScoringRules,
    /// The metrics each station offers picks on, from the window's shape.
    pub metrics: Vec<Metric>,
    /// Full day, day or night; `None` for windows from before the oracle attested only these.
    pub window_shape: Option<WindowShape>,
    /// Its Arkade escrows and how many have been refunded; none until the page adds them.
    pub refunds: RefundProgress,
    /// The oracle attested the contract's all-entry, no-score outcome.
    /// This identifies the allocation; it does not confirm that payments were sent.
    pub pot_refunded: bool,
    /// Pot-return allocation in contract player order, from the attested or expiry outcome.
    /// Older contracts can have unequal shares. None means the amounts cannot be verified.
    pub refund_shares: Option<Vec<u64>>,
    /// A queue split into pools at the start, one of its pools, or a single competition.
    pub queue: Queue,
    /// Reached by its link only: the public lists leave it out.
    pub unlisted: bool,
}

/// Where the entry fees of a competition that didn't run stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refunds {
    /// No entry fee was paid, so nothing is owed.
    Nothing,
    /// Escrowed fees can't be returned before their escrows' refund locktime.
    Locked(OffsetDateTime),
    Pending,
    Done,
    /// Refunds are over, but some fees weren't returned: an operator wrote off their escrows'
    /// refunds, or their Lightning payments were settled.
    Partly,
}

impl CompetitionView {
    /// The view of `competition`. One read without its contract, as the lists read them,
    /// says nothing about a returned pot until [`Self::add_contract`] is given the whole
    /// competition.
    pub fn new(competition: &Competition, now: OffsetDateTime) -> Self {
        let event = &competition.event_submission;
        let phase = Phase::of(competition, now);
        let queue = Queue::of(competition);
        let can_enter = phase == Phase::Upcoming
            && match &queue {
                Queue::Single => competition.total_entries < event.total_allowed_entries as u64,
                Queue::Queued(queue) => {
                    queue.pools.is_empty()
                        && queue.max_entries.is_none_or(|max| {
                            queue.entries.unwrap_or(competition.total_entries) < max
                        })
                }
                // A pool's players come from its competition's queue.
                Queue::Pool(_) => false,
            };
        let mut view = Self {
            id: competition.id.to_string(),
            phase,
            start: event.start_observation_date,
            end: event.end_observation_date,
            signing: event.signing_date,
            expiry: competition
                .event_announcement
                .as_ref()
                .and_then(|announcement| announcement.expiry)
                // Below this, a locktime is a block height, not a time.
                .filter(|expiry| *expiry >= 500_000_000)
                .and_then(|expiry| OffsetDateTime::from_unix_timestamp(i64::from(expiry)).ok()),
            entry_fee: event.entry_fee as u64,
            ticket_price: competition.calculate_invoice_amount(),
            network_fee: None,
            service_fee_percent: event.coordinator_fee.to_string(),
            total_pool: event.total_competition_pool as u64,
            total_entries: competition.total_entries,
            total_allowed_entries: event.total_allowed_entries as u64,
            paid_places: event.number_of_places_win as u64,
            can_enter,
            number_of_values_per_entry: event.number_of_values_per_entry,
            max_entries_per_player: event.max_entries_per_player,
            locations: event.locations.clone(),
            scoring_rules: event.scoring_rules(),
            metrics: event.metrics(),
            window_shape: event
                .scoring_fields
                .as_ref()
                .and_then(|_| event.window_shape()),
            refunds: RefundProgress::default(),
            pot_refunded: false,
            refund_shares: None,
            queue,
            unlisted: !competition.is_listed(),
        };
        view.add_contract(competition);
        view
    }

    /// Whether the pot went back to every entry, and in what shares, from the competition's
    /// contract and outcome.
    pub fn add_contract(&mut self, competition: &Competition) {
        self.pot_refunded = self.phase == Phase::Scored && competition.refunds_every_entry();
        self.refund_shares = (self.pot_refunded || self.phase == Phase::Expired)
            .then(|| refund_shares(competition, self.phase))
            .flatten();
    }

    /// `3 of 25`, or `40 entered` for a queue, which has no seat count. A queue that plays as
    /// one pool has seats: `20 seats · 17 left` while it takes entries, `3 of 20` after. It
    /// counts the entries the page lists; a fee paid for an entry that never arrived is in the
    /// refund note.
    pub fn entries(&self) -> String {
        match &self.queue {
            Queue::Queued(queue) => match queue.seats() {
                Some(seats) if self.can_enter => format!(
                    "{seats} seats · {} left",
                    seats.saturating_sub(self.entry_count(queue))
                ),
                Some(seats) => format!("{} of {seats}", self.entry_count(queue)),
                None => format!("{} entered", self.entry_count(queue)),
            },
            _ => format!("{} of {}", self.total_entries, self.total_allowed_entries),
        }
    }

    /// Whether anyone took part. For one that didn't run, every paid entry fee counts, as its
    /// refunds count them, even one whose entry never arrived.
    pub fn entered(&self) -> u64 {
        match self.phase {
            Phase::Unfilled | Phase::Cancelled | Phase::Failed => {
                self.total_entries.max(self.refunds.paid())
            }
            _ => self.total_entries,
        }
    }

    /// A queue's paid entries, those already moved to its pools included.
    fn entry_count(&self, queue: &QueueView) -> u64 {
        queue.entries.unwrap_or(self.total_entries)
    }

    /// Whether the result is late: the window has closed, and the oracle's signing time has
    /// passed without a result.
    pub fn result_is_late(&self, now: OffsetDateTime) -> bool {
        self.phase == Phase::AwaitingResult && now >= self.signing
    }

    /// The smallest and the largest pot among a queue's pools, and how many pools there are.
    fn pool_pots(&self, queue: &QueueView) -> (u64, u64, u64) {
        let (smallest, largest, pools) = pool_sizes(queue, self.entry_count(queue));
        (self.pot_of(smallest), self.pot_of(largest), pools)
    }

    /// What a pool of `players` puts in its pot.
    fn pot_of(&self, players: u64) -> u64 {
        self.entry_fee.saturating_mul(players)
    }

    /// The places a pool of `players` pays: a queue's pools below ten players pay one.
    fn places_for(&self, players: u64) -> u64 {
        match &self.queue {
            Queue::Queued(_) => u64::from(coordinator_escrow::queued::pool_places(
                self.paid_places as u32,
                players as usize,
            )),
            _ => self.paid_places,
        }
    }

    /// What first place takes from a queue's pool of `players`.
    fn first_prize(&self, players: u64) -> u64 {
        let share = get_percentage_weights(self.places_for(players) as usize)[0];
        self.pot_of(players) * share / 100
    }

    /// The pot. A queue's is what its entries put in each pool, which its winner takes: a
    /// range when its pools are not all the same size.
    pub fn pot(&self) -> String {
        match &self.queue {
            Queue::Queued(queue) => {
                let (smallest, largest, pools) = self.pool_pots(queue);
                let pot = sats_range(smallest, largest);
                if pools > 1 {
                    format!("{pot} per pool")
                } else {
                    pot
                }
            }
            _ => sats(self.total_pool),
        }
    }

    /// What entering costs now, all in: the ticket price and, when known, the network fee. Players
    /// see it as the "Entry fee"; `entry_fee` itself is only the pot's share of it.
    pub fn price(&self) -> u64 {
        self.ticket_price + self.network_fee.unwrap_or(0)
    }

    /// What first place wins: the top prize, or for a queue the pot a pool pays its winner with
    /// the entries so far, and never less than a smallest pool's. `None` when nothing is won
    /// by rank (the competition didn't run, or returned its pot).
    pub fn top_prize(&self) -> Option<u64> {
        self.prize_range().map(|(smallest, _)| smallest)
    }

    /// [`Self::top_prize`], with the largest pool's pot too for a queue whose pools differ.
    fn prize_range(&self) -> Option<(u64, u64)> {
        if !self.has_ranked_prizes() {
            return None;
        }
        match &self.queue {
            Queue::Queued(queue) => {
                let (smallest, largest, _) = pool_sizes(queue, self.entry_count(queue));
                let least = queue.min_players.unwrap_or(2);
                // A pool of ten that pays two places gives first place less than one of nine.
                let (a, b) = (
                    self.first_prize(smallest.max(least)),
                    self.first_prize(largest.max(least)),
                );
                Some((a.min(b), a.max(b)))
            }
            _ => self.prizes().first().map(|(_, amount)| (*amount, *amount)),
        }
    }

    /// How the paid places share the pot, `1st 70% · 2nd 30%`, when more than one is paid.
    pub fn prize_split(&self) -> Option<String> {
        let largest = match &self.queue {
            Queue::Queued(queue) => queue.max_players,
            _ => self.total_allowed_entries,
        };
        let places = self.places_for(largest);
        (places > 1 && self.has_ranked_prizes()).then(|| {
            get_percentage_weights(places as usize)
                .into_iter()
                .enumerate()
                .map(|(place, percent)| format!("{} {percent}%", format::ordinal(place + 1)))
                .collect::<Vec<_>>()
                .join(" · ")
        })
    }

    /// For a queue whose larger pools pay more than one place, what its smaller ones pay.
    pub fn prize_rule(&self) -> Option<String> {
        let queue = self.queue.queued()?;
        (queue.pools.is_empty() && self.prize_split().is_some()).then(|| {
            format!(
                "Under {} players: winner takes all",
                coordinator_escrow::queued::MULTI_PLACE_MIN_PLAYERS
            )
        })
    }

    /// What first place wins, as the pages show it: a range for a queue whose pools differ.
    pub fn win(&self) -> String {
        self.prize_range().map_or_else(
            || "—".to_owned(),
            |(smallest, largest)| sats_range(smallest, largest),
        )
    }

    /// Each paid place's share in percent and in sats, first place first.
    pub fn prizes(&self) -> Vec<(u64, u64)> {
        get_percentage_weights(self.paid_places as usize)
            .into_iter()
            .map(|percent| (percent, self.total_pool * percent / 100))
            .collect()
    }

    /// Ranked prizes apply to active competitions and scored winner outcomes.
    pub fn has_ranked_prizes(&self) -> bool {
        !self.pot_refunded
            && !matches!(
                self.phase,
                Phase::Unfilled | Phase::Cancelled | Phase::Failed | Phase::Expired
            )
    }

    /// A competition whose window started before it filled: cancelled, or
    /// about to be, with every entry fee returned. For a queue: too few entries for a pool.
    pub fn did_not_fill(&self) -> bool {
        match &self.queue {
            Queue::Queued(queue) if !queue.pools.is_empty() => false,
            Queue::Queued(queue) => {
                self.phase == Phase::Unfilled
                    || (self.phase == Phase::Cancelled
                        && queue
                            .min_players
                            .is_none_or(|min| self.entry_count(queue) < min))
            }
            _ => {
                self.phase == Phase::Unfilled
                    || (self.phase == Phase::Cancelled
                        && self.total_entries < self.total_allowed_entries)
            }
        }
    }

    /// Whether entry fees are owed back: it didn't fill, or it was cancelled for any reason,
    /// such as a failed kickoff check, or it failed, with fees paid.
    pub fn owes_refunds(&self) -> bool {
        self.did_not_fill()
            || (matches!(self.phase, Phase::Cancelled | Phase::Failed) && self.refunds.paid() > 0)
    }

    /// Where the entry fees of a competition that didn't fill stand at `now`. Escrowed fees
    /// are returned once their escrows' refund locktime passes, shortly after the start;
    /// held Lightning payments are released when the competition is cancelled.
    pub fn refunds(&self, now: OffsetDateTime) -> Refunds {
        let RefundProgress {
            escrowed,
            refunded,
            written_off,
            held,
            released,
            settled,
            opens_at,
        } = self.refunds;
        // A written-off escrow is no longer owed: refunds are over once the rest are.
        if escrowed > 0 || written_off > 0 {
            match opens_at {
                _ if refunded >= escrowed && written_off > 0 => Refunds::Partly,
                _ if refunded >= escrowed => Refunds::Done,
                Some(at) if at > now && refunded == 0 => Refunds::Locked(at),
                _ => Refunds::Pending,
            }
        } else if self.paid_nothing() {
            Refunds::Nothing
        } else if held > 0 && released >= held {
            Refunds::Done
        } else if held > 0 && released + settled >= held {
            // A settled payment can't be released: the rest were, and that is all.
            Refunds::Partly
        } else if held > 0 || self.phase == Phase::Unfilled {
            Refunds::Pending
        } else {
            Refunds::Done
        }
    }

    /// Nobody paid an entry fee: no escrow was funded and there are no entries.
    fn paid_nothing(&self) -> bool {
        self.refunds.paid() == 0 && self.total_entries == 0
    }

    pub fn url(&self) -> String {
        if self.can_enter {
            format!("/competitions/{}/entry-form", self.id)
        } else {
            format!("/competitions/{}/leaderboard", self.id)
        }
    }
}

/// The fewest and the most players among a queue's pools, and how many pools there are: the
/// pools it formed, or the ones its `entries` so far would make. Pools split the entries evenly,
/// so they differ by one player when the entries don't divide exactly.
fn pool_sizes(queue: &QueueView, entries: u64) -> (u64, u64, u64) {
    let sizes: Vec<u64> = queue.pools.iter().filter_map(|pool| pool.size).collect();
    if !sizes.is_empty() && sizes.len() == queue.pools.len() {
        let (min, max) = sizes.iter().fold((u64::MAX, 0), |(min, max), size| {
            (min.min(*size), max.max(*size))
        });
        return (min, max, sizes.len() as u64);
    }
    let pools = entries.div_ceil(queue.max_players.max(1)).max(1);
    (entries / pools, entries.div_ceil(pools), pools)
}

/// `65,000–70,000 sats`, or one amount when both ends are the same.
fn sats_range(smallest: u64, largest: u64) -> String {
    if smallest == largest {
        sats(smallest)
    } else {
        format!("{}–{}", format::thousands(smallest), sats(largest))
    }
}

fn refund_shares(competition: &Competition, phase: Phase) -> Option<Vec<u64>> {
    let params = competition.contract_parameters.as_ref()?;
    let outcome = if phase == Phase::Expired {
        dlctix::Outcome::Expiry
    } else {
        competition.get_current_outcome().ok()?
    };
    if params.players.is_empty() {
        return None;
    }
    params
        .players
        .iter()
        .map(|player| winner_payout_sats(params, &outcome, &player.pubkey).ok())
        .collect()
}

/// The badge for a competition's phase. Live is the loudest thing on the page;
/// finished competitions stay quiet, and say only whether they ran: how the pot or the
/// entry fees went back is in the title, the refund line and the leaderboard. A queue
/// split into pools says so: its pools carry on as competitions of their own.
pub fn phase_badge(competition: &CompetitionView) -> Markup {
    let split = competition
        .queue
        .queued()
        .is_some_and(|queue| !queue.pools.is_empty());
    badge(competition, split)
}

/// The badge in the list, where a split queue stands for its pools and shows
/// their phase (see [`listed`]).
fn list_badge(competition: &CompetitionView) -> Markup {
    badge(competition, false)
}

fn badge(competition: &CompetitionView, split: bool) -> Markup {
    let queue = competition.queue.queued();
    let (class, label) = match competition.phase {
        _ if split => ("badge badge-quiet", "Split into pools"),
        Phase::Upcoming if !competition.can_enter && queue.is_some() => {
            ("badge badge-open", "Entries closed")
        }
        Phase::Upcoming if !competition.can_enter => ("badge badge-open", "Full"),
        Phase::Upcoming => ("badge badge-open", "Open"),
        Phase::Live => ("badge badge-live", "Live"),
        Phase::AwaitingResult => ("badge badge-waiting", "Awaiting results"),
        Phase::Scored | Phase::Expired => ("badge badge-quiet", "Finished"),
        // Cancelled by the operator, not for want of entries.
        Phase::Cancelled if !competition.did_not_fill() => ("badge badge-quiet", "Cancelled"),
        Phase::Unfilled | Phase::Cancelled | Phase::Failed => ("badge badge-quiet", "Didn't run"),
    };
    let title = match competition.phase {
        _ if split => None,
        _ if competition.did_not_fill() => {
            Some(match (queue.is_some(), competition.paid_nothing()) {
                (true, true) => "Too few players entered to make a pool; no entry fees were paid",
                (true, false) => {
                    "Too few players entered to make a pool; every entry fee is returned"
                }
                (false, true) => "Not enough entries by the start; no entry fees were paid",
                (false, false) => "Not enough entries by the start; every entry fee is returned",
            })
        }
        Phase::Failed => Some("It stopped before it ran, so nothing is scored"),
        Phase::Scored if competition.pot_refunded => {
            Some("No entry scored, so the pot is shared back among the entries")
        }
        Phase::Expired => Some(
            "The oracle never signed a result in time, so the pot is shared back among the entries",
        ),
        _ => None,
    };
    html! { span class=(class) title=[title] { (label) } }
}

/// Under a competition that didn't run: where its entry fees stand. Escrow refunds open at
/// their locktime, so until then it says when; nothing when no fee was paid.
pub fn refund_line(competition: &CompetitionView, now: OffsetDateTime) -> Option<Markup> {
    if !competition.owes_refunds() {
        return None;
    }
    Some(match competition.refunds(now) {
        Refunds::Nothing => return None,
        Refunds::Locked(at) => html! { "Refunds open " (format::time(at, TimeStyle::DateTime)) },
        Refunds::Pending => html! { "Refunding…" },
        Refunds::Done => html! { "Refunded" },
        Refunds::Partly => html! { "Not all refunded" },
    })
}

/// What the list shows: which page of finished competitions, and whether
/// cancelled ones are included.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOptions {
    pub page: usize,
    pub show_cancelled: bool,
}

impl ListOptions {
    pub fn url_for(page: usize, show_cancelled: bool) -> String {
        let mut parts = Vec::new();
        if page > 0 {
            parts.push(format!("page={page}"));
        }
        if show_cancelled {
            parts.push("cancelled=1".to_owned());
        }
        if parts.is_empty() {
            "/competitions".to_owned()
        } else {
            format!("/competitions?{}", parts.join("&"))
        }
    }

    pub fn url(self) -> String {
        Self::url_for(self.page, self.show_cancelled)
    }
}

/// A link that replaces the list in place and records the new address.
fn list_link(url: String, label: Markup) -> Markup {
    html! {
        a href=(url) hx-get=(url) hx-target="#competitions-page" hx-select="#competitions-page"
          hx-swap="outerHTML" hx-push-url="true" { (label) }
    }
}

/// What the list shows for `options`: every competition taking entries, live or awaiting
/// results, one page of finished ones, and the one to feature.
struct Sections<'a> {
    open: Vec<&'a CompetitionView>,
    live: Vec<&'a CompetitionView>,
    waiting: Vec<&'a CompetitionView>,
    /// This page of the finished ones, newest first.
    finished: Vec<&'a CompetitionView>,
    page: usize,
    pages: usize,
    /// How many cancelled competitions there are, shown or not.
    cancelled: usize,
    featured: Option<&'a CompetitionView>,
}

impl<'a> Sections<'a> {
    /// `competitions` as [`listed`] gives them.
    fn of(competitions: &'a [CompetitionView], options: ListOptions) -> Self {
        let mut live = by_phase(competitions, &[Phase::Live]);
        let mut open = by_phase(competitions, &[Phase::Upcoming]);
        let mut waiting = by_phase(competitions, &[Phase::AwaitingResult]);
        let finished_phases: &[Phase] = if options.show_cancelled {
            &[
                Phase::Unfilled,
                Phase::Scored,
                Phase::Expired,
                Phase::Failed,
                Phase::Cancelled,
            ]
        } else {
            &[
                Phase::Unfilled,
                Phase::Scored,
                Phase::Expired,
                Phase::Failed,
            ]
        };
        let mut finished = by_phase(competitions, finished_phases);
        let cancelled = by_phase(competitions, &[Phase::Cancelled]).len();
        live.sort_by_key(|competition| std::cmp::Reverse(competition.start));
        open.sort_by_key(|competition| std::cmp::Reverse(competition.start));
        waiting.sort_by_key(|competition| std::cmp::Reverse(competition.end));
        finished.sort_by_key(|competition| std::cmp::Reverse(competition.end));

        let pages = finished.len().div_ceil(PAGE_SIZE).max(1);
        let page = options.page.min(pages - 1);
        let finished = finished
            .into_iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .collect();
        let featured = open
            .iter()
            .filter(|competition| competition.can_enter)
            .min_by_key(|competition| competition.start)
            .or_else(|| live.first())
            .copied();
        Self {
            open,
            live,
            waiting,
            finished,
            page,
            pages,
            cancelled,
            featured,
        }
    }

    fn shown(&self) -> impl Iterator<Item = &'a CompetitionView> + '_ {
        self.open
            .iter()
            .chain(&self.live)
            .chain(&self.waiting)
            .chain(&self.finished)
            .copied()
    }
}

/// The ids of the competitions the list shows for `options`: the ones a page needs refunds and
/// contracts for. A queue split into pools is shown by its own id.
pub fn shown_ids(competitions: &[CompetitionView], options: ListOptions) -> Vec<String> {
    let competitions = listed(competitions);
    Sections::of(&competitions, options)
        .shown()
        .map(|competition| competition.id.clone())
        .collect()
}

/// Competitions page content: intro, the one to enter now, then every group.
pub fn competitions_page(
    competitions: &[CompetitionView],
    options: ListOptions,
    now: OffsetDateTime,
) -> Markup {
    let competitions = &listed(competitions);
    let Sections {
        open,
        live,
        waiting,
        finished,
        page,
        pages,
        cancelled,
        featured,
    } = Sections::of(competitions, options);
    let show_cancelled = options.show_cancelled;

    html! {
        div id="competitions-page"
            hx-get=(options.url())
            hx-trigger="every 30s"
            hx-select="#competitions-page"
            hx-swap="outerHTML" {
            (intro(featured, now))

            @if !open.is_empty() {
                (group("Upcoming", &open, now))
            }
            @if !live.is_empty() {
                (group("Live", &live, now))
            }
            @if !waiting.is_empty() {
                (group("Awaiting results", &waiting, now))
            }
            section class="competition-group" {
                div class="group-heading" {
                    h2 class="title is-5" { "Finished" }
                    @if cancelled > 0 {
                        span class="cancelled-toggle" {
                            (list_link(
                                ListOptions::url_for(0, !show_cancelled),
                                html! { (if show_cancelled { "Hide" } else { "Show" }) " cancelled (" (cancelled) ")" },
                            ))
                        }
                    }
                }
                @if finished.is_empty() {
                    p class="empty-state" { "No finished competitions yet." }
                } @else {
                    (list(&finished, now))
                }
                @if pages > 1 {
                    nav class="pager" aria-label="Finished competitions pages" {
                        @if page > 0 {
                            (list_link(ListOptions::url_for(page - 1, show_cancelled), html! { "← Newer" }))
                        }
                        span { "Page " (page + 1) " of " (pages) }
                        @if page + 1 < pages {
                            (list_link(ListOptions::url_for(page + 1, show_cancelled), html! { "Older →" }))
                        }
                    }
                }
            }
        }
    }
}

/// The competitions the list shows. An unlisted competition is left out: it is reached by its
/// link. A queued competition stands for its pools, which its page links: a pool whose
/// competition is listed isn't, and a queue split into pools is listed where its least advanced
/// pool is, since it has no lifecycle of its own after the split.
fn listed(competitions: &[CompetitionView]) -> Vec<CompetitionView> {
    fn pools_of<'a>(
        competitions: &'a [CompetitionView],
        parent: &'a str,
    ) -> impl Iterator<Item = &'a CompetitionView> {
        competitions.iter().filter(move |competition| {
            matches!(&competition.queue, Queue::Pool(pool) if pool.parent_id == parent)
        })
    }
    let listed_parent = |competition: &CompetitionView| match &competition.queue {
        Queue::Pool(pool) => competitions
            .iter()
            .any(|parent| parent.id == pool.parent_id),
        _ => false,
    };
    competitions
        .iter()
        .filter(|competition| !competition.unlisted && !listed_parent(competition))
        .map(|competition| {
            let mut shown = competition.clone();
            if competition
                .queue
                .queued()
                .is_some_and(|queue| !queue.pools.is_empty())
            {
                if let Some(phase) = pools_of(competitions, &competition.id)
                    .map(|pool| pool.phase)
                    .min_by_key(|phase| progress(*phase))
                {
                    shown.phase = phase;
                }
            }
            shown
        })
        .collect()
}

/// How far along a competition in `phase` is, for listing a queue with its least advanced pool.
fn progress(phase: Phase) -> u8 {
    match phase {
        Phase::Upcoming => 0,
        Phase::Live => 1,
        Phase::AwaitingResult => 2,
        Phase::Scored => 3,
        Phase::Expired => 4,
        Phase::Unfilled => 5,
        Phase::Failed => 6,
        Phase::Cancelled => 7,
    }
}

fn by_phase<'a>(competitions: &'a [CompetitionView], phases: &[Phase]) -> Vec<&'a CompetitionView> {
    competitions
        .iter()
        .filter(|competition| phases.contains(&competition.phase))
        .collect()
}

/// How to play, and the competition to act on now.
fn intro(featured: Option<&CompetitionView>, now: OffsetDateTime) -> Markup {
    html! {
        section class="intro" {
            div class="intro-text" {
                h1 class="title is-4" { "Daily Fantasy Weather" }
                p {
                    span class="intro-line" { "Choose a city, score points on correctly forecasted weather readings." }
                    " "
                    span class="intro-line" { "Pay and win with sats, all via the Lightning Network." }
                }
                p class="intro-link" {
                    a href="/help" hx-get="/help" hx-target="#main-content" hx-push-url="true" { "How it works →" }
                }
            }
            @if let Some(competition) = featured {
                (featured_card(competition, now))
            }
        }
    }
}

fn featured_card(competition: &CompetitionView, now: OffsetDateTime) -> Markup {
    html! {
        div class="featured-card" {
            div class="featured-status" {
                (list_badge(competition))
                span class="countdown" {
                    @if competition.phase == Phase::Live {
                        "Observations end in " (format::duration(competition.end - now))
                    } @else {
                        "Entries close in " (format::duration(competition.start - now))
                    }
                }
            }
            p class="featured-window" {
                (format::zoned_time(competition.start, format::TimeStyle::Weekday))
                " · " (format::competition_duration(competition.start, competition.end))
            }
            dl class="featured-facts" {
                @if competition.can_enter {
                    div { dt { "Entry fee" } dd { (sats(competition.price())) } }
                }
                div {
                    dt { "Prizes" }
                    dd {
                        (competition.win())
                        @if let Some(split) = competition.prize_split() { span class="cell-note prize-note" { (split) } }
                        @if let Some(rule) = competition.prize_rule() { span class="cell-note prize-note" { (rule) } }
                    }
                }
                div { dt { "Entries" } dd { (competition.entries()) } }
            }
            a class=(if competition.can_enter { "button is-primary is-fullwidth" } else { "button is-fullwidth" })
              href=(competition.url()) hx-get=(competition.url())
              hx-target="#main-content" hx-push-url="true" {
                @if competition.can_enter { "Enter" } @else { "Watch the leaderboard" }
            }
        }
    }
}

fn group(title: &str, competitions: &[&CompetitionView], now: OffsetDateTime) -> Markup {
    html! {
        section class="competition-group" {
            div class="group-heading" {
                h2 class="title is-5" { (title) }
            }
            (list(competitions, now))
        }
    }
}

fn list(competitions: &[&CompetitionView], now: OffsetDateTime) -> Markup {
    html! {
        div class="competition-list" {
            div class="competition-header" aria-hidden="true" {
                span { "Status" }
                span { "Starts" }
                span { "Duration" }
                span { "Entry fee" }
                span { "Prizes" }
                span { "Entries" }
                span {}
            }
            @for competition in competitions {
                (competition_row(competition, now))
            }
        }
    }
}

/// One competition; the whole row links to its entry form or leaderboard.
pub fn competition_row(competition: &CompetitionView, now: OffsetDateTime) -> Markup {
    let action = if competition.can_enter {
        "Enter"
    } else if competition.phase == Phase::Cancelled {
        "Details"
    } else {
        "Leaderboard"
    };
    // Phones show these facts below the window. Only a competition taking entries has an
    // entry fee, the all-in price; one that didn't run wins nothing.
    let mut facts = Vec::new();
    if competition.can_enter {
        facts.push(format!("Entry fee {}", sats(competition.price())));
    }
    if competition.top_prize().is_some() {
        facts.push(match competition.prize_split() {
            Some(split) => format!("Prizes {} ({split})", competition.win()),
            None => format!("Prizes {}", competition.win()),
        });
    }
    facts.push(format::competition_duration(
        competition.start,
        competition.end,
    ));
    facts.push(match competition.queue {
        Queue::Queued(_) => competition.entries(),
        _ => format!("{} entries", competition.entries()),
    });
    let facts = facts.join(" · ");
    html! {
        a class="competition-row" data-competition-id=(competition.id) data-facts=(facts)
          href=(competition.url()) hx-get=(competition.url())
          hx-target="#main-content" hx-push-url="true" {
            span class="cell-status" { (list_badge(competition)) }
            span class="cell-window" {
                (format::zoned_time(competition.start, format::TimeStyle::Weekday))
                @match competition.phase {
                    Phase::Upcoming => { span class="cell-note" { "starts in " (format::duration(competition.start - now)) } }
                    Phase::Live => { span class="cell-note" { "observations end in " (format::duration(competition.end - now)) } }
                    Phase::AwaitingResult if competition.result_is_late(now) => { span class="cell-note" { "results are late" } }
                    Phase::Expired => { span class="cell-note" { "no result · pot shared back" } }
                    Phase::Scored if competition.pot_refunded => { span class="cell-note" { "no winner · pot shared back" } }
                    _ => {}
                }
                @if let Some(refunds) = refund_line(competition, now) {
                    span class="cell-note refund-line" { (refunds) }
                }
                @match &competition.queue {
                    Queue::Queued(queue) if queue.pools.is_empty() && queue.seats().is_none() => {
                        span class="cell-note" { "pools of up to " (queue.max_players) }
                    }
                    Queue::Queued(queue) if queue.pools.is_empty() => {}
                    Queue::Queued(queue) if queue.pools.len() == 1 => { span class="cell-note" { "1 pool" } }
                    Queue::Queued(queue) => { span class="cell-note" { (queue.pools.len()) " pools" } }
                    Queue::Pool(pool) => { span class="cell-note" { (pool.label()) } }
                    Queue::Single => {}
                }
            }
            span class="cell-duration" data-label="Duration" {
                (format::competition_duration(competition.start, competition.end))
            }
            span class="cell-fee" data-label="Entry fee" {
                @if competition.can_enter { (sats(competition.price())) } @else { "—" }
            }
            span class="cell-win" data-label="Prizes" {
                (competition.win())
                @if let Some(split) = competition.prize_split() { span class="cell-note prize-note" { (split) } }
                @if let Some(rule) = competition.prize_rule() { span class="cell-note prize-note" { (rule) } }
            }
            span class="cell-entries" data-label="Entries" { (competition.entries()) }
            span class="cell-action" { (action) " →" }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use time::macros::datetime;

    pub(crate) const NOW: OffsetDateTime = datetime!(2026-09-24 12:00 UTC);

    pub(crate) fn view(id: &str, phase: Phase, start_offset_minutes: i64) -> CompetitionView {
        let start = NOW + time::Duration::minutes(start_offset_minutes);
        CompetitionView {
            id: id.to_owned(),
            phase,
            start,
            end: start + time::Duration::minutes(10),
            signing: start + time::Duration::minutes(15),
            expiry: None,
            entry_fee: 5000,
            ticket_price: 5250,
            network_fee: None,
            service_fee_percent: "5%".into(),
            total_pool: 15000,
            total_entries: 1,
            total_allowed_entries: 3,
            paid_places: 1,
            can_enter: phase == Phase::Upcoming,
            number_of_values_per_entry: 9,
            max_entries_per_player: 1,
            locations: vec!["KPWM".into()],
            scoring_rules: ScoringRules::Fixed,
            metrics: Metric::ALL.to_vec(),
            window_shape: None,
            refunds: RefundProgress::default(),
            pot_refunded: false,
            refund_shares: None,
            queue: Queue::Single,
            unlisted: false,
        }
    }

    pub(crate) const POOL: &str = "01a0c226-0000-7000-8000-000000000001";

    /// A queue taking entries, with `entries` so far.
    pub(crate) fn queued(id: &str, entries: u64) -> CompetitionView {
        let mut queue = view(id, Phase::Upcoming, 60);
        queue.total_entries = entries;
        queue.queue = Queue::Queued(QueueView {
            min_players: Some(2),
            max_players: 25,
            entries: Some(entries),
            max_entries: None,
            pools: vec![],
        });
        queue
    }

    fn position(html: &str, needle: &str) -> usize {
        html.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    }

    #[test]
    fn groups_come_in_order_with_the_newest_finished_first() {
        // Upcoming first: entering is the thing to do on this page.
        let competitions = vec![
            view("finished-old", Phase::Scored, -300),
            view("open", Phase::Upcoming, 60),
            view("finished-new", Phase::Scored, -100),
            view("waiting", Phase::AwaitingResult, -30),
            view("live", Phase::Live, -5),
        ];
        let html = competitions_page(&competitions, ListOptions::default(), NOW).into_string();
        let live = position(&html, r#"data-competition-id="live""#);
        let open = position(&html, r#"data-competition-id="open""#);
        let waiting = position(&html, r#"data-competition-id="waiting""#);
        let newer = position(&html, r#"data-competition-id="finished-new""#);
        let older = position(&html, r#"data-competition-id="finished-old""#);
        assert!(open < live && live < waiting && waiting < newer && newer < older);
    }

    #[test]
    fn the_open_competition_is_featured_with_a_countdown_and_enter() {
        let html = competitions_page(
            &[view("open", Phase::Upcoming, 133)],
            ListOptions::default(),
            NOW,
        )
        .into_string();
        assert!(html.contains("Entries close in 2 h 13 min"));
        assert!(html.contains(r#"href="/competitions/open/entry-form""#));
        assert!(html.contains("Daily Fantasy Weather"));
        assert!(html.contains(r#"href="/help""#));
        // What it costs, all in, and what first place wins; the network fee is in the entry fee
        // once known.
        assert!(html.contains("<dt>Entry fee</dt><dd>5,250 sats</dd>"));
        assert!(!html.contains(">Price<"));
        assert!(html.contains("<dt>Prizes</dt><dd>15,000 sats</dd>"));
        assert!(!html.contains("Paid places") && !html.contains(">Pot<"));
        let mut priced = view("open", Phase::Upcoming, 133);
        priced.network_fee = Some(437);
        let row = competition_row(&priced, NOW).into_string();
        assert!(row.contains(r#"data-label="Entry fee">5,687 sats</span>"#));
        assert!(row.contains("Entry fee 5,687 sats · Prizes 15,000 sats · 10 min · 1 of 3 entries"));
    }

    #[test]
    fn cancelled_competitions_are_hidden_until_asked_for() {
        let competitions = vec![
            view("done", Phase::Scored, -100),
            view("unfilled", Phase::Cancelled, -200),
        ];
        let hidden = competitions_page(&competitions, ListOptions::default(), NOW).into_string();
        assert!(!hidden.contains("unfilled"));
        assert!(hidden.contains("Show cancelled (1)"));
        assert!(hidden.contains(r#"href="/competitions?cancelled=1""#));

        let shown = competitions_page(
            &competitions,
            ListOptions {
                page: 0,
                show_cancelled: true,
            },
            NOW,
        )
        .into_string();
        assert!(shown.contains("unfilled"));
        assert!(
            shown.contains("Didn&#39;t run") || shown.contains("Didn't run"),
            "an unfilled competition says so"
        );
        assert!(!shown.contains("badge-failed"));
        assert!(shown.contains("Hide cancelled (1)"));
    }

    fn text(markup: Markup) -> String {
        markup.into_string().replace("&#39;", "'")
    }

    /// The window started before it filled: not live, and not hidden with
    /// the cancelled ones while its refunds are on their way.
    #[test]
    fn an_unfilled_competition_says_its_refund_is_pending() {
        let unfilled = view("unfilled", Phase::Unfilled, -5);
        let html = text(competitions_page(
            std::slice::from_ref(&unfilled),
            ListOptions::default(),
            NOW,
        ));
        assert!(html.contains(">Didn't run</span>"));
        assert!(html.contains("Refunding…"));
        assert!(!html.contains("badge-live"));
        assert!(!html.contains("ends in"));
        assert!(unfilled.did_not_fill());
        assert!(!unfilled.can_enter);
    }

    /// Nothing about refunds with nothing paid in. Escrowed fees say when their refunds open
    /// while the escrows are locked, "Refunding…" once they are open, and "Refunded" once every
    /// escrowed fee is back.
    #[test]
    fn an_unfilled_competition_says_where_its_refunds_stand() {
        let badge = |view: &CompetitionView| text(phase_badge(view));
        let line = |view: &CompetitionView| refund_line(view, NOW).map(text);

        let mut empty = view("empty", Phase::Unfilled, -5);
        empty.total_entries = 0;
        assert!(badge(&empty).contains(">Didn't run</span>"));
        assert!(badge(&empty).contains("no entry fees were paid"));
        assert_eq!(line(&empty), None);

        // A fee paid into escrow without an entry is still owed back, once its escrow's
        // refund leaf opens.
        let opens_at = NOW + time::Duration::hours(20);
        let mut escrowed = empty.clone();
        escrowed.refunds = RefundProgress {
            escrowed: 1,
            refunded: 0,
            written_off: 0,
            opens_at: Some(opens_at),
            ..Default::default()
        };
        assert!(badge(&escrowed).contains("every entry fee is returned"));
        assert_eq!(escrowed.refunds(NOW), Refunds::Locked(opens_at));
        let locked = line(&escrowed).unwrap();
        assert!(locked.starts_with("Refunds open <time"), "{locked}");
        assert!(locked.contains(">Sep 25, 08:00 UTC</time>"), "{locked}");
        assert!(!locked.contains("pending"));

        // Open: under way, whatever the count.
        assert_eq!(
            escrowed.refunds(opens_at + time::Duration::minutes(1)),
            Refunds::Pending
        );
        escrowed.refunds.opens_at = Some(NOW - time::Duration::minutes(1));
        assert_eq!(line(&escrowed).as_deref(), Some("Refunding…"));

        let mut cancelled = view("cancelled", Phase::Cancelled, -600);
        cancelled.total_entries = 2;
        cancelled.refunds = RefundProgress {
            escrowed: 3,
            refunded: 2,
            written_off: 0,
            opens_at: Some(NOW + time::Duration::hours(1)),
            ..Default::default()
        };
        // Some are back already, so the rest are being refunded, not locked.
        assert_eq!(line(&cancelled).as_deref(), Some("Refunding…"));
        cancelled.refunds.refunded = 3;
        assert_eq!(line(&cancelled).as_deref(), Some("Refunded"));
        assert!(badge(&cancelled).contains(">Didn't run</span>"));

        // An escrow an operator wrote off is no longer owed, but wasn't returned either: over
        // once the rest are back, and not all refunded.
        cancelled.refunds.refunded = 1;
        cancelled.refunds.escrowed = 2;
        cancelled.refunds.written_off = 1;
        assert_eq!(line(&cancelled).as_deref(), Some("Refunding…"));
        cancelled.refunds.refunded = 2;
        assert_eq!(cancelled.refunds(NOW), Refunds::Partly);
        assert_eq!(line(&cancelled).as_deref(), Some("Not all refunded"));
        let mut written_off = empty.clone();
        written_off.refunds = RefundProgress {
            escrowed: 0,
            refunded: 0,
            written_off: 1,
            opens_at: None,
            ..Default::default()
        };
        assert_eq!(written_off.refunds(NOW), Refunds::Partly);
        assert_eq!(line(&written_off).as_deref(), Some("Not all refunded"));
        assert!(!badge(&written_off).contains("no entry fees were paid"));
        // Its entries are the ones that arrived; the refund line counts the fee.
        assert_eq!(written_off.entries(), "0 of 3");
        assert_eq!(written_off.entered(), 1);

        // A paid ticket whose entry never arrived is not a tenth entry beside "didn't fill".
        let mut short = view("short", Phase::Cancelled, -600);
        short.total_entries = 9;
        short.total_allowed_entries = 10;
        short.refunds = RefundProgress {
            escrowed: 10,
            ..Default::default()
        };
        assert_eq!(short.entries(), "9 of 10");
        assert!(short.did_not_fill());

        // Held Lightning payments are released when it is cancelled.
        let mut held = view("held", Phase::Cancelled, -600);
        held.total_entries = 2;
        assert_eq!(line(&held).as_deref(), Some("Refunded"));

        // The list shows the line under the window.
        let row = text(competition_row(&escrowed, NOW));
        assert!(row.contains(r#"<span class="cell-note refund-line">Refunding…</span>"#));
    }

    /// Finished competitions say only whether they ran; an operator's cancellation stays
    /// "Cancelled".
    #[test]
    fn finished_competitions_say_finished_or_did_not_run() {
        let badge = |view: &CompetitionView| text(phase_badge(view));
        let mut returned = view("returned", Phase::Scored, -60);
        returned.pot_refunded = true;
        for finished in [
            view("scored", Phase::Scored, -60),
            returned.clone(),
            view("expired", Phase::Expired, -60),
        ] {
            assert!(
                badge(&finished).contains(">Finished</span>"),
                "{}",
                finished.id
            );
        }
        assert!(text(competition_row(&returned, NOW)).contains("no winner · pot shared back"));
        assert!(badge(&view("failed", Phase::Failed, -60)).contains(">Didn't run</span>"));
        let mut operator = view("operator", Phase::Cancelled, -60);
        operator.total_entries = 3;
        assert!(badge(&operator).contains(">Cancelled</span>"));
        assert_eq!(refund_line(&operator, NOW).map(text), None);
        // Its entry fees held by Lightning: refunded once every invoice is released, not all
        // refunded when one was settled instead.
        operator.refunds = RefundProgress {
            held: 2,
            released: 1,
            ..Default::default()
        };
        assert!(operator.owes_refunds());
        assert_eq!(
            refund_line(&operator, NOW).map(text).as_deref(),
            Some("Refunding…")
        );
        operator.refunds.released = 2;
        assert_eq!(
            refund_line(&operator, NOW).map(text).as_deref(),
            Some("Refunded")
        );
        operator.refunds.released = 1;
        operator.refunds.settled = 1;
        assert_eq!(
            refund_line(&operator, NOW).map(text).as_deref(),
            Some("Not all refunded")
        );
        assert!(badge(&operator).contains(">Cancelled</span>"));

        // Cancelled with every seat taken, as a failed kickoff check does: the escrowed fees
        // are owed back all the same.
        let mut full = view("full", Phase::Cancelled, -60);
        full.total_entries = full.total_allowed_entries;
        full.refunds = RefundProgress {
            escrowed: 5,
            refunded: 0,
            written_off: 0,
            opens_at: Some(NOW - time::Duration::minutes(1)),
            ..Default::default()
        };
        assert!(!full.did_not_fill());
        assert!(badge(&full).contains(">Cancelled</span>"));
        assert_eq!(
            refund_line(&full, NOW).map(text).as_deref(),
            Some("Refunding…")
        );
        assert!(text(competition_row(&full, NOW))
            .contains(r#"<span class="cell-note refund-line">Refunding…</span>"#));
        full.refunds.refunded = 5;
        assert_eq!(
            refund_line(&full, NOW).map(text).as_deref(),
            Some("Refunded")
        );
        // A failed competition with escrowed fees says the same.
        let mut failed = view("failed", Phase::Failed, -60);
        failed.refunds = RefundProgress {
            escrowed: 3,
            refunded: 0,
            written_off: 0,
            opens_at: None,
            ..Default::default()
        };
        assert!(failed.owes_refunds());
        assert!(
            refund_line(&failed, NOW).is_some(),
            "a failed competition with escrows shows its refunds"
        );
        for old in [
            "Pot return<",
            "Contract expired",
            "Didn't fill",
            "Too few entries",
            ">Failed<",
        ] {
            for competition in [&returned, &operator] {
                assert!(!badge(competition).contains(old));
            }
        }
    }

    /// An unlisted competition is reached by its link only: the list leaves it out, and it is
    /// never featured.
    #[test]
    fn unlisted_competitions_are_not_listed() {
        let mut hidden = view("hidden-open", Phase::Upcoming, 30);
        hidden.unlisted = true;
        let mut hidden_done = view("hidden-done", Phase::Scored, -100);
        hidden_done.unlisted = true;
        let competitions = [
            hidden,
            hidden_done,
            view("listed-open", Phase::Upcoming, 60),
        ];
        let html = competitions_page(&competitions, ListOptions::default(), NOW).into_string();
        assert!(!html.contains("hidden-open") && !html.contains("hidden-done"));
        assert!(html.contains(r#"href="/competitions/listed-open/entry-form""#));
        assert!(html.contains("No finished competitions yet."));
        assert_eq!(
            shown_ids(&competitions, ListOptions::default()),
            vec!["listed-open".to_owned()]
        );
    }

    /// The rows the handler completes are exactly the rows the page shows.
    #[test]
    fn shown_ids_are_the_rows_the_page_shows() {
        let mut competitions: Vec<_> = (0..25)
            .map(|index| {
                view(
                    &format!("done-{index:02}"),
                    Phase::Scored,
                    -1000 + index * 20,
                )
            })
            .collect();
        competitions.push(view("open", Phase::Upcoming, 60));
        competitions.push(view("live", Phase::Live, -5));
        competitions.push(view("cancelled", Phase::Cancelled, -30));
        for page in 0..4 {
            for show_cancelled in [false, true] {
                let options = ListOptions {
                    page,
                    show_cancelled,
                };
                let html = competitions_page(&competitions, options, NOW).into_string();
                let mut rendered: Vec<_> = html
                    .match_indices(r#"class="competition-row" data-competition-id=""#)
                    .map(|(at, found)| {
                        let rest = &html[at + found.len()..];
                        rest[..rest.find('"').unwrap()].to_owned()
                    })
                    .collect();
                let mut shown = shown_ids(&competitions, options);
                rendered.sort();
                shown.sort();
                assert_eq!(rendered, shown, "page {page}, cancelled {show_cancelled}");
            }
        }
    }

    #[test]
    fn finished_competitions_page_ten_at_a_time() {
        let competitions: Vec<_> = (0..25)
            .map(|index| {
                view(
                    &format!("done-{index:02}"),
                    Phase::Scored,
                    -1000 + index * 20,
                )
            })
            .collect();
        let first = competitions_page(&competitions, ListOptions::default(), NOW).into_string();
        assert_eq!(first.matches("class=\"competition-row\"").count(), 10);
        assert!(first.contains("done-24") && !first.contains("done-14"));
        assert!(first.contains("Page 1 of 3"));
        assert!(first.contains(r#"href="/competitions?page=1""#));

        let last = competitions_page(
            &competitions,
            ListOptions {
                page: 2,
                show_cancelled: false,
            },
            NOW,
        )
        .into_string();
        assert_eq!(last.matches("class=\"competition-row\"").count(), 5);
        assert!(last.contains("done-00"));
        assert!(last.contains("← Newer"));
    }

    #[test]
    fn rows_are_links_so_the_whole_row_is_clickable() {
        let html = competition_row(&view("live", Phase::Live, -5), NOW).into_string();
        assert!(html.starts_with("<a class=\"competition-row\""));
        assert!(html.contains(r#"href="/competitions/live/leaderboard""#));
        assert!(html.contains(r#"data-label="Duration">10 min</span>"#));
        assert!(!html.contains("ends in"));
    }

    #[test]
    fn refunded_and_cancelled_rows_do_not_advertise_winner_prizes() {
        let mut refunded = view("refunded", Phase::Scored, -60);
        refunded.pot_refunded = true;
        for competition in [refunded, view("cancelled", Phase::Cancelled, -60)] {
            let html = competition_row(&competition, NOW).into_string();
            assert!(html.contains(r#"data-label="Prizes">—</span>"#));
            assert!(!html.contains("Prizes 15,000"));
            // Nothing to buy either.
            assert!(html.contains(r#"data-label="Entry fee">—</span>"#));
        }
    }

    /// A queue has no seat count: it shows how many entered, never "X of Y" or "Full", and how
    /// its entries are grouped.
    #[test]
    fn a_queue_shows_how_many_entered_and_its_pool_size() {
        let queue = queued("q", 40);
        let row = competition_row(&queue, NOW).into_string();
        assert!(
            row.contains(r#"data-label="Entries">40 entered</span>"#),
            "{row}"
        );
        assert!(row.contains("pools of up to 25"));
        // Forty entries make two pools of twenty.
        assert!(row.contains("Entry fee 5,250 sats · Prizes 100,000 sats · 10 min · 40 entered"));
        assert_eq!(queue.pot(), "100,000 sats per pool");
        let six = queued("q", 6);
        assert_eq!(six.win(), "30,000 sats");
        // Before anyone enters, a pool's winner takes at least a smallest pool's pot.
        assert_eq!(queued("q", 0).win(), "10,000 sats");
        assert!(row.contains(r#"href="/competitions/q/entry-form""#));
        assert!(!row.contains(" of 3"));
        assert!(phase_badge(&queue).into_string().contains(">Open</span>"));

        let page = competitions_page(std::slice::from_ref(&queue), ListOptions::default(), NOW)
            .into_string();
        assert!(!page.contains("1st place"));
        assert!(!page.contains("Full"));

        let mut closed = queued("q", 200);
        closed.can_enter = false;
        let badge = phase_badge(&closed).into_string();
        assert!(badge.contains("Entries closed") && !badge.contains("Full"));
    }

    /// Twenty-seven entries make pools of fourteen and thirteen: the pot and the prize are a
    /// range, never the smaller pool's alone.
    #[test]
    fn a_queue_with_uneven_pools_shows_the_range_of_their_pots() {
        let queue = queued("q", 27);
        assert_eq!(queue.pot(), "65,000–70,000 sats per pool");
        assert_eq!(queue.win(), "65,000–70,000 sats");
        assert_eq!(queue.top_prize(), Some(65_000));
        let row = competition_row(&queue, NOW).into_string();
        assert!(row.contains(r#"data-label="Prizes">65,000–70,000 sats</span>"#));

        // Once formed, the pools' own sizes decide, whatever the entry count says.
        let mut split = queued("q", 30);
        split.phase = Phase::Scored;
        split.can_enter = false;
        if let Queue::Queued(queue) = &mut split.queue {
            queue.pools = [(0, 14), (1, 13)]
                .into_iter()
                .map(|(index, size)| PoolLink {
                    id: POOL.into(),
                    index: Some(index),
                    size: Some(size),
                })
                .collect();
        }
        assert_eq!(split.pot(), "65,000–70,000 sats per pool");
        assert_eq!(split.win(), "65,000–70,000 sats");

        // Pools of one size keep one figure.
        if let Queue::Queued(queue) = &mut split.queue {
            queue.pools[0].size = Some(13);
        }
        assert_eq!(split.pot(), "65,000 sats per pool");
        assert_eq!(split.win(), "65,000 sats");
    }

    /// A result past its signing time is said to be late in the list; one still due is not.
    #[test]
    fn a_result_past_its_signing_time_is_late() {
        // Ends ten minutes after its start; signing is due five minutes after that.
        let due_soon = view("w", Phase::AwaitingResult, -14);
        assert!(!due_soon.result_is_late(NOW));
        assert!(!competition_row(&due_soon, NOW)
            .into_string()
            .contains("results are late"));
        let late = view("w", Phase::AwaitingResult, -60);
        assert!(late.result_is_late(NOW));
        assert!(competition_row(&late, NOW)
            .into_string()
            .contains(r#"<span class="cell-note">results are late</span>"#));
        assert!(!view("w", Phase::Scored, -60).result_is_late(NOW));
    }

    #[test]
    fn a_queue_split_into_pools_says_so_and_each_pool_says_which() {
        const PARENT: &str = "01a0c225-f3c4-71f3-9f62-4b74859cfc25";
        const OTHER_POOL: &str = "01a0c226-0000-7000-8000-000000000002";
        let mut split = queued(PARENT, 30);
        split.phase = Phase::AwaitingResult;
        split.can_enter = false;
        split.queue = Queue::Queued(QueueView {
            min_players: Some(2),
            max_players: 25,
            entries: Some(30),
            max_entries: None,
            pools: vec![
                PoolLink {
                    id: POOL.into(),
                    index: Some(0),
                    size: Some(15),
                },
                PoolLink {
                    id: OTHER_POOL.into(),
                    index: Some(1),
                    size: Some(15),
                },
            ],
        });
        assert!(phase_badge(&split)
            .into_string()
            .contains(">Split into pools</span>"));
        assert!(!split.did_not_fill());
        // The list shows the phase the queue is listed with, not the split.
        let row = competition_row(&split, NOW).into_string();
        assert!(row.contains(r#"<span class="cell-note">2 pools</span>"#));
        assert!(row.contains(">Awaiting results</span>"));
        assert!(!row.contains("Split into pools"));
        let mut one_pool = split.clone();
        if let Queue::Queued(queue) = &mut one_pool.queue {
            queue.pools.truncate(1);
        }
        assert!(competition_row(&one_pool, NOW)
            .into_string()
            .contains(r#"<span class="cell-note">1 pool</span>"#));

        let pool = |id: &str, index, phase| {
            let mut pool = view(id, phase, -5);
            pool.queue = Queue::Pool(PoolOf {
                parent_id: PARENT.into(),
                index: Some(index),
            });
            pool
        };
        let row = competition_row(&pool(OTHER_POOL, 1, Phase::Live), NOW).into_string();
        assert!(row.contains(r#"<span class="cell-note">Pool 2</span>"#));
        assert!(row.contains(r#"data-label="Entries">1 of 3</span>"#));

        // The list shows the queue once, with its least advanced pool, and not its pools.
        let all = [
            split.clone(),
            pool(POOL, 0, Phase::Scored),
            pool(OTHER_POOL, 1, Phase::Live),
        ];
        let shown = listed(&all);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].phase, Phase::Live);
        let page = competitions_page(&all, ListOptions::default(), NOW).into_string();
        assert!(!page.contains("Awaiting results"));
        assert!(!page.contains("Split into pools"));
        assert!(page.contains(">Live</span>"));
        assert!(!page.contains("Pool 1") && !page.contains("Pool 2"));
        assert!(page.contains("2 pools"));

        // A pool whose queue isn't listed is listed itself.
        let orphan = [pool(POOL, 0, Phase::Scored)];
        assert_eq!(listed(&orphan).len(), 1);
    }

    #[test]
    fn a_queue_too_small_for_a_pool_is_refunded() {
        let mut small = queued("q", 1);
        small.phase = Phase::Unfilled;
        small.can_enter = false;
        small.refunds = RefundProgress {
            escrowed: 1,
            refunded: 0,
            written_off: 0,
            opens_at: None,
            ..Default::default()
        };
        assert!(small.did_not_fill());
        let badge = text(phase_badge(&small));
        assert!(badge.contains(">Didn't run</span>"));
        assert!(badge.contains("Too few players entered to make a pool"));
        assert_eq!(
            refund_line(&small, NOW).map(text).as_deref(),
            Some("Refunding…")
        );
        small.phase = Phase::Cancelled;
        if let Queue::Queued(queue) = &mut small.queue {
            queue.entries = Some(5);
        }
        assert!(!small.did_not_fill(), "a queue big enough was cancelled");
    }

    #[test]
    fn refund_amounts_use_the_attested_contract_and_preserve_historical_shares() {
        use crate::domain::CreateEvent;
        use dlctix::{
            bitcoin::{Amount, FeeRate},
            hashlock,
            secp::Scalar,
            ContractParameters, EventLockingConditions, MarketMaker, Outcome, PayoutWeights,
            Player,
        };
        let point = |byte: u8| Scalar::from_slice(&[byte; 32]).unwrap().base_point_mul();
        let attestation = Scalar::from_slice(&[20; 32]).unwrap();
        let event = EventLockingConditions {
            locking_points: vec![attestation.base_point_mul().into()],
            expiry: None,
        };
        let mut competition = Competition::new(&CreateEvent {
            id: uuid::Uuid::now_v7(),
            signing_date: NOW,
            start_observation_date: NOW - time::Duration::hours(2),
            end_observation_date: NOW - time::Duration::hours(1),
            locations: vec!["KPWM".into()],
            number_of_values_per_entry: 1,
            number_of_places_win: 1,
            total_allowed_entries: 3,
            entry_fee: 1_000,
            coordinator_fee: crate::domain::CoordinatorFee::whole_percent(10),
            // Intentionally different: the signed funding value controls the refund.
            total_competition_pool: 6_000,
            relative_locktime_block_delta: None,
            unlisted: false,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
        });
        competition.total_entries = 3;
        competition.event_announcement = Some(event.clone());
        competition.attestation = Some(attestation.into());
        assert!(CompetitionView::new(&competition, NOW)
            .refund_shares
            .is_none());

        competition.contract_parameters = Some(ContractParameters {
            market_maker: MarketMaker { pubkey: point(5) },
            players: (1u8..=3)
                .map(|key| Player {
                    pubkey: point(key),
                    ticket_hash: hashlock::sha256(&[key + 10; 32]),
                    payout_hash: hashlock::sha256(&[key + 1; 32]),
                })
                .collect(),
            event,
            outcome_payouts: [(
                Outcome::Attestation(0),
                PayoutWeights::from([(0, 34), (1, 33), (2, 33)]),
            )]
            .into(),
            fee_rate: FeeRate::from_sat_per_vb_u32(1),
            funding_value: Amount::from_sat(3_000),
            relative_locktime_block_delta: 72,
        });
        let historical = CompetitionView::new(&competition, NOW);
        assert!(historical.pot_refunded);
        assert_eq!(historical.refund_shares, Some(vec![1_020, 990, 990]));

        competition
            .contract_parameters
            .as_mut()
            .unwrap()
            .outcome_payouts
            .insert(
                Outcome::Attestation(0),
                PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
            );
        assert_eq!(
            CompetitionView::new(&competition, NOW).refund_shares,
            Some(vec![1_000; 3])
        );

        // Invalid player indexes cannot be displayed as a plausible allocation.
        competition
            .contract_parameters
            .as_mut()
            .unwrap()
            .outcome_payouts
            .insert(
                Outcome::Attestation(0),
                PayoutWeights::from([(0, 1), (1, 1), (9, 1)]),
            );
        assert!(CompetitionView::new(&competition, NOW)
            .refund_shares
            .is_none());
        competition.attestation = None;
        assert!(CompetitionView::new(&competition, NOW)
            .refund_shares
            .is_none());

        // Expiry has its own signed allocation, independent of the attested outcome.
        competition.expiry_broadcasted_at = Some(NOW);
        competition
            .contract_parameters
            .as_mut()
            .unwrap()
            .outcome_payouts
            .insert(
                Outcome::Expiry,
                PayoutWeights::from([(0, 1), (1, 1), (2, 1)]),
            );
        let expired = CompetitionView::new(&competition, NOW);
        assert_eq!(expired.phase, Phase::Expired);
        assert!(!expired.pot_refunded);
        assert_eq!(expired.refund_shares, Some(vec![1_000; 3]));
        assert!(phase_badge(&expired)
            .into_string()
            .contains(">Finished</span>"));
    }

    /// The default competition: a queue of 20 seats that plays as one pool and pays 70% and 30%
    /// from ten players, its winner taking the pot below that.
    pub(crate) fn twenty_seats(entries: u64) -> CompetitionView {
        let mut competition = queued("q", entries);
        competition.paid_places = 2;
        if let Queue::Queued(queue) = &mut competition.queue {
            queue.max_players = 20;
            queue.max_entries = Some(20);
        }
        competition
    }

    #[test]
    fn a_one_pool_queue_shows_its_seats_and_how_its_prizes_split() {
        let open = twenty_seats(3);
        assert_eq!(open.entries(), "20 seats · 17 left");
        assert_eq!(open.prize_split().as_deref(), Some("1st 70% · 2nd 30%"));
        assert_eq!(
            open.prize_rule().as_deref(),
            Some("Under 10 players: winner takes all")
        );
        // Three players: first place takes their pot.
        assert_eq!(open.win(), "15,000 sats");
        let row = competition_row(&open, NOW).into_string();
        assert!(
            row.contains(r#"data-label="Entries">20 seats · 17 left</span>"#),
            "{row}"
        );
        assert!(row.contains("1st 70% · 2nd 30%"));
        assert!(row.contains("Under 10 players: winner takes all"));
        assert!(row.contains("Prizes 15,000 sats (1st 70% · 2nd 30%)"));
        // One pool: no pool size to explain.
        assert!(!row.contains("pools of up to"));

        // From ten players first place takes 70% of the pot.
        assert_eq!(twenty_seats(10).win(), "35,000 sats");
        assert_eq!(twenty_seats(20).win(), "70,000 sats");
        assert_eq!(twenty_seats(20).entries(), "20 seats · 0 left");
        let mut closed = twenty_seats(12);
        closed.can_enter = false;
        assert_eq!(closed.entries(), "12 of 20");

        // A queue that pays one place shows no split.
        assert!(queued("q", 3).prize_split().is_none());
        assert!(queued("q", 3).prize_rule().is_none());
    }
}
