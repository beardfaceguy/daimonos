//! Compatibility facade for the original headless frontend API.
//!
//! Task 1331 extracts daemon-session ownership into [`crate::session_client`].
//! Existing headless callers keep their names while the TUI and future actor
//! layer consume the reusable `SessionClient` surface directly.

#![allow(dead_code, unused_imports)]

pub use crate::session_client::{
    SessionClient as HeadlessFrontend, SessionClientError as HeadlessError,
    SessionClientOutcome as ReceiveOutcome,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_transport::in_memory_transport_pair;
    use crate::session_protocol::{ClientInfo, ClientKind};

    #[test]
    fn legacy_headless_names_construct_the_reusable_client() {
        let (_server, transport) = in_memory_transport_pair(1, "daemon");
        let frontend: HeadlessFrontend<_> = HeadlessFrontend::new(
            transport,
            ClientInfo {
                id: "compat-client".to_string(),
                kind: ClientKind::Headless,
                label: "compatibility test".to_string(),
            },
            Vec::new(),
            8,
        );
        assert!(!frontend.is_attached());
        let _: Option<HeadlessError> = None;
        let _: Option<ReceiveOutcome> = None;
    }
}
