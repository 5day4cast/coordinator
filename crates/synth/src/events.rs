//! What is happening, as it happens, for pages watching over server-sent events.

use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    RunStarted {
        run_id: String,
        scenario: String,
    },
    StepStarted {
        run_id: String,
        step: String,
    },
    /// Sent once the step is saved, so a page that reloads on it shows the step.
    StepFinished {
        run_id: String,
        step: String,
        passed: bool,
    },
    RunFinished {
        run_id: String,
        passed: bool,
    },
    Rebalanced,
    /// Synth learned more about where a run's money went, which carries on after the run ends:
    /// attestation, payouts, refunds.
    TrailUpdated {
        run_id: String,
    },
}

/// Where events go. Nobody listening is not an error: events are only for pages.
#[derive(Clone)]
pub struct Events(broadcast::Sender<Event>);

impl Events {
    pub fn new() -> Self {
        Self(broadcast::channel(256).0)
    }

    pub fn send(&self, event: Event) {
        let _ = self.0.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.subscribe()
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}
