//! The entry form: the competition's terms, a pick for each forecast, and the
//! button that pays for the ticket. Picks are plain radio buttons; paying needs
//! the WASM wallet, so `entry_form.js` handles the submit.

use maud::{html, Markup};

use crate::domain::{
    leaderboard::{Metric, Rule},
    PayoutTermsQuote, TicketStatus, ENTRIES_PAUSED,
};
use crate::templates::{
    components::tip,
    format::{self, sats, MetricText, TimeStyle},
    fragments::loading::{placeholder, Pending},
    pages::competitions::CompetitionView,
    shared_map::{station_map, StationPin},
};

/// One station's forecasts, the values its picks are judged against.
#[derive(Debug, Clone)]
pub struct StationForecast {
    pub station_id: String,
    /// `Portland International, ME`, when the oracle knows the station.
    pub station_name: Option<String>,
    /// Each metric's forecast and what a pick on it is scored against; the rule is `None` while
    /// a lines competition's band is not known yet.
    pub forecasts: Vec<(Metric, Option<f64>, Option<Rule>)>,
}

/// Where this entry's winnings (and any refund) go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayoutDestination {
    /// Nobody is logged in, so the account is unknown.
    LoggedOut,
    /// The Lightning Address on the entrant's account.
    Address(String),
    /// The account has no Lightning Address; winners submit an invoice.
    NoAddress,
}

/// The entry form's forecasts: each station's, or why they aren't here yet.
#[derive(Debug, Clone)]
pub enum Forecasts {
    Ready {
        stations: Vec<StationForecast>,
        pins: Vec<StationPin>,
    },
    Pending(Pending),
}

/// The network fee the form shows: what a ticket issued now would carry. A ticket's own fee
/// replaces it once the ticket is issued (`entry_form.js`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkFee {
    Estimate(u64),
    /// No ticket is issued while the fee is this high a share of the entry.
    Paused(u64),
    /// No fee estimate, so no ticket either.
    Unavailable,
}

impl NetworkFee {
    fn sats(self) -> Option<u64> {
        match self {
            NetworkFee::Estimate(fee) | NetworkFee::Paused(fee) => Some(fee),
            NetworkFee::Unavailable => None,
        }
    }
}

/// Entry form for a competition.
pub fn entry_form(
    competition: &CompetitionView,
    forecasts: &Forecasts,
    terms: Option<&PayoutTermsQuote>,
    destination: &PayoutDestination,
    network_fee: NetworkFee,
) -> Markup {
    let paused = matches!(network_fee, NetworkFee::Paused(_));
    let picks_allowed = competition.number_of_values_per_entry;
    let queue = competition.queue.queued();
    let pickable = match forecasts {
        Forecasts::Ready { stations, .. } => stations
            .iter()
            .flat_map(|station| &station.forecasts)
            .filter(|(_, forecast, _)| forecast.is_some())
            .count(),
        Forecasts::Pending(_) => 0,
    };
    html! {
        div id="entryContainer" class="entry-form" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }
            h1 class="title is-4" { "Enter this competition" }

            dl class="entry-facts" {
                div {
                    dt { "Entries close" (tip("Picks lock then, and the readings count from that moment on.")) }
                    dd { (format::time(competition.start, TimeStyle::DateTime)) }
                }
                div {
                    dt { "Price" }
                    dd { (price(competition, network_fee)) }
                }
                div {
                    dt {
                        "Win"
                        @if queue.is_some() {
                            (tip("What a pool's winner takes; it grows as more players enter."))
                        }
                    }
                    dd {
                        (competition.win())
                        @if queue.is_none() && competition.paid_places > 1 {
                            span class="fact-note" { "for 1st; top " (competition.paid_places) " paid" }
                        }
                    }
                }
                div {
                    dt {
                        "Entries"
                        @if let Some(queue) = queue {
                            (tip(&format!("{}.", queue.pool_note())))
                        }
                    }
                    dd { (competition.entries()) }
                }
                @if picks_allowed < pickable {
                    div {
                        dt { "Picks" }
                        dd { "up to " (picks_allowed) }
                    }
                }
                // One entry per player needs no line; only a competition allowing more says so.
                @if competition.max_entries_per_player > 1 {
                    div {
                        dt { "Per player" }
                        dd { "up to " (competition.max_entries_per_player) " entries" }
                    }
                }
            }

            p class="how-to-pick" {
                a href="/help#scoring" target="_blank" rel="noopener" { "How scoring works" }
            }

            // What the wallet checks the entry's terms against: what this form shows.
            form id="entryForm" data-competition-id=(competition.id)
                 data-entry-fee=(competition.entry_fee)
                 data-ticket-price=(competition.ticket_price)
                 data-network-fee=[network_fee.sats()]
                 data-total-pool=(competition.total_pool)
                 data-winner-count=(competition.paid_places)
                 data-max-values=(picks_allowed)
                 data-kind=[queue.map(|_| "queued")]
                 data-pool-min-players=[queue.and_then(|queue| queue.min_players)]
                 data-pool-max-players=[queue.map(|queue| queue.max_players)] {
                (forecast_choices(&competition.id, forecasts, 0))
            }

            (payout_line(&competition.id, terms, destination))

            details class="entry-advanced" {
                summary { "Advanced: how the entry is held and paid" }
                ul {
                    li {
                        "Your picks and ticket are locked into a Bitcoin contract with the other entries"
                        @if queue.is_some() { " in your pool" }
                        ". "
                        "A Keymeld enclave signs it for you, so you don't need to stay online."
                    }
                    @if let Some(terms) = terms {
                        li {
                            "If the result is never settled cooperatively, entrants can reclaim funds on-chain after "
                            (terms.relative_locktime_block_delta) " blocks (about "
                            (format::duration(time::Duration::minutes(i64::from(terms.relative_locktime_block_delta) * 10)))
                            ")."
                        }
                        li { "On-chain fees for the contract are capped at " (terms.max_fee_rate_sat_vb) " sat/vB." }
                        li {
                            "Winner shares by rank: "
                            @for (place, (percent, _)) in competition.prizes().iter().enumerate() {
                                @if place > 0 { ", " }
                                (percent) "%"
                            }
                            ". A tie at the last paid place is broken the way the oracle ranks entries."
                        }
                        @if !terms.enabled {
                            li {
                                "This older competition has no payout escrow: collecting winnings by invoice "
                                "reveals the entry's keys before payment, and payment is not guaranteed."
                            }
                        }
                    }
                    // Filled from the WASM build's pinned enclave measurements once it loads.
                    li id="keymeldTrust" class="is-hidden" {}
                }
            }

            div class="entry-submit" {
                @if paused {
                    div id="entriesPaused" class="notification is-warning" {
                        (ENTRIES_PAUSED) ". Entries already taken are unaffected; check back later."
                    }
                }
                button type="button" id="submitEntry" class="button is-primary is-medium"
                       disabled[paused] {
                    @match (paused, network_fee.sats()) {
                        (true, _) => { "Entries paused" }
                        (false, Some(fee)) => { "Pay " (sats(competition.ticket_price + fee)) " and enter" }
                        (false, None) => { "Pay and enter" }
                    }
                }
                div id="successMessage" class="notification is-success hidden" {
                    "You're in. "
                    a href="/entries" hx-get="/entries" hx-target="#main-content" hx-push-url="true" { "See your entries" }
                }
                div id="errorMessage" class="notification is-danger hidden" {}
            }
        }
    }
}

/// The ticket's price: its total, which opens to the entry, service and network fees. The
/// total and the network fee carry ids, so the ticket's own fee can replace the estimate once
/// the ticket is issued (`entry_form.js`).
fn price(competition: &CompetitionView, network_fee: NetworkFee) -> Markup {
    let network_fee = network_fee.sats();
    let service = competition
        .ticket_price
        .saturating_sub(competition.entry_fee);
    html! {
        details class="price-details" {
            summary {
                span id="ticketTotal" {
                    @match network_fee {
                        Some(fee) => (sats(competition.ticket_price + fee)),
                        None => { (sats(competition.ticket_price)) " + network fee" }
                    }
                }
            }
            ul class="price-lines" {
                li { "Entry fee " span { (sats(competition.entry_fee)) } }
                @if service > 0 {
                    li { "Service fee (" (competition.service_fee_percent) ") " span { (sats(service)) } }
                }
                @if network_fee != Some(0) {
                    li {
                        "Network fee "
                        span id="networkFee" {
                            @match network_fee {
                                Some(fee) => (sats(fee)),
                                None => "unavailable right now",
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Where the entry form's forecasts load from while they are on their way:
/// public, so it loads again without a signature and without touching the
/// payout line or picks already made.
pub fn forecasts_url(competition_id: &str) -> String {
    format!("/competitions/{competition_id}/entry-forecasts")
}

/// The picks, a map of the stations and Over / Par / Under for each forecast;
/// or, while the forecasts are not here, a placeholder that loads them (see
/// `fragments::loading`). `asked`: how many times the placeholder has asked.
pub fn forecast_choices(competition_id: &str, forecasts: &Forecasts, asked: u8) -> Markup {
    match forecasts {
        Forecasts::Ready { stations, pins } => html! {
            div id="entryForecasts" {
                @if !pins.is_empty() { (station_map(pins)) }
                @for station in stations { (station_picks(station)) }
            }
        },
        Forecasts::Pending(pending) => placeholder(
            "entryForecasts",
            &forecasts_url(competition_id),
            "forecasts",
            *pending,
            asked,
        ),
    }
}

/// Where winnings and refunds go, as one line. Refreshed when the user logs
/// in or out without touching the picks already made.
pub fn payout_line(
    competition_id: &str,
    terms: Option<&PayoutTermsQuote>,
    destination: &PayoutDestination,
) -> Markup {
    let refunds = terms.is_some_and(|terms| terms.arkade);
    let url = format!("/competitions/{competition_id}/entry-form/payout");
    html! {
        p id="entryPayoutDestination" class="payout-line"
          hx-get=(url) hx-trigger="fw:login from:body, fw:logout from:body" hx-swap="outerHTML" {
            @match (terms.map(|terms| terms.enabled), destination) {
                (Some(false), _) => {
                    "Winners of this competition submit a Lightning invoice after the result."
                }
                (_, PayoutDestination::LoggedOut) => { "Log in to enter." }
                (_, PayoutDestination::Address(address)) => {
                    "Winnings"
                    @if refunds { " and refunds" }
                    " go to " strong { (address) } "."
                }
                (_, PayoutDestination::NoAddress) => {
                    "No Lightning Address on your account: "
                    a href="/payouts" hx-get="/payouts" hx-target="#main-content" hx-push-url="true" { "add one" }
                    " to be paid automatically, or submit an invoice to collect winnings."
                }
            }
        }
    }
}

fn station_picks(station: &StationForecast) -> Markup {
    html! {
        fieldset class="station-picks" id=(format!("station-{}", station.station_id)) data-station=(station.station_id) {
            legend {
                @if let Some(name) = &station.station_name {
                    (name) " "
                }
                span class="station-code" { (station.station_id) }
            }
            @for (metric, forecast, rule) in &station.forecasts {
                (pick_row(&station.station_id, *metric, *forecast, *rule))
            }
        }
    }
}

/// How a ticket's payment stands, as the payment dialog shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketProgress {
    Waiting,
    Paid,
    Failed(&'static str),
}

impl From<&TicketStatus> for TicketProgress {
    fn from(status: &TicketStatus) -> Self {
        match status {
            TicketStatus::Created | TicketStatus::Reserved => TicketProgress::Waiting,
            TicketStatus::Paid | TicketStatus::Settled => TicketProgress::Paid,
            TicketStatus::Expired => {
                TicketProgress::Failed("Ticket payment expired. Please request a new ticket.")
            }
            TicketStatus::Used => TicketProgress::Failed("Ticket has already been used."),
            TicketStatus::Cancelled => TicketProgress::Failed("Competition has been cancelled."),
        }
    }
}

impl TicketProgress {
    /// The `HX-Trigger` header that tells `entry_form.js` the payment is
    /// over, once it is.
    pub fn event(self) -> Option<String> {
        match self {
            TicketProgress::Waiting => None,
            TicketProgress::Paid => Some("fw:ticket-paid".into()),
            TicketProgress::Failed(message) => {
                Some(serde_json::json!({ "fw:ticket-failed": { "message": message } }).to_string())
            }
        }
    }
}

/// The payment dialog's status line. While the ticket is unpaid it asks
/// again every 2 s, replacing itself; once paid or failed it stops. A tick
/// while a request is still out (a Nostr extension waiting for approval, say)
/// is dropped, so no request is left queued to run after the paid answer.
pub fn ticket_status(url: &str, progress: TicketProgress) -> Markup {
    html! {
        @match progress {
            TicketProgress::Waiting => {
                div id="paymentStatus" class="mt-4" hx-get=(url) hx-trigger="every 2s" hx-sync="drop"
                    hx-swap="outerHTML" {
                    p class="has-text-info" { "Waiting for payment..." }
                    progress class="progress is-info" max="100" {}
                }
            }
            TicketProgress::Paid => {
                div id="paymentStatus" class="mt-4" { p class="has-text-success" { "Payment received!" } }
            }
            TicketProgress::Failed(message) => {
                div id="paymentStatus" class="mt-4" { p class="has-text-danger" { (message) } }
            }
        }
    }
}

/// Under / Par / Over for one forecast, as radio buttons named `KPWM_temp_high`, none
/// checked at first; choosing the checked one again takes the pick back (`entry_form.js`). With a Par
/// band the buttons show the ranges themselves: `< 67.4°F`, `67.4–70.2°F`, `> 70.2°F`.
pub(crate) fn pick_row(
    station_id: &str,
    metric: Metric,
    forecast: Option<f64>,
    rule: Option<Rule>,
) -> Markup {
    let name = format!("{station_id}_{}", metric.id());
    let band = match (forecast, rule) {
        (Some(value), Some(Rule::Line { lower, upper })) => Some((value + lower, value + upper)),
        _ => None,
    };
    let options = [("under", "Under"), ("par", "Par"), ("over", "Over")].map(|(value, word)| {
        let text = match band {
            Some((low, _)) if value == "under" => format!("< {}", metric.bound(low)),
            Some((low, high)) if value == "par" => metric.range(low, high),
            Some((_, high)) => format!("> {}", metric.bound(high)),
            None => word.to_owned(),
        };
        (value, word, text)
    });
    html! {
        div class="pick-row" {
            span class="pick-metric" {
                (metric.label())
                @match forecast {
                    Some(value) => { " " strong class="pick-forecast" { (metric.value(value)) } }
                    None => { " " span class="pick-forecast is-missing" { "no forecast yet" } }
                }
            }
            div class="pick-options" role="radiogroup" aria-label=(format!("{} at {station_id}", metric.label())) {
                @for (value, word, text) in &options {
                    label class="pick-option" title=[band.map(|_| word)] {
                        input type="radio" name=(name) value=(value) disabled[forecast.is_none()]
                              aria-label=[band.map(|_| format!("{word} ({text})"))];
                        span { (text) }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_polls_until_it_is_paid_or_fails() {
        let url = "/competitions/c1/tickets/t1/status";
        let waiting =
            ticket_status(url, TicketProgress::from(&TicketStatus::Reserved)).into_string();
        assert!(waiting.contains(
            r#"hx-get="/competitions/c1/tickets/t1/status" hx-trigger="every 2s" hx-sync="drop""#
        ));
        assert_eq!(TicketProgress::Waiting.event(), None);

        let paid = TicketProgress::from(&TicketStatus::Settled);
        assert!(!ticket_status(url, paid).into_string().contains("hx-get"));
        assert_eq!(paid.event().as_deref(), Some("fw:ticket-paid"));

        let expired = TicketProgress::from(&TicketStatus::Expired);
        let html = ticket_status(url, expired).into_string();
        assert!(!html.contains("hx-get") && html.contains("expired"));
        let event: serde_json::Value = serde_json::from_str(&expired.event().unwrap()).unwrap();
        assert!(event["fw:ticket-failed"]["message"]
            .as_str()
            .unwrap()
            .contains("expired"));
    }
    use crate::domain::{leaderboard::Phase, WindowShape};
    use crate::infra::oracle::ScoringRules;
    use crate::templates::pages::competitions::tests::view;

    fn terms(arkade: bool) -> PayoutTermsQuote {
        PayoutTermsQuote {
            enabled: true,
            relative_locktime_block_delta: 2880,
            max_fee_rate_sat_vb: 100,
            arkade,
        }
    }

    fn station() -> StationForecast {
        StationForecast {
            station_id: "KPWM".into(),
            station_name: Some("Portland International, ME".into()),
            forecasts: vec![
                (Metric::TempHigh, Some(69.0), Some(Rule::Fixed)),
                (Metric::TempLow, Some(41.0), Some(Rule::Fixed)),
                (Metric::WindSpeed, Some(7.0), Some(Rule::Fixed)),
            ],
        }
    }

    fn form(destination: PayoutDestination) -> String {
        entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Ready {
                stations: vec![station()],
                pins: vec![],
            },
            Some(&terms(true)),
            &destination,
            NetworkFee::Estimate(50),
        )
        .into_string()
    }

    #[test]
    fn entry_form_shows_the_ticket_price_the_browser_approves() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains(r#"data-ticket-price="5250""#));
        assert!(html.contains(r#"data-network-fee="50""#));
        assert!(html.contains(r#"<span id="ticketTotal">5,300 sats</span>"#));
        assert!(html.contains("Pay 5,300 sats and enter"));
    }

    /// The price is one total, "what it costs me"; tapping it opens the entry, service and
    /// network fees. The total and the network fee carry ids for the ticket's own fee.
    #[test]
    fn entry_form_shows_one_price_that_opens_to_its_fees() {
        let html = form(PayoutDestination::LoggedOut);
        let details = html.find(r#"<details class="price-details">"#).unwrap();
        let total = html
            .find(r#"<span id="ticketTotal">5,300 sats</span>"#)
            .unwrap();
        let lines = html.find(r#"<ul class="price-lines">"#).unwrap();
        assert!(details < total && total < lines, "the total is the summary");
        assert!(html.contains("Entry fee <span>5,000 sats</span>"));
        assert!(html.contains("Service fee (5%) <span>250 sats</span>"));
        assert!(html.contains(r#"Network fee <span id="networkFee">50 sats</span>"#));
        assert!(!html.contains(">Ticket<") && !html.contains(">Pot<"));

        // Without an estimate no price is claimed for it, and no ticket can be issued either.
        let unavailable = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Pending(Pending::Loading),
            Some(&terms(true)),
            &PayoutDestination::LoggedOut,
            NetworkFee::Unavailable,
        )
        .into_string();
        assert!(unavailable.contains("5,250 sats + network fee"));
        assert!(unavailable.contains(r#"<span id="networkFee">unavailable right now</span>"#));
        assert!(!unavailable.contains("data-network-fee"));
        assert!(unavailable.contains("Pay and enter"));

        // With network fees off there is no line for one.
        let off = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Pending(Pending::Loading),
            Some(&terms(true)),
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(0),
        )
        .into_string();
        assert!(off.contains(r#"<span id="ticketTotal">5,250 sats</span>"#));
        assert!(!off.contains("networkFee"));
        assert!(off.contains("Pay 5,250 sats and enter"));
    }

    /// What winning pays sits beside the price: first place's prize.
    #[test]
    fn entry_form_shows_what_first_place_wins() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains("<dt>Win</dt><dd>15,000 sats</dd>"), "{html}");
        let mut two = view("c1", Phase::Upcoming, 60);
        two.paid_places = 2;
        let html = entry_form(
            &two,
            &Forecasts::Pending(Pending::Loading),
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
        )
        .into_string();
        assert!(html.contains("10,500 sats") && html.contains("for 1st; top 2 paid"));
    }

    #[test]
    fn only_a_competition_allowing_several_entries_per_player_says_so() {
        assert!(!form(PayoutDestination::LoggedOut).contains("Per player"));
        let mut competition = view("c1", Phase::Upcoming, 60);
        competition.max_entries_per_player = 3;
        let html = entry_form(
            &competition,
            &Forecasts::Pending(Pending::Loading),
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
        )
        .into_string();
        assert!(html.contains("up to 3 entries"));
    }

    /// The deadline is a fact of its own, and how scoring works is a link to the help page
    /// rather than a paragraph on the form.
    #[test]
    fn entry_form_shows_the_deadline_and_links_the_rules() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains("Entries close"));
        assert!(
            !html.contains("Window"),
            "the deadline is the one time the form needs"
        );
        assert!(html.contains(r#"href="/help#scoring""#));
        assert!(!html.contains("scores 20 points"));
        assert!(!html.contains("Pick as many readings"));
    }

    /// While entries are paused the form says so before any picks are made, and cannot be
    /// submitted.
    #[test]
    fn entry_form_says_when_entries_are_paused() {
        let html = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Pending(Pending::Loading),
            Some(&terms(true)),
            &PayoutDestination::LoggedOut,
            NetworkFee::Paused(600),
        )
        .into_string();
        assert!(html.contains(ENTRIES_PAUSED));
        assert!(html.contains(r#"id="submitEntry" class="button is-primary is-medium" disabled"#));
        assert!(html.contains("Entries paused"));
        assert!(!html.contains("and enter"));
    }

    #[test]
    fn picks_show_airport_names_and_the_oracle_forecast() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains("Portland International, ME"));
        assert!(!html.contains("Station KPWM"));
        assert!(html.contains("69°F") && html.contains("41°F") && html.contains("7 knots"));
        assert!(!html.contains("12.5") && !html.contains("75°F") && !html.contains("58°F"));
        assert!(html.contains(r#"name="KPWM_temp_high" value="over""#));
    }

    #[test]
    fn entering_is_the_consent_with_one_line_about_refunds() {
        let html = form(PayoutDestination::Address("freya@lnurl.example".into()));
        assert!(!html.contains(r#"type="checkbox""#));
        assert!(!html.contains("I authorize"));
        assert!(html.contains("Winnings and refunds go to <strong>freya@lnurl.example</strong>."));
        assert_eq!(html.matches("refund").count(), 1, "one line about refunds");
    }

    /// A queue's form shows how many entered and the pool size, and carries what the wallet
    /// checks the entry's terms against. Entering is still the only consent.
    #[test]
    fn a_queue_form_shows_entries_and_pools_and_carries_them_for_the_wallet() {
        use crate::templates::pages::competitions::tests::queued;
        let html = entry_form(
            &queued("q", 40),
            &Forecasts::Ready {
                stations: vec![station()],
                pins: vec![],
            },
            Some(&terms(true)),
            &PayoutDestination::Address("freya@lnurl.example".into()),
            NetworkFee::Estimate(50),
        )
        .into_string();
        assert!(html.contains("40 entered"));
        // How pools work is a tooltip, not a line on the form.
        assert!(
            html.contains(r#"data-tip="Players are split into pools of up to 25 at the start.""#)
        );
        // Forty entries make two pools of twenty: each winner takes 100,000 sats.
        assert!(html.contains("<dd>100,000 sats</dd>"), "{html}");
        assert!(html.contains("pool&#39;s winner") || html.contains("pool's winner"));
        assert!(!html.contains(" of 3"));
        assert!(html.contains(r#"data-kind="queued""#));
        assert!(html.contains(r#"data-pool-min-players="2""#));
        assert!(html.contains(r#"data-pool-max-players="25""#));
        assert!(html.contains(r#"data-entry-fee="5000""#));
        assert!(html.contains("with the other entries in your pool."));
        assert!(!html.contains(r#"type="checkbox""#));
        assert_eq!(html.matches("refund").count(), 1, "one line about refunds");

        let single = form(PayoutDestination::LoggedOut);
        assert!(!single.contains("data-kind") && !single.contains("data-pool-"));
    }

    #[test]
    fn a_logged_out_visitor_is_asked_to_log_in_and_nothing_more() {
        let line =
            payout_line("c1", Some(&terms(false)), &PayoutDestination::LoggedOut).into_string();
        assert!(line.contains("Log in to enter."));
        assert!(!line.contains("returns to your wallet"));
    }

    #[test]
    fn contract_jargon_sits_under_advanced_in_plain_words() {
        let html = form(PayoutDestination::NoAddress);
        let advanced = html.find("<details").unwrap();
        let delay = html.find("2880 blocks").unwrap();
        assert!(delay > advanced);
        assert!(html.contains("about 20 days"));
        assert!(html.contains("capped at 100 sat/vB"));
        // One paid place: the winner takes it all.
        assert!(html.contains("Winner shares by rank: 100%"));
        assert!(html.contains(">add one</a>"));
    }

    /// With a Par band the buttons are the ranges, low to high, and still say which pick each is.
    #[test]
    fn lines_competitions_put_the_ranges_in_the_pick_buttons() {
        let mut competition = view("c1", Phase::Upcoming, 60);
        competition.scoring_rules = ScoringRules::Lines;
        let mut station = station();
        station.forecasts = vec![
            (
                Metric::TempHigh,
                Some(69.0),
                Some(Rule::Line {
                    lower: -1.6,
                    upper: 1.2,
                }),
            ),
            (Metric::WindSpeed, Some(7.0), None),
        ];
        let html = entry_form(
            &competition,
            &Forecasts::Ready {
                stations: vec![station],
                pins: vec![],
            },
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
        )
        .into_string();
        let under = html.find("&lt; 67.4°F").expect("under button");
        let par = html.find("67.4–70.2°F").expect("par button");
        let over = html.find("&gt; 70.2°F").expect("over button");
        assert!(under < par && par < over);
        assert!(html.contains(r#"aria-label="Par (67.4–70.2°F)""#));
        assert!(!html.contains("pick-par") && !html.contains("lately"));
        // Without a band yet the buttons say Over, Par, Under.
        assert!(html.contains("<span>Over</span>"));
    }

    #[test]
    fn a_day_competition_offers_highs_and_wind_only() {
        let mut competition = view("c1", Phase::Upcoming, 60);
        competition.window_shape = Some(WindowShape::Day);
        let mut station = station();
        station.forecasts = vec![
            (Metric::TempHigh, Some(69.0), Some(Rule::Fixed)),
            (Metric::WindSpeed, Some(7.0), Some(Rule::Fixed)),
        ];
        let html = entry_form(
            &competition,
            &Forecasts::Ready {
                stations: vec![station],
                pins: vec![],
            },
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
        )
        .into_string();
        assert!(html.contains("KPWM_temp_high") && html.contains("KPWM_wind_speed"));
        assert!(!html.contains("KPWM_temp_low"));
    }

    #[test]
    fn a_missing_forecast_disables_its_picks() {
        let mut station = station();
        station.forecasts = vec![
            (Metric::TempHigh, Some(70.0), Some(Rule::Fixed)),
            (Metric::WindSpeed, None, Some(Rule::Fixed)),
        ];
        let forecasts = Forecasts::Ready {
            stations: vec![station],
            pins: vec![],
        };
        let html = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &forecasts,
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
        )
        .into_string();
        assert!(html.contains("no forecast yet"));
        assert!(html.contains("disabled"));
    }

    #[test]
    fn forecasts_on_their_way_load_on_their_own_without_touching_the_form() {
        let loading =
            forecast_choices("c1", &Forecasts::Pending(Pending::Loading), 0).into_string();
        assert!(loading.contains("Still loading forecasts"));
        assert!(loading.contains(r#"id="entryForecasts""#));
        assert!(loading.contains("/competitions/c1/entry-forecasts?again=1"));
        assert!(!loading.contains("type=\"radio\""));
        assert!(!loading.contains("entryPayoutDestination"));
        let failed =
            forecast_choices("c1", &Forecasts::Pending(Pending::Unavailable), 0).into_string();
        assert!(failed.contains("unavailable right now"));
        assert!(failed.contains(">Retry</button>"));
        assert!(!failed.contains("hx-trigger"));
    }
}
