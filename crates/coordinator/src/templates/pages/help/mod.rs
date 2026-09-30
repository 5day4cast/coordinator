//! How it works: the rules, scoring and statuses the competition pages leave out, so those
//! pages can show just the numbers. The walkthrough is drawn with the entry form's own pick
//! row, so it looks like the form it explains.

use maud::{html, Markup};

use crate::domain::leaderboard::{progress::LINE_POINTS, Metric, PickState, Rule};
use crate::templates::{
    assets::{HOW_IT_WORKS_JPG, HOW_IT_WORKS_MP4},
    fragments::{entry_form::pick_row, picks::state_badge},
};

/// A numbered marker tying a spot in the walkthrough to its note.
fn callout(number: u8) -> Markup {
    html! { span class="callout" aria-hidden="true" { (number) } }
}

pub fn help_page() -> Markup {
    html! {
        div class="help-page" {
            a class="back-link" href="/competitions" hx-get="/competitions"
              hx-target="#main-content" hx-push-url="true" { "← All competitions" }
            h1 class="title is-4" { "How it works" }
            (video())

            nav class="help-contents" aria-label="On this page" {
                a href="#walkthrough" { "Entering" }
                a href="#scoring" { "Scoring" }
                a href="#readings" { "Readings" }
                a href="#timing" { "Deadlines and results" }
                a href="#statuses" { "Following your picks" }
                a href="#pools" { "Entries and pools" }
                a href="#payouts" { "Paying and payouts" }
                a href="#advanced" { "How the tech works" }
            }

            (walkthrough())

            section id="scoring" class="content" {
                h2 { "Scoring" }
                p {
                    "Each competition covers a few airport weather stations. For each reading — high, "
                    "low, wind — pick whether it lands " strong { "Under" } ", in " strong { "Par" }
                    " or " strong { "Over" } "."
                }
                ul {
                    li {
                        "Pick as many readings as you like. A wrong or skipped pick scores 0 and costs "
                        "nothing, so filling in every pick is always your best play."
                    }
                    li {
                        strong { "Par is a range" } " around the forecast, set from how that airport's "
                        "forecasts have missed over the last 60 days, so Under, Par and Over are about "
                        "equally likely. A right pick scores " (LINE_POINTS) " points."
                    }
                    li {
                        "Par includes both ends of the range, and the reading is compared before rounding: "
                        "with Par 80.4–83.0°F, 83.0°F is Par and 83.1°F is Over."
                    }
                    li { "When a competition caps how many picks you make, its page says so beside the entry fee." }
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
                    "Readings come from NOAA's reports for each airport station. A daytime window has "
                    "highs and wind only; a night window has lows and wind only."
                }
            }

            section id="timing" class="content" {
                h2 { "Deadlines and results" }
                ul {
                    li {
                        strong { "Entries close when the window starts." }
                        " Every entry's picks become public then."
                    }
                    li {
                        "While the window is " strong { "live" } ", the leaderboard scores the readings so far "
                        "and can still change."
                    }
                    li {
                        "Once the window ends, the competition is " strong { "awaiting results" } ". "
                        "Stations report hourly and NOAA can take up to 24 hours to publish, so the oracle "
                        "signs the result at a set time after the window."
                    }
                    li {
                        "When the oracle signs, the competition is " strong { "finished" } ": the leaderboard "
                        "stops changing and payouts go out."
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
                        "A competition takes any number of entries and splits them into pools of up to 25 "
                        "when entries close. Each pool runs as its own competition, with its own pot and leaderboard."
                    }
                    li {
                        "A pool needs a minimum number of players. When Bitcoin network fees spike, that "
                        "minimum rises to 5 entries."
                    }
                    li { "If a competition or pool doesn't get enough entries, it doesn't run, and every entry fee is refunded." }
                }
            }

            section id="payouts" class="content" {
                h2 { "Paying and payouts" }
                ul {
                    li {
                        "You pay one entry fee over Lightning, all in. "
                        a href="#advanced" { "What's in it" } " is under How the tech works."
                    }
                    li {
                        "Winnings go to the Lightning Address on your account. Without one, you submit an "
                        "invoice on the Payouts page to collect."
                    }
                    li {
                        "If a competition doesn't run, your payment comes back: to your Lightning Address, "
                        "or, for a held payment, it is never collected and returns to your wallet."
                    }
                }
            }

            section id="advanced" class="content" {
                details {
                    summary { h2 class="is-inline" { "How the tech works" } }
                    h3 { "Where your sats go" }
                    ul {
                        li {
                            strong { "Pot contribution" } ": goes into the pot. Every entrant's contribution "
                            "together is what the winners share."
                        }
                        li {
                            strong { "Service fee" } ": a percentage of the pot contribution that runs the site. "
                            "It is not part of the pot."
                        }
                        li {
                            strong { "Network fee" } ": your share of the Bitcoin transaction fees the "
                            "competition's contract needs, estimated for a full pool at the fee rate when "
                            "your ticket is issued, and fixed from then on. If fees later rise, the site "
                            "absorbs the difference; if they fall, it keeps the surplus."
                        }
                        li {
                            "When the network fee would be too large a share of the entry, new tickets pause "
                            "until fees come down. Entries already taken are unaffected."
                        }
                    }
                    p {
                        "For example, an entry fee of 5,330 sats is 5,000 sats pot contribution + 150 sats "
                        "service fee (3%) + 180 sats network fee. Every entrant pays the same entry fee, so "
                        "five entrants make a 25,000-sat pot."
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

/// A minute's walk through entering, the leaderboard and a player's picks, recorded from these
/// pages (`just help-video`). It loads only when played; the sections below say it all in words.
fn video() -> Markup {
    html! {
        figure class="help-video" {
            video controls playsinline preload="none" width="540" height="1168"
                  poster=(HOW_IT_WORKS_JPG.url)
                  aria-label="How it works: entering a competition, then following the leaderboard and your picks" {
                source src=(HOW_IT_WORKS_MP4.url) type="video/mp4";
                a href=(HOW_IT_WORKS_MP4.url) { "Watch how it works" }
            }
            figcaption {
                "How it works, in under a minute."
                br;
                "Music: "
                a href="https://incompetech.com/music/royalty-free/index.html?isrc=USUAN1300010" {
                    "Local Forecast"
                }
                " by Kevin MacLeod (incompetech.com), "
                a href="https://creativecommons.org/licenses/by/4.0/" { "CC BY 4.0" }
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
            h2 class="title is-4" { "Entering a competition" }
            div class="help-walkthrough" {
                figure class="help-shot entry-form" inert aria-label="An example entry form" {
                    dl class="entry-facts" {
                        div { dt { "Entries close" (callout(1)) } dd { "Sep 30, 6:56 PM" } }
                        div { dt { "Entry fee" (callout(2)) } dd { "5,687 sats" } }
                        div { dt { "Win" (callout(3)) } dd { "30,000 sats" } }
                    }
                    fieldset class="station-picks" {
                        legend { "Chicago/O'Hare International, IL " span class="station-code" { "KORD" } (callout(4)) }
                        (pick_row("KORD", Metric::TempHigh, Some(77.0), Some(band)))
                        p class="help-pointer" { "↑ " (callout(5)) }
                    }
                    button type="button" class="button is-primary" { "Pay 5,687 sats and enter" (callout(6)) }
                }
                ol class="help-callouts content" {
                    li { "The deadline. Picks lock when the observation window starts." }
                    li { "What entering costs, all in. Tap it to see the fees it's made of." }
                    li { "What first place wins once the result is final." }
                    li { "Each airport and its forecast: here, a 77°F high at Chicago O'Hare." }
                    li {
                        "Your pick, left to right: Under, Par (73.4–76.0°F, both ends included) or Over. "
                        "Tap a chosen pick again to clear it; a skipped reading scores 0 and costs nothing."
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
    fn the_walkthrough_uses_the_real_pick_buttons_and_does_nothing() {
        let html = help_page().into_string();
        assert!(html.contains(r#"class="help-shot entry-form" inert"#));
        assert!(
            html.contains("&lt; 73.4°F")
                && html.contains("73.4–76.0°F")
                && html.contains("&gt; 76.0°F")
        );
        // Nothing the live form's script looks for.
        assert!(!html.contains("entryForm") && !html.contains("submitEntry"));
    }

    /// The video loads only when played, from this site's own hashed assets.
    #[test]
    fn the_video_waits_to_be_played() {
        let html = help_page().into_string();
        assert!(html.contains(r#"<video controls playsinline preload="none""#));
        assert!(html.contains(&format!(r#"poster="{}""#, HOW_IT_WORKS_JPG.url)));
        assert!(html.contains(&format!(
            r#"<source src="{}" type="video/mp4">"#,
            HOW_IT_WORKS_MP4.url
        )));
        assert!(!html.contains("autoplay"));
        assert!(!video().into_string().contains("muted"));
    }

    /// The video's music is credited as its CC BY 4.0 licence asks.
    #[test]
    fn the_video_credits_its_music() {
        let html = help_page().into_string();
        assert!(html.contains(
            r#"Music: <a href="https://incompetech.com/music/royalty-free/index.html?isrc=USUAN1300010">Local Forecast</a> by Kevin MacLeod (incompetech.com), <a href="https://creativecommons.org/licenses/by/4.0/">CC BY 4.0</a>"#
        ));
    }

    #[test]
    fn every_section_the_forms_link_to_is_here() {
        let html = help_page().into_string();
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
        assert!(html.contains("scores 0 and costs nothing"));
    }
}
