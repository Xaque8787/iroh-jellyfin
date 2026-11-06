//! Connects the transport's device checks to the persisted host state.

use std::sync::Arc;

use iroh_transport::host::{AuthDecision, DeviceRegistry};
use iroh_transport::pairing::RedeemError;
use iroh_transport::policy::TransportPolicy;
use iroh_transport::protocol::error_code;
use iroh_transport::websocket::WebSocketSettings;

use crate::state::{HostState, PairOutcome, now_secs};

pub struct StateRegistry {
    state: Arc<HostState>,
}

impl StateRegistry {
    pub fn new(state: Arc<HostState>) -> Self {
        Self { state }
    }
}

impl DeviceRegistry for StateRegistry {
    fn authorize(
        &self,
        endpoint_id: &str,
        device_name: &str,
        pairing_token: Option<&str>,
    ) -> AuthDecision {
        match self
            .state
            .pair(endpoint_id, device_name, pairing_token, now_secs())
        {
            Ok(Ok(PairOutcome::AlreadyAuthorized)) => AuthDecision::Authorized,
            Ok(Ok(PairOutcome::Paired)) => AuthDecision::Paired,
            Ok(Err(_)) if pairing_token.is_none() => AuthDecision::Denied {
                code: error_code::NOT_AUTHORIZED,
                reason: "unknown device and no pairing token".into(),
            },
            Ok(Err(RedeemError::RateLimited)) => AuthDecision::Denied {
                code: error_code::PAIRING_RATE_LIMITED,
                reason: "too many failed pairing attempts".into(),
            },
            Ok(Err(error)) => AuthDecision::Denied {
                code: error_code::PAIRING_TOKEN_INVALID,
                reason: format!("{error:?}"),
            },
            Err(error) => AuthDecision::Denied {
                code: error_code::NOT_AUTHORIZED,
                reason: format!("host state unavailable: {error}"),
            },
        }
    }

    fn is_authorized(&self, endpoint_id: &str) -> bool {
        self.state.is_authorized(endpoint_id)
    }

    fn policy(&self) -> TransportPolicy {
        self.state.policy()
    }

    fn websocket(&self) -> WebSocketSettings {
        self.state.websocket()
    }
}
