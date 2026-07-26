#![allow(dead_code)] // ACP/daemon wiring follows in the next SessionCore slice.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex as StdMutex;
use tokio::sync::oneshot;

use crate::session_protocol::{ApprovalDecision, ApprovalRequest, ClientCapability};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalResolution {
    pub approval_id: String,
    pub decision: ApprovalDecision,
    pub resolved_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    DuplicateRequest,
    UnknownApproval,
    AlreadyResolved,
    MissingCapability(ClientCapability),
    AllowAlwaysDisabled,
    AllowAlwaysUnavailable,
}

struct PendingApproval {
    request: ApprovalRequest,
    sender: oneshot::Sender<ApprovalResolution>,
}

#[derive(Default)]
struct ApprovalState {
    pending: HashMap<String, PendingApproval>,
    resolved: HashSet<String>,
    resolved_order: VecDeque<String>,
}

pub struct ApprovalBroker {
    state: StdMutex<ApprovalState>,
    allow_always: bool,
    max_resolved: usize,
}

impl ApprovalBroker {
    pub fn new(allow_always: bool, max_resolved: usize) -> Self {
        Self {
            state: StdMutex::new(ApprovalState::default()),
            allow_always,
            max_resolved,
        }
    }

    pub fn register(
        &self,
        mut request: ApprovalRequest,
    ) -> Result<oneshot::Receiver<ApprovalResolution>, ApprovalError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.pending.contains_key(&request.id) || state.resolved.contains(&request.id) {
            return Err(ApprovalError::DuplicateRequest);
        }
        request.allow_always_available &= self.allow_always;
        let (sender, receiver) = oneshot::channel();
        state
            .pending
            .insert(request.id.clone(), PendingApproval { request, sender });
        Ok(receiver)
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
            ApprovalDecision::AllowAlways => {
                if !self.allow_always {
                    return Err(ApprovalError::AllowAlwaysDisabled);
                }
                ClientCapability::ApproveAlways
            }
            ApprovalDecision::AllowOnce | ApprovalDecision::Deny => ClientCapability::ApproveOnce,
        };
        if !capabilities.contains(&required) {
            return Err(ApprovalError::MissingCapability(required));
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.resolved.contains(approval_id) {
            return Err(ApprovalError::AlreadyResolved);
        }
        if decision == ApprovalDecision::AllowAlways
            && state
                .pending
                .get(approval_id)
                .is_some_and(|pending| !pending.request.allow_always_available)
        {
            return Err(ApprovalError::AllowAlwaysUnavailable);
        }
        let Some(pending) = state.pending.remove(approval_id) else {
            return Err(ApprovalError::UnknownApproval);
        };
        let resolution = ApprovalResolution {
            approval_id: approval_id.to_string(),
            decision,
            resolved_by: resolved_by.to_string(),
        };
        let _ = pending.sender.send(resolution.clone());
        self.record_resolved(&mut state, approval_id.to_string());
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
                approval_id: approval_id.clone(),
                decision: ApprovalDecision::Deny,
                resolved_by: resolved_by.to_string(),
            };
            let _ = pending.sender.send(resolution.clone());
            self.record_resolved(&mut state, approval_id);
            resolutions.push(resolution);
        }
        resolutions
    }

    pub fn resolved_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .resolved
            .len()
    }

    fn record_resolved(&self, state: &mut ApprovalState, approval_id: String) {
        state.resolved.insert(approval_id.clone());
        state.resolved_order.push_back(approval_id);
        while state.resolved_order.len() > self.max_resolved {
            if let Some(expired) = state.resolved_order.pop_front() {
                state.resolved.remove(&expired);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_protocol::{ApprovalDecision, ApprovalRequest, ClientCapability};

    fn request(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: id.to_string(),
            tool_call_id: format!("tool-{id}"),
            tool: "exec".to_string(),
            detail: "run tests".to_string(),
            allow_always_available: true,
        }
    }

    #[tokio::test]
    async fn register_exposes_pending_and_resolution_wakes_waiter() {
        let broker = ApprovalBroker::new(false, 8);
        let waiter = broker.register(request("a1")).unwrap();
        let mut expected = request("a1");
        expected.allow_always_available = false;
        assert_eq!(broker.pending(), vec![expected]);

        let resolution = broker
            .resolve(
                "a1",
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            )
            .unwrap();
        assert_eq!(
            resolution,
            ApprovalResolution {
                approval_id: "a1".to_string(),
                decision: ApprovalDecision::AllowOnce,
                resolved_by: "local".to_string(),
            }
        );
        assert_eq!(waiter.await.unwrap(), resolution);
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn observer_cannot_answer_privileged_approval() {
        let broker = ApprovalBroker::new(false, 8);
        let _waiter = broker.register(request("a1")).unwrap();
        assert_eq!(
            broker.resolve(
                "a1",
                "observer",
                &[ClientCapability::Observe],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::MissingCapability(
                ClientCapability::ApproveOnce
            ))
        );
        let mut expected = request("a1");
        expected.allow_always_available = false;
        assert_eq!(broker.pending(), vec![expected]);
    }

    #[test]
    fn host_policy_is_reflected_in_pending_approval_options() {
        let disabled = ApprovalBroker::new(false, 8);
        let _waiter = disabled.register(request("a1")).unwrap();
        assert!(!disabled.pending()[0].allow_always_available);

        let enabled = ApprovalBroker::new(true, 8);
        let mut per_request_disabled = request("a2");
        per_request_disabled.allow_always_available = false;
        let _waiter = enabled.register(per_request_disabled).unwrap();
        assert_eq!(
            enabled.resolve(
                "a2",
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::AllowAlwaysUnavailable)
        );
    }

    #[test]
    fn allow_always_requires_both_host_policy_and_capability() {
        let disabled = ApprovalBroker::new(false, 8);
        let _waiter = disabled.register(request("a1")).unwrap();
        assert_eq!(
            disabled.resolve(
                "a1",
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            ),
            Err(ApprovalError::AllowAlwaysDisabled)
        );

        let enabled = ApprovalBroker::new(true, 8);
        let _waiter = enabled.register(request("a2")).unwrap();
        assert_eq!(
            enabled.resolve(
                "a2",
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
                "a2",
                "remote",
                &[ClientCapability::ApproveAlways],
                ApprovalDecision::AllowAlways,
            )
            .is_ok());
    }

    #[test]
    fn first_resolution_wins_and_late_answers_are_idempotently_rejected() {
        let broker = ApprovalBroker::new(false, 8);
        let _waiter = broker.register(request("a1")).unwrap();
        broker
            .resolve(
                "a1",
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            )
            .unwrap();
        assert_eq!(
            broker.resolve(
                "a1",
                "remote",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::AllowOnce,
            ),
            Err(ApprovalError::AlreadyResolved)
        );
        assert_eq!(
            broker.register(request("a1")).unwrap_err(),
            ApprovalError::DuplicateRequest
        );
    }

    #[tokio::test]
    async fn cancel_all_denies_every_pending_waiter() {
        let broker = ApprovalBroker::new(false, 8);
        let first = broker.register(request("a1")).unwrap();
        let second = broker.register(request("a2")).unwrap();
        let resolutions = broker.cancel_all("daemon_shutdown");
        assert_eq!(resolutions.len(), 2);
        assert!(broker.pending().is_empty());
        assert_eq!(first.await.unwrap().decision, ApprovalDecision::Deny);
        assert_eq!(second.await.unwrap().decision, ApprovalDecision::Deny);
    }

    #[test]
    fn resolved_tombstones_are_bounded() {
        let broker = ApprovalBroker::new(false, 2);
        for id in ["a1", "a2", "a3"] {
            let _waiter = broker.register(request(id)).unwrap();
            broker
                .resolve(
                    id,
                    "local",
                    &[ClientCapability::ApproveOnce],
                    ApprovalDecision::Deny,
                )
                .unwrap();
        }
        assert_eq!(broker.resolved_count(), 2);
        assert_eq!(
            broker.resolve(
                "a1",
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::UnknownApproval)
        );
        assert_eq!(
            broker.resolve(
                "a3",
                "local",
                &[ClientCapability::ApproveOnce],
                ApprovalDecision::Deny,
            ),
            Err(ApprovalError::AlreadyResolved)
        );
    }
}
