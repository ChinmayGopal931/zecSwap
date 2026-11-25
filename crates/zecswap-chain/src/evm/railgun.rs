use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::eips::{BlockId, Decodable2718};
use alloy::network::TransactionBuilder;
use alloy::primitives::aliases::U120;
use alloy::primitives::{Bytes, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::RpcError;
use zecswap_railgun::{Keys, OutputCiphertext, ShieldNote};

use super::canonical::{self, Malformed, Shape};
use super::{Address, B256, IErc20, Settlement, U256};
use crate::Error;

pub const MAX_CALLDATA_BYTES: usize = 64 * 1024;
pub const MAX_TRANSACTIONS: usize = 16;
const MAX_NULLIFIERS: usize = 13;
const MAX_COMMITMENTS: usize = 13;
const MAX_ANNOTATION_BYTES: usize = 62;
const MAX_MEMO_BYTES: usize = 1024;

const CIPHERTEXT: Shape = Shape::Tuple(&[
    Shape::Words(6),
    Shape::Bytes(MAX_ANNOTATION_BYTES),
    Shape::Bytes(MAX_MEMO_BYTES),
]);
pub(super) const TRANSACTION: Shape = Shape::Tuple(&[
    Shape::Words(9),
    Shape::Array(&Shape::Words(1), MAX_NULLIFIERS),
    Shape::Capping(&Shape::Words(1), MAX_COMMITMENTS),
    Shape::Tuple(&[Shape::Words(6), Shape::Capped(&CIPHERTEXT)]),
    Shape::Words(5),
]);
const TRANSACT: &[Shape] = &[Shape::Array(&TRANSACTION, MAX_TRANSACTIONS)];

sol! {
    struct G1Point { uint256 x; uint256 y; }
    struct G2Point { uint256[2] x; uint256[2] y; }
    struct SnarkProof { G1Point a; G2Point b; G1Point c; }
    struct CommitmentCiphertext {
        bytes32[4] ciphertext;
        bytes32 blindedSenderViewingKey;
        bytes32 blindedReceiverViewingKey;
        bytes annotationData;
        bytes memo;
    }
    struct BoundParams {
        uint16 treeNumber;
        uint72 minGasPrice;
        uint8 unshield;
        uint64 chainID;
        address adaptContract;
        bytes32 adaptParams;
        CommitmentCiphertext[] commitmentCiphertext;
    }
    struct TokenData { uint8 tokenType; address tokenAddress; uint256 tokenSubID; }
    struct Preimage { bytes32 npk; TokenData token; uint120 value; }
    struct Ciphertext { bytes32[3] encryptedBundle; bytes32 shieldKey; }
    struct ShieldRequest { Preimage preimage; Ciphertext ciphertext; }
    struct Transaction {
        SnarkProof proof;
        bytes32 merkleRoot;
        bytes32[] nullifiers;
        bytes32[] commitments;
        BoundParams boundParams;
        Preimage unshieldPreimage;
    }

    #[sol(rpc)]
    interface IRailgunSmartWallet {
        function transact(Transaction[] _transactions) external payable;
        function shield(ShieldRequest[] _shieldRequests) external payable;
        function nullifiers(uint256 treeNumber, bytes32 nullifier) external view returns (bool);
    }

    #[sol(rpc)]
    interface IRelayAdapt {
        struct Call { address to; bytes data; uint256 value; }
        struct ActionData { bytes31 random; bool requireSuccess; uint256 minGasLimit; Call[] calls; }
        function relay(Transaction[] _transactions, ActionData _actionData) external payable;
        function shield(ShieldRequest[] _shieldRequests) external;
        function railgun() external view returns (address);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendPolicy {
    pub railgun: Address,
    pub token: Address,
    pub fee: u128,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u128,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("{0}")]
    Rejected(&'static str),
    #[error("these notes are already spent")]
    Spent,
    #[error("{}", .0.reason())]
    Later(Later),
    #[error(transparent)]
    Chain(#[from] Error),
    #[error("the broadcast of {0} failed; it may still land")]
    Unknown(B256),
    #[error("the private RPC refused {0}")]
    Refused(B256),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Later {
    GasPrice,
    Eth,
}

impl Later {
    pub fn reason(self) -> &'static str {
        match self {
            Later::GasPrice => "the network's gas price is above the relayer's cap",
            Later::Eth => "the relayer is low on ETH",
        }
    }
}

type Result<T, E = SendError> = std::result::Result<T, E>;

pub struct Transact {
    chain_id: u64,
    railgun: Address,
    data: Bytes,
    transactions: Vec<Transaction>,
    min_gas_price: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub hash: B256,
    pub nonce: u64,
    pub raw: Bytes,
}

impl Signed {
    pub fn call(&self) -> Option<(Address, u128, Bytes)> {
        let envelope = TxEnvelope::decode_2718(&mut self.raw.as_ref()).ok()?;
        Some((
            envelope.to()?,
            envelope.value().try_into().ok()?,
            envelope.input().clone(),
        ))
    }

    pub fn signer(&self) -> Option<Address> {
        let envelope = TxEnvelope::decode_2718(&mut self.raw.as_ref()).ok()?;
        envelope
            .signature()
            .recover_address_from_prehash(&envelope.signature_hash())
            .ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Mined { succeeded: bool },
    Unknown,
}

impl SendPolicy {
    pub fn decode(&self, chain_id: u64, to: Address, value: u128, data: Bytes) -> Result<Transact> {
        require(
            to == self.railgun && value == 0,
            "wrong destination or ETH value",
        )?;
        require(
            data.len() <= MAX_CALLDATA_BYTES,
            "the calldata is too large",
        )?;
        let call = canonical::decode::<IRailgunSmartWallet::transactCall>(&data, TRANSACT)
            .map_err(|malformed| {
                SendError::Rejected(match malformed {
                    Malformed::NotTheCall => "not a Railgun transact call",
                    Malformed::Noncanonical => "noncanonical transact calldata",
                    Malformed::TooLong => "the calldata holds more than the relayer takes",
                })
            })?;
        let transactions = call._transactions;
        require(
            !transactions.is_empty(),
            "expected at least one Railgun transaction",
        )?;
        let mut min_gas_price = 0;
        for tx in &transactions {
            let bound = &tx.boundParams;
            require(
                !tx.nullifiers.is_empty() && !tx.commitments.is_empty(),
                "each Railgun transaction must spend notes",
            )?;
            require(bound.chainID == chain_id, "a proof is for another chain")?;
            require(
                bound.adaptContract.is_zero() && bound.adaptParams.is_zero(),
                "a proof is bound to an adapt contract",
            )?;
            require(
                bound.unshield <= 1,
                "redirected unshields are not supported",
            )?;
            require(
                bound.unshield == 0 || erc20(&tx.unshieldPreimage.token, self.token),
                "a withdrawal must pay out the relayer's token",
            )?;
            min_gas_price = min_gas_price.max(bound.minGasPrice.to::<u128>());
        }
        Ok(Transact {
            chain_id,
            railgun: self.railgun,
            data,
            transactions,
            min_gas_price,
        })
    }

    pub fn check(&self, transact: &Transact, keys: &Keys) -> Result<u128> {
        require(
            transact.min_gas_price <= self.max_gas_price_wei,
            "a proof's minimum gas price is above the relayer's cap",
        )?;
        let paid = transact.fee_paid(keys, self.token);
        require(
            paid >= self.fee,
            "the transaction does not pay the relayer's fee",
        )?;
        Ok(paid)
    }
}

impl Transact {
    pub fn id(&self) -> B256 {
        keccak256(&self.data)
    }

    pub fn nullifiers(&self) -> Vec<(u16, B256)> {
        self.transactions
            .iter()
            .flat_map(|tx| {
                let tree = tx.boundParams.treeNumber;
                tx.nullifiers
                    .iter()
                    .map(move |nullifier| (tree, *nullifier))
            })
            .collect()
    }

    pub fn unshielded(&self) -> Option<u128> {
        self.transactions
            .iter()
            .filter(|tx| tx.boundParams.unshield == 1)
            .map(|tx| tx.unshieldPreimage.value.to::<u128>())
            .reduce(u128::saturating_add)
    }

    pub fn fee_paid(&self, keys: &Keys, token: Address) -> u128 {
        self.transactions
            .iter()
            .filter_map(|tx| {
                let output = tx.boundParams.commitmentCiphertext.first()?;
                let received = keys.receive(
                    &tx.commitments.first()?.0,
                    &OutputCiphertext {
                        ciphertext: output.ciphertext.map(|word| word.0),
                        blinded_sender_viewing_key: output.blindedSenderViewingKey.0,
                        memo: output.memo.to_vec(),
                    },
                )?;
                (received.token == token.into_word().0).then_some(received.value)
            })
            .fold(0, u128::saturating_add)
    }
}

impl Settlement {
    pub async fn spends_spent_notes(&self, transact: &Transact) -> Result<bool, Error> {
        let unavailable = || Error::Contract("Railgun's nullifiers are unavailable".into());
        let spent = |tree: u16, nullifier| IRailgunSmartWallet::nullifiersCall {
            treeNumber: U256::from(tree),
            nullifier,
        };
        if !self.multicall_deployed().await.map_err(|_| unavailable())? {
            let railgun = IRailgunSmartWallet::new(transact.railgun, &self.provider);
            for (tree, nullifier) in transact.nullifiers() {
                let read = railgun.call_builder(&spent(tree, nullifier)).call().await;
                if read.map_err(|_| unavailable())? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        let calls = transact
            .nullifiers()
            .into_iter()
            .map(|(tree, nullifier)| (transact.railgun, spent(tree, nullifier).abi_encode()))
            .collect();
        let answers = self
            .aggregate_by(calls, BlockId::latest(), MAX_TRANSACTIONS * MAX_NULLIFIERS)
            .await
            .map_err(|_| unavailable())?;
        for answer in answers {
            let read = IRailgunSmartWallet::nullifiersCall::abi_decode_returns(&answer);
            if read.map_err(|_| unavailable())? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn send_transact(
        &self,
        policy: &SendPolicy,
        transact: &Transact,
        covers: impl FnOnce(u64, u128) -> bool,
        record: impl FnOnce(&Signed) -> Result<(), Error>,
    ) -> Result<B256> {
        require(
            transact.railgun == policy.railgun,
            "the relayer's Railgun deployment changed; post again",
        )?;
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()).into());
        };
        let gas_price = self
            .provider
            .get_gas_price()
            .await
            .map_err(|_| Error::Contract("the gas price is unavailable".into()))?
            .max(transact.min_gas_price);
        require(
            gas_price <= policy.max_gas_price_wei,
            "the network's gas price is above the relayer's cap",
        )?;
        let balance = self
            .provider
            .get_balance(account)
            .await
            .map_err(|_| Error::Contract("the relayer's balance is unavailable".into()))?;
        let afford = |gas: u64| balance >= U256::from(gas) * U256::from(gas_price);
        let mut tx = TransactionRequest::default()
            .with_from(account)
            .with_to(policy.railgun)
            .with_value(U256::ZERO)
            .with_input(transact.data.clone())
            .with_chain_id(transact.chain_id)
            .with_gas_limit(policy.max_gas_limit)
            .with_gas_price(gas_price);
        let estimate = match self
            .provider
            .estimate_gas(tx.clone())
            .block(BlockId::pending())
            .await
        {
            Ok(gas) => gas,
            Err(RpcError::ErrorResp(_)) => {
                if !afford(policy.max_gas_limit) {
                    return Err(SendError::Rejected("the relayer is low on ETH"));
                }
                if self.spends_spent_notes(transact).await? {
                    return Err(SendError::Spent);
                }
                return Err(SendError::Rejected("the transaction fails in simulation"));
            }
            Err(_) => return Err(Error::Contract("the simulation is unavailable".into()).into()),
        };
        require(
            estimate <= policy.max_gas_limit,
            "the transaction needs more gas than the relayer's cap",
        )?;
        require(
            covers(estimate, gas_price),
            "the transaction does not pay the relayer's fee",
        )?;
        let gas_limit = estimate
            .saturating_add(estimate / 5)
            .min(policy.max_gas_limit);
        require(afford(gas_limit), "the relayer is low on ETH")?;
        let _sending = self.sending.turn().await;
        tx.set_gas_limit(gas_limit);
        let signed = self.sign_next(tx, 0).await?;
        record(&signed)?;
        match self.provider.send_raw_transaction(&signed.raw).await {
            Ok(_) => {
                tracing::info!(transaction_hash = %signed.hash, "sent a Railgun transaction as its broadcaster");
                Ok(signed.hash)
            }
            Err(_) => Err(SendError::Unknown(signed.hash)),
        }
    }

    pub async fn shield(
        &self,
        token: Address,
        value: u128,
        note: &ShieldNote,
    ) -> Result<B256, Error> {
        let railgun = self.railgun().await?;
        self.submit(IErc20::new(token, &self.provider).approve(railgun, U256::from(value)))
            .await?;
        let request = ShieldRequest {
            preimage: Preimage {
                npk: note.npk.into(),
                token: TokenData {
                    tokenType: 0,
                    tokenAddress: token,
                    tokenSubID: U256::ZERO,
                },
                value: U120::from(value),
            },
            ciphertext: Ciphertext {
                encryptedBundle: note.ciphertext.encrypted_bundle.map(B256::from),
                shieldKey: note.ciphertext.shield_key.into(),
            },
        };
        let receipt = self
            .submit(IRailgunSmartWallet::new(railgun, &self.provider).shield(vec![request]))
            .await?;
        Ok(receipt.transaction_hash)
    }

    pub async fn transaction_status(&self, hash: B256) -> Result<Status, Error> {
        let unavailable = |_| Error::Contract("the transaction's status is unavailable".into());
        if let Some(receipt) = self
            .provider
            .get_transaction_receipt(hash)
            .await
            .map_err(unavailable)?
        {
            return Ok(Status::Mined {
                succeeded: receipt.status(),
            });
        }
        Ok(
            match self
                .provider
                .get_transaction_by_hash(hash)
                .await
                .map_err(unavailable)?
            {
                Some(_) => Status::Pending,
                None => Status::Unknown,
            },
        )
    }

    pub async fn nonce_taken(&self, nonce: u64, depth: u64) -> Result<bool, Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection has no nonce".into()))?;
        let unavailable = |_| Error::Contract("the relayer's nonce is unavailable".into());
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(unavailable)?;
        let Some(block) = latest.checked_sub(depth) else {
            return Ok(false);
        };
        let count = self
            .provider
            .get_transaction_count(account)
            .block_id(block.into())
            .await
            .map_err(unavailable)?;
        Ok(count > nonce)
    }

    pub async fn nonces(&self) -> Result<(u64, u64), Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection has no nonce".into()))?;
        let unavailable = |_| Error::Contract("the account's nonce is unavailable".into());
        let count = || self.provider.get_transaction_count(account);
        let mined = count().latest().await.map_err(unavailable)?;
        let sent = count().pending().await.map_err(unavailable)?;
        Ok((mined, sent))
    }

    pub async fn rebroadcast(&self, signed: &Signed) -> Result<bool, Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection cannot send".into()))?;
        let _sending = self.sending.turn().await;
        let next = self
            .provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(|_| Error::Contract("the relayer's nonce is unavailable".into()))?;
        if next != signed.nonce {
            return Ok(false);
        }
        let _pending = self
            .broadcaster()
            .send_raw_transaction(&signed.raw)
            .await
            .map_err(|_| Error::Contract("the broadcast failed again".into()))?;
        tracing::info!(transaction_hash = %signed.hash, "broadcast a Railgun transaction again");
        Ok(true)
    }
}

pub(super) fn erc20(token: &TokenData, address: Address) -> bool {
    token.tokenType == 0 && token.tokenAddress == address && token.tokenSubID.is_zero()
}

pub(super) fn require(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(SendError::Rejected(reason))
    }
}
