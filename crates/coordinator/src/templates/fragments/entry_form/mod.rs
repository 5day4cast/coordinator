//! The entry form: the competition's terms, a pick for each forecast, and the
//! button that pays for the ticket. Picks are plain radio buttons; paying needs
//! the WASM wallet, so `entry_form.js` handles the submit.

use maud::{html, Markup};

use crate::domain::{
    leaderboard::{Metric, Rule},
    PaidTicket, PayoutTermsQuote, TicketStatus, UnpaidTicket, ARKADE_UNAVAILABLE, ENTRIES_PAUSED,
    SETTLE_ONLY_PAUSED,
};
use crate::templates::{
    components::{tip, tip_start},
    format::{self, city_name, sats, MetricText, TimeStyle},
    fragments::entries_paused_banner,
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
    /// No ticket for this Arkade competition is issued while the Arkade server is failing
    /// batch steps.
    ArkadeUnavailable(u64),
    /// No fee estimate, so no ticket either.
    Unavailable,
    /// The coordinator takes no new entries (settle-only mode).
    SettleOnly,
}

impl NetworkFee {
    fn sats(self) -> Option<u64> {
        match self {
            NetworkFee::Estimate(fee)
            | NetworkFee::Paused(fee)
            | NetworkFee::ArkadeUnavailable(fee) => Some(fee),
            NetworkFee::Unavailable | NetworkFee::SettleOnly => None,
        }
    }
}

/// Entry form for a competition. `unpaid` is the logged-in player's oldest unpaid ticket in it,
/// which Pay resumes, and `paid` their first paid ticket whose entry never went in, which Pay
/// enters without paying again (`entry_form.js`).
pub fn entry_form(
    competition: &CompetitionView,
    forecasts: &Forecasts,
    terms: Option<&PayoutTermsQuote>,
    destination: &PayoutDestination,
    network_fee: NetworkFee,
    unpaid: Option<&UnpaidTicket>,
    paid: Option<&PaidTicket>,
) -> Markup {
    // Why no ticket is issued now, in the one sentence the player sees.
    let paused = match network_fee {
        NetworkFee::Paused(_) => Some(format!(
            "{ENTRIES_PAUSED}. Entries already taken are unaffected; check back later."
        )),
        NetworkFee::ArkadeUnavailable(_) => Some(format!("{ARKADE_UNAVAILABLE}.")),
        NetworkFee::SettleOnly => Some(format!("{SETTLE_ONLY_PAUSED}.")),
        NetworkFee::Estimate(_) | NetworkFee::Unavailable => None,
    };
    // Settle-only mode says so in a banner at the top instead of beside the button.
    let banner = network_fee == NetworkFee::SettleOnly;
    let picks_allowed = competition.number_of_values_per_entry;
    // A row is one metric at one station; a competition may take fewer picks than it has rows.
    let rows = (competition.locations.len() * competition.metrics.len()).max(picks_allowed);
    let queue = competition.queue.queued();
    html! {
        div id="entryContainer" class="entry-form" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }
            h1 class="title is-4" { "Enter this competition" }
            @if banner { (entries_paused_banner()) }

            dl class="entry-facts" {
                div {
                    dt { "Entries close" (tip_start("Picks lock then, and the readings count from that moment on.")) }
                    dd { (format::zoned_time(competition.start, TimeStyle::Weekday)) }
                }
                div {
                    dt { "Entry fee" }
                    dd { (price(competition, network_fee)) }
                }
                div {
                    dt {
                        "Prizes"
                        @if queue.is_some() {
                            (tip("What a pool's winner takes; it grows as more players enter."))
                        }
                    }
                    dd {
                        (competition.win())
                        @if let Some(split) = competition.prize_split() {
                            span class="fact-note prize-note" { (split) }
                        }
                        @if let Some(rule) = competition.prize_rule() {
                            span class="fact-note prize-note" { (rule) }
                        }
                        span class="fact-note prize-note" { "Ties go to the earliest entry" }
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
                div {
                    dt { "Picks required" }
                    dd {
                        (picks_allowed)
                        @if picks_allowed < rows { " of " (rows) }
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
                 data-pick-rows=(rows)
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
                (unpaid_notice(&competition.id, unpaid, time::OffsetDateTime::now_utc()))
                (paid_notice(&competition.id, paid, time::OffsetDateTime::now_utc()))
                // How many picks are still to make; `entry_form.js` counts as the player picks.
                p id="picksLeft" class="picks-left" role="status" aria-live="polite" {
                    (picks_to_make(picks_allowed, rows))
                }
                @if let (Some(reason), false) = (&paused, banner) {
                    div id="entriesPaused" class="notification is-warning" { (reason) }
                }
                @let pay_label = match network_fee.sats() {
                    Some(fee) => format!("Pay {} and enter", sats(competition.ticket_price + fee)),
                    None => "Pay and enter".to_string(),
                };
                // A paid ticket is entered without paying again, paused or not. `entry_form.js`
                // relabels the button when the paid notice comes or goes with a log-in; while
                // entries are paused there is no price to go back to.
                button type="button" id="submitEntry" class="button is-primary is-medium"
                       disabled[paused.is_some() && paid.is_none()]
                       data-pay-label=[paused.is_none().then_some(&pay_label)] {
                    @if paid.is_some() {
                        (FINISH_LABEL)
                    } @else if paused.is_some() {
                        "Entries paused"
                    } @else {
                        (pay_label)
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

/// Pay's label for a ticket already paid for.
const FINISH_LABEL: &str = "Enter";

/// Where the paid-entry notice reloads from when the player logs in or out.
pub fn paid_url(competition_id: &str) -> String {
    format!("/competitions/{competition_id}/entry-form/paid")
}

/// The player's paid ticket in this competition whose entry never went in (their page reloaded
/// before it did), if they hold one, and how long is left to finish it: Pay enters the picks on
/// the form under it, without paying again. Empty otherwise, and reloaded on log-in and log-out
/// without touching the picks.
pub fn paid_notice(
    competition_id: &str,
    paid: Option<&PaidTicket>,
    now: time::OffsetDateTime,
) -> Markup {
    html! {
        div id="entryPaid" hx-get=(paid_url(competition_id))
            hx-trigger="fw:login from:body, fw:logout from:body" hx-swap="outerHTML"
            data-ticket-id=[paid.map(|ticket| ticket.ticket_id)]
            data-entry-id=[paid.map(|ticket| ticket.entry_id)]
            data-entry-key=[paid.and_then(|ticket| ticket.ephemeral_pubkey.as_deref())] {
            @if let Some(ticket) = paid {
                p class="notification is-info paid-entry" {
                    "Paid — make your picks to finish entering; "
                    (format::duration(ticket.finish_by - now)) " left."
                }
            }
        }
    }
}

/// Where the unpaid-entry notice reloads from when the player logs in or out.
pub fn unpaid_url(competition_id: &str) -> String {
    format!("/competitions/{competition_id}/entry-form/unpaid")
}

/// The player's unpaid entry in this competition, if they hold one: Pay pays its invoice, with
/// the picks on the form, rather than starting another entry. Empty otherwise, and reloaded on
/// log-in and log-out without touching the picks.
pub fn unpaid_notice(
    competition_id: &str,
    unpaid: Option<&UnpaidTicket>,
    now: time::OffsetDateTime,
) -> Markup {
    html! {
        div id="entryUnpaid" hx-get=(unpaid_url(competition_id))
            hx-trigger="fw:login from:body, fw:logout from:body" hx-swap="outerHTML"
            data-ticket-id=[unpaid.map(|ticket| ticket.ticket_id)] {
            @if let Some(ticket) = unpaid {
                p class="notification is-warning unpaid-entry" {
                    "You have an unpaid entry"
                    @if let Some(expires) = ticket.invoice_expires_at {
                        "; its invoice expires in " (format::duration(expires - now))
                    }
                    ". Make your picks and press Pay to pay it."
                }
            }
        }
    }
}

/// The counter under the picks before any is made; `picksLeft` in `entry_form.js` says the same.
fn picks_to_make(picks: usize, rows: usize) -> String {
    let noun = if picks == 1 { "pick" } else { "picks" };
    if picks < rows {
        format!("{picks} {noun} to make: any {picks} of the {rows} rows.")
    } else {
        format!("{picks} {noun} to make: one in every row.")
    }
}

/// The total the player pays. The issued ticket can replace the estimate through its id.
fn price(competition: &CompetitionView, network_fee: NetworkFee) -> Markup {
    html! {
        span id="ticketTotal" {
            @match network_fee.sats() {
                Some(fee) => (sats(competition.ticket_price + fee)),
                None => "Unavailable right now",
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
                // Says so when Pay is clicked with nothing picked (`entry_form.js`), right above
                // the first pick, which then takes focus.
                p id="picksMessage" class="notification is-danger hidden" role="alert" {}
                p class="city-heading" {
                    strong { "City" }
                    (tip_start("Weather is measured at each city's named airport station. The forecast is NOAA's; the pick ranges use historical forecast errors."))
                }
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
                    " go to " strong { (address) }
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
                    (city_name(name)) " "
                    (tip(&format!("Weather station: {name} ({})", station.station_id)))
                }
                span class="station-code" { (station.station_id) }
            }
            div class="pick-heading" aria-hidden="true" {
                span class="pick-metric" { "NOAA forecast" }
                span { "Your pick" }
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
    pub fn event(self, ticket_id: &str) -> Option<String> {
        match self {
            TicketProgress::Waiting => None,
            TicketProgress::Paid => {
                Some(serde_json::json!({ "fw:ticket-paid": { "ticket_id": ticket_id } }).to_string())
            }
            TicketProgress::Failed(message) => {
                Some(serde_json::json!({ "fw:ticket-failed": { "ticket_id": ticket_id, "message": message } }).to_string())
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
    fn an_unpaid_entry_says_when_its_invoice_expires_and_that_pay_pays_it() {
        let now = time::macros::datetime!(2026-10-06 12:00 UTC);
        let ticket = UnpaidTicket {
            ticket_id: uuid::Uuid::from_u128(7),
            competition_id: uuid::Uuid::from_u128(1),
            invoice_expires_at: Some(now + time::Duration::minutes(42)),
        };
        let html = unpaid_notice("c1", Some(&ticket), now).into_string();
        assert!(html.contains(r#"id="entryUnpaid""#));
        assert!(html.contains(&format!(r#"data-ticket-id="{}""#, ticket.ticket_id)));
        assert!(html.contains("You have an unpaid entry; its invoice expires in 42 min."));
        assert!(html.contains("Make your picks and press Pay to pay it."));
        // Reloaded on log-in and log-out, signed (htmx_auth.js), without touching the picks.
        assert!(html.contains(r#"hx-get="/competitions/c1/entry-form/unpaid""#));
        assert!(html.contains(r#"hx-trigger="fw:login from:body, fw:logout from:body""#));
        assert!(!html.contains("checkbox"), "no extra consent");

        let waiting = UnpaidTicket {
            invoice_expires_at: None,
            ..ticket
        };
        let html = unpaid_notice("c1", Some(&waiting), now).into_string();
        assert!(html.contains("You have an unpaid entry. Make your picks"));

        let none = unpaid_notice("c1", None, now).into_string();
        assert!(
            none.contains(r#"id="entryUnpaid""#),
            "kept for the log-in reload"
        );
        assert!(!none.contains("data-ticket-id") && !none.contains("unpaid entry"));
    }

    #[test]
    fn the_entry_form_carries_the_unpaid_entry_beside_pay() {
        let ticket = UnpaidTicket {
            ticket_id: uuid::Uuid::from_u128(7),
            competition_id: uuid::Uuid::from_u128(1),
            invoice_expires_at: None,
        };
        let html = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Ready {
                stations: vec![station()],
                pins: vec![],
            },
            Some(&terms(true)),
            &PayoutDestination::Address("thor@lnurl.5day4cast.com".into()),
            NetworkFee::Estimate(50),
            Some(&ticket),
            None,
        )
        .into_string();
        let notice = html.find("You have an unpaid entry").unwrap();
        assert!(notice < html.find(r#"id="submitEntry""#).unwrap());
        assert!(!form(PayoutDestination::NoAddress).contains("unpaid entry"));
    }

    #[test]
    fn a_paid_ticket_says_to_make_picks_and_pay_enters_without_paying() {
        let now = time::macros::datetime!(2026-10-06 12:00 UTC);
        let mut ticket = PaidTicket {
            ticket_id: uuid::Uuid::from_u128(7),
            competition_id: uuid::Uuid::from_u128(1),
            entry_id: uuid::Uuid::from_u128(8),
            ephemeral_pubkey: Some("02aa".into()),
            finish_by: now + time::Duration::minutes(42),
        };
        let html = paid_notice("c1", Some(&ticket), now).into_string();
        assert!(html.contains(r#"id="entryPaid""#));
        assert!(html.contains(&format!(r#"data-ticket-id="{}""#, ticket.ticket_id)));
        assert!(html.contains(&format!(r#"data-entry-id="{}""#, ticket.entry_id)));
        assert!(html.contains(r#"data-entry-key="02aa""#));
        assert!(html.contains("Paid — make your picks to finish entering; 42 min left."));
        // Reloaded on log-in and log-out, signed (htmx_auth.js), without touching the picks.
        assert!(html.contains(r#"hx-get="/competitions/c1/entry-form/paid""#));
        assert!(html.contains(r#"hx-trigger="fw:login from:body, fw:logout from:body""#));
        let none = paid_notice("c1", None, now).into_string();
        assert!(
            none.contains(r#"id="entryPaid""#),
            "kept for the log-in reload"
        );
        assert!(!none.contains("data-ticket-id") && !none.contains("Paid —"));

        // The form says how long is left from the time it is rendered.
        ticket.finish_by = time::OffsetDateTime::now_utc() + time::Duration::minutes(42);
        let form = |paid: Option<&PaidTicket>, fee: NetworkFee| {
            entry_form(
                &view("c1", Phase::Upcoming, 60),
                &Forecasts::Ready {
                    stations: vec![station()],
                    pins: vec![],
                },
                Some(&terms(true)),
                &PayoutDestination::Address("thor@lnurl.5day4cast.com".into()),
                fee,
                None,
                paid,
            )
            .into_string()
        };
        let html = form(Some(&ticket), NetworkFee::Estimate(50));
        let notice = html.find("Paid — make your picks").unwrap();
        let button = html.find(r#"id="submitEntry""#).unwrap();
        assert!(notice < button);
        assert!(html[notice..button].contains(" min left."));
        assert!(
            html[button..].contains(">Enter</button>"),
            "nothing more to pay"
        );
        assert!(
            html.contains(r#"data-pay-label="Pay "#),
            "the label to go back to"
        );
        // Paused entries take no new money; a paid ticket is still entered.
        let paused = form(Some(&ticket), NetworkFee::Paused(600));
        let button = paused.find(r#"id="submitEntry""#).unwrap();
        assert!(!paused[button..paused[button..].find('>').unwrap() + button].contains("disabled"));
        assert!(paused[button..].contains(">Enter</button>"));
        let unpaid = form(None, NetworkFee::Estimate(50));
        assert!(!unpaid.contains("Paid — make"));
        assert!(unpaid.contains(" and enter</button>"));
    }

    #[test]
    fn a_ticket_polls_until_it_is_paid_or_fails() {
        let url = "/competitions/c1/tickets/t1/status";
        let waiting =
            ticket_status(url, TicketProgress::from(&TicketStatus::Reserved)).into_string();
        assert!(waiting.contains(
            r#"hx-get="/competitions/c1/tickets/t1/status" hx-trigger="every 2s" hx-sync="drop""#
        ));
        assert_eq!(TicketProgress::Waiting.event("t1"), None);

        let paid = TicketProgress::from(&TicketStatus::Settled);
        assert!(!ticket_status(url, paid).into_string().contains("hx-get"));
        let event: serde_json::Value = serde_json::from_str(&paid.event("t1").unwrap()).unwrap();
        assert_eq!(event["fw:ticket-paid"]["ticket_id"], "t1");

        let expired = TicketProgress::from(&TicketStatus::Expired);
        let html = ticket_status(url, expired).into_string();
        assert!(!html.contains("hx-get") && html.contains("expired"));
        let event: serde_json::Value = serde_json::from_str(&expired.event("t1").unwrap()).unwrap();
        assert_eq!(event["fw:ticket-failed"]["ticket_id"], "t1");
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
            None,
            None,
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
        // At the start of its line, so its bubble grows rightwards and stays on a phone.
        assert!(html.contains(r#"<dt>Entries close<span class="tip tip-start""#));
    }

    #[test]
    fn entry_form_shows_only_the_total_and_does_not_understate_an_unknown_fee() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(html.contains(r#"<dt>Entry fee</dt><dd><span id="ticketTotal">5,300 sats</span>"#));
        assert!(!html.contains("price-details") && !html.contains("Pot contribution"));
        let unavailable = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Pending(Pending::Loading),
            Some(&terms(true)),
            &PayoutDestination::LoggedOut,
            NetworkFee::Unavailable,
            None,
            None,
        )
        .into_string();
        assert!(unavailable.contains(r#"<span id="ticketTotal">Unavailable right now</span>"#));
        assert!(!unavailable.contains("data-network-fee"));
        assert!(unavailable.contains("Pay and enter"));
    }

    /// What winning pays sits beside the price: first place's prize.
    #[test]
    fn entry_form_shows_what_first_place_wins() {
        let html = form(PayoutDestination::LoggedOut);
        assert!(
            html.contains(r#"<dt>Prizes</dt><dd>15,000 sats<span class="fact-note prize-note">Ties go to the earliest entry</span></dd>"#),
            "{html}"
        );
        let mut two = view("c1", Phase::Upcoming, 60);
        two.paid_places = 2;
        let html = entry_form(
            &two,
            &Forecasts::Pending(Pending::Loading),
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
            None,
            None,
        )
        .into_string();
        assert!(html.contains("10,500 sats") && html.contains("1st 70% · 2nd 30%"));
        assert!(!html.contains("winner takes all"));
    }

    /// The default competition: one pool of 20 seats paying two places from ten players.
    #[test]
    fn a_one_pool_queue_shows_its_seats_and_how_the_prizes_split() {
        let mut competition = crate::templates::pages::competitions::tests::queued("q", 3);
        competition.paid_places = 2;
        if let crate::templates::pages::competitions::Queue::Queued(queue) = &mut competition.queue
        {
            queue.max_players = 20;
            queue.max_entries = Some(20);
        }
        let html = entry_form(
            &competition,
            &Forecasts::Pending(Pending::Loading),
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
            None,
            None,
        )
        .into_string();
        assert!(html.contains("20 seats · 17 left"), "{html}");
        assert!(html.contains("1st 70% · 2nd 30%"));
        assert!(html.contains("Under 10 players: winner takes all"));
        assert!(html.contains("Ties go to the earliest entry"));
        assert!(html.contains("Up to 20 players, all in one pool"));
        // Three players so far: first place takes their whole pot.
        assert!(html.contains("<dd>15,000 sats"));
    }

    /// A competition taking fewer picks than it has rows says how many of them, and counts down.
    #[test]
    fn the_form_says_how_many_of_the_rows_to_pick() {
        let mut competition = view("c1", Phase::Upcoming, 60);
        competition.locations = vec!["KPWM".into(), "KBOS".into(), "KJFK".into(), "KORD".into()];
        competition.number_of_values_per_entry = 3;
        let some = entry_form(
            &competition,
            &Forecasts::Pending(Pending::Loading),
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
            None,
            None,
        )
        .into_string();
        assert!(
            some.contains("<dt>Picks required</dt><dd>3 of 12</dd>"),
            "{some}"
        );
        assert!(some.contains(r#"data-max-values="3" data-pick-rows="12""#));
        assert!(some.contains("3 picks to make: any 3 of the 12 rows."));

        competition.number_of_values_per_entry = 12;
        let all = entry_form(
            &competition,
            &Forecasts::Pending(Pending::Loading),
            None,
            &PayoutDestination::LoggedOut,
            NetworkFee::Estimate(50),
            None,
            None,
        )
        .into_string();
        assert!(all.contains("<dt>Picks required</dt><dd>12</dd>"));
        assert!(all.contains("12 picks to make: one in every row."));
        assert_eq!(picks_to_make(1, 3), "1 pick to make: any 1 of the 3 rows.");
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
            None,
            None,
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
        // Localized, it names the reader's zone: the one time on the form.
        assert_eq!(html.matches("data-zone").count(), 1);
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
            None,
            None,
        )
        .into_string();
        assert!(html.contains(ENTRIES_PAUSED));
        assert!(html.contains(r#"id="submitEntry" class="button is-primary is-medium" disabled"#));
        assert!(html.contains("Entries paused"));
        assert!(!html.contains("and enter"));
    }

    /// In settle-only mode the form shows the plain banner at the top, no other notice, and
    /// cannot be submitted.
    #[test]
    fn entry_form_shows_the_banner_while_entries_are_paused_for_settlement() {
        let html = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Pending(Pending::Loading),
            Some(&terms(true)),
            &PayoutDestination::LoggedOut,
            NetworkFee::SettleOnly,
            None,
            None,
        )
        .into_string();
        assert!(html.contains(r#"id="entriesPausedBanner""#));
        assert!(html.contains("Entries are paused."));
        assert!(!html.contains(r#"id="entriesPaused" "#));
        assert!(!html.contains(ENTRIES_PAUSED));
        assert!(html.contains(r#"id="submitEntry" class="button is-primary is-medium" disabled"#));
        assert!(!html.contains("and enter"));
    }

    /// While the Arkade server is failing batch steps the form says so in one sentence, in place
    /// of the fee pause, and cannot be submitted.
    #[test]
    fn entry_form_says_when_entries_wait_for_arkade() {
        let html = entry_form(
            &view("c1", Phase::Upcoming, 60),
            &Forecasts::Pending(Pending::Loading),
            Some(&terms(true)),
            &PayoutDestination::LoggedOut,
            NetworkFee::ArkadeUnavailable(50),
            None,
            None,
        )
        .into_string();
        assert!(html.contains(
            "Entries are paused while the Arkade network recovers; try again in a little while."
        ));
        assert!(!html.contains(ENTRIES_PAUSED));
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
        assert!(
            html.contains("Winnings and refunds go to <strong>freya@lnurl.example</strong></p>")
        );
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
            None,
            None,
        )
        .into_string();
        assert!(html.contains("40 entered"));
        // How pools work is a tooltip, not a line on the form.
        assert!(
            html.contains(r#"data-tip="Players are split into pools of up to 25 at the start.""#)
        );
        // Forty entries make two pools of twenty: each winner takes 100,000 sats.
        assert!(html.contains("<dd>100,000 sats<"), "{html}");
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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

    const CSS: &str = include_str!("entry_form.css");

    /// Map pins are neither blue nor grey, stand apart from the hover colour, and show on the
    /// map's land (3:1, WCAG's bar for graphics) in both themes.
    #[test]
    fn the_map_pins_are_not_blue_or_grey() {
        use crate::templates::css_check::{contrast, hsl, rgba, rule, value};
        let hover = value(rule(CSS, ".station-pins a:focus .station-pin"), "fill");
        for (theme, pin, map) in [
            ("light", ".station-pin", ".station-map"),
            (
                "dark",
                r#"[data-theme="dark"] .station-pin"#,
                r#"[data-theme="dark"] .station-map"#,
            ),
        ] {
            let fill = value(rule(CSS, pin), "fill");
            let (hue, saturation, _, _) = hsl(fill);
            assert!(!(170.0..=260.0).contains(&hue), "{theme}: {fill} is blue");
            assert!(saturation >= 0.5, "{theme}: {fill} is grey");
            let apart = (hue - hsl(hover).0).abs();
            assert!(
                apart.min(360.0 - apart) >= 30.0,
                "{theme}: {fill} is like {hover}"
            );
            let land = value(rule(CSS, map), "--station-map-land");
            let ratio = contrast(rgba(fill).0, rgba(land).0);
            assert!(ratio >= 3.0, "{theme}: {fill} on {land} is {ratio:.2}:1");
        }
    }

    /// On a phone the facts wrap, so a "?" can sit anywhere on the line; its bubble hangs from
    /// the list's left edge and is no wider than the list, so it stays on the screen.
    #[test]
    fn a_fact_s_bubble_hangs_from_the_list_on_a_phone() {
        use crate::templates::css_check::{rule, value};
        assert_eq!(value(rule(CSS, ".entry-facts"), "position"), "relative");
        assert_eq!(value(rule(CSS, ".entry-facts .tip"), "position"), "static");
        let bubble = rule(CSS, ".entry-facts .tip::after");
        assert_eq!(value(bubble, "left"), "0");
        assert_eq!(value(bubble, "max-width"), "100%");
        assert_eq!(value(bubble, "transform"), "none");
        // The "?" is no longer what the tap area hangs from, so it is 44 px itself.
        assert!(CSS.contains("  .entry-facts .tip {\n    width: 44px;\n    height: 44px;"));
    }

    /// A fact's 44 px "?" takes its own room above, below and after it: hanging into the next
    /// fact, that fact covered 13 px of it. It is square, since a tap follows rounded corners
    /// and the term took them.
    #[test]
    fn the_facts_taps_are_44_px_and_not_covered() {
        use crate::templates::css_check::{rule, value};
        let touch = &CSS[CSS
            .find("@media screen and (max-width: 768px) and (pointer: coarse)")
            .unwrap()..];
        let tip = rule(touch, ".entry-facts .tip");
        assert_eq!(value(tip, "margin"), "0");
        assert_eq!(
            value(tip, "margin-left"),
            "calc(0.3rem + (1.05rem - 44px) / 2)"
        );
        assert_eq!(value(tip, "border-radius"), "0");
    }

    /// A pin and its label are one link, whose tap on a touch screen is a square around the
    /// pin of at least 46 px: 1 map unit scaled up as far as 46 px over the narrowest map each
    /// width draws (398 px at most, 58 px under the screen's width). Elsewhere it takes no taps.
    #[test]
    fn a_pin_takes_a_46_px_tap_on_a_touch_screen() {
        use crate::templates::css_check::{rule, value};
        use crate::templates::shared_map::{station_map, StationPin};
        let map = station_map(&[StationPin {
            station_id: "KORD".into(),
            label: "ORD".into(),
            name: "Chicago/O'Hare International, IL".into(),
            svg_x: 382.5,
            svg_y: 110.0,
        }])
        .into_string();
        let link = &map[map.find("<a ").unwrap()..map.find("</a>").unwrap()];
        assert!(link.contains(
            r#"<rect class="station-pin-hit" x="382.0" y="109.5" width="1" height="1"></rect>"#
        ));
        assert!(link.contains(r#"class="station-pin-label""#));
        let idle = rule(CSS, ".station-pin-hit");
        assert_eq!(value(idle, "pointer-events"), "none");
        // Scaled up, a 1 unit outline would add half the square again on each side.
        assert_eq!(value(idle, "stroke-width"), "0");
        assert_eq!(value(idle, "transform-box"), "fill-box");
        assert_eq!(value(idle, "transform-origin"), "center");
        let coarse = &CSS[CSS.find("@media (pointer: coarse)").unwrap()..];
        assert_eq!(
            value(rule(coarse, ".station-pin-hit"), "pointer-events"),
            "all"
        );
        // (media query, narrowest screen it covers, scale)
        for (media, screen, scale) in [
            ("@media (pointer: coarse)", 456.0, 70.0),
            (
                "@media screen and (max-width: 455px) and (pointer: coarse)",
                360.0,
                92.0,
            ),
            (
                "@media screen and (max-width: 359px) and (pointer: coarse)",
                320.0,
                106.0,
            ),
        ] {
            let block = &CSS[CSS.find(media).unwrap()..];
            assert_eq!(
                value(rule(block, ".station-pin-hit"), "transform"),
                format!("scale({scale})")
            );
            let map_width = f64::min(screen - 58.0, 398.0);
            let tap = scale * map_width / 599.96;
            assert!(tap >= 46.0, "{media}: {tap:.1} px");
        }
    }
}
