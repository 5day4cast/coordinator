//! The home page: what the game is, the competition to enter now, and every
//! competition grouped by where it stands.

use maud::{html, Markup};
use time::OffsetDateTime;

use crate::domain::{get_percentage_weights, Competition, CompetitionState};
use crate::templates::format::{self, sats, thousands};

/// Finished competitions shown per page.
pub const PAGE_SIZE: usize = 10;

/// Where a competition stands, as players see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Taking entries until its observation window starts.
    Open,
    /// Inside its observation window.
    Live,
    /// Window over; waiting for the oracle's result or the payout contract.
    AwaitingResults,
    Finished,
    Cancelled,
    Failed,
}

impl Phase {
    pub fn of(competition: &Competition, now: OffsetDateTime) -> Self {
        match competition.get_state() {
            CompetitionState::Failed => Phase::Failed,
            CompetitionState::Cancelled => Phase::Cancelled,
            CompetitionState::Completed
            | CompetitionState::Attested
            | CompetitionState::OutcomeBroadcasted
            | CompetitionState::DeltaBroadcasted
            | CompetitionState::ExpiryBroadcasted => Phase::Finished,
            _ if now < competition.event_submission.start_observation_date => Phase::Open,
            _ if now < competition.event_submission.end_observation_date => Phase::Live,
            _ => Phase::AwaitingResults,
        }
    }
}

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
}

impl CompetitionView {
    pub fn new(competition: &Competition, now: OffsetDateTime) -> Self {
        let event = &competition.event_submission;
        let phase = Phase::of(competition, now);
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
            can_enter: phase == Phase::Open
                && competition.total_entries < event.total_allowed_entries as u64,
            number_of_values_per_entry: event.number_of_values_per_entry,
            locations: event.locations.clone(),
        }
    }

    /// Each paid place's share in percent and in sats, first place first.
    pub fn prizes(&self) -> Vec<(u64, u64)> {
        get_percentage_weights(self.paid_places as usize)
            .into_iter()
            .map(|percent| (percent, self.total_pool * percent / 100))
            .collect()
    }

    /// A cancelled competition that simply never filled.
    pub fn did_not_fill(&self) -> bool {
        self.phase == Phase::Cancelled && self.total_entries < self.total_allowed_entries
    }

    pub fn url(&self) -> String {
        if self.can_enter {
            format!("/competitions/{}/entry-form", self.id)
        } else {
            format!("/competitions/{}/leaderboard", self.id)
        }
    }
}

/// The badge for a competition's phase. Live is the loudest thing on the page;
/// finished and cancelled competitions stay quiet.
pub fn phase_badge(competition: &CompetitionView) -> Markup {
    let (class, label) = match competition.phase {
        Phase::Open if !competition.can_enter => ("badge badge-open", "Full"),
        Phase::Open => ("badge badge-open", "Open"),
        Phase::Live => ("badge badge-live", "Live"),
        Phase::AwaitingResults => ("badge badge-waiting", "Awaiting results"),
        Phase::Finished => ("badge badge-quiet", "Finished"),
        Phase::Cancelled if competition.did_not_fill() => ("badge badge-quiet", "Didn't fill"),
        Phase::Cancelled => ("badge badge-quiet", "Cancelled"),
        Phase::Failed => ("badge badge-failed", "Failed"),
    };
    let title = competition
        .did_not_fill()
        .then_some("Not enough entries by the start; every entry fee is returned");
    html! { span class=(class) title=[title] { (label) } }
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
    let mut live = by_phase(competitions, &[Phase::Live]);
    let mut open = by_phase(competitions, &[Phase::Open]);
    let mut waiting = by_phase(competitions, &[Phase::AwaitingResults]);
    let finished_phases: &[Phase] = if options.show_cancelled {
        &[Phase::Finished, Phase::Failed, Phase::Cancelled]
    } else {
        &[Phase::Finished, Phase::Failed]
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
            hx-swap="outerHTML"
            hx-disinherit="*" {
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
    let prize = competition.prizes().first().map(|(_, amount)| *amount);
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
                div { dt { "Pot" } dd { (sats(competition.total_pool)) } }
                @if let Some(prize) = prize {
                    div { dt { "1st place" } dd { (sats(prize)) } }
                }
                div { dt { "Entries" } dd { (competition.total_entries) " of " (competition.total_allowed_entries) } }
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
    // Phones show these facts as one line under the status and window.
    let facts = format!(
        "Entry {} · Pot {} · {} of {} entries · {} paid {}",
        sats(competition.ticket_price),
        sats(competition.total_pool),
        competition.total_entries,
        competition.total_allowed_entries,
        competition.paid_places,
        if competition.paid_places == 1 {
            "place"
        } else {
            "places"
        },
    );
    html! {
        a class="competition-row" data-competition-id=(competition.id) data-facts=(facts)
          href=(competition.url()) hx-get=(competition.url())
          hx-target="#main-content" hx-push-url="true" {
            span class="cell-status" { (phase_badge(competition)) }
            span class="cell-window" {
                (format::window(competition.start, competition.end))
                @match competition.phase {
                    Phase::Open => { span class="cell-note" { "starts in " (format::duration(competition.start - now)) } }
                    Phase::Live => { span class="cell-note" { "ends in " (format::duration(competition.end - now)) } }
                    _ => {}
                }
            }
            span class="cell-fee" data-label="Entry" { (sats(competition.ticket_price)) }
            span class="cell-pot" data-label="Pot" { (sats(competition.total_pool)) }
            span class="cell-entries" data-label="Entries" {
                (competition.total_entries) " of " (competition.total_allowed_entries)
            }
            span class="cell-places" data-label="Paid places" { (competition.paid_places) }
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
            can_enter: phase == Phase::Open,
            number_of_values_per_entry: 9,
            locations: vec!["KPWM".into()],
        }
    }

    fn position(html: &str, needle: &str) -> usize {
        html.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing"))
    }

    #[test]
    fn groups_come_in_order_with_the_newest_finished_first() {
        let competitions = vec![
            view("finished-old", Phase::Finished, -300),
            view("open", Phase::Open, 60),
            view("finished-new", Phase::Finished, -100),
            view("waiting", Phase::AwaitingResults, -30),
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
            &[view("open", Phase::Open, 133)],
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
            view("done", Phase::Finished, -100),
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
            shown.contains("fill</span>"),
            "an unfilled competition says so"
        );
        assert!(!shown.contains("badge-failed"));
        assert!(shown.contains("Hide cancelled (1)"));
    }

    #[test]
    fn finished_competitions_page_ten_at_a_time() {
        let competitions: Vec<_> = (0..25)
            .map(|index| {
                view(
                    &format!("done-{index:02}"),
                    Phase::Finished,
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
}
