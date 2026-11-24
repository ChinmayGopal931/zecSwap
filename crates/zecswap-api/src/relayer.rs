use alloy_primitives::{Address, B256, Bytes, FixedBytes};
use serde::{Deserialize, Serialize};
use zecswap_railgun::{ShieldCiphertext, ShieldNote};

use crate::decimal;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terms {
    pub relayer: Address,
    pub chain_id: u64,
    pub contract: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_window: Option<u64>,
    #[serde(with = "decimal")]
    pub fee: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_funding: Option<ReverseFundingTerms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub railgun_sends: Option<RailgunSendTerms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deposits: Option<DepositTerms>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositTerms {
    pub contract: Address,
    pub relayer: Address,
    pub token: Address,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub fee_bps: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_expires_at: Option<u64>,
    #[serde(with = "decimal")]
    pub min_value: u128,
    pub max_validity: u64,
    #[serde(with = "decimal")]
    pub max_gas_price_wei: u128,
}

pub fn deposit_fee(fee: u128, fee_bps: u32, value: u128) -> u128 {
    fee.saturating_add(value.saturating_mul(fee_bps.into()).div_ceil(10_000))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RailgunSendTerms {
    pub railgun_address: String,
    pub railgun_proxy: Address,
    pub token: Address,
    #[serde(with = "decimal")]
    pub fee: u128,
    #[serde(
        default,
        with = "crate::optional_decimal",
        skip_serializing_if = "Option::is_none"
    )]
    pub fee_per_unit_gas: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_expires_at: Option<u64>,
    pub max_gas_limit: u64,
    #[serde(with = "decimal")]
    pub max_gas_price_wei: u128,
    pub max_calldata_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseFundingTerms {
    pub relay_adapt: Address,
    pub token: Address,
    pub maker: Address,
    pub max_gas_limit: u64,
    #[serde(with = "decimal")]
    pub max_gas_price_wei: u128,
    pub max_calldata_bytes: usize,
    #[serde(with = "decimal")]
    pub fee: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claim {
    pub swap_id: B256,
    pub terms: crate::Terms,
    pub secret: B256,
    pub payout: Payout,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub held: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Payout {
    pub swap_id: B256,
    pub terms: crate::Terms,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub signature: FixedBytes<65>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rescue {
    pub swap_id: B256,
    pub terms: crate::Terms,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub nonce: u64,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Note {
    pub npk: B256,
    pub encrypted_bundle: [B256; 3],
    pub shield_key: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RailgunTransact {
    pub chain_id: u64,
    pub to: Address,
    pub data: Bytes,
    #[serde(with = "decimal")]
    pub value: u128,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub code: crate::service::ErrorCode,
    pub error: String,
    pub transactions: Vec<B256>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShieldDeposit {
    pub chain_id: u64,
    pub contract: Address,
    pub from: Address,
    #[serde(with = "decimal")]
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub signature: FixedBytes<65>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueDeposit {
    pub chain_id: u64,
    pub contract: Address,
    pub from: Address,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub nonce: u64,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sent {
    pub transactions: Vec<B256>,
}

impl Terms {
    pub fn fits_windows(&self, windows: (u64, u64)) -> bool {
        crate::fits_windows(
            self.contract_kind.as_deref(),
            self.min_window,
            self.late_window,
            windows,
        )
    }
}

impl From<&ShieldNote> for Note {
    fn from(note: &ShieldNote) -> Self {
        Self {
            npk: note.npk.into(),
            encrypted_bundle: note.ciphertext.encrypted_bundle.map(B256::from),
            shield_key: note.ciphertext.shield_key.into(),
        }
    }
}

impl From<&Note> for ShieldNote {
    fn from(note: &Note) -> Self {
        Self {
            npk: note.npk.0,
            ciphertext: ShieldCiphertext {
                encrypted_bundle: note.encrypted_bundle.map(|word| word.0),
                shield_key: note.shield_key.0,
            },
        }
    }
}
