use std::sync::Arc;

use axum::{extract::State, response::Html};

use crate::{
    startup::AppState,
    templates::{
        layouts::base::{base, PageConfig},
        pages::competitions::{competitions_page, CompetitionView},
    },
};

pub async fn public_page_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let config = PageConfig {
        title: "Fantasy Weather",
        api_base: &state.remote_url,
        oracle_base: &state.oracle_url,
        network: &state.bitcoin.network.to_string(),
    };

    let competitions = match state.coordinator.get_competitions().await {
        Ok(comps) => comps
            .into_iter()
            .map(|c| CompetitionView {
                id: c.id.to_string(),
                start_time: c.event_submission.observation_date.to_string(),
                end_time: c.event_submission.observation_date.to_string(),
                signing_time: c.event_submission.signing_date.to_string(),
                status: format!("{:?}", c.current_state),
                entry_fee: c.event_submission.entry_fee,
                total_pool: c.event_submission.total_competition_pool,
                total_entries: c.event_submission.number_of_players_signed_up,
                num_winners: c.event_submission.number_of_places_win as u64,
                can_enter: c.can_enter(),
            })
            .collect(),
        Err(_) => vec![],
    };

    let content = competitions_page(&competitions);
    Html(base(&config, content).into_string())
}
