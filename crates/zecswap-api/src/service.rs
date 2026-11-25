use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    InvalidRequest,
    Rejected,
    UnknownQuote,
    UnknownSwap,
    Unavailable,
    WatchtowerUnavailable,
    Internal,
    NotFound,
    MethodNotAllowed,
    TokenRequired,
    FeeRequired,
    AlreadySpent,
    Conflict,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MakerInfo {
    pub api_version: u32,
    pub maker: Address,
    pub chain_id: u64,
    pub contract: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_window: Option<u64>,
    pub token: Address,
    pub zcash_network: crate::Network,
    pub reverse_enabled: bool,
    pub token_return_key: Option<String>,
}

impl MakerInfo {
    pub fn fits_windows(&self, windows: (u64, u64)) -> bool {
        crate::fits_windows(
            self.contract_kind.as_deref(),
            self.min_window,
            self.late_window,
            windows,
        )
    }
}
