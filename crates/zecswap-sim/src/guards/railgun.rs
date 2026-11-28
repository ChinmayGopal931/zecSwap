use std::collections::BTreeMap;

use aes::Aes256;
use aes_gcm::AesGcm;
use aes_gcm::aead::consts::U16;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use alloy::primitives::aliases::{U72, U120};
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolValue;
use ark_bn254::Fr;
use ark_ff::{BigInteger, PrimeField};
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use light_poseidon::{Poseidon, PoseidonHasher};
use sha2::{Digest, Sha256};
use zecswap_chain::evm::railgun::{
    BoundParams, CommitmentCiphertext, G1Point, G2Point, IRelayAdapt, Preimage, SnarkProof,
    TokenData, Transaction,
};
use zecswap_railgun::Receiver;

use crate::seed::Seed;

type Gcm = AesGcm<Aes256, U16>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coin {
    pub nullifier: B256,
    pub token: Address,
    pub value: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reads {
    Clean,
    Garbled,
    Claims(u128),
}

#[derive(Clone, Copy, Debug)]
pub struct Out {
    pub to: Receiver,
    pub token: Address,
    pub value: u128,
    pub reads: Reads,
}

#[derive(Clone, Copy, Debug)]
pub struct Exit {
    pub to: B256,
    pub token: Address,
    pub value: u128,
    pub kind: u8,
}

#[derive(Clone, Debug)]
pub struct Draft {
    pub spends: Vec<Coin>,
    pub outs: Vec<Out>,
    pub exit: Option<Exit>,
    pub chain_id: u64,
    pub adapt: Address,
    pub min_gas_price: u128,
    pub root: B256,
    pub annotation: usize,
    pub memo: usize,
}

pub struct Circuit {
    key: PrivateKeySigner,
    coins: BTreeMap<B256, Coin>,
}

fn poseidon(inputs: &[Fr]) -> [u8; 32] {
    let hash = Poseidon::<Fr>::new_circom(inputs.len())
        .and_then(|mut hasher| hasher.hash(inputs))
        .expect("circom parameters exist for two and three inputs");
    hash.into_bigint()
        .to_bytes_be()
        .try_into()
        .expect("a BN254 scalar is 32 bytes")
}

fn field(bytes: &[u8]) -> Fr {
    Fr::from_be_bytes_mod_order(bytes)
}

pub fn erc20(token: Address) -> TokenData {
    TokenData {
        tokenType: 0,
        tokenAddress: token,
        tokenSubID: U256::ZERO,
    }
}

pub fn exit_note(exit: &Exit) -> Preimage {
    Preimage {
        npk: exit.to,
        token: erc20(exit.token),
        value: U120::from(exit.value),
    }
}

pub fn root() -> B256 {
    keccak256(b"zecswap-sim")
}

fn seal(
    seed: Seed,
    tag: &str,
    out: &Out,
    random: [u8; 16],
    memo: usize,
) -> ([B256; 4], B256, Vec<u8>) {
    let scalar = Scalar::from_bytes_mod_order(seed.bytes::<32>(&format!("{tag}-blind")));
    let blinded = EdwardsPoint::mul_base(&scalar).compress().to_bytes();
    let stated = match out.reads {
        Reads::Claims(value) => value,
        Reads::Clean | Reads::Garbled => out.value,
    };
    let mut plain = vec![0u8; 96 + memo];
    plain[..32].copy_from_slice(&out.to.master_public_key);
    plain[32..64].copy_from_slice(out.token.into_word().as_slice());
    plain[64..80].copy_from_slice(&random);
    plain[80..96].copy_from_slice(&stated.to_be_bytes());
    for (at, byte) in plain[96..].iter_mut().enumerate() {
        *byte = 0x40 + (at % 23) as u8;
    }
    let iv = seed.bytes::<16>(&format!("{tag}-iv"));
    let theirs = CompressedEdwardsY(out.to.viewing_public_key).decompress();
    let sealed = theirs.map(|theirs| {
        let shared: [u8; 32] = Sha256::digest((theirs * scalar).compress().as_bytes()).into();
        Gcm::new(GenericArray::from_slice(&shared))
            .encrypt_in_place_detached(GenericArray::from_slice(&iv), &[], &mut plain)
            .expect("a note is short enough to seal")
    });
    let mut words = [B256::ZERO; 4];
    match sealed.filter(|_| out.reads != Reads::Garbled) {
        Some(tag) => {
            words[0][..16].copy_from_slice(&iv);
            words[0][16..].copy_from_slice(&tag);
        }
        None => words[0] = B256::from(seed.bytes::<32>(&format!("{tag}-noise"))),
    }
    for (word, chunk) in words[1..].iter_mut().zip(plain[..96].chunks(32)) {
        word.copy_from_slice(chunk);
    }
    (words, B256::from(blinded), plain[96..].to_vec())
}

pub fn adapt_params(transactions: &[Transaction], action: &IRelayAdapt::ActionData) -> B256 {
    let nullifiers: Vec<Vec<B256>> = transactions
        .iter()
        .map(|transaction| transaction.nullifiers.clone())
        .collect();
    keccak256((nullifiers, U256::from(transactions.len()), action.clone()).abi_encode_params())
}

impl Circuit {
    pub fn new(seed: Seed) -> Self {
        let key = PrivateKeySigner::from_bytes(&B256::from(seed.bytes::<32>("railgun-circuit")))
            .expect("a signing key");
        Self {
            key,
            coins: BTreeMap::new(),
        }
    }

    pub fn verifier(&self) -> Address {
        self.key.address()
    }

    pub fn mint(&mut self, coin: Coin) {
        self.coins.insert(coin.nullifier, coin);
    }

    fn holds(&self, draft: &Draft) -> bool {
        let mut sums: BTreeMap<Address, (u128, u128)> = BTreeMap::new();
        for coin in &draft.spends {
            if self.coins.get(&coin.nullifier) != Some(coin) {
                return false;
            }
            sums.entry(coin.token).or_default().0 += coin.value;
        }
        for out in &draft.outs {
            sums.entry(out.token).or_default().1 += out.value;
        }
        if let Some(exit) = &draft.exit {
            sums.entry(exit.token).or_default().1 += exit.value;
        }
        !draft.spends.is_empty() && sums.values().all(|(spent, made)| spent == made)
    }

    pub fn build(&self, seed: Seed, tag: &str, draft: &Draft) -> Transaction {
        let mut commitments = Vec::new();
        let mut ciphertexts = Vec::new();
        for (at, out) in draft.outs.iter().enumerate() {
            let tag = format!("{tag}-out-{at}");
            let random = seed.bytes::<16>(&format!("{tag}-random"));
            let npk = poseidon(&[field(&out.to.master_public_key), field(&random)]);
            let token = out.token.into_word();
            commitments.push(B256::from(poseidon(&[
                field(&npk),
                field(token.as_slice()),
                Fr::from(out.value),
            ])));
            let (ciphertext, blinded, memo) = seal(seed, &tag, out, random, draft.memo);
            ciphertexts.push(CommitmentCiphertext {
                ciphertext,
                blindedSenderViewingKey: blinded,
                blindedReceiverViewingKey: B256::from(seed.bytes::<32>(&format!("{tag}-theirs"))),
                annotationData: vec![0x61; draft.annotation].into(),
                memo: memo.into(),
            });
        }
        let unshield = match &draft.exit {
            Some(exit) => {
                let note = exit_note(exit);
                commitments.push(keccak256(note.abi_encode()));
                note
            }
            None => Preimage {
                npk: B256::ZERO,
                token: erc20(Address::ZERO),
                value: U120::ZERO,
            },
        };
        let zero = G1Point {
            x: U256::ZERO,
            y: U256::ZERO,
        };
        Transaction {
            proof: SnarkProof {
                a: zero.clone(),
                b: G2Point {
                    x: [U256::ZERO; 2],
                    y: [U256::ZERO; 2],
                },
                c: zero,
            },
            merkleRoot: draft.root,
            nullifiers: draft.spends.iter().map(|coin| coin.nullifier).collect(),
            commitments,
            boundParams: BoundParams {
                treeNumber: 0,
                minGasPrice: U72::from(draft.min_gas_price),
                unshield: draft.exit.map_or(0, |exit| exit.kind),
                chainID: draft.chain_id,
                adaptContract: draft.adapt,
                adaptParams: B256::ZERO,
                commitmentCiphertext: ciphertexts,
            },
            unshieldPreimage: unshield,
        }
    }

    pub fn prove(&self, transaction: &mut Transaction, draft: &Draft) {
        if !self.holds(draft) {
            return;
        }
        let bound = keccak256(transaction.boundParams.abi_encode());
        let digest = keccak256(
            (
                transaction.merkleRoot,
                transaction.nullifiers.clone(),
                transaction.commitments.clone(),
                bound,
            )
                .abi_encode_params(),
        );
        let signature = self.key.sign_hash_sync(&digest).expect("a signature");
        transaction.proof.a.x = signature.r();
        transaction.proof.a.y = signature.s();
        transaction.proof.c.x = U256::from(27 + u8::from(signature.v()));
    }
}
