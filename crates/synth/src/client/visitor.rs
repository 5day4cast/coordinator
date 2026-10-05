//! The entry form as a visitor's browser loads it.
//!
//! Synth's players enter through the API, which takes their picks without looking at a
//! forecast, so its runs passed while the public form showed "no forecast yet" on every line and
//! let nobody pick. This asks for what the form asks for, the forecasts fragment the coordinator
//! renders at `/competitions/{id}/entry-forecasts`, and reads it as the page shows it. The
//! coordinator leaves a line without a forecast unless the oracle has one for every day of the
//! competition's window, and a line without one has no pick anyone can make.

use std::time::Duration;

use super::CoordinatorClient;
use anyhow::{Context, Result};
use uuid::Uuid;

/// How long the page waits before asking again for forecasts still loading, and how many times
/// it asks by itself, as the coordinator's placeholder tells it to.
const ASK_AGAIN: Duration = Duration::from_secs(2);
const MAX_ASKS: u8 = 15;

/// Tries at a form that did not show its picks, and the wait between them: the coordinator asks
/// the oracle again within 30 seconds of a failed fetch.
const FORM_TRIES: u32 = 4;
const FORM_RETRY: Duration = Duration::from_secs(30);

/// What the entry form's forecasts fragment shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryForm {
    /// The picks: the stations shown, how many lines they have, one per station and metric, and
    /// the lines without a forecast, by the name of their pick.
    Picks {
        stations: Vec<String>,
        lines: usize,
        missing: Vec<String>,
    },
    /// The coordinator is still fetching the forecasts.
    Loading,
    /// The page says there are no forecasts to show, in these words.
    Unavailable(String),
    /// The page says entries have closed.
    Closed,
}

/// Whether a visitor can make every pick on a competition's entry form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormCheck {
    /// Forecast lines the form shows.
    pub lines: usize,
    /// Of them, those without a forecast.
    pub missing: usize,
    /// Why a visitor cannot enter; None when they can.
    pub blocked: Option<String>,
}

impl EntryForm {
    /// Read the fragment as the coordinator renders it: a `pick-row` for each line, whose
    /// forecast is marked `is-missing` and whose picks are `disabled` when the coordinator has
    /// no forecast for it.
    pub fn read(html: &str) -> Self {
        let mut parts = html.split("class=\"pick-row\"");
        let before = parts.next().unwrap_or_default();
        let lines: Vec<&str> = parts.collect();
        if lines.is_empty() {
            return if before.contains("Entries have closed") {
                EntryForm::Closed
            } else if before.contains("Still loading") {
                EntryForm::Loading
            } else if before.contains("unavailable right now") {
                EntryForm::Unavailable("the oracle's forecasts are unavailable".into())
            } else if before.contains("Something went wrong") {
                EntryForm::Unavailable("the coordinator failed to load the forecasts".into())
            } else {
                EntryForm::Unavailable("the form shows no picks".into())
            };
        }
        let missing = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains("is-missing") || line.contains(" disabled"))
            .map(|(index, line)| {
                attribute(line, "name").map_or_else(|| format!("line {}", index + 1), str::to_owned)
            })
            .collect();
        let stations = html
            .split("<fieldset")
            .skip(1)
            .filter_map(|station| attribute(station, "data-station"))
            .map(str::to_owned)
            .collect();
        EntryForm::Picks {
            stations,
            lines: lines.len(),
            missing,
        }
    }

    /// Whether a visitor can enter a competition on `stations` through this form: it shows
    /// each station, and a forecast on every line.
    pub fn check(&self, stations: &[String]) -> FormCheck {
        match self {
            EntryForm::Picks {
                stations: shown,
                lines,
                missing,
            } => {
                let absent: Vec<&str> = stations
                    .iter()
                    .filter(|station| !shown.contains(station))
                    .map(String::as_str)
                    .collect();
                let blocked = if !missing.is_empty() {
                    Some(format!(
                        "{} of {lines} forecast lines on the entry form have no forecast, so \
                         their picks are disabled: {}",
                        missing.len(),
                        missing.join(", ")
                    ))
                } else if !absent.is_empty() {
                    Some(format!(
                        "the entry form shows no picks for {}",
                        absent.join(", ")
                    ))
                } else {
                    None
                };
                FormCheck {
                    lines: *lines,
                    missing: missing.len(),
                    blocked,
                }
            }
            EntryForm::Loading => FormCheck::blocked(
                "the entry form was still loading its forecasts after the page stopped asking",
            ),
            EntryForm::Unavailable(why) => {
                FormCheck::blocked(&format!("the entry form has no picks to make: {why}"))
            }
            EntryForm::Closed => FormCheck::blocked("the entry form says entries have closed"),
        }
    }

    /// Whether the form may show its picks if asked again shortly: its forecasts were on their
    /// way or the oracle's were unavailable. Picks without a forecast stay as they are.
    fn may_recover(&self) -> bool {
        matches!(self, EntryForm::Loading | EntryForm::Unavailable(_))
    }
}

impl FormCheck {
    fn blocked(why: &str) -> Self {
        Self {
            lines: 0,
            missing: 0,
            blocked: Some(why.to_owned()),
        }
    }
}

/// The value of the first `name="…"` attribute in `html`.
fn attribute<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let start = html.find(&format!("{name}=\""))? + name.len() + 2;
    let value = &html[start..];
    Some(&value[..value.find('"')?])
}

impl CoordinatorClient {
    /// The forecasts fragment of a competition's entry form; `asked` is how many times the page
    /// has asked for it by itself.
    async fn entry_forecasts(&self, competition_id: &Uuid, asked: u8) -> Result<String> {
        let mut url = format!(
            "{}/competitions/{}/entry-forecasts",
            self.base_url(),
            competition_id
        );
        if asked > 0 {
            url = format!("{url}?again={asked}");
        }
        let resp = super::retry_transport(3, || async {
            anyhow::Ok(self.http().get(&url).send().await?)
        })
        .await
        .context("Failed to load the entry form's forecasts")?;
        // The page swaps in a 500, which says the load failed.
        if !resp.status().is_success()
            && resp.status() != reqwest::StatusCode::INTERNAL_SERVER_ERROR
        {
            anyhow::bail!("Entry form forecasts failed ({})", resp.status());
        }
        resp.text()
            .await
            .context("Failed to read the entry form's forecasts")
    }

    /// Load a competition's entry form as a visitor's page does: while the coordinator says the
    /// forecasts are still loading, the page asks again every two seconds, fifteen times at most.
    pub async fn entry_form(&self, competition_id: &Uuid) -> Result<EntryForm> {
        let mut asked = 0;
        loop {
            let form = EntryForm::read(&self.entry_forecasts(competition_id, asked).await?);
            if form != EntryForm::Loading || asked >= MAX_ASKS {
                return Ok(form);
            }
            asked += 1;
            tokio::time::sleep(ASK_AGAIN).await;
        }
    }

    /// Whether a visitor can enter a competition on `stations` through its form, once the form
    /// has had time to get its forecasts: one that shows no picks is loaded again a few times,
    /// half a minute apart.
    pub async fn check_entry_form(
        &self,
        competition_id: &Uuid,
        stations: &[String],
    ) -> Result<FormCheck> {
        let mut tries = 1;
        loop {
            let form = self.entry_form(competition_id).await?;
            if !form.may_recover() || tries >= FORM_TRIES {
                return Ok(form.check(stations));
            }
            tries += 1;
            tokio::time::sleep(FORM_RETRY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, http::StatusCode, routing::get, Router};
    use std::collections::HashMap;

    /// A line of the form as the coordinator renders it, with or without its forecast.
    fn line(station: &str, metric: &str, forecast: Option<&str>) -> String {
        let name = format!("{station}_{metric}");
        let value = match forecast {
            Some(value) => format!("<strong class=\"pick-forecast\">{value}</strong>"),
            None => "<span class=\"pick-forecast is-missing\">no forecast yet</span>".into(),
        };
        let disabled = if forecast.is_some() { "" } else { " disabled" };
        let options: String = ["under", "par", "over"]
            .iter()
            .map(|pick| {
                format!(
                    "<label class=\"pick-option\"><input type=\"radio\" name=\"{name}\" \
                     value=\"{pick}\"{disabled}><span>{pick}</span></label>"
                )
            })
            .collect();
        format!(
            "<div class=\"pick-row\"><span class=\"pick-metric\">{metric} {value}</span>\
             <div class=\"pick-options\" role=\"radiogroup\">{options}</div></div>"
        )
    }

    /// The forecasts fragment for `stations`, each with a high and a wind line.
    fn form(stations: &[(&str, Option<&str>, Option<&str>)]) -> String {
        let stations: String = stations
            .iter()
            .map(|(station, high, wind)| {
                format!(
                    "<fieldset class=\"station-picks\" id=\"station-{station}\" \
                     data-station=\"{station}\"><legend>{station}</legend>{}{}</fieldset>",
                    line(station, "temp_high", *high),
                    line(station, "wind_speed", *wind)
                )
            })
            .collect();
        format!("<div id=\"entryForecasts\"><p class=\"city-heading\">City</p>{stations}</div>")
    }

    fn stations(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_form_with_every_forecast_can_be_entered() {
        let html = form(&[
            ("KSTS", Some("98°F"), Some("7 mph")),
            ("KUKI", Some("102°F"), Some("5 mph")),
        ]);
        let read = EntryForm::read(&html);
        assert_eq!(
            read,
            EntryForm::Picks {
                stations: stations(&["KSTS", "KUKI"]),
                lines: 4,
                missing: Vec::new(),
            }
        );
        let check = read.check(&stations(&["KSTS", "KUKI"]));
        assert_eq!((check.lines, check.missing, check.blocked), (4, 0, None));
    }

    /// What the weekend's visitors met: every line without its forecast, every pick disabled.
    #[test]
    fn lines_without_a_forecast_block_the_form_and_are_named() {
        let html = form(&[("KSTS", None, None), ("KUKI", Some("102°F"), None)]);
        let check = EntryForm::read(&html).check(&stations(&["KSTS", "KUKI"]));
        assert_eq!((check.lines, check.missing), (4, 3));
        let why = check.blocked.unwrap();
        assert!(why.starts_with("3 of 4 forecast lines"), "{why}");
        assert!(
            why.ends_with("KSTS_temp_high, KSTS_wind_speed, KUKI_wind_speed"),
            "{why}"
        );
    }

    #[test]
    fn a_station_the_form_leaves_out_blocks_it() {
        let html = form(&[("KSTS", Some("98°F"), Some("7 mph"))]);
        let check = EntryForm::read(&html).check(&stations(&["KSTS", "KUKI"]));
        assert_eq!(check.missing, 0);
        assert_eq!(
            check.blocked.as_deref(),
            Some("the entry form shows no picks for KUKI")
        );
    }

    #[test]
    fn a_form_without_picks_says_why() {
        let loading = "<div id=\"entryForecasts\"><p class=\"notice\">Still loading forecasts \
                       from the oracle…</p></div>";
        assert_eq!(EntryForm::read(loading), EntryForm::Loading);
        let unavailable = "<div id=\"entryForecasts\"><p class=\"notice\">The oracle's forecasts \
                           are unavailable right now; this site asks again within 30 seconds.</p>\
                           </div>";
        let read = EntryForm::read(unavailable);
        assert!(read.may_recover());
        assert!(read
            .check(&[])
            .blocked
            .unwrap()
            .contains("the oracle's forecasts are unavailable"));
        let closed = "<p class=\"notice\">Entries have closed. <a href=\"/x\">View</a></p>";
        assert_eq!(EntryForm::read(closed), EntryForm::Closed);
        assert!(EntryForm::read(closed).check(&[]).blocked.is_some());
        assert!(matches!(EntryForm::read(""), EntryForm::Unavailable(_)));
    }

    /// The page asks again while the forecasts load, and the check reads what it then shows.
    #[tokio::test]
    async fn the_form_is_loaded_as_the_page_loads_it() {
        let competition = Uuid::now_v7();
        let ready = form(&[("KSTS", Some("98°F"), None)]);
        let router = Router::new().route(
            "/competitions/{id}/entry-forecasts",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let ready = ready.clone();
                async move {
                    // Loading at first, and ready when the page asks again by itself.
                    match query.get("again").map(String::as_str) {
                        Some("1") => (StatusCode::OK, ready),
                        _ => (
                            StatusCode::OK,
                            "<div id=\"entryForecasts\"><p>Still loading forecasts</p></div>"
                                .to_owned(),
                        ),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = CoordinatorClient::new(&url, None);

        let check = client
            .check_entry_form(&competition, &stations(&["KSTS"]))
            .await
            .unwrap();
        assert_eq!((check.lines, check.missing), (2, 1));
        assert!(check.blocked.unwrap().contains("KSTS_wind_speed"));

        server.abort();
        let _ = server.await;
    }
}
