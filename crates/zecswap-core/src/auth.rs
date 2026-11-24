use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use k256::elliptic_curve::bigint::U512;
use k256::elliptic_curve::generic_array::GenericArray;
use k256::elliptic_curve::ops::Reduce;
use k256::{NonZeroScalar, Scalar};
use sha3::{Digest, Keccak256};

use crate::{Error, PublicShare};

const DOMAIN_TYPE: &[u8] =
    b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const LOCK_CLAIM_TYPE: &[u8] = b"LockClaim(bytes32 id,uint64 deadline)";
const PAYOUT_TYPE: &[u8] = b"Payout(bytes32 id,address relayer,uint128 fee)";
const RESCUE_TYPE: &[u8] =
    b"Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)";
const OPEN_REVERSE_TYPE: &[u8] = b"OpenReverse(address maker,address user,address token,uint128 amount,bytes32 makerKey,bytes32 userKey,uint64 t0,uint64 t1,bytes32 refundNote,uint64 deadline)";
const RECEIVE_WITH_AUTHORIZATION_TYPE: &[u8] = b"ReceiveWithAuthorization(address from,address to,uint256 value,uint256 validAfter,uint256 validBefore,bytes32 nonce)";
const DEPOSIT_TYPE: &[u8] = b"Deposit(uint256 chainId,address deposits,bytes32 npk,bytes32[3] encryptedBundle,bytes32 shieldKey,uint128 fee,address relayer)";
const DEPOSIT_RESCUE_TYPE: &[u8] =
    b"Rescue(address from,bytes32 note,address relayer,uint128 fee,uint64 nonce,uint64 deadline)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReverseOpen {
    pub maker: [u8; 20],
    pub user: [u8; 20],
    pub token: [u8; 20],
    pub amount: u128,
    pub maker_share: PublicShare,
    pub user_share: PublicShare,
    pub t0: u64,
    pub t1: u64,
    pub refund_note: [u8; 32],
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RescueAuthorization {
    pub nonce: u64,
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    pub chain_id: u64,
    pub contract: [u8; 20],
}

impl Domain {
    pub fn open_reverse(&self, terms: &ReverseOpen) -> [u8; 32] {
        self.digest(&[
            &keccak(&[OPEN_REVERSE_TYPE]),
            &address(&terms.maker),
            &address(&terms.user),
            &address(&terms.token),
            &uint(terms.amount),
            &keccak(&[&terms.maker_share.to_affine_bytes()]),
            &keccak(&[&terms.user_share.to_affine_bytes()]),
            &uint(terms.t0.into()),
            &uint(terms.t1.into()),
            &terms.refund_note,
            &uint(terms.deadline.into()),
        ])
    }

    pub fn ready(&self, id: &[u8; 32], deadline: u64) -> [u8; 32] {
        self.digest(&[
            &keccak(&[b"Ready(bytes32 id,uint64 deadline)"]),
            id,
            &uint(deadline.into()),
        ])
    }

    pub fn lock_refund(&self, id: &[u8; 32], deadline: u64) -> [u8; 32] {
        self.digest(&[
            &keccak(&[b"LockRefund(bytes32 id,uint64 deadline)"]),
            id,
            &uint(deadline.into()),
        ])
    }

    pub fn refund_payout(&self, id: &[u8; 32], relayer: &[u8; 20], fee: u128) -> [u8; 32] {
        self.digest(&[
            &keccak(&[b"RefundPayout(bytes32 id,address relayer,uint128 fee)"]),
            id,
            &address(relayer),
            &uint(fee),
        ])
    }

    pub fn lock_claim(&self, id: &[u8; 32], deadline: u64) -> [u8; 32] {
        self.digest(&[&keccak(&[LOCK_CLAIM_TYPE]), id, &uint(deadline.into())])
    }

    pub fn payout(&self, id: &[u8; 32], relayer: &[u8; 20], fee: u128) -> [u8; 32] {
        self.digest(&[&keccak(&[PAYOUT_TYPE]), id, &address(relayer), &uint(fee)])
    }

    pub fn rescue(
        &self,
        id: &[u8; 32],
        note: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
        authorization: RescueAuthorization,
    ) -> [u8; 32] {
        self.digest(&[
            &keccak(&[RESCUE_TYPE]),
            id,
            note,
            &address(relayer),
            &uint(fee),
            &uint(authorization.nonce.into()),
            &uint(authorization.deadline.into()),
        ])
    }

    fn digest(&self, fields: &[&[u8]]) -> [u8; 32] {
        typed(
            &separator(b"ZecSwap", b"1", self.chain_id, &self.contract),
            fields,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenDomain {
    pub name: String,
    pub version: String,
    pub chain_id: u64,
    pub token: [u8; 20],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveAuthorization {
    pub from: [u8; 20],
    pub to: [u8; 20],
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    pub nonce: [u8; 32],
}

impl TokenDomain {
    pub fn separator(&self) -> [u8; 32] {
        separator(
            self.name.as_bytes(),
            self.version.as_bytes(),
            self.chain_id,
            &self.token,
        )
    }

    pub fn receive_with_authorization(&self, authorization: &ReceiveAuthorization) -> [u8; 32] {
        typed(
            &self.separator(),
            &[
                &keccak(&[RECEIVE_WITH_AUTHORIZATION_TYPE]),
                &address(&authorization.from),
                &address(&authorization.to),
                &uint(authorization.value),
                &uint(authorization.valid_after.into()),
                &uint(authorization.valid_before.into()),
                &authorization.nonce,
            ],
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositsDomain {
    pub chain_id: u64,
    pub contract: [u8; 20],
}

impl DepositsDomain {
    pub fn deposit_nonce(&self, note: &[[u8; 32]; 5], fee: u128, relayer: &[u8; 20]) -> [u8; 32] {
        let [npk, bundle @ .., shield_key] = note;
        keccak(&[
            &keccak(&[DEPOSIT_TYPE]),
            &uint(self.chain_id.into()),
            &address(&self.contract),
            npk,
            &bundle.concat(),
            shield_key,
            &uint(fee),
            &address(relayer),
        ])
    }

    pub fn rescue(
        &self,
        from: &[u8; 20],
        note: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
        authorization: RescueAuthorization,
    ) -> [u8; 32] {
        typed(
            &separator(b"ZecSwap Deposits", b"1", self.chain_id, &self.contract),
            &[
                &keccak(&[DEPOSIT_RESCUE_TYPE]),
                &address(from),
                note,
                &address(relayer),
                &uint(fee),
                &uint(authorization.nonce.into()),
                &uint(authorization.deadline.into()),
            ],
        )
    }
}

fn separator(name: &[u8], version: &[u8], chain_id: u64, contract: &[u8; 20]) -> [u8; 32] {
    keccak(&[
        &keccak(&[DOMAIN_TYPE]),
        &keccak(&[name]),
        &keccak(&[version]),
        &uint(chain_id.into()),
        &address(contract),
    ])
}

fn typed(separator: &[u8; 32], fields: &[&[u8]]) -> [u8; 32] {
    keccak(&[b"\x19\x01", separator, &keccak(fields)])
}

pub struct AuthKey(SigningKey);

impl AuthKey {
    pub(crate) fn from_uniform(bytes: &[u8; 64]) -> Result<Self, Error> {
        let scalar = <Scalar as Reduce<U512>>::reduce_bytes(GenericArray::from_slice(bytes));
        Option::from(NonZeroScalar::new(scalar))
            .map(|scalar: NonZeroScalar| Self(SigningKey::from(scalar)))
            .ok_or(Error::InvalidScalar)
    }

    pub fn address(&self) -> [u8; 20] {
        address_of(self.0.verifying_key())
    }

    pub fn sign(&self, digest: &[u8; 32]) -> [u8; 65] {
        sign(&self.0, digest)
    }
}

pub(crate) fn sign(key: &SigningKey, digest: &[u8; 32]) -> [u8; 65] {
    let (signature, recovery) = key
        .sign_prehash_recoverable(digest)
        .expect("a 32-byte prehash signs");
    let mut bytes = [0; 65];
    bytes[..64].copy_from_slice(&signature.to_bytes());
    bytes[64] = 27 + recovery.to_byte();
    bytes
}

pub fn signer(digest: &[u8; 32], signature: &[u8; 65]) -> Option<[u8; 20]> {
    if !matches!(signature[64], 27 | 28) {
        return None;
    }
    let parsed = Signature::from_slice(&signature[..64]).ok()?;
    if parsed.normalize_s().is_some() {
        return None;
    }
    let recovery = RecoveryId::from_byte(signature[64].checked_sub(27)?)?;
    VerifyingKey::recover_from_prehash(digest, &parsed, recovery)
        .ok()
        .map(|key| address_of(&key))
}

pub(crate) fn address_of(key: &VerifyingKey) -> [u8; 20] {
    let point = key.to_encoded_point(false);
    Keccak256::digest(&point.as_bytes()[1..])[12..]
        .try_into()
        .expect("20 bytes")
}

pub(crate) fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

pub(crate) fn uint(value: u128) -> [u8; 32] {
    let mut word = [0; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

pub(crate) fn address(value: &[u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(value);
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> AuthKey {
        let mut scalar = [0; 32];
        scalar[31] = byte;
        AuthKey(SigningKey::from_bytes(&scalar.into()).unwrap())
    }

    #[test]
    fn address_is_ethereums() {
        assert_eq!(
            key(1).address(),
            *b"\x7e\x5f\x45\x52\x09\x1a\x69\x12\x5d\x5d\xfc\xb7\xb8\xc2\x65\x90\x29\x39\x5b\xdf"
        );
    }

    #[test]
    fn deposit_hashes_match_the_contracts_known_answers() {
        fn word(text: &str) -> [u8; 32] {
            let mut word = [0; 32];
            for (i, byte) in word.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&text[2 + 2 * i..4 + 2 * i], 16).unwrap();
            }
            word
        }
        let deposits = DepositsDomain {
            chain_id: 11155111,
            contract: [0x11; 20],
        };
        let note = [[0x22; 32], [0x33; 32], [0x44; 32], [0x55; 32], [0x66; 32]];
        let nonce = deposits.deposit_nonce(&note, 150_000, &[0x77; 20]);
        assert_eq!(
            nonce,
            word("0x6917e0f85282cee681821f5eaf494c05f60a95699f31bf72e99f45365d6f8544")
        );
        let token = TokenDomain {
            name: "ZecSwap Test USD".into(),
            version: "2".into(),
            chain_id: 11155111,
            token: [0x88; 20],
        };
        assert_eq!(
            token.separator(),
            word("0x71fb9e856a21ce6c4c42af22617f8c5f42bc79024fa4c2acf2f845f51140175b")
        );
        let depositor = key(1).address();
        let authorization = ReceiveAuthorization {
            from: depositor,
            to: deposits.contract,
            value: 25_000_000,
            valid_after: 0,
            valid_before: 1_790_000_000,
            nonce,
        };
        assert_eq!(
            token.receive_with_authorization(&authorization),
            word("0x7b8010094423feaf967f85141de01415ea756128fbbcc9b5f548d4afa05404f9")
        );
        let rescue = RescueAuthorization {
            nonce: 0,
            deadline: 1_790_000_000,
        };
        let committed = word("0x4df47be579bd966f5104be761d3cc5704c776403467845b092d22901e91d24b0");
        assert_eq!(
            deposits.rescue(&depositor, &committed, &[0x77; 20], 150_000, rescue),
            word("0xa417fb0cbab5c5f12b9ec4c5d409c4148463a48675c333823dc3ce034837fc2c")
        );
    }
}
