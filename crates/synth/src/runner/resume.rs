//! Carry on a run a restart cut short, from the last step it finished.
//!
//! A run's plan, its competition and each step's details are saved as it goes, so a restarted
//! synth can take a run up where it was: its finished steps stand as they were saved, and the
//! steps after them run again. Those are waits on the coordinator, which answer at once for a
//! state already reached, and entries that had not begun. Nothing is paid twice: an entry that
//! reached its payment is never run again; synth asks the coordinator what became of its ticket
//! instead.

use std::collections::{HashMap, VecDeque};

use time::OffsetDateTime;
use uuid::Uuid;

use crate::db::{TestRun, TestStep};
use crate::scenarios::{self, ScenarioConfig, StepResult, StepStatus};
use crate::trail::EntryTrace;

/// What a resumed run did before the restart.
#[derive(Debug, Clone)]
pub struct Prior {
    /// When the run first started.
    pub started_at: OffsetDateTime,
    steps: Vec<TestStep>,
}

impl Prior {
    pub fn new(started_at: OffsetDateTime, steps: Vec<TestStep>) -> Self {
        Self { started_at, steps }
    }

    /// The last saved row of step `name`.
    pub fn step(&self, name: &str) -> Option<&TestStep> {
        self.steps.iter().rev().find(|step| step.step_name == name)
    }

    /// The steps still running at the restart, by name, and their rows: each finishes in its
    /// own row when it runs again.
    pub fn open_rows(&self) -> HashMap<String, String> {
        self.steps
            .iter()
            .filter(|step| !finished(&step.status))
            .map(|step| (step.step_name.clone(), step.id.clone()))
            .collect()
    }

    /// The finished steps, to be recognised when they are checked again.
    pub fn replay(&self) -> Replay {
        let mut done: HashMap<String, VecDeque<bool>> = HashMap::new();
        for step in self.steps.iter().filter(|step| finished(&step.status)) {
            done.entry(step.step_name.clone())
                .or_default()
                .push_back(step.status == "failed");
        }
        Replay { done }
    }
}

/// Whether a saved step had finished: passed or failed. A step still running, or marked
/// interrupted by an older synth, had not.
fn finished(status: &str) -> bool {
    matches!(status, "passed" | "failed")
}

/// The steps a resumed run finished before the restart. A step that runs again and ends the way
/// it ended then is not saved a second time.
#[derive(Debug, Default)]
pub struct Replay {
    /// Whether each finished row of a step failed, by name, in the order they were saved.
    done: HashMap<String, VecDeque<bool>>,
}

impl Replay {
    /// Whether a step named `name` that ended failed or not repeats one saved before the
    /// restart. Each saved row is repeated at most once, so a step that runs more than once in
    /// a run is matched row by row.
    pub fn repeats(&mut self, name: &str, failed: bool) -> bool {
        let Some(rows) = self.done.get_mut(name) else {
            return false;
        };
        if rows.front() != Some(&failed) {
            return false;
        }
        rows.pop_front();
        true
    }

    /// Whether step `name` finished before the restart, and has not been repeated yet.
    pub fn finished(&self, name: &str) -> bool {
        self.done.get(name).is_some_and(|rows| !rows.is_empty())
    }
}

/// Where a run left off, as far as its saved rows tell.
#[derive(Debug, Clone)]
pub struct ResumePoint {
    pub config: ScenarioConfig,
    pub competition_id: Uuid,
    /// Whether the step creating the competition finished. If not, the coordinator is asked
    /// whether the competition exists before the run carries on.
    pub created: bool,
}

/// The plan a recorded run executes, and its competition: what [`super::Runner::run_recorded`]
/// checks before running it, and what a resumed run starts from.
pub fn recorded_plan(run: &TestRun) -> anyhow::Result<(ScenarioConfig, Uuid)> {
    let config: ScenarioConfig = serde_json::from_str(
        run.config_json
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Recorded run has no resolved plan"))?,
    )?;
    anyhow::ensure!(
        config.seed.is_some() && config.entry_plan.len() == config.users,
        "Recorded run has no resolved entry plan"
    );
    anyhow::ensure!(
        config.planned_scenario.as_deref() == Some(run.scenario.as_str()),
        "Recorded plan belongs to another scenario"
    );
    let competition_id = config
        .competition_id
        .ok_or_else(|| anyhow::anyhow!("Recorded plan has no competition id"))?;
    Ok((config, competition_id))
}

/// Whether a run the last shutdown left running can carry on, and from where; if not, why.
pub fn resume_point(run: &TestRun, steps: &[TestStep]) -> Result<ResumePoint, String> {
    if !resumable(&run.scenario) {
        return Err(format!(
            "a {} run is not resumed after a restart",
            run.scenario
        ));
    }
    let (config, competition_id) =
        recorded_plan(run).map_err(|error| format!("its plan cannot be read: {error:#}"))?;
    let created = steps
        .iter()
        .rev()
        .find(|step| step.step_name == scenarios::REFUSED_AS_SMALL_STEP)
        .is_some_and(|step| step.status == "passed");
    Ok(ResumePoint {
        config,
        competition_id,
        created,
    })
}

/// The scenarios whose steps carry on after a restart: every one whose players enter through the
/// shared entry flow. A stress burst and an operator's competition are not taken up again.
fn resumable(scenario: &str) -> bool {
    super::SCENARIOS.contains(&scenario) && scenario != scenarios::stress::STRESS_FULL_POOL
}

/// What a resumed run does about an entry step it had saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryResume {
    /// It never began, or began without reaching its payment: run it from the start.
    Run,
    /// It finished: keep what it recorded.
    Finished,
    /// It reached its payment without recording how it ended. Never pay again: ask the
    /// coordinator what became of its ticket.
    Reconcile,
}

/// What to do about an entry step saved with `status` and `trace` before the restart.
pub fn entry_resume(status: Option<&str>, trace: Option<&EntryTrace>) -> EntryResume {
    match (status, trace) {
        (None, _) => EntryResume::Run,
        (Some(status), _) if finished(status) => EntryResume::Finished,
        (Some(_), Some(trace))
            if trace.paid || trace.settled_by_test_endpoint || trace.may_have_paid() =>
        {
            EntryResume::Reconcile
        }
        (Some(_), _) => EntryResume::Run,
    }
}

/// How an entry that reached its payment before the restart ends, once the coordinator has said
/// whether its ticket is paid (`trace.paid`), and why, in words for its step.
pub fn reconciled(trace: &EntryTrace) -> (StepStatus, &'static str) {
    let abandons = trace.behavior == Some(scenarios::EntryBehavior::AbandonPaid);
    match (trace.paid, trace.entry_submitted) {
        (true, true) => (
            StepStatus::Passed,
            "paid and submitted before synth restarted",
        ),
        (true, false) if abandons => (
            StepStatus::Passed,
            "paid before synth restarted, and abandoned as planned",
        ),
        (true, false) => (
            StepStatus::Skipped,
            "paid before synth restarted; the entry was not submitted after it, so the ticket \
             waits for its refund",
        ),
        (false, _) => (
            StepStatus::Skipped,
            "its payment was in flight when synth restarted and the coordinator has not seen it \
             paid; it was not paid again",
        ),
    }
}

/// An entry step that finished before the restart, as it was saved.
pub fn finished_entry(row: &TestStep, trace: &EntryTrace) -> StepResult {
    let status = if trace.seat_taken {
        StepStatus::Skipped
    } else if row.status == "failed" {
        StepStatus::Failed
    } else {
        StepStatus::Passed
    };
    StepResult {
        name: row.step_name.clone(),
        status,
        duration_ms: row.duration_ms.unwrap_or_default(),
        details: serde_json::to_value(trace).ok(),
        error: row.error_message.clone(),
    }
}

/// A step that finished before the restart, as it was saved, with its details.
pub fn finished_step(row: &TestStep) -> StepResult {
    StepResult {
        name: row.step_name.clone(),
        status: if row.status == "failed" {
            StepStatus::Failed
        } else {
            StepStatus::Passed
        },
        duration_ms: row.duration_ms.unwrap_or_default(),
        details: row
            .details_json
            .as_deref()
            .and_then(|details| serde_json::from_str(details).ok()),
        error: row.error_message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::EntryBehavior;

    fn row(name: &str, status: &str) -> TestStep {
        TestStep {
            id: Uuid::now_v7().to_string(),
            run_id: "run".into(),
            step_name: name.into(),
            status: status.into(),
            started_at: "2026-10-03T10:36:25Z".into(),
            completed_at: None,
            duration_ms: Some(5),
            details_json: None,
            error_message: (status == "failed").then(|| "it failed".into()),
        }
    }

    fn run(scenario: &str, config: Option<&ScenarioConfig>) -> TestRun {
        TestRun {
            id: "run".into(),
            scenario: scenario.into(),
            status: "running".into(),
            started_at: "2026-10-03T10:36:25Z".into(),
            completed_at: None,
            error_message: None,
            config_json: config.map(|config| serde_json::to_string(config).unwrap()),
            competition_id: None,
            money: None,
        }
    }

    fn plan(scenario: &str) -> ScenarioConfig {
        let mut config = ScenarioConfig::default().resolve_plan(scenario).unwrap();
        config.competition_id = Some(Uuid::now_v7());
        config
    }

    #[test]
    fn a_run_resumes_from_its_plan_unless_it_cannot() {
        let config = plan("escrow_refund");
        let created = [
            row("prepare_user_behavior", "passed"),
            row("create_competition", "passed"),
            row("user_alice_enter", "running"),
        ];
        let point = resume_point(&run("escrow_refund", Some(&config)), &created).unwrap();
        assert_eq!(point.competition_id, config.competition_id.unwrap());
        assert!(point.created);
        // Cut short while creating: the coordinator is asked before the run carries on.
        let point = resume_point(
            &run("escrow_refund", Some(&config)),
            &[row("create_competition", "running")],
        )
        .unwrap();
        assert!(!point.created);

        let unsaved = resume_point(&run("escrow_refund", None), &created).unwrap_err();
        assert!(unsaved.contains("plan cannot be read"), "{unsaved}");
        let other = resume_point(&run("late_submission", Some(&config)), &created).unwrap_err();
        assert!(other.contains("another scenario"), "{other}");
        let stress = resume_point(&run("stress_full_pool", Some(&config)), &created).unwrap_err();
        assert_eq!(
            stress,
            "a stress_full_pool run is not resumed after a restart"
        );
        assert!(resume_point(&run("manual_competition", Some(&config)), &created).is_err());
    }

    /// An entry is run again only if it never reached its payment; one that may have paid is
    /// reconciled with the coordinator, never paid again.
    #[test]
    fn an_entry_that_reached_its_payment_is_never_run_again() {
        let mut trace = EntryTrace {
            user: "alice".into(),
            payment_started: Some(false),
            ..EntryTrace::default()
        };
        assert_eq!(entry_resume(None, None), EntryResume::Run);
        assert_eq!(entry_resume(Some("running"), None), EntryResume::Run);
        assert_eq!(
            entry_resume(Some("running"), Some(&trace)),
            EntryResume::Run,
            "waiting to arrive, or holding a ticket it never paid"
        );
        trace.ticket_id = Some(Uuid::now_v7());
        trace.payment_hash = Some("hash".into());
        assert_eq!(
            entry_resume(Some("running"), Some(&trace)),
            EntryResume::Run
        );
        // The intent is saved before the payment leaves.
        trace.payment_started = Some(true);
        assert_eq!(
            entry_resume(Some("running"), Some(&trace)),
            EntryResume::Reconcile
        );
        assert_eq!(
            entry_resume(Some("interrupted"), Some(&trace)),
            EntryResume::Reconcile
        );
        trace.paid = true;
        assert_eq!(
            entry_resume(Some("running"), Some(&trace)),
            EntryResume::Reconcile
        );
        assert_eq!(
            entry_resume(Some("passed"), Some(&trace)),
            EntryResume::Finished
        );
        assert_eq!(entry_resume(Some("failed"), None), EntryResume::Finished);
    }

    #[test]
    fn a_reconciled_entry_says_what_became_of_its_payment() {
        let mut trace = EntryTrace {
            user: "alice".into(),
            payment_started: Some(true),
            ..EntryTrace::default()
        };
        let (status, why) = reconciled(&trace);
        assert_eq!(status, StepStatus::Skipped);
        assert!(why.contains("not paid again"), "{why}");
        trace.paid = true;
        assert_eq!(reconciled(&trace).0, StepStatus::Skipped);
        trace.behavior = Some(EntryBehavior::AbandonPaid);
        assert_eq!(reconciled(&trace).0, StepStatus::Passed);
        trace.behavior = Some(EntryBehavior::Complete);
        trace.entry_submitted = true;
        assert_eq!(reconciled(&trace).0, StepStatus::Passed);
    }

    /// Finished steps are not saved again when their checks pass again; one that now ends
    /// differently is, and so is a step's later run under the same name.
    #[test]
    fn finished_steps_are_recognised_once_each() {
        let prior = Prior::new(
            OffsetDateTime::now_utc(),
            vec![
                row("wait_pools_funding_confirmed", "passed"),
                row("pool_0_kickoff_failed", "passed"),
                row("wait_pools_funding_confirmed", "passed"),
                row("refund_alice", "failed"),
                row("wait_pools_awaiting_attestation", "running"),
            ],
        );
        let mut replay = prior.replay();
        assert!(replay.finished("wait_pools_funding_confirmed"));
        assert!(replay.repeats("wait_pools_funding_confirmed", false));
        assert!(replay.repeats("wait_pools_funding_confirmed", false));
        assert!(!replay.repeats("wait_pools_funding_confirmed", false));
        assert!(!replay.finished("wait_pools_funding_confirmed"));
        assert!(!replay.repeats("refund_alice", false), "now it passed");
        assert!(replay.repeats("refund_alice", true));
        assert!(!replay.repeats("wait_pools_awaiting_attestation", false));
        let open = prior.open_rows();
        assert_eq!(open.len(), 1);
        assert!(open.contains_key("wait_pools_awaiting_attestation"));
    }
}
