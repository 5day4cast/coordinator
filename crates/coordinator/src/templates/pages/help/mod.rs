//! How it works: the rules, scoring and statuses the competition pages leave out, so those
//! pages can show just the numbers. The walkthrough is drawn with the entry form's own pick
//! row, so it looks like the form it explains.

use maud::{html, Markup};

use crate::domain::leaderboard::{progress::LINE_POINTS, Metric, PickState, Rule};
use crate::templates::fragments::{entry_form::pick_row, picks::state_badge};

/// A numbered marker tying a spot in the walkthrough to its note.
fn callout(number: u8) -> Markup {
    html! { span class="callout" aria-hidden="true" { (number) } }
}

/// `open_advanced` renders "How the tech works" unfolded: links into it say so in
/// their address, since a fragment alone cannot open a `details`.
pub fn help_page(open_advanced: bool) -> Markup {
    html! {
        div class="help-page" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }
            h1 class="title is-4" { "How it works" }
            p class="content" {
                "Daily Fantasy Weather works like daily fantasy sports: make weather picks for a roster of US cities, "
                "then compete on how well your picks match the observed weather."
            }

            nav class="help-contents" aria-label="On this page" {
                a href="#walkthrough" { "Entering" }
                a href="#scoring" { "Scoring" }
                a href="#readings" { "Readings" }
                a href="#timing" { "Deadlines and results" }
                a href="#statuses" { "Following your picks" }
                a href="#pools" { "Entries and pools" }
                a href="#payouts" { "Paying and payouts" }
                a href="/help?open=advanced#advanced" { "How the tech works" }
            }

            (walkthrough())

            section id="scoring" class="content" {
                h2 { "Scoring" }
                p {
                    "Each competition lists its cities and weather categories. For each category — high temperature, "
                    "low temperature or sustained wind speed — choose one of three ranges: " strong { "Under" } ", in " strong { "Par" }
                    " or " strong { "Over" } "."
                }
                ul {
                    li {
                        "Complete every category for every city. The entry page shows the required number of picks "
                        "and checks it before you pay."
                    }
                    li {
                        strong { "Par is a range" } " around the forecast, set from how recent forecasts "
                        "have missed: that airport's own once it has enough history, and all airports' "
                        "together until then. Under, Par and Over are about equally likely. A right pick "
                        "scores " (LINE_POINTS) " points."
                    }
                    li {
                        "Par includes both ends of the range, and the reading is compared before rounding: "
                        "with Par 80.4–83.0°F, 83.0°F is Par and 83.1°F is Over."
                    }
                    li { "An incorrect pick scores 0. There are no negative points." }
                }
            }

            section id="readings" class="content" {
                h2 { "Readings" }
                ul {
                    li { strong { "High" } ": the highest temperature observed during the window." }
                    li { strong { "Low" } ": the lowest temperature observed during the window." }
                    li { strong { "Wind" } ": the highest sustained wind speed observed during the window, in knots." }
                }
                p {
                    "All observations come from NOAA's reports for the airport station named beside each city. "
                    "A full-day competition scores all three categories; a daytime window has highs and wind, "
                    "and a night window has lows and wind."
                }
                p {
                    "The bold forecast values are NOAA's forecasts. They are not adjusted by the historical model. "
                    "The model sets the three pick ranges from past forecast errors. Forecasts help you choose; "
                    "observed weather decides the score."
                }
            }

            section id="timing" class="content" {
                h2 { "Deadlines and results" }
                ul {
                    li {
                        strong { "Submit your picks and finish paying before entries close." }
                        " Observations start at that time, and every entry's picks become public. "
                        "Start times use your local time zone. Duration is the length of the observation window."
                    }
                    li {
                        "While the window is " strong { "live" } ", the leaderboard scores the readings so far "
                        "and can still change."
                    }
                    li {
                        "Once the window ends, the competition is " strong { "awaiting results" } ". "
                        "Stations report hourly, so the final result can arrive later than the observation window's end. "
                        "The oracle signs once the signing time has passed and all required observations are verified. "
                        "If coverage never becomes complete, the contract expires and entrants can recover their funds."
                    }
                    li {
                        "When the oracle signs, the competition is " strong { "finished" } ": the leaderboard "
                        "stops changing and payout processing begins."
                    }
                    li {
                        "Tied scores share a rank. If a tie spans the last paid place, the entry submitted "
                        "first is paid."
                    }
                }
            }

            section id="statuses" class="content" {
                h2 { "Following your picks" }
                p {
                    "While the window is live, each pick shows the reading so far and the points it would "
                    "score if the window ended now. Highs and winds can only rise and lows can only fall, "
                    "so some picks settle before the window ends:"
                }
                dl class="help-statuses" {
                    @for (state, meaning) in [
                        (PickState::LockedIn, "Right, and nothing left in the window can change it."),
                        (PickState::Out, "Wrong, and nothing left in the window can change it."),
                        (PickState::AwaitingResult, "The window has closed; the oracle's own reading decides."),
                        (PickState::Final, "The oracle's signed result."),
                    ] {
                        div { dt { (state_badge(state)) } dd { (meaning) } }
                    }
                }
            }

            section id="pools" class="content" {
                h2 { "Entries and pools" }
                ul {
                    li {
                        strong { "One entry per player." } " Each account enters a competition once, "
                        "unless its page says it allows more."
                    }
                    li {
                        "A competition accepts entries up to its listed limit and splits them into pools of up to 25 "
                        "when entries close. Each pool runs as its own competition, with its own pot and leaderboard."
                    }
                    li {
                        "A pool needs a minimum number of players. The entry page shows the current minimum, "
                        "which can rise when Bitcoin network fees increase."
                    }
                    li { "If a competition or pool doesn't get enough entries, it doesn't run. Funded entries follow the refund process below." }
                }
            }

            section id="payouts" class="content" {
                h2 { "Entry fees and prizes" }
                ul {
                    li {
                        "The entry fee is the total you pay over Lightning. The payment button and invoice show that total."
                    }
                    li {
                        "Prizes are sent automatically to the Lightning Address you confirm when entering. "
                        "If you use invoice payouts, submit a Lightning invoice on the Payouts page. "
                        "The entry page tells you where prizes and refunds will go before you pay."
                    }
                    li {
                        "If a competition doesn't run, its funded entry payments are refunded to the Lightning "
                        "Address confirmed when entering. Refund processing starts after the escrow's refund "
                        "time; cancellation does not return the payment immediately. "
                        "Refunds return the escrow amount minus the refund swap fee."
                    }
                }
            }

            section id="advanced" class="content" {
                details open[open_advanced] {
                    summary { h2 class="is-inline" { "How the tech works" } }
                    h3 id="where-your-sats-go" { "Entry fees and the prize pool" }
                    p {
                        "Your entry fee includes the costs of running and settling the competition. "
                        "The Prizes amount shows what a winner receives. For a queued competition, "
                        "that prize grows as more players join its pool."
                    }
                    h3 { "The network underneath" }
                    ul {
                        li {
                            "You pay and are paid over the Lightning Network, so payments settle in seconds "
                            "without waiting for a block."
                        }
                        li {
                            "Behind it, entry fees are held in escrow on Arkade, a Bitcoin layer that batches "
                            "many payments into one on-chain transaction, and each competition's pot is funded "
                            "in one batch."
                        }
                    }
                    h3 { "The contract" }
                    ul {
                        li {
                            "Every entry in a competition is locked into one Bitcoin contract (a DLC). The "
                            a href="https://www.4casttruth.win/" target="_blank" rel="noopener" { "4cast Truth oracle" }
                            " signs the final readings, and that signature is what unlocks the winners' payouts."
                        }
                        li {
                            "A Keymeld enclave co-signs the contract for you, so you don't need to stay online "
                            "during the competition. Your wallet checks the enclave before it trusts it."
                        }
                        li {
                            "If the result is never settled cooperatively, entrants can reclaim their funds "
                            "on-chain after a timelock. Each competition's page lists its timelock and fee cap "
                            "under Advanced."
                        }
                    }
                }
            }
        }
    }
}

/// An entry form as it looks, inert, with numbered notes.
fn walkthrough() -> Markup {
    let band = Rule::Line {
        lower: -3.6,
        upper: -1.0,
    };
    html! {
        section id="walkthrough" {
            h2 class="title is-4" { "Selection process" }
            p class="content" {
                "Choose one range for each weather category shown. For example, a high-temperature pick below 66°F "
                "scores only if the highest observed temperature stays below 66°F for the entire window. "
                "A low-temperature range of 43.1–46.7°F scores when the lowest observation falls within that range, "
                "including either boundary. A wind pick above 11.5 knots scores if the highest sustained wind exceeds 11.5 knots."
            }
            div class="help-walkthrough" {
                figure class="help-shot entry-form" inert aria-label="An example entry form" {
                    dl class="entry-facts" {
                        div { dt { "Entries close" (callout(1)) } dd { "Sun, 1:00 PM EDT" } }
                        div { dt { "Entry fee" (callout(2)) } dd { "5,687 sats" } }
                        div { dt { "Prizes" (callout(3)) } dd { "30,000 sats" } }
                    }
                    fieldset class="station-picks" {
                        legend { "Chicago, IL " span class="station-code" { "KORD" } (callout(4)) }
                        (pick_row("KORD", Metric::TempHigh, Some(77.0), Some(band)))
                        p class="help-pointer" { "↑ " (callout(5)) }
                    }
                    button type="button" class="button is-primary" { "Pay 5,687 sats and enter" (callout(6)) }
                }
                ol class="help-callouts content" {
                    li { "The deadline. Picks lock when the observation window starts." }
                    li { "The total you pay. The invoice and payment button show the same entry fee." }
                    li { "What first place wins once the result is final." }
                    li { "The city and NOAA forecast: here, a 77°F high for Chicago. The city tooltip identifies the airport station." }
                    li {
                        "Your pick, left to right: Under, Par (73.4–76.0°F, both ends included) or Over. "
                        "Choose one range for each category. You can change a pick before paying."
                    }
                    li { "Pay the Lightning invoice and you're in. Your picks show on the leaderboard once entries close." }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_into_the_folded_section_open_it() {
        let folded = help_page(false).into_string();
        assert!(folded.contains("<details>"), "{folded}");
        assert!(folded.contains(r##"href="/help?open=advanced#advanced""##));
        assert!(folded.contains(r#"<h3 id="where-your-sats-go">"#));

        let unfolded = help_page(true).into_string();
        assert!(unfolded.contains("<details open>"), "{unfolded}");
    }

    #[test]
    fn the_walkthrough_uses_the_real_pick_buttons_and_does_nothing() {
        let html = help_page(false).into_string();
        assert!(html.contains(r#"class="help-shot entry-form" inert"#));
        assert!(
            html.contains("&lt; 73.4°F")
                && html.contains("73.4–76.0°F")
                && html.contains("&gt; 76.0°F")
        );
        // Nothing the live form's script looks for.
        assert!(!html.contains("entryForm") && !html.contains("submitEntry"));
    }

    #[test]
    fn the_help_uses_current_examples_and_explains_forecast_provenance() {
        let html = help_page(false).into_string();
        assert!(!html.contains("<video") && !html.contains("Pick as many"));
        assert!(html.contains("bold forecast values are NOAA"));
        assert!(html.contains("There are no negative points"));
    }

    /// How much history a Par range is fitted on is the oracle's setting, and an airport with
    /// too little of its own uses every airport's: the help names no number of days.
    #[test]
    fn par_is_explained_without_a_number_of_days() {
        let html = help_page(false).into_string().replace("&#39;", "'");
        assert!(html.contains("that airport's own once it has enough history"));
        assert!(html.contains("all airports' together until then"));
        assert!(!html.contains("60 days"), "{html}");
    }

    #[test]
    fn every_section_the_forms_link_to_is_here() {
        let html = help_page(false).into_string();
        for id in [
            "walkthrough",
            "scoring",
            "readings",
            "timing",
            "statuses",
            "pools",
            "payouts",
            "advanced",
        ] {
            assert!(html.contains(&format!(r#"id="{id}""#)), "{id}");
        }
        assert!(html.contains("highest sustained wind speed"));
        assert!(html.contains("An incorrect pick scores 0"));
    }
}
