use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::session_protocol::ClientCapability;

const AUTH_DOMAIN: &[u8] = b"daimonos-remote-v2\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingClaim {
    pub secret: String,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub id: String,
    pub device_id: String,
    pub fingerprint: String,
    pub label: String,
    pub requested_capabilities: Vec<ClientCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketGrant {
    pub ticket: String,
    pub device_id: String,
    pub capabilities: Vec<ClientCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDevice {
    pub device_id: String,
    pub label: String,
    pub capabilities: Vec<ClientCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    UnknownClaim,
    ExpiredClaim,
    InvalidDeviceKey,
    PairingNotFound,
    PairingAlreadyResolved,
    CapabilitiesNotRequested,
    UnknownTicket,
    DeviceKeyMismatch,
    InvalidSignature,
    DeviceLimitReached,
}

#[derive(Debug)]
struct ClaimState {
    expires_at: Instant,
}

#[derive(Debug)]
enum PairingResolution {
    Pending,
    Approved(TicketGrant),
    Denied,
}

#[derive(Debug)]
struct PendingState {
    request: PendingPairing,
    verifying_key: VerifyingKey,
    resolution: PairingResolution,
}

#[derive(Debug)]
struct TicketState {
    device_id: String,
    label: String,
    verifying_key: VerifyingKey,
    capabilities: Vec<ClientCapability>,
}

#[derive(Debug, Default)]
struct AuthorityState {
    claims: HashMap<String, ClaimState>,
    pending: HashMap<String, PendingState>,
    tickets: HashMap<String, TicketState>,
    device_notifiers: HashMap<String, tokio::sync::watch::Sender<bool>>,
}

#[derive(Debug)]
pub struct PairingAuthority {
    state: Mutex<AuthorityState>,
    claim_ttl: Duration,
    max_devices: usize,
}

impl Default for PairingAuthority {
    fn default() -> Self {
        Self::new(Duration::from_secs(5 * 60), 64)
    }
}

impl PairingAuthority {
    pub fn new(claim_ttl: Duration, max_devices: usize) -> Self {
        Self {
            state: Mutex::new(AuthorityState::default()),
            claim_ttl,
            max_devices,
        }
    }

    pub fn create_claim(&self) -> PairingClaim {
        self.create_claim_at(Instant::now())
    }

    fn create_claim_at(&self, now: Instant) -> PairingClaim {
        let secret = random_secret();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.claims.retain(|_, claim| claim.expires_at > now);
        state.claims.insert(
            secret.clone(),
            ClaimState {
                expires_at: now + self.claim_ttl,
            },
        );
        PairingClaim {
            secret,
            expires_in_secs: self.claim_ttl.as_secs(),
        }
    }

    pub fn submit_pairing(
        &self,
        claim: &str,
        device_public_key: &str,
        label: String,
        requested_capabilities: Vec<ClientCapability>,
    ) -> Result<PendingPairing, AuthError> {
        self.submit_pairing_at(
            claim,
            device_public_key,
            label,
            requested_capabilities,
            Instant::now(),
        )
    }

    fn submit_pairing_at(
        &self,
        claim: &str,
        device_public_key: &str,
        label: String,
        requested_capabilities: Vec<ClientCapability>,
        now: Instant,
    ) -> Result<PendingPairing, AuthError> {
        let verifying_key = decode_verifying_key(device_public_key)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let claim_state = state.claims.remove(claim).ok_or(AuthError::UnknownClaim)?;
        if claim_state.expires_at <= now {
            return Err(AuthError::ExpiredClaim);
        }
        let device_id = device_id(&verifying_key);
        let request = PendingPairing {
            id: uuid::Uuid::new_v4().to_string(),
            fingerprint: display_fingerprint(&device_id),
            device_id,
            label,
            requested_capabilities: deduplicate_capabilities(requested_capabilities),
        };
        state.pending.insert(
            request.id.clone(),
            PendingState {
                request: request.clone(),
                verifying_key,
                resolution: PairingResolution::Pending,
            },
        );
        Ok(request)
    }

    pub fn pending_pairings(&self) -> Vec<PendingPairing> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending: Vec<_> = state
            .pending
            .values()
            .filter(|pending| matches!(pending.resolution, PairingResolution::Pending))
            .map(|pending| pending.request.clone())
            .collect();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }

    pub fn approve(
        &self,
        pairing_id: &str,
        capabilities: Vec<ClientCapability>,
    ) -> Result<TicketGrant, AuthError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let device_id = state
            .pending
            .get(pairing_id)
            .ok_or(AuthError::PairingNotFound)?
            .request
            .device_id
            .clone();
        if !state.device_notifiers.contains_key(&device_id)
            && state.device_notifiers.len() >= self.max_devices
        {
            return Err(AuthError::DeviceLimitReached);
        }
        let pending = state
            .pending
            .get_mut(pairing_id)
            .ok_or(AuthError::PairingNotFound)?;
        if !matches!(pending.resolution, PairingResolution::Pending) {
            return Err(AuthError::PairingAlreadyResolved);
        }
        let capabilities = deduplicate_capabilities(capabilities);
        if capabilities
            .iter()
            .any(|capability| !pending.request.requested_capabilities.contains(capability))
        {
            return Err(AuthError::CapabilitiesNotRequested);
        }
        let grant = TicketGrant {
            ticket: random_secret(),
            device_id: pending.request.device_id.clone(),
            capabilities: capabilities.clone(),
        };
        let ticket_state = TicketState {
            device_id: pending.request.device_id.clone(),
            label: pending.request.label.clone(),
            verifying_key: pending.verifying_key,
            capabilities,
        };
        pending.resolution = PairingResolution::Approved(grant.clone());
        state.tickets.insert(grant.ticket.clone(), ticket_state);
        state
            .device_notifiers
            .entry(grant.device_id.clone())
            .or_insert_with(|| tokio::sync::watch::channel(false).0);
        Ok(grant)
    }

    pub fn deny(&self, pairing_id: &str) -> Result<(), AuthError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pending = state
            .pending
            .get_mut(pairing_id)
            .ok_or(AuthError::PairingNotFound)?;
        if !matches!(pending.resolution, PairingResolution::Pending) {
            return Err(AuthError::PairingAlreadyResolved);
        }
        pending.resolution = PairingResolution::Denied;
        Ok(())
    }

    pub fn pairing_grant(&self, pairing_id: &str) -> Result<Option<TicketGrant>, AuthError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pending = state
            .pending
            .get(pairing_id)
            .ok_or(AuthError::PairingNotFound)?;
        match &pending.resolution {
            PairingResolution::Pending => Ok(None),
            PairingResolution::Approved(grant) => Ok(Some(grant.clone())),
            PairingResolution::Denied => Err(AuthError::PairingAlreadyResolved),
        }
    }

    pub fn finish_pairing(&self, pairing_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = state.pending.remove(pairing_id) else {
            return;
        };
        let PairingResolution::Approved(grant) = pending.resolution else {
            return;
        };
        let replaces_existing = state
            .tickets
            .iter()
            .any(|(ticket, state)| ticket != &grant.ticket && state.device_id == grant.device_id);
        if replaces_existing {
            if let Some(notifier) = state.device_notifiers.remove(&grant.device_id) {
                notifier.send_replace(true);
            }
            state.tickets.retain(|ticket, state| {
                ticket == &grant.ticket || state.device_id != grant.device_id
            });
            state
                .device_notifiers
                .insert(grant.device_id, tokio::sync::watch::channel(false).0);
        }
    }

    pub fn abort_pairing(&self, pairing_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pending) = state.pending.remove(pairing_id) else {
            return;
        };
        if let PairingResolution::Approved(grant) = pending.resolution {
            state.tickets.remove(&grant.ticket);
            if !state
                .tickets
                .values()
                .any(|ticket| ticket.device_id == grant.device_id)
            {
                state.device_notifiers.remove(&grant.device_id);
            }
        }
    }

    pub fn authenticate(
        &self,
        ticket: &str,
        device_public_key: &str,
        challenge: &str,
        signature: &str,
    ) -> Result<AuthenticatedDevice, AuthError> {
        let presented_key = decode_verifying_key(device_public_key)?;
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ticket_state = state.tickets.get(ticket).ok_or(AuthError::UnknownTicket)?;
        if ticket_state.verifying_key != presented_key {
            return Err(AuthError::DeviceKeyMismatch);
        }
        let signature_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| AuthError::InvalidSignature)?;
        let signature =
            Signature::from_slice(&signature_bytes).map_err(|_| AuthError::InvalidSignature)?;
        presented_key
            .verify(&auth_message(challenge, ticket), &signature)
            .map_err(|_| AuthError::InvalidSignature)?;
        Ok(AuthenticatedDevice {
            device_id: ticket_state.device_id.clone(),
            label: ticket_state.label.clone(),
            capabilities: ticket_state.capabilities.clone(),
        })
    }

    pub fn revoke_device(&self, device_id: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = state.tickets.len();
        state
            .tickets
            .retain(|_, ticket| ticket.device_id != device_id);
        if let Some(notifier) = state.device_notifiers.remove(device_id) {
            notifier.send_replace(true);
        }
        state.tickets.len() != before
    }

    pub fn revocation_receiver(
        &self,
        device_id: &str,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .device_notifiers
            .get(device_id)
            .map(tokio::sync::watch::Sender::subscribe)
    }
}

pub fn auth_message(challenge: &str, ticket: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(AUTH_DOMAIN.len() + challenge.len() + ticket.len() + 2);
    message.extend_from_slice(AUTH_DOMAIN);
    message.extend_from_slice(challenge.as_bytes());
    message.push(0);
    message.extend_from_slice(ticket.as_bytes());
    message
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn decode_verifying_key(encoded: &str) -> Result<VerifyingKey, AuthError> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AuthError::InvalidDeviceKey)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| AuthError::InvalidDeviceKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| AuthError::InvalidDeviceKey)
}

fn device_id(key: &VerifyingKey) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

fn display_fingerprint(device_id: &str) -> String {
    device_id
        .as_bytes()
        .chunks(4)
        .take(8)
        .map(|chunk| std::str::from_utf8(chunk).expect("hex fingerprint"))
        .collect::<Vec<_>>()
        .join(":")
}

fn deduplicate_capabilities(capabilities: Vec<ClientCapability>) -> Vec<ClientCapability> {
    let mut deduplicated = Vec::new();
    for capability in capabilities {
        if !deduplicated.contains(&capability) {
            deduplicated.push(capability);
        }
    }
    deduplicated
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn encoded_key(key: &SigningKey) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
    }

    fn pair(authority: &PairingAuthority, key: &SigningKey) -> (PendingPairing, TicketGrant) {
        let claim = authority.create_claim();
        let pending = authority
            .submit_pairing(
                &claim.secret,
                &encoded_key(key),
                "test phone".to_string(),
                vec![ClientCapability::Observe, ClientCapability::Prompt],
            )
            .unwrap();
        let grant = authority
            .approve(
                &pending.id,
                vec![ClientCapability::Observe, ClientCapability::Prompt],
            )
            .unwrap();
        authority.finish_pairing(&pending.id);
        (pending, grant)
    }

    #[test]
    fn pairing_claim_is_single_use_and_expiring() {
        let authority = PairingAuthority::new(Duration::from_secs(60), 64);
        let now = Instant::now();
        let claim = authority.create_claim_at(now);
        let key = SigningKey::from_bytes(&[7; 32]);
        authority
            .submit_pairing_at(
                &claim.secret,
                &encoded_key(&key),
                "phone".to_string(),
                vec![ClientCapability::Observe],
                now,
            )
            .unwrap();
        assert_eq!(
            authority.submit_pairing_at(
                &claim.secret,
                &encoded_key(&key),
                "phone".to_string(),
                vec![ClientCapability::Observe],
                now,
            ),
            Err(AuthError::UnknownClaim)
        );

        let expired = authority.create_claim_at(now);
        assert_eq!(
            authority.submit_pairing_at(
                &expired.secret,
                &encoded_key(&key),
                "phone".to_string(),
                vec![ClientCapability::Observe],
                now + Duration::from_secs(61),
            ),
            Err(AuthError::ExpiredClaim)
        );
        authority.create_claim_at(now + Duration::from_secs(62));
        assert_eq!(
            authority
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .claims
                .len(),
            1
        );
    }

    #[test]
    fn local_approval_cannot_grant_unrequested_capability() {
        let authority = PairingAuthority::default();
        let key = SigningKey::from_bytes(&[8; 32]);
        let claim = authority.create_claim();
        let pending = authority
            .submit_pairing(
                &claim.secret,
                &encoded_key(&key),
                "phone".to_string(),
                vec![ClientCapability::Observe],
            )
            .unwrap();

        assert_eq!(
            authority.approve(
                &pending.id,
                vec![ClientCapability::Observe, ClientCapability::Prompt],
            ),
            Err(AuthError::CapabilitiesNotRequested)
        );
    }

    #[test]
    fn ticket_requires_proof_from_bound_device_key() {
        let authority = PairingAuthority::default();
        let key = SigningKey::from_bytes(&[9; 32]);
        let (_, grant) = pair(&authority, &key);
        let challenge = "fresh-challenge";
        let signature = key.sign(&auth_message(challenge, &grant.ticket));
        let signature =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes());

        let authenticated = authority
            .authenticate(&grant.ticket, &encoded_key(&key), challenge, &signature)
            .unwrap();
        assert_eq!(authenticated.device_id, grant.device_id);
        assert_eq!(authenticated.capabilities, grant.capabilities);

        let attacker = SigningKey::from_bytes(&[10; 32]);
        let attacker_signature = attacker.sign(&auth_message(challenge, &grant.ticket));
        assert_eq!(
            authority.authenticate(
                &grant.ticket,
                &encoded_key(&attacker),
                challenge,
                &base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(attacker_signature.to_bytes()),
            ),
            Err(AuthError::DeviceKeyMismatch)
        );
    }

    #[test]
    fn revocation_invalidates_every_ticket_for_device() {
        let authority = PairingAuthority::default();
        let key = SigningKey::from_bytes(&[11; 32]);
        let (pending, grant) = pair(&authority, &key);
        assert!(authority.revoke_device(&pending.device_id));
        let signature = key.sign(&auth_message("challenge", &grant.ticket));

        assert_eq!(
            authority.authenticate(
                &grant.ticket,
                &encoded_key(&key),
                "challenge",
                &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            ),
            Err(AuthError::UnknownTicket)
        );
    }

    #[test]
    fn completed_pairing_state_is_released() {
        let authority = PairingAuthority::default();
        let key = SigningKey::from_bytes(&[12; 32]);
        let claim = authority.create_claim();
        let pending = authority
            .submit_pairing(
                &claim.secret,
                &encoded_key(&key),
                "phone".to_string(),
                vec![ClientCapability::Observe],
            )
            .unwrap();
        authority
            .approve(&pending.id, vec![ClientCapability::Observe])
            .unwrap();
        assert!(authority.pairing_grant(&pending.id).unwrap().is_some());
        authority.finish_pairing(&pending.id);
        assert_eq!(
            authority.pairing_grant(&pending.id),
            Err(AuthError::PairingNotFound)
        );
    }

    #[test]
    fn pairing_abort_removes_an_undelivered_ticket() {
        let authority = PairingAuthority::default();
        let key = SigningKey::from_bytes(&[13; 32]);
        let claim = authority.create_claim();
        let pending = authority
            .submit_pairing(
                &claim.secret,
                &encoded_key(&key),
                "phone".to_string(),
                vec![ClientCapability::Observe],
            )
            .unwrap();
        let grant = authority
            .approve(&pending.id, vec![ClientCapability::Observe])
            .unwrap();
        authority.abort_pairing(&pending.id);
        let signature = key.sign(&auth_message("challenge", &grant.ticket));
        assert_eq!(
            authority.authenticate(
                &grant.ticket,
                &encoded_key(&key),
                "challenge",
                &base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes()),
            ),
            Err(AuthError::UnknownTicket)
        );
    }

    #[test]
    fn paired_device_limit_is_bounded_and_repair_replaces_ticket() {
        let authority = PairingAuthority::new(Duration::from_secs(60), 1);
        let first_key = SigningKey::from_bytes(&[14; 32]);
        let (_, first_grant) = pair(&authority, &first_key);
        let revoked = authority
            .revocation_receiver(&first_grant.device_id)
            .unwrap();
        let second_key = SigningKey::from_bytes(&[15; 32]);
        let second_claim = authority.create_claim();
        let second_pending = authority
            .submit_pairing(
                &second_claim.secret,
                &encoded_key(&second_key),
                "second".to_string(),
                vec![ClientCapability::Observe],
            )
            .unwrap();
        assert_eq!(
            authority.approve(&second_pending.id, vec![ClientCapability::Observe]),
            Err(AuthError::DeviceLimitReached)
        );

        let repair_claim = authority.create_claim();
        let repair_pending = authority
            .submit_pairing(
                &repair_claim.secret,
                &encoded_key(&first_key),
                "first".to_string(),
                vec![ClientCapability::Observe],
            )
            .unwrap();
        let replacement = authority
            .approve(&repair_pending.id, vec![ClientCapability::Observe])
            .unwrap();
        assert_ne!(first_grant.ticket, replacement.ticket);
        assert!(authority
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tickets
            .contains_key(&first_grant.ticket));
        authority.finish_pairing(&repair_pending.id);
        assert!(*revoked.borrow());
        assert!(!authority
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .tickets
            .contains_key(&first_grant.ticket));
    }
}
