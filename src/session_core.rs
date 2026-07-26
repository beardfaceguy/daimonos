use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use tokio::sync::oneshot;

use crate::session_protocol::{ApprovalDecision, ApprovalRequest, ClientCapability};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalResolution {
    pub approval_id: String,
    pub decision: ApprovalDecision,
    pub resolved_by: String,
}

pub struct RegisteredApproval {
    pub request: ApprovalRequest,
    pub receiver: oneshot::Receiver<ApprovalResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    NotPending,
    MissingCapability(ClientCapability),
    AllowAlwaysUnavailable,
    IdExhausted,
}

struct PendingApproval {
    request: ApprovalRequest,
    sender: oneshot::Sender<ApprovalResolution>,
}

#[derive(Default)]
struct ApprovalState {
    pending: HashMap<String, PendingApproval>,
    next_id: u64,
}

pub struct ApprovalBroker {
    state: StdMutex<ApprovalState>,
    allow_always: bool,
}

impl ApprovalBroker {
    pub fn new(allow_always: bool) -> Self {
        Self {
            state: StdMutex::new(ApprovalState::default()),
            allow_always,
        }
    }

    /// Register one approval using a broker-generated, session-local monotonic
    /// id. Callers never choose ids, so an evicted/late response cannot collide
    /// with a newer request during this broker's lifetime.
    pub fn register(
        &self,
        mut request: ApprovalRequest,
    ) -> Result<RegisteredApproval, ApprovalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or(ApprovalError::IdExhausted)?;
        request.id = format!("approval-{}", state.next_id);
        request.allow_always_available &= self.allow_always;
        let (sender, receiver) = oneshot::channel();
        state.pending.insert(
            request.id.clone(),
            PendingApproval {
                request: request.clone(),
                sender,
            },
        );
        Ok(RegisteredApproval { request, receiver })
    }

    pub fn pending(&self) -> Vec<ApprovalRequest> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending: Vec<_> = state
            .pending
            .values()
            .map(|pending| pending.request.clone())
            .collect();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }

    pub fn resolve(
        &self,
        approval_id: &str,
        resolved_by: &str,
        capabilities: &[ClientCapability],
        decision: ApprovalDecision,
    ) -> Result<ApprovalResolution, ApprovalError> {
        let required = match decision {
            ApprovalDecision::AllowAlways => ClientCapability::ApproveAlways,
            ApprovalDecision::AllowOnce | ApprovalDecision::Deny => ClientCapability::ApproveOnce,
        };
        if !capabilities.contains(&required) {
            return Err(ApprovalError::MissingCapability(required));
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = state.pending.get(approval_id) else {
            // Deliberately identical for unknown and already-resolved ids: both
            // are safe no-ops, and the response does not disclose broker history.
            return Err(ApprovalError::NotPending);
        };
        if decision == ApprovalDecision::AllowAlways && !pending.request.allow_always_available {
            return Err(ApprovalError::AllowAlwaysUnavailable);
        }
        let Some(pending) = state.pending.remove(approval_id) else {
            return Err(ApprovalError::NotPending);
        };
        let resolution = ApprovalResolution {
            approval_id: approval_id.to_string(),
            decision,
            resolved_by: resolved_by.to_string(),
        };
        let _ = pending.sender.send(resolution.clone());
        Ok(resolution)
    }

    pub fn cancel_all(&self, resolved_by: &str) -> Vec<ApprovalResolution> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending: Vec<_> = state.pending.drain().collect();
        pending.sort_by(|left, right| left.0.cmp(&right.0));
        let mut resolutions = Vec::with_capacity(pending.len());
        for (approval_id, pending) in pending {
            let resolution = ApprovalResolution {
                approval_id,
                decision: ApprovalDecision::Deny,
                resolved_by: resolved_by.to_string(),
            };
            let _ = pending.sender.send(resolution.clone());
            resolutions.push(resolution);
        }
        resolutions
    }
}

impl Drop for ApprovalBroker {
    fn drop(&mut self) {
        let _ = self.cancel_all("broker_drop");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            id: String::new(),
            tool_call_id: "tool".to_string(),
            tool: "exec".to_string(),
            detail: "run tests".to_string(),
            allow_always_available: true,
        }
    }

    #[tokio::test]
    async fn register_exposes_pending_and_resolution_wakes_waiter() {
        let broker = ApprovalBroker::new(false);
        let registered = broker.register(request()).unwrap();
        assert_eq!(broker.pending(), vec![registered.request.clone()]);
        assert!(!registered.request.allow_always_available);

        let resolution = broker
            .resolve(
                &registered.request.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            )
            .unwrap();
        assert_eq!(registered.receiver.await.unwrap(), resolution);
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn observer_cannot_answer_privileged_approval() {
        let broker = ApprovalBroker::new(false);
        let registered = broker.register(request()).unwrap();
        assert_eq!(
            broker.resolve(
                &registered.request.id,
                "observer",
                &[ClientCapability::Observe],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::MissingCapability(
                ClientCapability::ApproveOnce
            ))
        );
        assert_eq!(broker.pending(), vec![registered.request]);
    }

    #[test]
    fn allow_always_requires_host_policy_request_option_and_capability() {
        let disabled = ApprovalBroker::new(false);
        let registered = disabled.register(request()).unwrap();
        assert_eq!(
            disabled.resolve(
                &registered.request.id,
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::AllowAlwaysUnavailable)
        );

        let enabled = ApprovalBroker::new(true);
        let mut unavailable = request();
        unavailable.allow_always_available = false;
        let unavailable = enabled.register(unavailable).unwrap();
        assert_eq!(
            enabled.resolve(
                &unavailable.request.id,
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::AllowAlwaysUnavailable)
        );

        let available = enabled.register(request()).unwrap();
        assert_eq!(
            enabled.resolve(
                &available.request.id,
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::MissingCapability(
                ClientCapability::ApproveAlways
            ))
        );
        assert!(enabled
            .resolve(
                &available.request.id,
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            )
            .is_ok());
    }

    #[test]
    fn first_resolution_wins_and_late_or_unknown_answers_are_safe_noops() {
        let broker = ApprovalBroker::new(false);
        let registered = broker.register(request()).unwrap();
        broker
            .resolve(
                &registered.request.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            )
            .unwrap();
        assert_eq!(
            broker.resolve(
                &registered.request.id,
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            ),
            Err(ApprovalError::NotPending)
        );
        assert_eq!(
            broker.resolve(
                "never-issued",
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::NotPending)
        );
    }

    #[test]
    fn broker_assigns_monotonic_ids_that_are_never_reused() {
        let broker = ApprovalBroker::new(false);
        let first = broker.register(request()).unwrap();
        let second = broker.register(request()).unwrap();
        assert_eq!(first.request.id, "approval-1");
        assert_eq!(second.request.id, "approval-2");
        broker
            .resolve(
                &first.request.id,
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            )
            .unwrap();
        let third = broker.register(request()).unwrap();
        assert_eq!(third.request.id, "approval-3");
    }

    #[tokio::test]
    async fn cancel_all_denies_every_pending_waiter() {
        let broker = ApprovalBroker::new(false);
        let first = broker.register(request()).unwrap();
        let second = broker.register(request()).unwrap();
        let resolutions = broker.cancel_all("daemon_shutdown");
        assert_eq!(resolutions.len(), 2);
        assert!(broker.pending().is_empty());
        assert_eq!(
            first.receiver.await.unwrap().decision,
            ApprovalDecision::Deny
        );
        assert_eq!(
            second.receiver.await.unwrap().decision,
            ApprovalDecision::Deny
        );
    }

    #[tokio::test]
    async fn dropping_broker_structurally_denies_pending_waiters() {
        let registered = {
            let broker = ApprovalBroker::new(false);
            broker.register(request()).unwrap()
        };
        let resolution = registered.receiver.await.unwrap();
        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert_eq!(resolution.resolved_by, "broker_drop");
    }
}
