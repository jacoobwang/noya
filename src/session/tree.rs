use super::{
    event::{EventEnvelope, SessionEvent},
    model::{SessionBranch, SessionTree, SessionTreeNode},
};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;

impl SessionTree {
    pub(super) fn replay(events: &[EventEnvelope]) -> Result<Self> {
        let first = events.first().context("session log is empty")?;
        ensure!(first.seq == 1, "session tree must start at sequence 1");
        let mut tree = Self {
            nodes: vec![SessionTreeNode {
                seq: 1,
                parent_seq: None,
                event_type: event_type(&first.event),
                turn_id: first.turn_id,
            }],
            active_head_seq: 1,
            ..Self::default()
        };
        let mut known = HashMap::from([(1_u64, ())]);
        for event in &events[1..] {
            ensure!(
                !known.contains_key(&event.seq),
                "duplicate session sequence {}",
                event.seq
            );
            let parent_seq = event.parent_seq.or_else(|| event.seq.checked_sub(1));
            if let Some(parent) = parent_seq {
                ensure!(
                    known.contains_key(&parent),
                    "event {} references unknown parent {}",
                    event.seq,
                    parent
                );
            }
            tree.nodes.push(SessionTreeNode {
                seq: event.seq,
                parent_seq,
                event_type: event_type(&event.event),
                turn_id: event.turn_id,
            });
            known.insert(event.seq, ());
            tree.apply_branch_event(event)?;
        }
        Ok(tree)
    }

    pub(super) fn apply_branch_event(&mut self, event: &EventEnvelope) -> Result<()> {
        match &event.event {
            SessionEvent::BranchCreated {
                branch_id,
                name,
                from_seq,
            } => {
                ensure!(!name.trim().is_empty(), "branch name cannot be empty");
                ensure!(
                    self.nodes.iter().any(|node| node.seq == *from_seq),
                    "branch source does not exist"
                );
                ensure!(self.branch(*branch_id).is_none(), "branch already exists");
                self.branches.push(SessionBranch {
                    branch_id: *branch_id,
                    name: name.clone(),
                    from_seq: *from_seq,
                    head_seq: event.seq,
                    created_seq: event.seq,
                    summary: None,
                    summary_model: None,
                });
                self.active_branch_id = Some(*branch_id);
                self.active_head_seq = event.seq;
            }
            SessionEvent::BranchSelected { branch_id, head_seq } => {
                let branch = self
                    .branches
                    .iter()
                    .find(|branch| branch.branch_id == *branch_id)
                    .context("selected branch does not exist")?;
                ensure!(branch.head_seq == *head_seq, "selected branch head is stale");
                self.active_branch_id = Some(*branch_id);
                self.active_head_seq = *head_seq;
            }
            SessionEvent::BranchSummary {
                branch_id,
                from_seq,
                summary,
                summary_model,
            } => {
                ensure!(!summary.trim().is_empty(), "branch summary cannot be empty");
                let branch = self
                    .branches
                    .iter_mut()
                    .find(|branch| branch.branch_id == *branch_id)
                    .context("branch summary references unknown branch")?;
                ensure!(
                    branch.from_seq <= *from_seq,
                    "branch summary source is before branch origin"
                );
                branch.summary = Some(summary.clone());
                branch.summary_model = Some(summary_model.clone());
                branch.head_seq = event.seq;
                if self.active_branch_id == Some(*branch_id) {
                    self.active_head_seq = event.seq;
                }
            }
            _ => {
                let parent = event.parent_seq.or_else(|| event.seq.checked_sub(1));
                if parent == Some(self.active_head_seq) {
                    self.active_head_seq = event.seq;
                    if let Some(branch_id) = self.active_branch_id
                        && let Some(branch) = self
                            .branches
                            .iter_mut()
                            .find(|branch| branch.branch_id == branch_id)
                    {
                        branch.head_seq = event.seq;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn add_node(&mut self, event: &EventEnvelope) -> Result<()> {
        let parent_seq = event.parent_seq;
        ensure!(
            !self.nodes.iter().any(|node| node.seq == event.seq),
            "duplicate session tree sequence {}",
            event.seq
        );
        if let Some(parent) = parent_seq {
            ensure!(
                self.nodes.iter().any(|node| node.seq == parent),
                "event {} references unknown parent {}",
                event.seq,
                parent
            );
        }
        self.nodes.push(SessionTreeNode {
            seq: event.seq,
            parent_seq,
            event_type: event_type(&event.event),
            turn_id: event.turn_id,
        });
        self.apply_branch_event(event)
    }

    pub fn completed_turn_boundary(&self, seq: Option<u64>) -> Result<u64> {
        if let Some(target) = seq {
            return self
                .nodes
                .iter()
                .find(|node| node.seq == target && node.event_type == "turn_completed")
                .map(|node| node.seq)
                .context("branch source must be a completed turn boundary");
        }
        self.active_path()
            .into_iter()
            .rev()
            .find(|seq| {
                self.nodes
                    .iter()
                    .any(|node| node.seq == *seq && node.event_type == "turn_completed")
            })
            .context("session has no completed turn boundary")
    }
}

fn event_type(event: &SessionEvent) -> String {
    match event {
        SessionEvent::SessionCreated(_) => "session_created",
        SessionEvent::RuntimeStarted(_) => "runtime_started",
        SessionEvent::SkillActivated { .. } => "skill_activated",
        SessionEvent::SkillDeactivated { .. } => "skill_deactivated",
        SessionEvent::TitleChanged { .. } => "title_changed",
        SessionEvent::ModelChanged { .. } => "model_changed",
        SessionEvent::TurnStarted(_) => "turn_started",
        SessionEvent::AssistantCompleted(_) => "assistant_completed",
        SessionEvent::ToolStarted(_) => "tool_started",
        SessionEvent::ToolFinished(_) => "tool_finished",
        SessionEvent::TurnCompleted => "turn_completed",
        SessionEvent::TurnFailed(_) => "turn_failed",
        SessionEvent::TurnCancelled { .. } => "turn_cancelled",
        SessionEvent::TurnInterrupted { .. } => "turn_interrupted",
        SessionEvent::ContextReset { .. } => "context_reset",
        SessionEvent::ContextCompacted(_) => "context_compacted",
        SessionEvent::SessionForked { .. } => "session_forked",
        SessionEvent::BranchCreated { .. } => "branch_created",
        SessionEvent::BranchSelected { .. } => "branch_selected",
        SessionEvent::BranchSummary { .. } => "branch_summary",
        SessionEvent::SessionArchived => "session_archived",
    }
    .to_string()
}
