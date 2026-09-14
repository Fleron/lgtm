//! Pure tracker layer: config persistence, status→column mapping and
//! urgency scoring. Nothing here knows about `ReviewApp` or gpui.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Which of the tracker's three columns a project Status option belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Column {
    Queue,
    Flight,
    Hidden,
}

/// Which issues the tracker shows, by assignee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssigneeFilter {
    Me,
    MeAndUnassigned,
    All,
}

/// Taskwarrior-style urgency coefficients, one per term, mapped to GitHub data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Coefficients {
    pub(crate) due: f64,
    pub(crate) blocking: f64,
    pub(crate) priority_high: f64,
    pub(crate) priority_medium: f64,
    pub(crate) priority_low: f64,
    pub(crate) active: f64,
    pub(crate) age: f64,
    pub(crate) milestone: f64,
    pub(crate) labels: f64,
    pub(crate) comments: f64,
    pub(crate) blocked: f64,
}

impl Default for Coefficients {
    fn default() -> Self {
        Coefficients {
            due: 12.0,
            blocking: 8.0,
            priority_high: 6.0,
            priority_medium: 3.9,
            priority_low: 1.8,
            active: 4.0,
            age: 2.0,
            milestone: 10.0,
            labels: 1.0,
            comments: 1.0,
            blocked: -5.0,
        }
    }
}

fn default_assignee_filter() -> AssigneeFilter {
    AssigneeFilter::Me
}

fn default_type_colors() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Bug".to_string(), "red".to_string()),
        ("Feature".to_string(), "blue".to_string()),
        ("Task".to_string(), "peach".to_string()),
        ("Epic".to_string(), "mauve".to_string()),
    ])
}

/// Persisted at `~/.cache/lgtm/tracker.json`, same ad-hoc JSON pattern as
/// `subscriptions.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TrackerConfig {
    #[serde(default)]
    pub(crate) status_columns: BTreeMap<String, Column>,
    #[serde(default)]
    pub(crate) coefficients: Coefficients,
    #[serde(default = "default_assignee_filter")]
    pub(crate) assignee_filter: AssigneeFilter,
    #[serde(default)]
    pub(crate) type_colors: BTreeMap<String, String>,
}

impl TrackerConfig {
    /// Seed a fresh config from a board's Status options: Backlog/Todo/Ready
    /// go to the queue, Done/Merged are hidden, everything else (including
    /// options the seeder doesn't recognize) is in flight.
    pub(crate) fn seed(status_options: &[String]) -> Self {
        let status_columns = status_options
            .iter()
            .map(|option| {
                let column = match option.as_str() {
                    "Backlog" | "Todo" | "Ready" => Column::Queue,
                    "Done" | "Merged" => Column::Hidden,
                    _ => Column::Flight,
                };
                (option.clone(), column)
            })
            .collect();
        TrackerConfig {
            status_columns,
            coefficients: Coefficients::default(),
            assignee_filter: AssigneeFilter::Me,
            type_colors: default_type_colors(),
        }
    }

    /// The column a Status option maps to; unknown options are in flight,
    /// and no status at all means the queue (nothing to triage away).
    pub(crate) fn column_for(&self, status: Option<&str>) -> Column {
        match status {
            None => Column::Queue,
            Some(status) => self
                .status_columns
                .get(status)
                .copied()
                .unwrap_or(Column::Flight),
        }
    }
}

fn tracker_config_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".cache")
            .join("lgtm")
            .join("tracker.json"),
    )
}

pub(crate) fn load_tracker_config() -> Option<TrackerConfig> {
    let path = tracker_config_path()?;
    let json = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&json).ok()
}

pub(crate) fn save_tracker_config(config: &TrackerConfig) {
    let Some(path) = tracker_config_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, json);
    }
}

/// The project's Priority field, High/Medium/Low.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    High,
    Medium,
    Low,
}

/// Everything [`urgency`] needs about one issue; unix seconds throughout.
pub(crate) struct UrgencyInput {
    pub(crate) due: Option<i64>,
    pub(crate) now: i64,
    pub(crate) blocking_count: u32,
    pub(crate) priority: Option<Priority>,
    pub(crate) active: bool,
    pub(crate) created_at: i64,
    pub(crate) has_milestone: bool,
    /// The milestone's due date, when it has one.
    pub(crate) milestone_due: Option<i64>,
    pub(crate) has_labels: bool,
    pub(crate) has_comments: bool,
    pub(crate) blocked: bool,
}

/// One day, in seconds.
const DAY: f64 = 86400.0;

/// Due factor for anything 14+ days out, and for a milestone with no date.
const FAR_DUE_FACTOR: f64 = 0.2;

/// Due-date term: 0.2 at 14 days out, linear up to 1.0 at the due date and
/// 1.0 for anything already overdue. `None` contributes nothing.
fn due_factor(due: Option<i64>, now: i64) -> f64 {
    let Some(due) = due else {
        return 0.0;
    };
    let days_out = (due - now) as f64 / DAY;
    if days_out <= 0.0 {
        1.0
    } else if days_out >= 14.0 {
        FAR_DUE_FACTOR
    } else {
        1.0 - days_out / 14.0 * 0.8
    }
}

/// Age term: linear from 0 at creation to 1.0 at 365 days old.
fn age_factor(created_at: i64, now: i64) -> f64 {
    let age_days = (now - created_at) as f64 / DAY;
    (age_days / 365.0).clamp(0.0, 1.0)
}

/// Taskwarrior-style urgency score. Monotonic in each input: closer due
/// dates, older issues, and set flags never lower the score.
pub(crate) fn urgency(input: &UrgencyInput, coeffs: &Coefficients) -> f64 {
    let due_term = coeffs.due * due_factor(input.due, input.now);
    let blocking_term = if input.blocking_count > 0 {
        coeffs.blocking
    } else {
        0.0
    };
    let priority_term = match input.priority {
        Some(Priority::High) => coeffs.priority_high,
        Some(Priority::Medium) => coeffs.priority_medium,
        Some(Priority::Low) => coeffs.priority_low,
        None => 0.0,
    };
    let active_term = if input.active { coeffs.active } else { 0.0 };
    let age_term = coeffs.age * age_factor(input.created_at, input.now);
    let milestone_term = if input.has_milestone {
        coeffs.milestone * input.milestone_due.map_or(FAR_DUE_FACTOR, |due| due_factor(Some(due), input.now))
    } else {
        0.0
    };
    let labels_term = if input.has_labels { coeffs.labels } else { 0.0 };
    let comments_term = if input.has_comments {
        coeffs.comments
    } else {
        0.0
    };
    let blocked_term = if input.blocked { coeffs.blocked } else { 0.0 };

    due_term
        + blocking_term
        + priority_term
        + active_term
        + age_term
        + milestone_term
        + labels_term
        + comments_term
        + blocked_term
}

/// A due date's urgency bucket, for row colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DueState {
    Overdue,
    Soon,
    Normal,
}

/// `due` relative to `now`, at day granularity above an hour: red/yellow
/// row colouring only needs to know which side of due/24h an item falls on.
pub(crate) fn due_state(due: i64, now: i64) -> DueState {
    let diff = due - now;
    if diff < 0 {
        DueState::Overdue
    } else if diff < 86400 {
        DueState::Soon
    } else {
        DueState::Normal
    }
}

/// Compact relative countdown to `due` (`3d`, `18h`, `-2d` when overdue).
pub(crate) fn due_countdown(due: i64, now: i64) -> String {
    let diff = due - now;
    let overdue = diff < 0;
    let magnitude = diff.abs();
    let value = if magnitude >= 86400 {
        format!("{}d", magnitude / 86400)
    } else if magnitude >= 3600 {
        format!("{}h", magnitude / 3600)
    } else {
        format!("{}m", magnitude / 60)
    };
    if overdue {
        format!("-{value}")
    } else {
        value
    }
}

/// Days-from-civil-date algorithm (Howard Hinnant), duplicated in miniature
/// from `comments::parse_iso_utc` since that one only accepts full
/// `YYYY-MM-DDTHH:MM:SSZ` timestamps and Projects v2 date fields are bare
/// `YYYY-MM-DD`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds (midnight UTC) for a bare `YYYY-MM-DD` date, as returned by
/// a Projects v2 date field.
pub(crate) fn parse_iso_date(s: &str) -> Option<i64> {
    if s.len() < 10 {
        return None;
    }
    let y = s.get(0..4)?.parse().ok()?;
    let m = s.get(5..7)?.parse().ok()?;
    let d = s.get(8..10)?.parse().ok()?;
    Some(days_from_civil(y, m, d) * 86400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_input(now: i64) -> UrgencyInput {
        UrgencyInput {
            due: None,
            now,
            blocking_count: 0,
            priority: None,
            active: false,
            created_at: now,
            has_milestone: false,
            milestone_due: None,
            has_labels: false,
            has_comments: false,
            blocked: false,
        }
    }

    #[test]
    fn milestone_term_scales_with_its_due_date() {
        let coeffs = Coefficients::default();
        let now = 1_000_000;
        let undated = UrgencyInput { has_milestone: true, ..base_input(now) };
        let far = UrgencyInput { milestone_due: Some(now + 30 * 86400), ..undated };
        let soon = UrgencyInput { milestone_due: Some(now + 7 * 86400), ..undated };
        let overdue = UrgencyInput { milestone_due: Some(now - 86400), ..undated };
        assert_eq!(urgency(&undated, &coeffs), coeffs.milestone * 0.2);
        assert_eq!(urgency(&far, &coeffs), coeffs.milestone * 0.2);
        assert!((urgency(&soon, &coeffs) - coeffs.milestone * 0.6).abs() < 1e-9);
        assert_eq!(urgency(&overdue, &coeffs), coeffs.milestone);
    }

    #[test]
    fn urgency_with_nothing_set_is_zero() {
        let now = 1_000_000;
        assert_eq!(urgency(&base_input(now), &Coefficients::default()), 0.0);
    }

    #[test]
    fn urgency_due_term_maxes_out_once_overdue() {
        let coeffs = Coefficients::default();
        let now = 1_000_000;
        let mut overdue = base_input(now);
        overdue.due = Some(now - 86400);
        assert_eq!(urgency(&overdue, &coeffs), coeffs.due);

        let mut further_overdue = base_input(now);
        further_overdue.due = Some(now - 10 * 86400);
        assert_eq!(urgency(&further_overdue, &coeffs), coeffs.due);
    }

    #[test]
    fn urgency_is_monotonic_in_due_proximity() {
        let coeffs = Coefficients::default();
        let now = 1_000_000;
        let mut far = base_input(now);
        far.due = Some(now + 20 * 86400);
        let mut near = base_input(now);
        near.due = Some(now + 2 * 86400);
        assert!(urgency(&near, &coeffs) > urgency(&far, &coeffs));
    }

    #[test]
    fn urgency_is_monotonic_in_age() {
        let coeffs = Coefficients::default();
        let now = 1_000_000;
        let mut young = base_input(now);
        young.created_at = now - 10 * 86400;
        let mut old = base_input(now);
        old.created_at = now - 300 * 86400;
        assert!(urgency(&old, &coeffs) > urgency(&young, &coeffs));
    }

    #[test]
    fn urgency_blocked_is_a_negative_term() {
        let coeffs = Coefficients::default();
        let now = 1_000_000;
        let mut blocked = base_input(now);
        blocked.blocked = true;
        assert_eq!(urgency(&blocked, &coeffs), coeffs.blocked);
    }

    #[test]
    fn urgency_zero_priority_contributes_nothing() {
        let coeffs = Coefficients::default();
        let now = 1_000_000;
        assert_eq!(urgency(&base_input(now), &coeffs), 0.0);
    }

    #[test]
    fn due_countdown_formats_relative_time_both_directions() {
        let now = 1_000_000;
        assert_eq!(due_countdown(now + 3 * 86400, now), "3d");
        assert_eq!(due_countdown(now + 18 * 3600, now), "18h");
        assert_eq!(due_countdown(now - 2 * 86400, now), "-2d");
    }

    #[test]
    fn due_state_thresholds() {
        let now = 1_000_000;
        assert_eq!(due_state(now - 1, now), DueState::Overdue);
        assert_eq!(due_state(now + 3600, now), DueState::Soon);
        assert_eq!(due_state(now + 2 * 86400, now), DueState::Normal);
    }

    #[test]
    fn seed_maps_known_statuses_and_defaults_unknown_to_flight() {
        let options = [
            "Backlog".to_string(),
            "Todo".to_string(),
            "Ready".to_string(),
            "In Progress".to_string(),
            "Done".to_string(),
            "Merged".to_string(),
            "Some Custom Status".to_string(),
        ];
        let config = TrackerConfig::seed(&options);
        assert_eq!(config.column_for(Some("Backlog")), Column::Queue);
        assert_eq!(config.column_for(Some("Todo")), Column::Queue);
        assert_eq!(config.column_for(Some("Ready")), Column::Queue);
        assert_eq!(config.column_for(Some("Done")), Column::Hidden);
        assert_eq!(config.column_for(Some("Merged")), Column::Hidden);
        assert_eq!(config.column_for(Some("In Progress")), Column::Flight);
        assert_eq!(config.column_for(Some("Some Custom Status")), Column::Flight);
        assert_eq!(config.column_for(Some("Never Seen")), Column::Flight);
        assert_eq!(config.column_for(None), Column::Queue);
    }

    #[test]
    fn parse_iso_date_matches_known_epoch() {
        assert_eq!(parse_iso_date("1970-01-01"), Some(0));
        assert_eq!(parse_iso_date("2001-09-09"), Some(999_993_600));
        assert_eq!(parse_iso_date("bad"), None);
        assert_eq!(parse_iso_date("2026-09-13T00:00:00Z"), Some(1_789_257_600));
    }
}
