//! Funded competitions whose result is late, for the operator pages: past the oracle's signing
//! time, with no signed result and no expiry broadcast yet. Each says what the oracle reported
//! about its event at the last check, which readings are incomplete, and when the contract
//! expires and shares the pot back.
//!
//! The oracle's answer is read from the leaderboards' weather cache, so no operator page waits
//! on the oracle. A page that finds no answer cached says so, and the next load has it.

use std::time::Duration;

use maud::{html, Markup};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    domain::{leaderboard::CompetitionWeather, Competition},
    infra::{oracle_weather::SettlementBlock, refresh_cache::Cached},
    startup::AppState,
};

/// A competition past its signing time without a result.
#[derive(Debug, Clone, PartialEq)]
pub struct LateResult {
    pub signing: OffsetDateTime,
    /// When the contract expires unsigned; `None` when neither the stored contract nor the
    /// oracle's event has said.
    pub expiry: Option<OffsetDateTime>,
    pub oracle: OracleAnswer,
}

/// What the oracle last said about a late event.
#[derive(Debug, Clone, PartialEq)]
pub enum OracleAnswer {
    /// Its last check of the event's data failed, so it cannot sign.
    Blocked {
        block: SettlementBlock,
        incomplete: Vec<IncompleteReading>,
    },
    /// It reports the event without a block, and has not signed it.
    Unsigned,
    /// Nothing has been read from the oracle yet, or the read failed.
    Unknown,
}

/// A station and metric the event scores whose reading lacks a forecast or an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompleteReading {
    pub station: String,
    pub metric: &'static str,
    pub no_forecast: bool,
    pub no_observation: bool,
}

/// Whether `competition` is funded, past its signing time, and neither signed nor expired.
pub fn is_late(competition: &Competition, now: OffsetDateTime) -> bool {
    competition.awaiting_attestation_at.is_some()
        && competition.attestation.is_none()
        && competition.outcome_broadcasted_at.is_none()
        && competition.expiry_broadcasted_at.is_none()
        && competition.cancelled_at.is_none()
        && competition.failed_at.is_none()
        && now >= competition.event_submission.signing_date
}

/// What is known about a late `competition` from its stored contract and the oracle's cached
/// answer; `None` when it is not late.
pub fn late_result(
    competition: &Competition,
    weather: &Cached<CompetitionWeather>,
    now: OffsetDateTime,
) -> Option<LateResult> {
    if !is_late(competition, now) {
        return None;
    }
    let weather = weather.value();
    let stored_expiry = competition
        .event_announcement
        .as_ref()
        .and_then(|announcement| announcement.expiry)
        // Below this, a locktime is a block height, not a time.
        .filter(|expiry| *expiry >= 500_000_000)
        .and_then(|expiry| OffsetDateTime::from_unix_timestamp(i64::from(expiry)).ok());
    let oracle = match weather {
        None => OracleAnswer::Unknown,
        Some(weather) => match &weather.event.settlement_block {
            Some(block) => OracleAnswer::Blocked {
                block: block.clone(),
                incomplete: incomplete_readings(competition, weather),
            },
            None => OracleAnswer::Unsigned,
        },
    };
    Some(LateResult {
        signing: competition.event_submission.signing_date,
        expiry: stored_expiry.or(weather.and_then(|weather| weather.event.expiry)),
        oracle,
    })
}

/// The readings the event scores that the oracle holds without a forecast or an observation.
fn incomplete_readings(
    competition: &Competition,
    weather: &CompetitionWeather,
) -> Vec<IncompleteReading> {
    let event = &competition.event_submission;
    let mut incomplete = Vec::new();
    for station in &event.locations {
        for metric in event.metrics() {
            let reading = weather.event.reading(station, metric.id());
            let no_forecast = reading.is_none_or(|reading| reading.baseline.is_none());
            let no_observation = reading.is_none_or(|reading| reading.observed.is_none());
            if no_forecast || no_observation {
                incomplete.push(IncompleteReading {
                    station: station.clone(),
                    metric: metric.id(),
                    no_forecast,
                    no_observation,
                });
            }
        }
    }
    incomplete
}

/// The late result of `competition`, if it is late, from what the weather cache holds after
/// up to `wait`.
pub async fn read(
    state: &AppState,
    competition: &Competition,
    wait: Duration,
    now: OffsetDateTime,
) -> Option<LateResult> {
    if !is_late(competition, now) {
        return None;
    }
    let weather = state.leaderboards.weather(competition, wait).await;
    late_result(competition, &weather, now)
}

fn utc(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

impl IncompleteReading {
    /// `PAGK wind_speed (no forecast)`.
    fn label(&self) -> String {
        let missing = match (self.no_forecast, self.no_observation) {
            (true, true) => "no forecast, no observation",
            (true, false) => "no forecast",
            _ => "no observation",
        };
        format!("{} {} ({missing})", self.station, self.metric)
    }
}

impl LateResult {
    /// What the oracle said, in one line: for a table row.
    pub fn oracle_line(&self) -> String {
        match &self.oracle {
            OracleAnswer::Blocked { block, incomplete } => {
                let code = block.code();
                if incomplete.is_empty() {
                    format!("Oracle cannot sign: {code}")
                } else {
                    let readings: Vec<String> =
                        incomplete.iter().map(IncompleteReading::label).collect();
                    format!("Oracle cannot sign: {code}: {}", readings.join(", "))
                }
            }
            OracleAnswer::Unsigned => "Oracle reports no block, and has not signed".into(),
            OracleAnswer::Unknown => "Oracle status not read yet; reload shortly".into(),
        }
    }

    /// When the contract expires, in one line.
    pub fn expiry_line(&self) -> String {
        match self.expiry {
            Some(expiry) => format!("Contract expires {} if unsigned", utc(expiry)),
            None => "Contract expiry not read yet".into(),
        }
    }

    /// The whole of it, for a competition's own operator page.
    pub fn notice(&self) -> Markup {
        html! {
            div.notice {
                strong { "Result is late" }
                p {
                    "The oracle was due to sign at " (utc(self.signing)) ". "
                    @match self.expiry {
                        Some(expiry) => {
                            "Without a signed result the contract expires at " (utc(expiry))
                            ": the coordinator then broadcasts the expiry transaction and each \
                             entry is owed an equal share of the pot."
                        }
                        None => { "The contract's expiry time has not been read yet." }
                    }
                }
                @match &self.oracle {
                    OracleAnswer::Blocked { block, incomplete } => {
                        p {
                            "The oracle cannot sign: "
                            code { (block.code()) }
                            @if !block.message.is_empty() { " · " (block.message) }
                            @if let Some(at) = &block.checked_at { " · last checked " (at) }
                        }
                        @if !incomplete.is_empty() {
                            p { "Incomplete readings:" }
                            ul { @for reading in incomplete { li { (reading.label()) } } }
                        }
                    }
                    OracleAnswer::Unsigned => {
                        p { "The oracle reports no block for this event, and has not signed it." }
                    }
                    OracleAnswer::Unknown => {
                        p { "The oracle's status for this event has not been read yet. Reload shortly." }
                    }
                }
            }
        }
    }
}

/// Every late competition among `competitions`, the longest overdue first, with what the
/// weather cache holds of the oracle's answer right now.
pub async fn late_competitions<'a>(
    state: &AppState,
    competitions: &'a [Competition],
    now: OffsetDateTime,
) -> Vec<(&'a Competition, LateResult)> {
    let mut late = Vec::new();
    for competition in competitions {
        if let Some(result) = read(state, competition, Duration::ZERO, now).await {
            late.push((competition, result));
        }
    }
    late.sort_by_key(|(competition, result)| (result.signing, competition.id));
    late
}

/// The services page's card: how many results are late, and each one's reason and expiry.
pub fn services_card(late: &[(&Competition, LateResult)]) -> Markup {
    html! {
        article.service-signal id="late-results" {
            div.signal-heading {
                h3 { "Results past signing" }
                span class=(if late.is_empty() { "signal-status signal-ok" } else { "signal-status signal-review" }) {
                    @if late.is_empty() { "OK" } @else { "Review" }
                }
            }
            dl.signal-values {
                div { dt { "Funded competitions without a signed result" } dd { (late.len()) } }
            }
            @for (competition, result) in late {
                p {
                    a href=(format!("/admin/operations/{}", competition.id)) {
                        (competition.event_submission.locations.join(" · "))
                    }
                    " · signing was due " (utc(result.signing))
                }
                p.note { (result.oracle_line()) ". " (result.expiry_line()) "." }
            }
            @if late.is_empty() {
                p.note { "Counted from this coordinator's contracts and the oracle's own event pages, whatever the oracle's metrics count." }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CoordinatorFee, CreateEvent},
        infra::{
            oracle_weather::{EventReadings, Reading},
            refresh_cache::Fetched,
        },
    };
    use std::sync::Arc;
    use uuid::Uuid;

    fn signing() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_791_130_626).unwrap()
    }

    /// Funded and waiting for its result, with signing due at [`signing`].
    fn competition() -> Competition {
        let mut competition = Competition::new(&CreateEvent {
            id: Uuid::now_v7(),
            signing_date: signing(),
            start_observation_date: signing() - time::Duration::hours(25),
            end_observation_date: signing() - time::Duration::minutes(5),
            locations: vec!["PAGK".into(), "PAMD".into()],
            number_of_values_per_entry: 6,
            number_of_places_win: 1,
            total_allowed_entries: 7,
            entry_fee: 5_000,
            coordinator_fee: CoordinatorFee::whole_percent(3),
            total_competition_pool: 35_000,
            relative_locktime_block_delta: None,
            unlisted: true,
            scoring_rules: None,
            scoring_fields: None,
            max_entries_per_player: 1,
            contract_options: None,
        });
        competition.awaiting_attestation_at = Some(signing() - time::Duration::hours(24));
        competition
    }

    /// The oracle's event as its public API gave it for the stuck events: every reading
    /// complete but the wind forecasts, and a block that names them.
    fn blocked_weather() -> Cached<CompetitionWeather> {
        let reading = |target: &str, metric: &str, baseline, observed| Reading {
            target: target.into(),
            metric: metric.into(),
            baseline,
            observed,
        };
        let event = EventReadings {
            readings: vec![
                reading("PAGK", "temp_high", Some(37.0), Some(30.2)),
                reading("PAGK", "temp_low", Some(24.0), Some(15.98)),
                reading("PAGK", "wind_speed", None, Some(5.0)),
                reading("PAMD", "temp_high", Some(53.0), Some(52.0)),
                reading("PAMD", "temp_low", Some(48.0), Some(48.0)),
                reading("PAMD", "wind_speed", None, None),
            ],
            settlement_block: Some(SettlementBlock {
                code: "incomplete_readings".into(),
                message: "verified baseline and observation required for every enabled pair".into(),
                checked_at: Some("2026-10-05T06:28:46Z".into()),
            }),
            expiry: OffsetDateTime::from_unix_timestamp(1_791_217_026).ok(),
            ..Default::default()
        };
        Cached {
            latest: Some(Arc::new(Fetched::new(CompetitionWeather::of_event(event)))),
            refreshing: false,
        }
    }

    #[test]
    fn only_a_funded_unsigned_competition_past_signing_is_late() {
        let competition = competition();
        assert!(!is_late(&competition, signing() - time::Duration::SECOND));
        assert!(is_late(&competition, signing()));
        let mut unfunded = competition.clone();
        unfunded.awaiting_attestation_at = None;
        assert!(!is_late(&unfunded, signing()));
        let mut expired = competition.clone();
        expired.expiry_broadcasted_at = Some(signing());
        assert!(!is_late(&expired, signing() + time::Duration::DAY));
        let mut signed = competition;
        signed.attestation = Some(dlctix::secp::Scalar::one().into());
        assert!(!is_late(&signed, signing()));
    }

    /// The operator pages name the oracle's reason, the readings it lacks and the expiry, where
    /// they used to show only "Awaiting result".
    #[test]
    fn a_blocked_event_names_its_reason_its_incomplete_readings_and_the_expiry() {
        let competition = competition();
        let weather = blocked_weather();
        let now = signing() + time::Duration::hours(19);
        let late = late_result(&competition, &weather, now).expect("late");
        assert_eq!(late.signing, signing());
        assert_eq!(
            late.expiry,
            Some(OffsetDateTime::from_unix_timestamp(1_791_217_026).unwrap())
        );
        let OracleAnswer::Blocked { block, incomplete } = &late.oracle else {
            panic!("{:?}", late.oracle);
        };
        assert_eq!(block.code, "incomplete_readings");
        assert_eq!(
            incomplete
                .iter()
                .map(IncompleteReading::label)
                .collect::<Vec<_>>(),
            [
                "PAGK wind_speed (no forecast)",
                "PAMD wind_speed (no forecast, no observation)"
            ]
        );
        assert_eq!(
            late.oracle_line(),
            "Oracle cannot sign: incomplete_readings: PAGK wind_speed (no forecast), \
             PAMD wind_speed (no forecast, no observation)"
        );
        assert_eq!(
            late.expiry_line(),
            "Contract expires 2026-10-05T16:17:06Z if unsigned"
        );
        let notice = late.notice().into_string();
        assert!(notice.contains("due to sign at 2026-10-04T16:17:06Z"));
        assert!(notice.contains("expires at 2026-10-05T16:17:06Z"));
        assert!(notice.contains("<code>incomplete_readings</code>"));
        assert!(notice.contains("verified baseline and observation required"));
        assert!(notice.contains("last checked 2026-10-05T06:28:46Z"));
        assert!(notice.contains("<li>PAGK wind_speed (no forecast)</li>"));

        let card = services_card(&[(&competition, late)]).into_string();
        assert!(card.contains("signal-review") && card.contains(">Review<"));
        assert!(card.contains(&format!(r#"href="/admin/operations/{}""#, competition.id)));
        assert!(card.contains("PAGK · PAMD"));
        assert!(card.contains("Oracle cannot sign: incomplete_readings"));
        assert!(card.contains("Contract expires 2026-10-05T16:17:06Z if unsigned."));

        // Not late yet: nothing to show.
        assert_eq!(
            late_result(&competition, &weather, signing() - time::Duration::MINUTE),
            None
        );
        let none = services_card(&[]).into_string();
        assert!(none.contains("signal-ok") && none.contains("<dd>0</dd>"));
    }

    /// Without an answer from the oracle the page says so; the stored contract still gives the
    /// expiry.
    #[test]
    fn an_unread_oracle_is_unknown_and_the_stored_contract_gives_the_expiry() {
        let mut competition = competition();
        let unread = Cached {
            latest: None,
            refreshing: true,
        };
        let now = signing() + time::Duration::HOUR;
        let late = late_result(&competition, &unread, now).expect("late");
        assert_eq!(late.oracle, OracleAnswer::Unknown);
        assert_eq!(late.expiry, None);
        assert_eq!(late.expiry_line(), "Contract expiry not read yet");
        assert!(late.oracle_line().contains("not read yet"));

        competition.event_announcement = Some(dlctix::EventLockingConditions {
            locking_points: vec![],
            expiry: Some(1_791_217_026),
        });
        let late = late_result(&competition, &unread, now).expect("late");
        assert_eq!(
            late.expiry,
            Some(OffsetDateTime::from_unix_timestamp(1_791_217_026).unwrap())
        );
    }
}
