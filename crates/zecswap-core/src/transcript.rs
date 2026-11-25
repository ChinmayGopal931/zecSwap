use rand::CryptoRng;

use crate::{Error, PublicShare, SecretShare, ShareProof};

const MAKER_TAG: &[u8; 16] = b"ZecSwap/v1/maker";
const USER_TAG: &[u8; 16] = b"ZecSwap/v2/user_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapContext {
    pub chain_id: u64,
    pub contract: [u8; 20],
    pub quote_id: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Payout {
    pub user: [u8; 20],
    pub note: Option<[u8; 32]>,
}

impl SwapContext {
    pub fn prove_maker<R: CryptoRng>(&self, e: &SecretShare, rng: R) -> ShareProof {
        e.prove(&self.maker_message(&e.public()), rng)
    }

    pub fn verify_maker(&self, maker: &PublicShare, proof: &ShareProof) -> Result<(), Error> {
        maker.verify(&self.maker_message(maker), proof)
    }

    pub fn prove_user<R: CryptoRng>(
        &self,
        maker: &PublicShare,
        z: &SecretShare,
        payout: &Payout,
        rng: R,
    ) -> ShareProof {
        z.prove(&self.user_message(maker, &z.public(), payout), rng)
    }

    pub fn verify_user(
        &self,
        maker: &PublicShare,
        user: &PublicShare,
        payout: &Payout,
        proof: &ShareProof,
    ) -> Result<(), Error> {
        user.verify(&self.user_message(maker, user, payout), proof)
    }

    fn maker_message(&self, maker: &PublicShare) -> Vec<u8> {
        let mut message = self.header(MAKER_TAG);
        message.extend_from_slice(&maker.to_affine_bytes());
        message
    }

    fn user_message(&self, maker: &PublicShare, user: &PublicShare, payout: &Payout) -> Vec<u8> {
        let mut message = self.header(USER_TAG);
        message.extend_from_slice(&maker.to_affine_bytes());
        message.extend_from_slice(&user.to_affine_bytes());
        message.extend_from_slice(&payout.user);
        message.extend_from_slice(&payout.note.unwrap_or_default());
        message
    }

    fn header(&self, tag: &[u8; 16]) -> Vec<u8> {
        let mut message = Vec::with_capacity(16 + 8 + 20 + 32 + 64 + 64 + 20 + 32);
        message.extend_from_slice(tag);
        message.extend_from_slice(&self.chain_id.to_be_bytes());
        message.extend_from_slice(&self.contract);
        message.extend_from_slice(&self.quote_id);
        message
    }
}
