use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Reject,
    Modify(Value),
}

#[derive(Debug)]
pub struct ApprovalPrompt {
    pub request: ApprovalRequest,
    pub respond: oneshot::Sender<ApprovalDecision>,
}

#[derive(Clone)]
pub struct TurnControl {
    pub(super) cancellation: CancellationToken,
    approval_tx: Option<mpsc::UnboundedSender<ApprovalPrompt>>,
}

impl TurnControl {
    pub fn non_interactive() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            approval_tx: None,
        }
    }

    pub fn interactive(approval_tx: mpsc::UnboundedSender<ApprovalPrompt>) -> Self {
        Self {
            cancellation: CancellationToken::new(),
            approval_tx: Some(approval_tx),
        }
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(super) async fn request_approval(&self, request: ApprovalRequest) -> ApprovalDecision {
        let Some(sender) = &self.approval_tx else {
            return ApprovalDecision::Reject;
        };
        let (respond, response) = oneshot::channel();
        if sender.send(ApprovalPrompt { request, respond }).is_err() {
            return ApprovalDecision::Reject;
        }
        tokio::select! {
            decision = response => decision.unwrap_or(ApprovalDecision::Reject),
            _ = self.cancellation.cancelled() => ApprovalDecision::Reject,
        }
    }
}
