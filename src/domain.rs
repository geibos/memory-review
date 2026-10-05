//! Card lifecycle and validation of what the agent proposes. Pure, no I/O.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// What happens to the source notes when a card is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// One inbox note becomes one verified note.
    Promote,
    /// Several inbox notes become one verified note.
    Merge,
    /// The inbox notes are dropped.
    Delete,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Promote => "promote",
            Action::Merge => "merge",
            Action::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        [Action::Promote, Action::Merge, Action::Delete]
            .into_iter()
            .find(|a| a.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    AgentWorking,
    Applying,
    Accepted,
    Snoozed,
    Stale,
    Closed,
}

pub const ALL_STATUSES: [Status; 7] = [
    Status::Ready,
    Status::AgentWorking,
    Status::Applying,
    Status::Accepted,
    Status::Snoozed,
    Status::Stale,
    Status::Closed,
];

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ready => "ready",
            Status::AgentWorking => "agent_working",
            Status::Applying => "applying",
            Status::Accepted => "accepted",
            Status::Snoozed => "snoozed",
            Status::Stale => "stale",
            Status::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        ALL_STATUSES.into_iter().find(|st| st.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    SendToAgent,
    Regenerate,
    AgentDone,
    AgentFailed { back_to: Status },
    Accept,
    ApplyDone,
    ApplyRetry,
    Snooze,
    Unsnooze,
    SourcesChanged,
    SourcesGone,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("cannot apply {event:?} to a card in status `{}`", from.as_str())]
pub struct DomainError {
    pub from: Status,
    pub event: Event,
}

pub fn transition(from: Status, ev: &Event) -> Result<Status, DomainError> {
    use Status::*;
    let to = match (from, ev) {
        (Ready, Event::SendToAgent) | (Stale, Event::Regenerate) => Some(AgentWorking),
        (AgentWorking, Event::AgentDone) => Some(Ready),
        (AgentWorking, Event::AgentFailed { back_to }) => {
            matches!(back_to, Ready | Stale).then_some(*back_to)
        }
        (Ready, Event::Accept) | (Applying, Event::ApplyRetry) => Some(Applying),
        (Applying, Event::ApplyDone) => Some(Accepted),
        (Ready, Event::Snooze) => Some(Snoozed),
        (Snoozed, Event::Unsnooze) => Some(Ready),
        (Ready | Snoozed, Event::SourcesChanged) => Some(Stale),
        (Ready | Snoozed | Stale | AgentWorking, Event::SourcesGone) => Some(Closed),
        _ => None,
    };
    to.ok_or_else(|| DomainError {
        from,
        event: ev.clone(),
    })
}

/// Whether a human may add a comment to a card in this status.
pub fn can_comment(s: Status) -> bool {
    !matches!(s, Status::Accepted | Status::Closed)
}

/// The agent's answer, as received from the model.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AgentProposal {
    pub action: String,
    pub sources: Vec<String>,
    #[serde(default)]
    pub target_dir: Option<String>,
    #[serde(default)]
    pub target_title: Option<String>,
    #[serde(default)]
    pub draft: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    pub rationale: String,
}

/// A proposal that passed [`AgentProposal::validate`].
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedProposal {
    pub action: Action,
    pub sources: Vec<String>,
    pub target_dir: Option<String>,
    pub target_title: Option<String>,
    pub draft: Option<String>,
    pub tags: Vec<String>,
    pub rationale: String,
}

/// What the agent is allowed to claim.
pub struct ClaimContext<'a> {
    /// The note the job is about; must be among the sources.
    pub origin: &'a str,
    /// Inbox notes not held by any other open card (includes `origin`).
    pub free_inbox: &'a HashSet<String>,
}

impl AgentProposal {
    /// Checks the proposal against the rules; errors are English sentences
    /// meant to be shown back to the model.
    pub fn validate(self, ctx: &ClaimContext) -> Result<ValidatedProposal, String> {
        let action = Action::parse(self.action.trim()).ok_or_else(|| {
            format!(
                "`action` must be one of promote, merge, delete; got `{}`",
                self.action
            )
        })?;

        let mut sources: Vec<String> = Vec::new();
        for s in self.sources.iter().map(|s| s.trim()) {
            if !sources.iter().any(|x| x == s) {
                sources.push(s.to_string());
            }
        }
        if !sources.iter().any(|s| s == ctx.origin) {
            return Err(format!(
                "`sources` must include the note under review: {}",
                ctx.origin
            ));
        }
        let foreign: Vec<&str> = sources
            .iter()
            .filter(|s| !ctx.free_inbox.contains(s.as_str()))
            .map(String::as_str)
            .collect();
        if !foreign.is_empty() {
            return Err(format!(
                "these sources are not free inbox notes and cannot be used: {}",
                foreign.join(", ")
            ));
        }
        match (action, sources.len()) {
            (Action::Promote, 1) | (Action::Delete, _) => {}
            (Action::Merge, n) if n >= 2 => {}
            (Action::Promote, _) => {
                return Err("`promote` takes exactly one source; use `merge` for several".into());
            }
            (Action::Merge, _) => return Err("`merge` needs at least two sources".into()),
        }

        let rationale = self.rationale.trim().to_string();
        if rationale.is_empty() {
            return Err("`rationale` must not be empty".into());
        }

        if action == Action::Delete {
            return Ok(ValidatedProposal {
                action,
                sources,
                target_dir: None,
                target_title: None,
                draft: None,
                tags: Vec::new(),
                rationale,
            });
        }

        let target_dir = self
            .target_dir
            .as_deref()
            .map(str::trim)
            .ok_or("`target_dir` is required for promote and merge")?;
        if target_dir.is_empty()
            || target_dir == "."
            || target_dir == ".."
            || target_dir.contains(['/', '\\'])
            || target_dir.chars().count() > 64
        {
            return Err(format!(
                "`target_dir` must be a single folder name (no slashes, not `.`/`..`, at most 64 chars); got `{target_dir}`"
            ));
        }
        let target_title = self
            .target_title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or("`target_title` is required for promote and merge")?;
        if target_title.contains(['/', '\\']) {
            return Err("`target_title` must not contain slashes".into());
        }
        let draft = self
            .draft
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or("`draft` is required for promote and merge")?;

        Ok(ValidatedProposal {
            action,
            sources,
            target_dir: Some(target_dir.to_string()),
            target_title: Some(target_title.to_string()),
            draft: Some(draft.to_string()),
            tags: self
                .tags
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
            rationale,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path() {
        let s = transition(Status::Ready, &Event::Accept).unwrap();
        assert_eq!(s, Status::Applying);
        assert_eq!(transition(s, &Event::ApplyDone).unwrap(), Status::Accepted);
    }

    #[test]
    fn accept_only_from_ready() {
        for s in ALL_STATUSES.into_iter().filter(|s| *s != Status::Ready) {
            assert!(transition(s, &Event::Accept).is_err(), "{s:?}");
        }
    }

    #[test]
    fn agent_round_trip() {
        assert_eq!(
            transition(Status::Ready, &Event::SendToAgent).unwrap(),
            Status::AgentWorking
        );
        assert_eq!(
            transition(Status::Stale, &Event::Regenerate).unwrap(),
            Status::AgentWorking
        );
        assert_eq!(
            transition(Status::AgentWorking, &Event::AgentDone).unwrap(),
            Status::Ready
        );
    }

    #[test]
    fn agent_failure_returns_to_given_status() {
        assert_eq!(
            transition(
                Status::AgentWorking,
                &Event::AgentFailed {
                    back_to: Status::Stale
                }
            )
            .unwrap(),
            Status::Stale
        );
        assert!(
            transition(
                Status::AgentWorking,
                &Event::AgentFailed {
                    back_to: Status::Accepted
                }
            )
            .is_err()
        );
    }

    #[test]
    fn apply_retry_stays_applying() {
        assert_eq!(
            transition(Status::Applying, &Event::ApplyRetry).unwrap(),
            Status::Applying
        );
    }

    #[test]
    fn snooze_and_wake() {
        assert_eq!(
            transition(Status::Ready, &Event::Snooze).unwrap(),
            Status::Snoozed
        );
        assert_eq!(
            transition(Status::Snoozed, &Event::Unsnooze).unwrap(),
            Status::Ready
        );
        assert!(transition(Status::Applying, &Event::Snooze).is_err());
    }

    #[test]
    fn sources_changed_and_gone() {
        assert_eq!(
            transition(Status::Snoozed, &Event::SourcesChanged).unwrap(),
            Status::Stale
        );
        assert_eq!(
            transition(Status::Stale, &Event::SourcesGone).unwrap(),
            Status::Closed
        );
        assert!(transition(Status::Applying, &Event::SourcesChanged).is_err());
        assert!(transition(Status::Accepted, &Event::SourcesGone).is_err());
    }

    #[test]
    fn regeneration_with_no_sources_left_closes() {
        assert_eq!(
            transition(Status::AgentWorking, &Event::SourcesGone).unwrap(),
            Status::Closed
        );
    }

    #[test]
    fn comments_blocked_only_when_finished() {
        assert!(can_comment(Status::Ready));
        assert!(can_comment(Status::AgentWorking));
        assert!(can_comment(Status::Stale));
        assert!(!can_comment(Status::Accepted));
        assert!(!can_comment(Status::Closed));
    }

    #[test]
    fn status_and_action_roundtrip() {
        for s in ALL_STATUSES {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        for a in [Action::Promote, Action::Merge, Action::Delete] {
            assert_eq!(Action::parse(a.as_str()), Some(a));
        }
        assert_eq!(Status::parse("nope"), None);
    }

    fn free(notes: &[&str]) -> HashSet<String> {
        notes.iter().map(|s| s.to_string()).collect()
    }

    fn p(action: &str, sources: &[&str]) -> AgentProposal {
        AgentProposal {
            action: action.into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            target_dir: Some("ops".into()),
            target_title: Some("T".into()),
            draft: Some("- [fact] x".into()),
            tags: None,
            rationale: "r".into(),
        }
    }

    fn check(
        x: AgentProposal,
        origin: &str,
        free_inbox: &HashSet<String>,
    ) -> Result<ValidatedProposal, String> {
        x.validate(&ClaimContext { origin, free_inbox })
    }

    #[test]
    fn promote_valid() {
        let f = free(&["p/inbox/a"]);
        let v = check(p("promote", &["p/inbox/a"]), "p/inbox/a", &f).unwrap();
        assert_eq!(v.action, Action::Promote);
        assert_eq!(v.target_dir.as_deref(), Some("ops"));
        assert!(v.tags.is_empty());
    }

    #[test]
    fn unknown_action_rejected() {
        let f = free(&["p/inbox/a"]);
        assert!(check(p("archive", &["p/inbox/a"]), "p/inbox/a", &f).is_err());
    }

    #[test]
    fn sources_must_include_origin() {
        let f = free(&["p/inbox/a", "p/inbox/b", "p/inbox/c"]);
        assert!(check(p("merge", &["p/inbox/b", "p/inbox/c"]), "p/inbox/a", &f).is_err());
    }

    #[test]
    fn merge_does_not_accept_claimed_or_foreign_notes() {
        let f = free(&["p/inbox/a"]);
        let e = check(
            p("merge", &["p/inbox/a", "p/inbox/claimed"]),
            "p/inbox/a",
            &f,
        )
        .unwrap_err();
        assert!(e.contains("p/inbox/claimed"), "{e}");
        assert!(check(p("merge", &["p/inbox/a", "p/verified/x"]), "p/inbox/a", &f).is_err());
    }

    #[test]
    fn source_counts_per_action() {
        let f = free(&["p/inbox/a", "p/inbox/b"]);
        assert!(check(p("promote", &["p/inbox/a", "p/inbox/b"]), "p/inbox/a", &f).is_err());
        assert!(check(p("merge", &["p/inbox/a"]), "p/inbox/a", &f).is_err());
        assert!(check(p("merge", &["p/inbox/a", "p/inbox/b"]), "p/inbox/a", &f).is_ok());
    }

    #[test]
    fn target_dir_rejects_traversal() {
        let f = free(&["p/inbox/a"]);
        for bad in ["..", "../x", "a/b", "", " ", "a\\b", ".", &"x".repeat(65)] {
            let mut x = p("promote", &["p/inbox/a"]);
            x.target_dir = Some(bad.to_string());
            assert!(check(x, "p/inbox/a", &f).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn target_dir_trimmed() {
        let f = free(&["p/inbox/a"]);
        let mut x = p("promote", &["p/inbox/a"]);
        x.target_dir = Some("  infra ".into());
        assert_eq!(
            check(x, "p/inbox/a", &f).unwrap().target_dir.as_deref(),
            Some("infra")
        );
    }

    #[test]
    fn delete_needs_no_draft() {
        let f = free(&["p/inbox/a"]);
        let mut x = p("delete", &["p/inbox/a"]);
        x.draft = None;
        x.target_dir = None;
        x.target_title = None;
        let v = check(x, "p/inbox/a", &f).unwrap();
        assert_eq!(v.action, Action::Delete);
        assert!(v.draft.is_none() && v.target_dir.is_none() && v.target_title.is_none());
    }

    #[test]
    fn delete_drops_stray_draft_fields() {
        let f = free(&["p/inbox/a"]);
        let v = check(p("delete", &["p/inbox/a"]), "p/inbox/a", &f).unwrap();
        assert!(v.draft.is_none() && v.target_dir.is_none());
    }

    #[test]
    fn promote_needs_draft_title_and_dir() {
        let f = free(&["p/inbox/a"]);
        let mut x = p("promote", &["p/inbox/a"]);
        x.draft = None;
        assert!(check(x, "p/inbox/a", &f).is_err());
        let mut x = p("promote", &["p/inbox/a"]);
        x.draft = Some("   ".into());
        assert!(check(x, "p/inbox/a", &f).is_err());
        let mut x = p("promote", &["p/inbox/a"]);
        x.target_title = None;
        assert!(check(x, "p/inbox/a", &f).is_err());
        let mut x = p("promote", &["p/inbox/a"]);
        x.target_dir = None;
        assert!(check(x, "p/inbox/a", &f).is_err());
    }

    #[test]
    fn title_with_slash_rejected() {
        let f = free(&["p/inbox/a"]);
        let mut x = p("promote", &["p/inbox/a"]);
        x.target_title = Some("a/b".into());
        assert!(check(x, "p/inbox/a", &f).is_err());
    }

    #[test]
    fn duplicate_sources_deduped() {
        let f = free(&["p/inbox/a"]);
        let v = check(p("promote", &["p/inbox/a", "p/inbox/a"]), "p/inbox/a", &f).unwrap();
        assert_eq!(v.sources, vec!["p/inbox/a".to_string()]);
    }

    #[test]
    fn rationale_required() {
        let f = free(&["p/inbox/a"]);
        let mut x = p("promote", &["p/inbox/a"]);
        x.rationale = " ".into();
        assert!(check(x, "p/inbox/a", &f).is_err());
    }
}
