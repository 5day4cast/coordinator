//! The home page: what the game is, the competition to enter now, and every
//! competition grouped by where it stands.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::{
    get_percentage_weights, leaderboard::Phase, winner_payout_sats, Competition, RefundProgress,
};
use crate::infra::oracle::ScoringRules;
use crate::templates::format::{self, sats, thousands};

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
    pub entry_fee: u64,
    /// What the entrant pays: the entry fee plus the coordinator fee.
    pub ticket_price: u64,
    pub total_pool: u64,
    pub total_entries: u64,
    pub total_allowed_entries: u64,
    pub paid_places: u64,
    pub can_enter: bool,
    pub number_of_values_per_entry: usize,
    pub locations: Vec<String>,
    /// How the oracle scores the picks.
    pub scoring_rules: ScoringRules,
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
}

/// Where the entry fees of a competition that didn't run stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refunds {
    /// No entry fee was paid, so nothing is owed.
    Nothing,
    Pending,
    Done,
}

impl CompetitionView {
    pub fn new(competition: &Competition, now: OffsetDateTime) -> Self {
        let event = &competition.event_submission;
        let phase = Phase::of(competition, now);
        let pot_refunded = phase == Phase::Scored && competition.refunds_every_entry();
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
        Self {
            id: competition.id.to_string(),
            phase,
            start: event.start_observation_date,
            end: event.end_observation_date,
            entry_fee: event.entry_fee as u64,
            ticket_price: competition.calculate_invoice_amount(),
            total_pool: event.total_competition_pool as u64,
            total_entries: competition.total_entries,
            total_allowed_entries: event.total_allowed_entries as u64,
            paid_places: event.number_of_places_win as u64,
            can_enter,
            number_of_values_per_entry: event.number_of_values_per_entry,
            locations: event.locations.clone(),
            scoring_rules: event.scoring_rules(),
            refunds: RefundProgress::default(),
            pot_refunded,
            refund_shares: (pot_refunded || phase == Phase::Expired)
                .then(|| refund_shares(competition, phase))
                .flatten(),
            queue,
        }
    }

    /// `3 of 25`, or `40 entered` for a queue, which has no seat count.
    pub fn entries(&self) -> String {
        match &self.queue {
            Queue::Queued(queue) => format!("{} entered", self.entry_count(queue)),
            _ => format!("{} of {}", self.total_entries, self.total_allowed_entries),
        }
    }

    /// A queue's paid entries, those already moved to its pools included.
    fn entry_count(&self, queue: &QueueView) -> u64 {
        queue.entries.unwrap_or(self.total_entries)
    }

    /// The pot, or for a queue the pot of a full pool, which each pool's winner takes.
    pub fn pot(&self) -> String {
        match &self.queue {
            Queue::Queued(queue) => format!(
                "up to {}",
                sats(self.entry_fee.saturating_mul(queue.max_players))
            ),
            _ => sats(self.total_pool),
        }
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

    /// Where the entry fees of a competition that didn't fill stand. Escrowed fees are returned
    /// once their players are paid; held Lightning payments are released when the competition
    /// is cancelled.
    pub fn refunds(&self) -> Refunds {
        let RefundProgress { escrowed, refunded } = self.refunds;
        if escrowed > 0 {
            if refunded >= escrowed {
                Refunds::Done
            } else {
                Refunds::Pending
            }
        } else if self.total_entries == 0 {
            Refunds::Nothing
        } else if self.phase == Phase::Unfilled {
            Refunds::Pending
        } else {
            Refunds::Done
        }
    }

    pub fn url(&self) -> String {
        if self.can_enter {
            format!("/competitions/{}/entry-form", self.id)
        } else {
            format!("/competitions/{}/leaderboard", self.id)
        }
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
/// finished and cancelled competitions stay quiet.
pub fn phase_badge(competition: &CompetitionView) -> Markup {
    let queue = competition.queue.queued();
    let (class, label) = match competition.phase {
        // Its pools carry on as competitions of their own.
        _ if queue.is_some_and(|queue| !queue.pools.is_empty()) => {
            ("badge badge-quiet", "Split into pools")
        }
        Phase::Upcoming if !competition.can_enter && queue.is_some() => {
            ("badge badge-open", "Entries closed")
        }
        Phase::Upcoming if !competition.can_enter => ("badge badge-open", "Full"),
        Phase::Upcoming => ("badge badge-open", "Open"),
        Phase::Unfilled => ("badge badge-quiet", unfilled_label(competition)),
        Phase::Live => ("badge badge-live", "Live"),
        Phase::AwaitingResult => ("badge badge-waiting", "Awaiting results"),
        Phase::Scored if competition.pot_refunded => ("badge badge-quiet", "Pot return"),
        Phase::Scored => ("badge badge-quiet", "Finished"),
        Phase::Expired => ("badge badge-quiet", "Contract expired"),
        Phase::Cancelled if competition.did_not_fill() => {
            ("badge badge-quiet", unfilled_label(competition))
        }
        Phase::Cancelled => ("badge badge-quiet", "Cancelled"),
        Phase::Failed => ("badge badge-failed", "Failed"),
    };
    let title = competition
        .did_not_fill()
        .then(|| match competition.refunds() {
            Refunds::Nothing => "Not enough entries by the start; no entry fees were paid",
            _ => "Not enough entries by the start; every entry fee is returned",
        });
    html! { span class=(class) title=[title] { (label) } }
}

fn unfilled_label(competition: &CompetitionView) -> &'static str {
    match (competition.refunds(), competition.queue.queued().is_some()) {
        (Refunds::Nothing, false) => "Didn't fill",
        (Refunds::Pending, false) => "Didn't fill: refund pending",
        (Refunds::Done, false) => "Didn't fill: refunded",
        (Refunds::Nothing, true) => "Too few entries",
        (Refunds::Pending, true) => "Too few entries: refund pending",
        (Refunds::Done, true) => "Too few entries: refunded",
    }
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

/// Competitions page content: intro, the one to enter now, then every group.
pub fn competitions_page(
    competitions: &[CompetitionView],
    options: ListOptions,
    now: OffsetDateTime,
) -> Markup {
    let competitions = &listed(competitions);
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
    let shown: Vec<_> = finished
        .iter()
        .skip(page * PAGE_SIZE)
        .take(PAGE_SIZE)
        .copied()
        .collect();
    let featured = open
        .iter()
        .filter(|competition| competition.can_enter)
        .min_by_key(|competition| competition.start)
        .or_else(|| live.first())
        .copied();
    let show_cancelled = options.show_cancelled;

    html! {
        div id="competitions-page"
            hx-get=(options.url())
            hx-trigger="every 30s"
            hx-select="#competitions-page"
            hx-swap="outerHTML" {
            (intro(featured, now))

            @if !live.is_empty() {
                (group("Live", "Readings are being recorded now.", &live, now))
            }
            @if !open.is_empty() {
                (group("Upcoming", "Taking entries until the window starts.", &open, now))
            }
            @if !waiting.is_empty() {
                (group("Awaiting results", "The window is over; the result and payouts follow.", &waiting, now))
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
                @if shown.is_empty() {
                    p class="empty-state" { "No finished competitions yet." }
                } @else {
                    (list(&shown, now))
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

/// The competitions the list shows. A queued competition stands for its pools, which its page
/// links: a pool whose competition is listed isn't, and a queue split into pools is listed where
/// its least advanced pool is, since it has no lifecycle of its own after the split.
fn listed(competitions: &[CompetitionView]) -> Vec<CompetitionView> {
    let pools_of = |parent: &str| {
        competitions.iter().filter(move |competition| {
            matches!(&competition.queue, Queue::Pool(pool) if pool.parent_id == parent)
        })
    };
    let listed_parent = |competition: &CompetitionView| match &competition.queue {
        Queue::Pool(pool) => competitions
            .iter()
            .any(|parent| parent.id == pool.parent_id),
        _ => false,
    };
    competitions
        .iter()
        .filter(|competition| !listed_parent(competition))
        .map(|competition| {
            let mut shown = competition.clone();
            if competition.queue.queued().is_some_and(|queue| !queue.pools.is_empty()) {
                if let Some(phase) = pools_of(&competition.id)
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
                h1 class="title is-4" { "Call the weather, win the pot" }
                p {
                    "Each competition covers a few airport weather stations. For each reading, pick whether "
                    "it will come in over the forecast, on it (par) or under it. Pay the entry fee in sats; "
                    "when the window closes, the best scores take the pot."
                }
            }
            @if let Some(competition) = featured {
                (featured_card(competition, now))
            }
        }
    }
}

fn featured_card(competition: &CompetitionView, now: OffsetDateTime) -> Markup {
    // A queue's prize is its pools' pots, which depend on how many enter.
    let prize = competition
        .queue
        .queued()
        .is_none()
        .then(|| competition.prizes().first().map(|(_, amount)| *amount))
        .flatten();
    html! {
        div class="featured-card" {
            div class="featured-status" {
                (phase_badge(competition))
                span class="countdown" {
                    @if competition.phase == Phase::Live {
                        "Results in " (format::duration(competition.end - now))
                    } @else {
                        "Entries close in " (format::duration(competition.start - now))
                    }
                }
            }
            p class="featured-window" { (format::window(competition.start, competition.end)) }
            dl class="featured-facts" {
                div { dt { "Entry" } dd { (sats(competition.ticket_price)) } }
                div { dt { "Pot" } dd { (competition.pot()) } }
                @if let Some(prize) = prize {
                    div { dt { "1st place" } dd { (sats(prize)) } }
                }
                div { dt { "Entries" } dd { (competition.entries()) } }
            }
            @if let Some(queue) = competition.queue.queued() {
                p class="featured-note" { (queue.pool_note()) "." }
            }
            a class=(if competition.can_enter { "button is-primary is-fullwidth" } else { "button is-fullwidth" })
              href=(competition.url()) hx-get=(competition.url())
              hx-target="#main-content" hx-push-url="true" {
                @if competition.can_enter { "Enter" } @else { "Watch the leaderboard" }
            }
        }
    }
}

fn group(
    title: &str,
    help: &str,
    competitions: &[&CompetitionView],
    now: OffsetDateTime,
) -> Markup {
    html! {
        section class="competition-group" {
            div class="group-heading" {
                h2 class="title is-5" { (title) }
                span class="group-help" { (help) }
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
                span { "Window" }
                span { "Entry" }
                span { "Pot" }
                span { "Entries" }
                span { "Paid places" }
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
    // Phones show these facts below the window. Cancelled and refunded competitions
    // never advertise paid places: those were only their planned winner prizes.
    let mut facts = format!(
        "Entry {} · Pot {} · {}",
        sats(competition.ticket_price),
        competition.pot(),
        match competition.queue {
            Queue::Queued(_) => competition.entries(),
            _ => format!("{} entries", competition.entries()),
        },
    );
    if competition.has_ranked_prizes() {
        facts.push_str(&format!(
            " · {} paid {}",
            competition.paid_places,
            if competition.paid_places == 1 {
                "place"
            } else {
                "places"
            },
        ));
    }
    html! {
        a class="competition-row" data-competition-id=(competition.id) data-facts=(facts)
          href=(competition.url()) hx-get=(competition.url())
          hx-target="#main-content" hx-push-url="true" {
            span class="cell-status" { (phase_badge(competition)) }
            span class="cell-window" {
                (format::window(competition.start, competition.end))
                @match competition.phase {
                    Phase::Upcoming => { span class="cell-note" { "starts in " (format::duration(competition.start - now)) } }
                    Phase::Live => { span class="cell-note" { "ends in " (format::duration(competition.end - now)) } }
                    _ => {}
                }
                @match &competition.queue {
                    Queue::Queued(queue) if queue.pools.is_empty() => {
                        span class="cell-note" { "pools of up to " (queue.max_players) }
                    }
                    Queue::Queued(queue) => { span class="cell-note" { (queue.pools.len()) " pools" } }
                    Queue::Pool(pool) => { span class="cell-note" { (pool.label()) } }
                    Queue::Single => {}
                }
            }
            span class="cell-fee" data-label="Entry" { (sats(competition.ticket_price)) }
            span class="cell-pot" data-label="Pot" { (competition.pot()) }
            span class="cell-entries" data-label="Entries" { (competition.entries()) }
            span class="cell-places" data-label="Paid places" {
                @if competition.has_ranked_prizes() { (competition.paid_places) } @else { "—" }
            }
            span class="cell-action" { (action) " →" }
        }
    }
}

/// `5,000 entry fee + 250 coordinator fee`, or nothing without a fee.
pub fn fee_breakdown(competition: &CompetitionView) -> Option<String> {
    (competition.ticket_price > competition.entry_fee).then(|| {
        format!(
            "{} entry fee + {} coordinator fee",
            thousands(competition.entry_fee),
            thousands(competition.ticket_price - competition.entry_fee)
        )
    })
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
            entry_fee: 5000,
            ticket_price: 5250,
            total_pool: 15000,
            total_entries: 1,
            total_allowed_entries: 3,
            paid_places: 1,
            can_enter: phase == Phase::Upcoming,
            number_of_values_per_entry: 9,
            locations: vec!["KPWM".into()],
            scoring_rules: ScoringRules::Fixed,
            refunds: RefundProgress::default(),
            pot_refunded: false,
            refund_shares: None,
            queue: Queue::Single,
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
        assert!(live < open && open < waiting && waiting < newer && newer < older);
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
        assert!(html.contains("Call the weather"));
        assert!(html.contains("5,250 sats"));
        assert!(html.contains("15,000 sats"));
        assert!(html.contains("Paid places"));
        assert!(!html.contains("Winners"));
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
            shown.contains("Didn't fill"),
            "an unfilled competition says so"
        );
        assert!(!shown.contains("badge-failed"));
        assert!(shown.contains("Hide cancelled (1)"));
    }

    /// The window started before it filled: not live, and not hidden with
    /// the cancelled ones while its refunds are on their way.
    #[test]
    fn an_unfilled_competition_says_its_refund_is_pending() {
        let unfilled = view("unfilled", Phase::Unfilled, -5);
        let html = competitions_page(std::slice::from_ref(&unfilled), ListOptions::default(), NOW)
            .into_string();
        assert!(html.contains("Didn't fill: refund pending"));
        assert!(!html.contains("badge-live"));
        assert!(!html.contains("ends in"));
        assert!(unfilled.did_not_fill());
        assert!(!unfilled.can_enter);
    }

    /// "Refund pending" only while something is owed: never with nothing paid in, and it
    /// moves to "refunded" once every escrowed fee is back.
    #[test]
    fn an_unfilled_competition_says_where_its_refunds_stand() {
        let badge = |view: &CompetitionView| phase_badge(view).into_string();

        let mut empty = view("empty", Phase::Unfilled, -5);
        empty.total_entries = 0;
        assert!(badge(&empty).contains(">Didn't fill</span>"));
        assert!(badge(&empty).contains("no entry fees were paid"));

        // A fee paid into escrow without an entry is still owed back.
        let mut escrowed = empty.clone();
        escrowed.refunds = RefundProgress {
            escrowed: 1,
            refunded: 0,
        };
        assert!(badge(&escrowed).contains("Didn't fill: refund pending"));

        let mut cancelled = view("cancelled", Phase::Cancelled, -600);
        cancelled.total_entries = 2;
        cancelled.refunds = RefundProgress {
            escrowed: 3,
            refunded: 2,
        };
        assert!(badge(&cancelled).contains("Didn't fill: refund pending"));
        cancelled.refunds.refunded = 3;
        assert!(badge(&cancelled).contains("Didn't fill: refunded"));

        // Held Lightning payments are released when it is cancelled.
        let mut held = view("held", Phase::Cancelled, -600);
        held.total_entries = 2;
        assert!(badge(&held).contains("Didn't fill: refunded"));
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
        assert!(html.contains("ends in 5 min"));
    }

    #[test]
    fn refunded_and_cancelled_rows_do_not_advertise_winner_prizes() {
        let mut refunded = view("refunded", Phase::Scored, -60);
        refunded.pot_refunded = true;
        for competition in [refunded, view("cancelled", Phase::Cancelled, -60)] {
            let html = competition_row(&competition, NOW).into_string();
            assert!(!html.contains("1 paid place"));
            assert!(html.contains(r#"data-label="Paid places">—</span>"#));
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
        assert!(row.contains("Pot up to 125,000 sats · 40 entered"));
        assert!(row.contains(r#"href="/competitions/q/entry-form""#));
        assert!(!row.contains(" of 3"));
        assert!(phase_badge(&queue).into_string().contains(">Open</span>"));

        let page = competitions_page(std::slice::from_ref(&queue), ListOptions::default(), NOW)
            .into_string();
        assert!(page.contains("Players are split into pools of up to 25 at the start."));
        assert!(!page.contains("1st place"));
        assert!(!page.contains("Full"));

        let mut closed = queued("q", 200);
        closed.can_enter = false;
        let badge = phase_badge(&closed).into_string();
        assert!(badge.contains("Entries closed") && !badge.contains("Full"));
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
        assert!(competition_row(&split, NOW)
            .into_string()
            .contains(r#"<span class="cell-note">2 pools</span>"#));

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
        let listed = listed(&all);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].phase, Phase::Live);
        let page = competitions_page(&all, ListOptions::default(), NOW).into_string();
        assert!(!page.contains("Awaiting results"));
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
        };
        assert!(small.did_not_fill());
        assert!(phase_badge(&small)
            .into_string()
            .contains("Too few entries: refund pending"));
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
            .contains("Contract expired"));
    }
}
