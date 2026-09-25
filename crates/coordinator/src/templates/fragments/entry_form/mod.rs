//! The entry form: the competition's terms, a pick for each forecast, and the
//! button that pays for the ticket. Picks are plain radio buttons; paying needs
//! the WASM wallet, so `entry_form.js` handles the submit.

use maud::{html, Markup};

use crate::domain::{
    leaderboard::{
        progress::{OVER_OR_UNDER_POINTS, PAR_POINTS},
        Metric,
    },
    PayoutTermsQuote, TicketStatus,
};
use crate::templates::{
    format::{self, ordinal, sats, MetricText},
    fragments::loading::{placeholder, Pending},
    pages::competitions::{fee_breakdown, CompetitionView},
    shared_map::{station_map, StationPin},
};

/// One station's forecasts, the values its picks are judged against.
#[derive(Debug, Clone)]
pub struct StationForecast {
    pub station_id: String,
    /// `Portland International, ME`, when the oracle knows the station.
    pub station_name: Option<String>,
    pub forecasts: Vec<(Metric, Option<f64>)>,
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

/// Entry form for a competition
pub fn entry_form(
    competition: &CompetitionView,
    forecasts: &Forecasts,
    terms: Option<&PayoutTermsQuote>,
    destination: &PayoutDestination,
) -> Markup {
    let picks_allowed = competition.number_of_values_per_entry;
    let pickable = match forecasts {
        Forecasts::Ready { stations, .. } => stations
            .iter()
            .flat_map(|station| &station.forecasts)
            .filter(|(_, forecast)| forecast.is_some())
            .count(),
        Forecasts::Pending(_) => 0,
    };
    html! {
        div id="entryContainer" class="entry-form" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }
            h1 class="title is-4" { "Enter this competition" }

            dl class="entry-facts" {
                div { dt { "Window" } dd { (format::window(competition.start, competition.end)) } }
                div {
                    dt { "Ticket" }
                    dd {
                        (sats(competition.ticket_price))
                        @if let Some(breakdown) = fee_breakdown(competition) {
                            span class="fact-note" { (breakdown) }
                        }
                    }
                }
                div {
                    dt { "Pot" }
                    dd {
                        (sats(competition.total_pool))
                        span class="fact-note" {
                            @for (place, (percent, _)) in competition.prizes().iter().enumerate() {
                                @if place > 0 { ", " }
                                (ordinal(place + 1)) " " (percent) "%"
                            }
                        }
                    }
                }
                div { dt { "Entries" } dd { (competition.total_entries) " of " (competition.total_allowed_entries) } }
            }

            p class="how-to-pick" {
                "For each reading, pick whether it will come in over the forecast, under it, or on it. "
                strong { "Par" } " means the reading matches the forecast exactly (to the whole degree for temperatures) and scores "
                (PAR_POINTS) " points; a correct over or under scores " (OVER_OR_UNDER_POINTS) "."
                @if picks_allowed < pickable {
                    " Make up to " (picks_allowed) " picks."
                }
            }

            form id="entryForm" data-competition-id=(competition.id)
                 data-entry-fee=(competition.entry_fee)
                 data-ticket-price=(competition.ticket_price)
                 data-total-pool=(competition.total_pool)
                 data-winner-count=(competition.paid_places)
                 data-max-values=(picks_allowed) {
                (forecast_choices(&competition.id, forecasts, 0))
            }

            (payout_line(&competition.id, terms, destination))

            details class="entry-advanced" {
                summary { "Advanced: how the entry is held and paid" }
                ul {
                    li {
                        "Your picks and ticket are locked into a Bitcoin contract with the other entries. "
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
                button type="button" id="submitEntry" class="button is-primary is-medium" {
                    "Pay " (sats(competition.ticket_price)) " and enter"
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
                (_, PayoutDestination::LoggedOut) => {
                    "Log in to enter. Winnings"
                    @if refunds { " and refunds" }
                    " go to the Lightning Address on your account."
                }
                (_, PayoutDestination::Address(address)) => {
                    "Winnings"
                    @if refunds { ", and your refund if this competition doesn't fill," }
                    " go to " strong { (address) } "."
                }
                (_, PayoutDestination::NoAddress) => {
                    "Your account has no Lightning Address, so you would submit an invoice to collect winnings. "
                    a href="/payouts" hx-get="/payouts" hx-target="#main-content" hx-push-url="true" { "Add one" }
                    " to be paid automatically."
                }
            }
            // Without an Arkade escrow the ticket is a held invoice, which is
            // cancelled rather than collected if the competition doesn't fill.
            @if !refunds {
                " If this competition doesn't fill, your payment is never collected and returns to your wallet."
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
            @for (metric, forecast) in &station.forecasts {
                (pick_row(&station.station_id, *metric, *forecast))
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

/// Over / Par / Under for one forecast, as radio buttons named `KPWM_temp_high`,
/// with "No pick" (checked at first) to leave or take back a pick.
fn pick_row(station_id: &str, metric: Metric, forecast: Option<f64>) -> Markup {
    let name = format!("{station_id}_{}", metric.id());
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
                @for (value, label) in [("over", "Over"), ("par", "Par"), ("under", "Under")] {
                    label class="pick-option" {
                        input type="radio" name=(name) value=(value) disabled[forecast.is_none()];
                        span { (label) }
                    }
                }
                label class="pick-option is-none" {
                    input type="radio" name=(name) value="" checked;
                    span { "No pick" }
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
    use crate::domain::leaderboard::Phase;
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
                (Metric::TempHigh, Some(69.0)),
                (Metric::TempLow, Some(41.0)),
                (Metric::WindSpeed, Some(7.0)),
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
        )
        .into_string()
    }

    #[test]
    fn entry_form_shows_the_ticket_price_the_browser_approves() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains(r#"data-ticket-price="5250""#));
        assert!(html.contains("5,250 sats"));
        assert!(html.contains("5,000 entry fee + 250 coordinator fee"));
        assert!(html.contains("Pay 5,250 sats and enter"));
    }

    #[test]
    fn picks_show_airport_names_and_the_oracle_forecast() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains("Portland International, ME"));
        assert!(!html.contains("Station KPWM"));
        assert!(html.contains("69°F") && html.contains("41°F") && html.contains("7 knots"));
        assert!(!html.contains("12.5") && !html.contains("75°F") && !html.contains("58°F"));
        assert!(html.contains(r#"name="KPWM_temp_high" value="over""#));
        assert!(html.contains("matches the forecast exactly"));
    }

    #[test]
    fn entering_is_the_consent_with_one_line_about_refunds() {
        let html = form(PayoutDestination::Address("freya@lnurl.example".into()));
        assert!(!html.contains(r#"type="checkbox""#));
        assert!(!html.contains("I authorize"));
        assert!(
            html.contains("and your refund if this competition doesn&#39;t fill")
                || html.contains("and your refund if this competition doesn't fill")
        );
        assert!(html.contains("<strong>freya@lnurl.example</strong>"));
    }

    #[test]
    fn a_held_invoice_says_the_payment_returns_if_the_competition_does_not_fill() {
        let line = |arkade| {
            payout_line(
                "c1",
                Some(&terms(arkade)),
                &PayoutDestination::Address("freya@lnurl.example".into()),
            )
            .into_string()
        };
        assert!(line(false).contains("returns to your wallet"));
        assert!(!line(true).contains("returns to your wallet"));
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
        assert!(html.contains("Add one"));
    }

    #[test]
    fn a_missing_forecast_disables_its_picks() {
        let mut station = station();
        station.forecasts = vec![(Metric::TempHigh, Some(70.0)), (Metric::WindSpeed, None)];
        let forecasts = Forecasts::Ready {
            stations: vec![station],
            pins: vec![],
        };
        let html = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &forecasts,
            None,
            &PayoutDestination::LoggedOut,
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
