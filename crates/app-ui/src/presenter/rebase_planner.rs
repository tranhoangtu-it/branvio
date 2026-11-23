//! State machine & validation for the Interactive Rebase modal (`git rebase -i`).

use branvio_core::domain::CommitNode;
use branvio_core::ids::GitOid;
use compact_str::CompactString;
use git_engine::{RebaseAction, RebasePlanEntry, RebasePlanPayload};
use thiserror::Error;

/// Single commit row inside [`InteractiveRebasePlanner`] (ordered oldest-first top-to-bottom).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveRebaseRow {
    /// Target commit OID.
    pub oid: GitOid,
    /// Short 7-char display SHA.
    pub short_sha: CompactString,
    /// Commit author name.
    pub author: CompactString,
    /// Original one-line subject.
    pub original_subject: String,
    /// User-chosen rebase action.
    pub action: RebaseAction,
    /// Optional replacement commit message (used by [`RebaseAction::Reword`] and [`RebaseAction::Squash`]).
    pub updated_message: Option<String>,
}

/// Validation failures preventing execution of an invalid interactive rebase plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RebaseValidationError {
    /// Planner contains zero commits.
    #[error("interactive rebase plan contains no commits")]
    EmptyPlan,
    /// Every commit was set to [`RebaseAction::Drop`].
    #[error("at least one commit must be retained (cannot drop all commits)")]
    AllDropped,
    /// First non-`Drop` step cannot be `Squash` or `Fixup` because there is no parent step to meld into.
    #[error(
        "step {step_number} cannot be squash/fixup before an active pick, reword, or edit commit"
    )]
    FirstActiveCannotMeld {
        /// 1-based step index inside the ordered plan.
        step_number: usize,
    },
}

/// Pure-Rust interactive rebase planner producing [`RebasePlanPayload`] for Tier 3 `git rebase -i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveRebasePlanner {
    /// Base target revision onto which commits are rebased (`git rebase -i <onto_rev>`).
    pub onto_rev: CompactString,
    /// Ordered rebase steps from oldest (top, executed first) to newest (bottom).
    pub rows: Vec<InteractiveRebaseRow>,
    /// Currently highlighted row index inside the modal.
    pub selected_index: Option<usize>,
}

impl InteractiveRebasePlanner {
    /// Creates a planner from commits already ordered oldest-first.
    pub fn from_oldest_first(onto_rev: CompactString, rows: Vec<InteractiveRebaseRow>) -> Self {
        let selected_index = (!rows.is_empty()).then_some(0);
        Self {
            onto_rev,
            rows,
            selected_index,
        }
    }

    /// Creates a planner from newest-first graph commits, reversing into Git's oldest-first todo order.
    pub fn from_newest_first_nodes(
        onto_rev: CompactString,
        nodes_newest_first: &[CommitNode],
    ) -> Self {
        let rows = nodes_newest_first
            .iter()
            .rev()
            .map(|node| {
                let hex = node.oid.to_hex();
                let short_sha = CompactString::from(&hex[..7.min(hex.len())]);
                InteractiveRebaseRow {
                    oid: node.oid,
                    short_sha,
                    author: node.author.name.clone(),
                    original_subject: node.summary.to_string(),
                    action: RebaseAction::Pick,
                    updated_message: None,
                }
            })
            .collect();
        Self::from_oldest_first(onto_rev, rows)
    }

    /// Moves step at `index` one slot earlier (up toward older base). Returns `true` if moved.
    pub fn move_up(&mut self, index: usize) -> bool {
        if index == 0 || index >= self.rows.len() {
            return false;
        }
        self.rows.swap(index, index - 1);
        self.selected_index = Some(index - 1);
        true
    }

    /// Moves step at `index` one slot later (down toward tip). Returns `true` if moved.
    pub fn move_down(&mut self, index: usize) -> bool {
        if index + 1 >= self.rows.len() {
            return false;
        }
        self.rows.swap(index, index + 1);
        self.selected_index = Some(index + 1);
        true
    }

    /// Updates the [`RebaseAction`] for row `index`.
    pub fn set_action(&mut self, index: usize, action: RebaseAction) -> bool {
        let Some(row) = self.rows.get_mut(index) else {
            return false;
        };
        row.action = action;
        if action == RebaseAction::Reword && row.updated_message.is_none() {
            row.updated_message = Some(row.original_subject.clone());
        }
        if !matches!(action, RebaseAction::Reword | RebaseAction::Squash) {
            row.updated_message = None;
        }
        true
    }

    /// Updates custom commit message for row `index` (`Reword` / `Squash`).
    pub fn set_message(&mut self, index: usize, message: Option<String>) -> bool {
        let Some(row) = self.rows.get_mut(index) else {
            return false;
        };
        row.updated_message = message;
        true
    }

    /// Validates topological/todo invariants required by `git rebase -i`.
    pub fn validate(&self) -> Result<(), RebaseValidationError> {
        if self.rows.is_empty() {
            return Err(RebaseValidationError::EmptyPlan);
        }

        let mut seen_active_anchor = false;
        for (idx, row) in self.rows.iter().enumerate() {
            match row.action {
                RebaseAction::Drop => {}
                RebaseAction::Squash | RebaseAction::Fixup => {
                    if !seen_active_anchor {
                        return Err(RebaseValidationError::FirstActiveCannotMeld {
                            step_number: idx + 1,
                        });
                    }
                }
                RebaseAction::Pick | RebaseAction::Reword | RebaseAction::Edit => {
                    seen_active_anchor = true;
                }
            }
        }

        if !seen_active_anchor {
            return Err(RebaseValidationError::AllDropped);
        }

        Ok(())
    }

    /// Validates and exports a [`RebasePlanPayload`] consumable by `TokioGitCliExecutor::interactive_rebase`.
    pub fn to_payload(&self) -> Result<RebasePlanPayload, RebaseValidationError> {
        self.validate()?;
        Ok(RebasePlanPayload {
            onto_rev: self.onto_rev.clone(),
            entries: self
                .rows
                .iter()
                .map(|row| RebasePlanEntry {
                    oid: row.oid,
                    action: row.action,
                    updated_message: row.updated_message.clone(),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_row(hex_char: char, subject: &str) -> InteractiveRebaseRow {
        let hex: String = std::iter::repeat_n(hex_char, 40).collect();
        InteractiveRebaseRow {
            oid: GitOid::from_hex(&hex).unwrap(),
            short_sha: CompactString::from(&hex[..7]),
            author: CompactString::const_new("Tester"),
            original_subject: subject.to_string(),
            action: RebaseAction::Pick,
            updated_message: None,
        }
    }

    #[test]
    fn blocks_squash_or_fixup_before_first_anchor_and_allows_after_drop_plus_pick() {
        let mut planner = InteractiveRebasePlanner::from_oldest_first(
            CompactString::const_new("main"),
            vec![make_row('1', "c1"), make_row('2', "c2")],
        );

        planner.set_action(0, RebaseAction::Squash);
        assert_eq!(
            planner.validate(),
            Err(RebaseValidationError::FirstActiveCannotMeld { step_number: 1 })
        );

        // Even if row 0 is Dropped, row 1 as Fixup still lacks an active anchor before it!
        planner.set_action(0, RebaseAction::Drop);
        planner.set_action(1, RebaseAction::Fixup);
        assert_eq!(
            planner.validate(),
            Err(RebaseValidationError::FirstActiveCannotMeld { step_number: 2 })
        );

        planner.set_action(1, RebaseAction::Drop);
        assert_eq!(planner.validate(), Err(RebaseValidationError::AllDropped));
    }

    #[test]
    fn reorders_commits_and_exports_valid_payload() {
        let mut planner = InteractiveRebasePlanner::from_oldest_first(
            CompactString::const_new("main"),
            vec![
                make_row('a', "first commit"),
                make_row('b', "second commit"),
                make_row('c', "third commit"),
            ],
        );

        assert!(planner.move_up(2));
        planner.set_action(0, RebaseAction::Reword);
        planner.set_message(0, Some("feat: reworded first\n".to_string()));
        planner.set_action(1, RebaseAction::Squash);
        planner.set_message(1, Some("feat: first + third\n".to_string()));
        planner.set_action(2, RebaseAction::Fixup);

        let payload = planner.to_payload().unwrap();
        assert_eq!(payload.onto_rev.as_str(), "main");
        assert_eq!(payload.entries.len(), 3);
        assert_eq!(payload.entries[0].action, RebaseAction::Reword);
        assert_eq!(payload.entries[1].action, RebaseAction::Squash);
        assert_eq!(payload.entries[2].action, RebaseAction::Fixup);
        assert_eq!(
            payload.entries[0].oid.to_hex(),
            std::iter::repeat_n('a', 40).collect::<String>()
        );
        assert_eq!(
            payload.entries[1].oid.to_hex(),
            std::iter::repeat_n('c', 40).collect::<String>()
        );
    }
}
