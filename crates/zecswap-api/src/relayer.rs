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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deposits_percentage_is_rounded_up_onto_the_fee() {
        assert_eq!(deposit_fee(100_000, 10, 25_000_000), 125_000);
        assert_eq!(deposit_fee(100_000, 10, 1), 100_001);
        assert_eq!(deposit_fee(0, 10, 9_999), 10);
        assert_eq!(deposit_fee(0, 10, 10_000), 10);
        assert_eq!(deposit_fee(100_000, 0, 25_000_000), 100_000);
        assert_eq!(deposit_fee(u128::MAX, 10, u128::MAX), u128::MAX);
    }

    #[test]
    fn terms_name_the_windows_contract_and_a_lock_relayers_still_read() {
        let lock = r#"{"relayer":"0x2222222222222222222222222222222222222222","chainId":11155111,
            "contract":"0x3333333333333333333333333333333333333333","fee":"100000"}"#;
        let lock: Terms = serde_json::from_str(lock).unwrap();
        assert_eq!(lock.contract_kind, None);
        assert!(
            lock.fits_windows((1_800, 3_600)),
            "the wallet reads the contract itself"
        );
        let windows = Terms {
            contract_kind: Some(crate::WINDOWS_CONTRACT.into()),
            min_window: Some(1_800),
            late_window: Some(3_600),
            ..lock
        };
        let json = serde_json::to_value(&windows).unwrap();
        assert_eq!(json["contractKind"], "windows");
        assert_eq!(
            (json["minWindow"].clone(), json["lateWindow"].clone()),
            (1_800.into(), 3_600.into())
        );
        assert!(windows.fits_windows((1_800, 3_600)));
        assert!(!windows.fits_windows((1_800, 7_200)));
        assert!(!windows.fits_windows((600, 3_600)));
        let other = Terms {
            contract_kind: Some("locks".into()),
            ..windows
        };
        assert!(!other.fits_windows((1_800, 3_600)));
    }
}
