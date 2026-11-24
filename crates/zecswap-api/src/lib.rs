use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use zecswap_core::{PublicShare, ShareProof, ViewingKeys};

pub mod network;
pub mod relayer;
pub mod reverse;
#[cfg(feature = "server")]
pub mod server;
pub mod service;
pub mod tokens;

pub use network::Network;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuoteRequest {
    pub units: u32,
    pub payout: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_note: Option<B256>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub quote_id: B256,
    pub maker: Address,
    #[serde(with = "bytes64")]
    pub maker_share: PublicShare,
    #[serde(with = "bytes64")]
    pub maker_proof: ShareProof,
    pub chain_id: u64,
    pub contract: Address,
    pub token: Address,
    #[serde(with = "decimal")]
    pub amount: u128,
    pub deposit_zat: u64,
    pub expires_at: u64,
    #[serde(
        default,
        with = "optional_decimal",
        skip_serializing_if = "Option::is_none"
    )]
    pub network_cost: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_fee: Option<OpenFee>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFee {
    pub zat: u64,
    pub address: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Acceptance {
    #[serde(with = "bytes64")]
    pub user_share: PublicShare,
    #[serde(with = "bytes64")]
    pub user_proof: ShareProof,
    #[serde(with = "bytes64")]
    pub viewing_keys: ViewingKeys,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_request: Option<String>,
}

pub const TICKET_HEADER: &str = "ZecSwap-Ticket";
pub const WINDOWS_CONTRACT: &str = "windows";

pub fn fits_windows(
    kind: Option<&str>,
    min_window: Option<u64>,
    late_window: Option<u64>,
    (min, late): (u64, u64),
) -> bool {
    kind.is_none_or(|kind| kind == WINDOWS_CONTRACT)
        && min_window.is_none_or(|window| window == min)
        && late_window.is_none_or(|window| window == late)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Accepted {
    pub swap_id: B256,
    pub t0: u64,
    pub t1: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_window: Option<u64>,
    pub ticket: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub swap_id: B256,
    pub token_return: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Terms {
    pub maker: Address,
    pub token: Address,
    #[serde(with = "decimal")]
    pub amount: u128,
    #[serde(with = "bytes64")]
    pub maker_key: PublicShare,
    #[serde(with = "bytes64")]
    pub user_key: PublicShare,
    pub user: Address,
    pub t0: u64,
    pub t1: u64,
    pub payout_note: B256,
}

impl From<&zecswap_core::Terms> for Terms {
    fn from(terms: &zecswap_core::Terms) -> Self {
        Self {
            maker: terms.maker.into(),
            token: terms.token.into(),
            amount: terms.amount,
            maker_key: terms.maker_share,
            user_key: terms.user_share,
            user: terms.user.into(),
            t0: terms.t0,
            t1: terms.t1,
            payout_note: terms.payout_note.into(),
        }
    }
}

impl From<&Terms> for zecswap_core::Terms {
    fn from(terms: &Terms) -> Self {
        Self {
            maker: terms.maker.into(),
            token: terms.token.into(),
            amount: terms.amount,
            maker_share: terms.maker_key,
            user_share: terms.user_key,
            user: terms.user.into(),
            t0: terms.t0,
            t1: terms.t1,
            payout_note: terms.payout_note.0,
        }
    }
}

mod bytes64 {
    use alloy_primitives::FixedBytes;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use zecswap_core::{Error, PublicShare, ShareProof, ViewingKeys};

    pub(crate) trait Bytes64: Sized {
        fn encode(&self) -> [u8; 64];
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error>;
    }

    impl Bytes64 for PublicShare {
        fn encode(&self) -> [u8; 64] {
            self.to_affine_bytes()
        }
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error> {
            Self::from_affine_bytes(bytes)
        }
    }

    impl Bytes64 for ShareProof {
        fn encode(&self) -> [u8; 64] {
            self.to_bytes()
        }
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error> {
            Ok(Self::from_bytes(*bytes))
        }
    }

    impl Bytes64 for ViewingKeys {
        fn encode(&self) -> [u8; 64] {
            self.to_bytes()
        }
        fn decode(bytes: &[u8; 64]) -> Result<Self, Error> {
            Self::from_bytes(bytes)
        }
    }

    pub(crate) fn serialize<T: Bytes64, S: Serializer>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        FixedBytes(value.encode()).serialize(s)
    }

    pub(crate) fn deserialize<'de, T: Bytes64, D: Deserializer<'de>>(d: D) -> Result<T, D::Error> {
        T::decode(&FixedBytes::<64>::deserialize(d)?.0).map_err(D::Error::custom)
    }
}

pub(crate) mod decimal {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?.parse().map_err(D::Error::custom)
    }
}

pub(crate) mod optional_decimal {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(value: &Option<u128>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => s.collect_str(value),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u128>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|value| value.parse().map_err(D::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zecswap_core::derive_maker_share;

    use super::*;
    use crate::service::ErrorCode;

    #[test]
    fn a_quote_names_its_open_fee_only_where_the_maker_asks_one() {
        let quote = Quote {
            quote_id: B256::repeat_byte(5),
            maker: Address::repeat_byte(1),
            maker_share: derive_maker_share(&[9; 32], 0).unwrap().public(),
            maker_proof: ShareProof::from_bytes([0; 64]),
            chain_id: 11_155_111,
            contract: Address::repeat_byte(0x11),
            token: Address::repeat_byte(3),
            amount: 1_000_000,
            deposit_zat: 100_000,
            expires_at: 1_790_000_000,
            network_cost: None,
            open_fee: None,
        };
        let free = serde_json::to_value(&quote).unwrap();
        assert_eq!(free.get("openFee"), None);
        assert_eq!(
            serde_json::from_value::<Quote>(free.clone())
                .unwrap()
                .open_fee,
            None
        );

        let mut asking = free;
        asking["openFee"] = json!({"zat": 41_234, "address": "utest1fee"});
        let fee = OpenFee {
            zat: 41_234,
            address: "utest1fee".into(),
        };
        let read: Quote = serde_json::from_value(asking.clone()).unwrap();
        assert_eq!(read.open_fee, Some(fee));
        assert_eq!(serde_json::to_value(&read).unwrap(), asking);
        assert_eq!(json!(ErrorCode::FeeRequired), "feeRequired");
    }
}
