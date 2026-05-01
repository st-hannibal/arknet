//! On-chain gateway registry entries stored in `CF_GATEWAYS`.
//!
//! Gateways are router nodes with publicly accessible RPC endpoints.
//! Users and SDKs discover them via `/v1/gateways` to connect to the
//! network without depending on a single hardcoded URL.

use arknet_common::types::{Address, NodeId};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// HTTPS gateway reward multiplier: 1.2x (12000 basis points).
/// HTTPS gateways protect the last-mile (user → gateway) with TLS,
/// complementing the Noise-encrypted P2P layer (gateway → compute).
pub const HTTPS_MULTIPLIER_BPS: u32 = 12_000;

/// Non-HTTPS gateway: no multiplier (1.0x = 10000 basis points).
pub const HTTP_MULTIPLIER_BPS: u32 = 10_000;

/// Maximum URL length for a gateway registration.
pub const MAX_GATEWAY_URL_LEN: usize = 512;

/// On-chain record for a registered public gateway.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct GatewayEntry {
    /// Node operating this gateway.
    pub node_id: NodeId,
    /// Operator payout address.
    pub operator: Address,
    /// Public RPC URL.
    pub url: String,
    /// `true` if the gateway uses HTTPS.
    pub https: bool,
    /// Block height at which this gateway was registered.
    pub registered_at: u64,
}
