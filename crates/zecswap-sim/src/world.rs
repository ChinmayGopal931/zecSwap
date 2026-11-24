use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use alloy::primitives::{Address, B256, Bytes, U256, address};
use alloy::providers::MULTICALL3_ADDRESS;
use alloy::providers::bindings::IMulticall3;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use zecswap_chain::evm::{simulate_wall, wall_now};
use zecswap_core::{PublicShare, SecretShare, Terms};

use crate::node::Node;
use crate::seed::Seed;

pub const EPOCH: u64 = 1_800_000_000;
pub const CHAIN_ID: u64 = 11_155_111;
pub const RAILGUN: Address = address!("eCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea");
pub const MIN_WINDOW: u64 = 30 * 60;
pub const LATE_WINDOW: u64 = 60 * 60;
pub const UNIT: u128 = 5_000_000;
pub const UNIT_ZAT: u64 = 10_000_000;
const ETHER: u128 = 1_000_000_000_000_000_000;

sol! {
    struct TermsWords {
        address maker;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        address user;
        uint64 t0;
        uint64 t1;
        bytes32 payoutNote;
    }

    struct SwapWords {
        bytes32 termsHash;
        uint8 stage;
        bool paidOut;
        uint256 secret;
    }

    struct ShieldedWords {
        address from;
        bytes32 npk;
        address token;
        uint120 value;
        bytes32 ciphertextHash;
    }

    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function deposit(address token, uint256 amount) external;
    function balanceOf(address owner) external view returns (uint256);
    function claim(bytes32 id, TermsWords terms, uint256 userSecret) external;
    function refund(bytes32 id, TermsWords terms, uint256 makerSecret) external;
    function getSwap(bytes32 id) external view returns (SwapWords);
    function count() external view returns (uint256);
    function shielded(uint256 index) external view returns (ShieldedWords);
}

pub fn wall() -> u64 {
    wall_now()
}

pub fn key(seed: Seed, who: &str) -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::from(seed.bytes::<32>(who))).expect("a signing key")
}

fn artifact(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../../contracts/out/{name}.sol/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}; run `forge build` in contracts/ first"));
    let artifact: serde_json::Value = serde_json::from_str(&text).expect("a build artifact");
    let code: Bytes = serde_json::from_value(artifact["bytecode"]["object"].clone())
        .expect("the artifact's creation code");
    code.to_vec()
}

fn share_words(share: &PublicShare) -> [U256; 2] {
    let bytes = share.to_affine_bytes();
    [
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..]),
    ]
}

pub fn terms_words(terms: &Terms) -> TermsWords {
    TermsWords {
        maker: terms.maker.into(),
        token: terms.token.into(),
        amount: terms.amount,
        makerKey: share_words(&terms.maker_share),
        userKey: share_words(&terms.user_share),
        user: terms.user.into(),
        t0: terms.t0,
        t1: terms.t1,
        payoutNote: terms.payout_note.into(),
    }
}

#[derive(Clone)]
pub struct Chain {
    pub node: Node,
    pub contract: Address,
    pub token: Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub stage: u8,
    pub paid_out: bool,
    pub secret: U256,
    pub terms_hash: B256,
}

impl Chain {
    pub fn deploy(node: &Node, multicall: bool) -> Self {
        let deployer = address!("d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0");
        let create = |code: Vec<u8>| {
            node.transact(deployer, None, code.into())
                .1
                .expect("a deployed contract")
        };
        node.place(create(artifact("MockRailgun")), RAILGUN);
        let token = create(artifact("TestToken"));
        let mut code = artifact("ZecSwap");
        code.extend_from_slice(
            &(U256::from(MIN_WINDOW), U256::from(LATE_WINDOW), RAILGUN).abi_encode(),
        );
        let contract = create(code);
        if multicall {
            node.place(create(IMulticall3::BYTECODE.to_vec()), MULTICALL3_ADDRESS);
        }
        Self {
            node: node.clone(),
            contract,
            token,
        }
    }

    pub fn fund(&self, who: Address) {
        self.node.fund(who, U256::from(10 * ETHER));
    }

    pub fn stock(&self, maker: Address, amount: u128) {
        let amount = U256::from(amount);
        let call = |from, to, data: Vec<u8>| self.node.transact(from, Some(to), data.into());
        call(
            maker,
            self.token,
            mintCall { to: maker, amount }.abi_encode(),
        );
        let approval = approveCall {
            spender: self.contract,
            amount,
        };
        call(maker, self.token, approval.abi_encode());
        let inventory = depositCall {
            token: self.token,
            amount,
        };
        call(maker, self.contract, inventory.abi_encode());
    }

    pub fn recorded(&self, id: B256) -> Recorded {
        self.recorded_at(id, self.node.head().0)
    }

    pub fn recorded_at(&self, id: B256, block: u64) -> Recorded {
        let out = self
            .node
            .view_at(block, self.contract, getSwapCall { id }.abi_encode().into())
            .expect("the contract answers getSwap");
        let swap = getSwapCall::abi_decode_returns(&out).expect("a swap record");
        Recorded {
            stage: swap.stage,
            paid_out: swap.paidOut,
            secret: swap.secret,
            terms_hash: swap.termsHash,
        }
    }

    pub fn would_claim(&self, id: B256, terms: &Terms, z: &SecretShare) -> bool {
        let call = claimCall {
            id,
            terms: terms_words(terms),
            userSecret: U256::from_be_bytes(z.to_be_bytes()),
        };
        self.node
            .view(self.contract, call.abi_encode().into())
            .is_ok()
    }

    pub fn would_refund(&self, id: B256, terms: &Terms, e: &SecretShare) -> bool {
        self.would_refund_at(id, terms, e, self.node.head().0)
    }

    pub fn would_refund_at(&self, id: B256, terms: &Terms, e: &SecretShare, block: u64) -> bool {
        let call = refundCall {
            id,
            terms: terms_words(terms),
            makerSecret: U256::from_be_bytes(e.to_be_bytes()),
        };
        self.node
            .view_at(block, self.contract, call.abi_encode().into())
            .is_ok()
    }

    pub fn token_balance(&self, owner: Address) -> u128 {
        let out = self
            .node
            .view(self.token, balanceOfCall { owner }.abi_encode().into())
            .expect("the token answers balanceOf");
        balanceOfCall::abi_decode_returns(&out)
            .expect("a balance")
            .to()
    }

    pub fn shielded(&self) -> Vec<(B256, u128)> {
        let read = |data: Vec<u8>| {
            self.node
                .view(RAILGUN, data.into())
                .expect("Railgun answers")
        };
        let count: u64 = countCall::abi_decode_returns(&read(countCall {}.abi_encode()))
            .expect("a count")
            .to();
        (0..count)
            .map(|index| {
                let call = shieldedCall {
                    index: U256::from(index),
                };
                let note = shieldedCall::abi_decode_returns(&read(call.abi_encode()))
                    .expect("a shielded note");
                (note.npk, note.value.to())
            })
            .collect()
    }
}

pub struct Skewed<T> {
    skew: i64,
    inner: Pin<Box<dyn Future<Output = T>>>,
}

impl<T> Skewed<T> {
    pub fn new(skew: i64, inner: impl Future<Output = T> + 'static) -> Self {
        Self {
            skew,
            inner: Box::pin(inner),
        }
    }
}

impl<T> Future for Skewed<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let before = simulate_wall(Some(EPOCH as i64 + self.skew));
        let out = self.inner.as_mut().poll(cx);
        simulate_wall(before);
        out
    }
}
