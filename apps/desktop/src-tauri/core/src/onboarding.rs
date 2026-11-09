//! The first-run setup checklist's saved answers (app_config `onboarding.steps`). The GUI
//! decides when a step is done from live state (threads indexed, an agent connected,
//! distillation on, semantic coverage) and records it here, so a step stays done when that
//! state later changes, and a skipped step stays skipped across launches. GUI only: `cal`,
//! the hooks and the MCP server never read it.

use crate::knowledge::app_config;
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub const STEPS_KEY: &str = "onboarding.steps";

/// The checklist steps, in display order. `report` is done once the Report view has been
/// opened (the view records it), and skipped when its Today card is dismissed.
pub const STEPS: [&str; 5] = ["index", "agents", "memory", "semantic", "report"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    Done,
    Skipped,
}

/// Each step's saved state; None means not answered yet. Serialized with every key present
/// (as null when unanswered), the shape the frontend reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnboardingSteps {
    pub index: Option<StepState>,
    pub agents: Option<StepState>,
    pub memory: Option<StepState>,
    pub semantic: Option<StepState>,
    pub report: Option<StepState>,
}

impl OnboardingSteps {
    fn slot(&mut self, step: &str) -> Option<&mut Option<StepState>> {
        Some(match step {
            "index" => &mut self.index,
            "agents" => &mut self.agents,
            "memory" => &mut self.memory,
            "semantic" => &mut self.semantic,
            "report" => &mut self.report,
            _ => return None,
        })
    }
}

/// The saved states. A missing or malformed value reads as nothing answered, so a bad write
/// can only bring the checklist back, never break the app.
pub fn get(conn: &Connection) -> Result<OnboardingSteps> {
    Ok(app_config::get(conn, STEPS_KEY)?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default())
}

/// Record `state` ("done", "skipped", or None to clear) for `step` and return every step.
/// Indexing cannot be skipped: nothing else works on an empty index, and the checklist always
/// shows while it is empty.
pub fn set_step(conn: &Connection, step: &str, state: Option<&str>) -> Result<OnboardingSteps> {
    let state = match state {
        None => None,
        Some("done") => Some(StepState::Done),
        Some("skipped") => Some(StepState::Skipped),
        Some(other) => {
            bail!("unknown checklist state {other:?}: use \"done\", \"skipped\" or null")
        }
    };
    if step == "index" && state == Some(StepState::Skipped) {
        bail!("indexing can't be skipped: the other steps need an index");
    }
    let mut steps = get(conn)?;
    let Some(slot) = steps.slot(step) else {
        bail!(
            "unknown checklist step {step:?}: expected one of {}",
            STEPS.join(", ")
        );
    };
    *slot = state;
    app_config::put(conn, STEPS_KEY, &serde_json::to_string(&steps)?)?;
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::digest_test_support::temp_db;

    #[test]
    fn unset_reads_as_nothing_answered_with_every_key_present() {
        let conn = temp_db("onboarding_unset");
        let steps = get(&conn).unwrap();
        assert_eq!(steps, OnboardingSteps::default());
        // The frontend reads every key, so unanswered ones serialize as null.
        assert_eq!(
            serde_json::to_value(&steps).unwrap(),
            serde_json::json!({ "index": null, "agents": null, "memory": null, "semantic": null, "report": null })
        );
    }

    #[test]
    fn set_step_round_trips_and_clears() {
        let conn = temp_db("onboarding_round_trip");
        set_step(&conn, "index", Some("done")).unwrap();
        set_step(&conn, "agents", Some("skipped")).unwrap();
        set_step(&conn, "semantic", Some("done")).unwrap();
        let steps = set_step(&conn, "report", Some("done")).unwrap();
        assert_eq!(steps, get(&conn).unwrap());
        assert_eq!(
            serde_json::to_value(&steps).unwrap(),
            serde_json::json!({ "index": "done", "agents": "skipped", "memory": null, "semantic": "done", "report": "done" })
        );
        // Reopening a skipped step clears it.
        let steps = set_step(&conn, "agents", None).unwrap();
        assert_eq!(steps.agents, None);
        assert_eq!(steps.index, Some(StepState::Done));
    }

    #[test]
    fn rejects_unknown_steps_states_and_a_skipped_index() {
        let conn = temp_db("onboarding_reject");
        assert!(set_step(&conn, "telemetry", Some("done")).is_err());
        assert!(set_step(&conn, "memory", Some("later")).is_err());
        assert!(set_step(&conn, "index", Some("skipped")).is_err());
        // Nothing was written by the refused calls.
        assert_eq!(app_config::get(&conn, STEPS_KEY).unwrap(), None);
    }

    #[test]
    fn a_malformed_value_reads_as_nothing_answered() {
        let conn = temp_db("onboarding_malformed");
        app_config::put(&conn, STEPS_KEY, "{not json").unwrap();
        assert_eq!(get(&conn).unwrap(), OnboardingSteps::default());
        // A partial object (an older or newer app wrote fewer keys) keeps what it has.
        app_config::put(&conn, STEPS_KEY, r#"{"memory":"skipped","extra":1}"#).unwrap();
        let steps = get(&conn).unwrap();
        assert_eq!(steps.memory, Some(StepState::Skipped));
        assert_eq!(steps.index, None);
        // And the next write repairs it.
        set_step(&conn, "index", Some("done")).unwrap();
        assert_eq!(get(&conn).unwrap().memory, Some(StepState::Skipped));
    }
}
