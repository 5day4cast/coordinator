//! Export the player pages (home, entry form, a queued competition's entry form, help, live
//! leaderboard, picks dialog) with synthetic data and the bundled assets, for offline
//! screenshots and browser QA.
//! cargo run -p coordinator --example player_ui_fixtures -- /tmp/player-ui
//! Serve the directory (python3 -m http.server) and open `<page>-<theme>.html`. Scripts are left
//! out so nothing loads or rewrites the page: each file is exactly what the server renders.
use coordinator::{
    domain::{
        leaderboard::{Metric, Phase, PickProgress, PickState, Rule},
        RefundProgress,
    },
    infra::oracle::{ScoringRules, ValueOptions},
    templates::{
        assets,
        fragments::{
            entry_form::{entry_form, Forecasts, NetworkFee, PayoutDestination, StationForecast},
            leaderboard::{leaderboard, leaderboard_scores, LeaderboardRow, LeaderboardView},
            picks::{picks_detail, PickView},
        },
        layouts::base::{base, PageConfig},
        pages::{
            competitions::{competitions_page, CompetitionView, ListOptions, Queue, QueueView},
            help_page,
        },
        shared_map::{lat_lon_to_svg, StationPin},
    },
};
use maud::{html, Markup, PreEscaped};
use std::{error::Error, fs, path::PathBuf};
use time::{Duration, OffsetDateTime};

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(std::env::args().nth(1).ok_or("expected output directory")?);
    fs::create_dir_all(output.join("assets"))?;
    for asset in assets::ALL {
        fs::write(output.join(asset.url.trim_start_matches('/')), asset.bytes)?;
    }
    let now = OffsetDateTime::now_utc();

    let open = competition("open", Phase::Upcoming, now + Duration::hours(5), 4);
    let mut queued = competition("queued", Phase::Upcoming, now + Duration::hours(9), 6);
    queued.queue = Queue::Queued(QueueView {
        min_players: Some(2),
        max_players: 25,
        entries: Some(6),
        max_entries: None,
        pools: vec![],
    });
    let live = competition("live", Phase::Live, now - Duration::hours(2), 6);
    let finished = competition("done", Phase::Scored, now - Duration::days(2), 6);

    let stations = vec![
        station("KORD", "Chicago/O'Hare International, IL", 77.0, 55.0, 7.0),
        station(
            "KJFK",
            "New York/JF Kennedy International, NY",
            73.0,
            59.0,
            8.0,
        ),
    ];
    // The pins sit beside the stations' picks; a queued competition's form adds a "?" to
    // its Win and Entries.
    let forecasts = Forecasts::Ready {
        stations,
        pins: vec![
            pin(
                "KORD",
                "ORD",
                "Chicago/O'Hare International, IL",
                41.98,
                -87.90,
            ),
            pin(
                "KJFK",
                "JFK",
                "New York/JF Kennedy International, NY",
                40.64,
                -73.78,
            ),
        ],
    };
    let payout = PayoutDestination::Address("player@lightning.example".into());
    let entry = entry_form(&open, &forecasts, None, &payout, NetworkFee::Estimate(437));
    let entry_queued = entry_form(
        &queued,
        &forecasts,
        None,
        &payout,
        NetworkFee::Estimate(437),
    );

    let rows: Vec<LeaderboardRow> = [
        "npub1pg5z…wz89",
        "npub1v00n…7ef8",
        "npub1z4vy…v9un",
        "test1",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, player)| LeaderboardRow {
        rank: 1,
        entry_id: format!("01a0d0f5-0000-7000-8000-00000000000{index}"),
        player: player.into(),
        owner: format!("owner-{index}"),
        score: 0,
    })
    .collect();
    let board = LeaderboardView {
        rows,
        phase: Phase::Live,
        updated_at: Some(now - Duration::minutes(2)),
        any_readings: false,
        unverified: false,
    };
    // Rows arrive after the page loads; here they are put where they land, the last one
    // marked as the viewer's own as page.js would.
    let mut shown = live.clone();
    shown.total_entries = 0;
    let scores = leaderboard_scores(&live, &board, now)
        .into_string()
        .replace(
            r#"<tr class="is-clickable" data-owner="owner-3""#,
            r#"<tr class="is-clickable is-own" data-owner="owner-3""#,
        );
    let board_page = leaderboard(&shown, now)
        .into_string()
        .replace(r#"<p class="empty-state">No entries yet.</p>"#, &scores);

    let picks: Vec<PickProgress> = vec![
        pick(
            "KDEN",
            Metric::TempHigh,
            ValueOptions::Over,
            66.0,
            (-3.6, -1.0),
            None,
        ),
        pick(
            "KDEN",
            Metric::TempLow,
            ValueOptions::Over,
            55.0,
            (0.4, 4.0),
            None,
        ),
        pick(
            "KDEN",
            Metric::WindSpeed,
            ValueOptions::Under,
            11.0,
            (0.5, 2.5),
            None,
        ),
        pick(
            "KJFK",
            Metric::TempHigh,
            ValueOptions::Under,
            73.0,
            (-3.6, -1.0),
            Some(71.0),
        ),
        pick(
            "KJFK",
            Metric::TempLow,
            ValueOptions::Par,
            59.0,
            (0.4, 4.0),
            Some(61.0),
        ),
    ];
    let views: Vec<PickView> = picks
        .iter()
        .map(|pick| PickView {
            pick,
            station_name: Some(match pick.station_id.as_str() {
                "KDEN" => "Denver International, CO".into(),
                _ => "New York/JF Kennedy International, NY".into(),
            }),
        })
        .collect();
    let dialog = html! {
        div class="modal is-active" {
            div class="modal-background" {}
            div class="modal-content" {
                div class="box" id="entryValues" {
                    (picks_detail("01a0d0f5-0000-7000-8000-000086708c73", &views, Phase::Live,
                        Some(now - Duration::minutes(1)), now))
                }
            }
        }
    };

    let pages: [(&str, Markup); 6] = [
        (
            "home",
            competitions_page(
                &[open.clone(), queued.clone(), live.clone(), finished],
                ListOptions::default(),
                now,
            ),
        ),
        ("entry", entry),
        ("entry-queued", entry_queued),
        ("help", help_page(false)),
        ("leaderboard", PreEscaped(board_page)),
        ("picks", html! { (PreEscaped(scores)) (dialog) }),
    ];
    let config = PageConfig {
        title: "Offline player UI QA",
        api_base: "",
        oracle_base: "",
        network: "signet",
        wasm_version: "offline-fixture",
    };
    for (name, content) in pages {
        let page = without_scripts(&base(&config, content).into_string());
        for theme in ["light", "dark"] {
            let themed = page.replacen(
                r#"<html lang="en">"#,
                &format!(r#"<html lang="en" data-theme="{theme}">"#),
                1,
            );
            fs::write(output.join(format!("{name}-{theme}.html")), themed)?;
        }
    }
    Ok(())
}

fn without_scripts(page: &str) -> String {
    let mut out = String::with_capacity(page.len());
    let mut rest = page;
    while let Some(start) = rest.find("<script") {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find("</script>")
            .map_or(rest.len(), |end| start + end + "</script>".len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn competition(id: &str, phase: Phase, start: OffsetDateTime, entries: u64) -> CompetitionView {
    CompetitionView {
        id: id.into(),
        phase,
        start,
        end: start + Duration::DAY,
        signing: start + Duration::DAY + Duration::minutes(5),
        expiry: None,
        entry_fee: 5_000,
        ticket_price: 5_150,
        // What a ticket issued now adds, as the pages get it from the fee estimate.
        network_fee: Some(437),
        service_fee_percent: "3%".into(),
        total_pool: 30_000,
        total_entries: entries,
        total_allowed_entries: 6,
        paid_places: 1,
        can_enter: phase == Phase::Upcoming,
        number_of_values_per_entry: 6,
        max_entries_per_player: 1,
        locations: vec!["KORD".into(), "KJFK".into()],
        scoring_rules: ScoringRules::Lines,
        metrics: Metric::ALL.to_vec(),
        window_shape: None,
        refunds: RefundProgress::default(),
        pot_refunded: false,
        refund_shares: None,
        queue: Queue::Single,
        unlisted: false,
        funding: None,
    }
}

fn station(id: &str, name: &str, high: f64, low: f64, wind: f64) -> StationForecast {
    StationForecast {
        station_id: id.into(),
        station_name: Some(name.into()),
        forecasts: vec![
            (Metric::TempHigh, Some(high), Some(line(-3.6, -1.0))),
            (Metric::TempLow, Some(low), Some(line(0.4, 4.0))),
            (Metric::WindSpeed, Some(wind), Some(line(0.5, 2.5))),
        ],
    }
}

fn pin(id: &str, label: &str, name: &str, lat: f64, lon: f64) -> StationPin {
    let (svg_x, svg_y) = lat_lon_to_svg(lat, lon).expect("in the continental US");
    StationPin {
        station_id: id.into(),
        label: label.into(),
        name: name.into(),
        svg_x,
        svg_y,
    }
}

fn line(lower: f64, upper: f64) -> Rule {
    Rule::Line { lower, upper }
}

fn pick(
    station: &str,
    metric: Metric,
    choice: ValueOptions,
    forecast: f64,
    (lower, upper): (f64, f64),
    observed: Option<f64>,
) -> PickProgress {
    let hit = observed.is_some_and(|value| match choice {
        ValueOptions::Under => value < forecast + lower,
        ValueOptions::Par => (forecast + lower..=forecast + upper).contains(&value),
        ValueOptions::Over => value > forecast + upper,
    });
    PickProgress {
        station_id: station.into(),
        metric,
        pick: choice,
        rule: Some(line(lower, upper)),
        forecast: Some(forecast),
        observed,
        state: match (observed, hit) {
            (None, _) => PickState::Pending,
            (Some(_), true) => PickState::OnTrack,
            (Some(_), false) => PickState::OffTrack,
        },
        points: if hit { 10 } else { 0 },
        hit,
        hours_covered: 9.0,
        hours_total: 24.0,
    }
}
