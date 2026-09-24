//! Each page's live part, rendered once whenever what it shows changes, and pushed to every
//! browser watching it.
//!
//! A page names its topic: `dashboard`, or `run:<id>` for one run. Events from the runner and the
//! rebalancer are gathered for a moment, the topics they touch are rendered once each, and the
//! rendered HTML goes out as server-sent events to the pages watching those topics, which swap it
//! in. Nobody watching a topic means it is not rendered, so a run's page costs the coordinator
//! nothing while no one has it open.

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use tokio::sync::broadcast::{self, error::RecvError};

use super::assets;
use super::routes::{dashboard_live, Dashboard};
use super::run_detail::run_live;
use crate::events::Event;

/// How long events are gathered before the topics they touch are rendered.
const BATCH: Duration = Duration::from_millis(300);

pub(super) const DASHBOARD: &str = "dashboard";

pub(super) fn run_topic(run_id: &str) -> String {
    format!("run:{run_id}")
}

/// One topic's live part, rendered.
#[derive(Clone)]
struct Fragment {
    topic: String,
    html: String,
}

/// Where rendered fragments go, and how many pages watch each topic.
#[derive(Clone)]
pub struct Live {
    fragments: broadcast::Sender<Fragment>,
    watchers: Arc<Mutex<HashMap<String, usize>>>,
}

impl Live {
    pub fn new() -> Self {
        Self {
            fragments: broadcast::channel(64).0,
            watchers: Arc::default(),
        }
    }

    fn watched(&self) -> Vec<String> {
        self.watchers
            .lock()
            .expect("watchers lock")
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(topic, _)| topic.clone())
            .collect()
    }

    fn is_watched(&self, topic: &str) -> bool {
        self.watchers
            .lock()
            .expect("watchers lock")
            .get(topic)
            .is_some_and(|count| *count > 0)
    }
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

/// A page watching a topic, counted while its stream is open.
struct Watching {
    live: Live,
    topic: String,
}

impl Watching {
    fn new(live: Live, topic: String) -> Self {
        *live
            .watchers
            .lock()
            .expect("watchers lock")
            .entry(topic.clone())
            .or_default() += 1;
        Self { live, topic }
    }
}

impl Drop for Watching {
    fn drop(&mut self) {
        if let Some(count) = self
            .live
            .watchers
            .lock()
            .expect("watchers lock")
            .get_mut(&self.topic)
        {
            *count = count.saturating_sub(1);
        }
    }
}

/// The topics an event changes.
fn topics_of(event: &Event) -> Vec<String> {
    match event {
        Event::RunStarted { run_id, .. }
        | Event::StepStarted { run_id, .. }
        | Event::StepFinished { run_id, .. }
        | Event::RunFinished { run_id, .. }
        | Event::TrailUpdated { run_id } => vec![DASHBOARD.to_string(), run_topic(run_id)],
        Event::Rebalanced => vec![DASHBOARD.to_string()],
    }
}

/// Render the topics each batch of events touches, and push them to their watchers.
pub async fn render_changes(state: Dashboard) {
    let mut events = state.runner.events().subscribe();
    loop {
        let mut topics = BTreeSet::new();
        match events.recv().await {
            Ok(event) => topics.extend(topics_of(&event)),
            // Too much happened to say what; render everything someone is watching.
            Err(RecvError::Lagged(_)) => topics.extend(state.live.watched()),
            Err(RecvError::Closed) => return,
        }
        tokio::time::sleep(BATCH).await;
        loop {
            match events.try_recv() {
                Ok(event) => topics.extend(topics_of(&event)),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    topics.extend(state.live.watched())
                }
                Err(_) => break,
            }
        }
        for topic in topics {
            if !state.live.is_watched(&topic) {
                continue;
            }
            if let Some(live) = render(&state, &topic).await {
                let _ = state.live.fragments.send(Fragment {
                    topic,
                    html: live.into_string(),
                });
            }
        }
    }
}

async fn render(state: &Dashboard, topic: &str) -> Option<Markup> {
    match topic.strip_prefix("run:") {
        Some(run_id) => run_live(state, run_id).await,
        None if topic == DASHBOARD => Some(dashboard_live(state).await),
        None => None,
    }
}

#[derive(Deserialize)]
pub(super) struct TopicParams {
    topic: String,
}

/// The rendered live part of one topic, each time it changes.
pub(super) async fn stream(
    State(state): State<Dashboard>,
    Query(TopicParams { topic }): Query<TopicParams>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let watching = Watching::new(state.live.clone(), topic);
    let fragments = state.live.fragments.subscribe();
    let stream = futures::stream::unfold(
        (fragments, watching),
        |(mut fragments, watching)| async move {
            loop {
                match fragments.recv().await {
                    Ok(fragment) if fragment.topic == watching.topic => {
                        let event = SseEvent::default().event("live").data(fragment.html);
                        return Some((Ok(event), (fragments, watching)));
                    }
                    Ok(_) | Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// A page whose `live` part is kept current. `header` stays put; `live` is swapped in whole.
pub(super) fn page(title: &str, topic: &str, header: Markup, live: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                title { (title) }
                link rel="stylesheet" href=(assets::CSS_URL);
                script src=(assets::JS_URL) defer {}
            }
            body {
                (header)
                main #live data-topic=(topic) { (live) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_changes_the_dashboard_and_its_own_page() {
        let step = Event::StepFinished {
            run_id: "r1".into(),
            step: "create_competition".into(),
            passed: true,
        };
        assert_eq!(topics_of(&step), ["dashboard", "run:r1"]);
        assert_eq!(topics_of(&Event::Rebalanced), ["dashboard"]);
        let traced = Event::TrailUpdated {
            run_id: "r1".into(),
        };
        assert_eq!(topics_of(&traced), ["dashboard", "run:r1"]);
    }

    /// A topic is rendered only while a page watches it, and stops once the page goes.
    #[test]
    fn a_topic_is_watched_only_while_a_page_is_open() {
        let live = Live::new();
        assert!(!live.is_watched("run:r1"));
        let first = Watching::new(live.clone(), "run:r1".into());
        let second = Watching::new(live.clone(), "run:r1".into());
        drop(first);
        assert!(live.is_watched("run:r1"), "one page still has it open");
        drop(second);
        assert!(!live.is_watched("run:r1"));
        assert!(live.watched().is_empty());
    }
}
