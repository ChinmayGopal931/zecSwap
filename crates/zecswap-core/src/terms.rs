use crate::auth::{address, keccak, uint};
use crate::{PublicShare, ReverseOpen};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terms {
    pub maker: [u8; 20],
    pub token: [u8; 20],
    pub amount: u128,
    pub maker_share: PublicShare,
    pub user_share: PublicShare,
    pub user: [u8; 20],
    pub t0: u64,
    pub t1: u64,
    pub payout_note: [u8; 32],
}

impl Terms {
    pub fn hash(&self) -> [u8; 32] {
        keccak(&[
            &address(&self.maker),
            &address(&self.token),
            &uint(self.amount),
            &self.maker_share.to_affine_bytes(),
            &self.user_share.to_affine_bytes(),
            &address(&self.user),
            &uint(self.t0.into()),
            &uint(self.t1.into()),
            &self.payout_note,
        ])
    }
}

impl ReverseOpen {
    pub fn terms(&self) -> Terms {
        Terms {
            maker: self.user,
            token: self.token,
            amount: self.amount,
            maker_share: self.user_share,
            user_share: self.maker_share,
            user: self.maker,
            t0: self.t0,
            t1: self.t1,
            payout_note: [0; 32],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NetworkType, derive_maker_share, derive_user_keys};

    fn bytes<const N: usize>(hex: &str) -> [u8; N] {
        let hex = hex.trim_start_matches("0x");
        core::array::from_fn(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
    }

    #[test]
    fn hashes_match_the_contracts_known_answers() {
        let user_share = derive_user_keys(&[7; 64], NetworkType::Test, 0, 0)
            .unwrap()
            .share
            .public();
        let maker_share = derive_maker_share(&[9; 32], 0).unwrap().public();
        let pays_account = Terms {
            maker: bytes("0x09eD1F966745Be18C711C346242c0974DAd7c3e5"),
            token: [0x33; 20],
            amount: 150_000_000,
            maker_share,
            user_share,
            user: [0x44; 20],
            t0: 1_790_003_600,
            t1: 1_790_007_200,
            payout_note: [0; 32],
        };
        let pays_railgun = Terms {
            user: bytes("0x757De38c2d9880E44AB59827D1622403fBF88Ff5"),
            payout_note: bytes(
                "0x5af6901ba7cb01f49785a29c4a2e57e31af3e53382ce3dd2e35678897515ffc1",
            ),
            ..pays_account.clone()
        };
        let reverse = ReverseOpen {
            maker: pays_account.maker,
            user: pays_railgun.user,
            token: [0x33; 20],
            amount: 50_000_000,
            maker_share,
            user_share,
            t0: 1_790_003_600,
            t1: 1_790_007_200,
            refund_note: [0xee; 32],
            deadline: 1_790_000_000,
        }
        .terms();
        for (terms, hash) in [
            (
                pays_account,
                "0x909f60119225ae64356011c63945694ed0dc6aaaa8cd6002a897df9ab76efc66",
            ),
            (
                pays_railgun,
                "0xb7ca9333e9e443218d19c3b8aa345fa67a671ac24060877de1498efb01c99a29",
            ),
            (
                reverse,
                "0xa295e46f6db15997af4a950b5e6deed6ae5d3161b6571b2f445a34bc6570cead",
            ),
        ] {
            assert_eq!(terms.hash(), bytes(hash));
        }
    }
}
