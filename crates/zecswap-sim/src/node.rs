use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alloy::consensus::transaction::{Recovered, SignerRecoverable};
use alloy::consensus::{Transaction as _, TxEnvelope};
use alloy::eips::{BlockId, BlockNumberOrTag, Decodable2718, Typed2718};
use alloy::primitives::{Address, B256, Bloom, Bytes, Log, TxKind, U256, keccak256};
use alloy::rpc::types::{Filter, Transaction as RpcTransaction, TransactionRequest};
use revm::context::result::{ExecutionResult, Output};
use revm::context::{BlockEnv, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::state::AccountInfo;
use revm::{DatabaseCommit, DatabaseRef};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use zecswap_sim_evm::{State, run};

use crate::seed::Seed;

pub const SLOT: u64 = 12;
const GAS_LIMIT: u64 = 60_000_000;
const GWEI: u64 = 1_000_000_000;
const TIP: u64 = GWEI / 10;
pub(crate) const PROTECT: u64 = 25;

#[derive(Clone, Copy, Debug)]
pub struct Weather {
    pub reorg: u64,
    pub reorg_after_tx: u64,
    pub ambush: u64,
    pub reorg_depth: u64,
    pub fork_delay: u64,
    pub missed_slot: u64,
    pub delayed: u64,
    pub delay_blocks: u64,
    pub lost: u64,
    pub private_lost: u64,
    pub fee_swing: u64,
    pub rpc_error: u64,
    pub rpc_stall: u64,
    pub stale_read: u64,
}

impl Weather {
    pub const CALM: Self = Self {
        reorg: 0,
        reorg_after_tx: 0,
        ambush: 0,
        reorg_depth: 0,
        fork_delay: 0,
        missed_slot: 0,
        delayed: 0,
        delay_blocks: 0,
        lost: 0,
        private_lost: 0,
        fee_swing: 0,
        rpc_error: 0,
        rpc_stall: 0,
        stale_read: 0,
    };
}

#[derive(Clone, Debug)]
struct Tx {
    hash: B256,
    from: Address,
    envelope: TxEnvelope,
    private: bool,
    sent: u64,
}

#[derive(Clone, Debug)]
struct Mined {
    tx: Tx,
    succeeded: bool,
    gas_used: u64,
    logs: Vec<Log>,
    created: Option<Address>,
}

#[derive(Clone)]
struct Block {
    number: u64,
    hash: B256,
    parent: B256,
    timestamp: u64,
    base_fee: u64,
    mined: Vec<Mined>,
    state: State,
}

#[derive(Clone, Debug)]
struct Pooled {
    tx: Tx,
    arrived: u64,
    expires: Option<u64>,
    replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    pub hash: B256,
    pub from: Address,
    pub to: Option<Address>,
    pub input: Bytes,
    pub at: u64,
    pub landed: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct Logged {
    pub block: u64,
    pub tx: B256,
    pub log: Log,
}

#[derive(Clone, Debug)]
pub(crate) struct Known {
    pub from: Address,
    pub nonce: u64,
    pub private: bool,
    pub sent: u64,
    pub mined: Option<u64>,
    pub pooled: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Waiting {
    pub to: Option<Address>,
    pub input: Bytes,
    pub value: U256,
    pub gas: u64,
    pub max_fee: u128,
    pub tip: u128,
}

#[derive(Clone, Debug)]
pub(crate) struct Queue {
    pub from: Address,
    pub behind: u64,
    pub repriced: u64,
}

#[derive(Debug)]
pub struct Refusal {
    pub code: i64,
    pub message: String,
    pub data: Option<Bytes>,
}

impl Refusal {
    fn new(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
            data: None,
        }
    }

    fn params(why: impl std::fmt::Display) -> Self {
        Self {
            code: -32602,
            message: format!("invalid params: {why}"),
            data: None,
        }
    }
}

struct Inner {
    seed: Seed,
    chain_id: u64,
    weather: Weather,
    blocks: Vec<Block>,
    pool: Vec<Pooled>,
    seen: BTreeMap<B256, Tx>,
    published: Vec<Published>,
    rises: BTreeMap<(Address, u64), Vec<u64>>,
    fees: Vec<u64>,
    forks: u64,
    floor: u64,
    clears: u64,
    shaky: bool,
    ambushed: bool,
    eases: bool,
    trace: Vec<String>,
}

#[derive(Clone)]
pub struct Node(Arc<Mutex<Inner>>);

fn env(number: u64, timestamp: u64, base_fee: u64) -> BlockEnv {
    BlockEnv {
        number: U256::from(number),
        timestamp: U256::from(timestamp),
        basefee: base_fee,
        gas_limit: GAS_LIMIT,
        prevrandao: Some(B256::with_last_byte(1)),
        ..Default::default()
    }
}

fn quantity(value: impl std::fmt::LowerHex) -> Value {
    Value::String(format!("{value:#x}"))
}

pub(crate) fn raw(value: &impl serde::Serialize) -> Box<RawValue> {
    serde_json::value::to_raw_value(value).expect("a JSON value")
}

impl Inner {
    fn head(&self) -> &Block {
        self.blocks.last().expect("genesis")
    }

    fn note(&mut self, what: String) {
        let at = self.head().number;
        self.trace.push(format!("#{at} {what}"));
    }

    fn nonce(state: &State, who: Address) -> u64 {
        state
            .basic_ref(who)
            .ok()
            .flatten()
            .map_or(0, |account| account.nonce)
    }

    fn tx_env(&self, tx: &Tx) -> TxEnv {
        let envelope = &tx.envelope;
        TxEnv {
            tx_type: envelope.ty(),
            caller: tx.from,
            gas_limit: envelope.gas_limit(),
            gas_price: envelope.max_fee_per_gas(),
            gas_priority_fee: envelope.max_priority_fee_per_gas(),
            kind: envelope.kind(),
            value: envelope.value(),
            data: envelope.input().clone(),
            nonce: envelope.nonce(),
            chain_id: envelope.chain_id(),
            ..Default::default()
        }
    }

    fn next_fee(&self, number: u64) -> u64 {
        if let Some(fee) = self.fees.get(number as usize) {
            return *fee;
        }
        let last = *self.fees.last().expect("genesis fee");
        if self.weather.fee_swing == 0 {
            let eased = (last - last / 8).max(2 * GWEI);
            return match self.eases && last > eased {
                true => eased,
                false => last,
            };
        }
        let era = number / 8;
        let rising = self.seed.chance("fee-era", era, self.weather.fee_swing);
        let falling = !rising && self.seed.chance("fee-calm", era, 500);
        let next = if rising {
            last + last / 8
        } else if falling {
            last - last / 8
        } else {
            last
        };
        next.clamp(GWEI / 10, 400 * GWEI)
    }

    fn delay(&self, pooled: &Pooled) -> u64 {
        let tx = &pooled.tx;
        let key = u64::from_be_bytes(tx.hash[..8].try_into().unwrap()) ^ self.forks;
        let odds = match pooled.replayed {
            true => self.weather.fork_delay.max(self.weather.delayed),
            false => self.weather.delayed,
        };
        if self.seed.chance("tx-delayed", key, odds) {
            self.seed
                .between("tx-delay", key, 1, self.weather.delay_blocks.max(1))
        } else {
            0
        }
    }

    fn lost(&self, tx: &Tx) -> bool {
        let key = u64::from_be_bytes(tx.hash[..8].try_into().unwrap());
        let odds = if tx.private {
            self.weather.private_lost
        } else {
            self.weather.lost
        };
        self.seed.chance("tx-lost", key, odds)
    }

    fn build(&mut self, timestamp: u64) {
        let parent = self.head().clone();
        let number = parent.number + 1;
        let base_fee = self.next_fee(number);
        if self.fees.len() as u64 == number {
            self.fees.push(base_fee);
        }
        let block_env = env(number, timestamp, base_fee);
        let mut state = parent.state.clone();
        let mut mined = Vec::new();
        let mut waiting = std::mem::take(&mut self.pool);
        waiting.retain(|pooled| {
            pooled.expires.is_none_or(|until| number <= until) && !self.lost(&pooled.tx)
        });
        let order = self
            .seed
            .below("block-order", number ^ (self.forks << 32), 3);
        match order {
            0 => waiting.sort_by_key(|pooled| {
                std::cmp::Reverse(pooled.tx.envelope.max_priority_fee_per_gas().unwrap_or(0))
            }),
            1 => waiting.reverse(),
            _ => {}
        }
        loop {
            let next = waiting.iter().position(|pooled| {
                number > pooled.arrived + self.delay(pooled)
                    && pooled.tx.envelope.nonce() == Self::nonce(&state, pooled.tx.from)
                    && pooled.tx.envelope.max_fee_per_gas() >= u128::from(base_fee)
            });
            let Some(next) = next else { break };
            let pooled = waiting.remove(next);
            let Ok(ran) = run(
                &state,
                self.chain_id,
                &block_env,
                self.tx_env(&pooled.tx),
                false,
            ) else {
                continue;
            };
            if pooled.tx.private && !ran.result.is_success() {
                waiting.push(Pooled {
                    arrived: number,
                    ..pooled
                });
                continue;
            }
            state.commit(ran.state);
            let created = match &ran.result {
                ExecutionResult::Success {
                    output: Output::Create(_, address),
                    ..
                } => *address,
                _ => None,
            };
            self.published.push(Published {
                hash: pooled.tx.hash,
                from: pooled.tx.from,
                to: pooled.tx.envelope.to(),
                input: pooled.tx.envelope.input().clone(),
                at: number,
                landed: Some(ran.result.is_success()),
            });
            let count = (pooled.tx.from, pooled.tx.envelope.nonce() + 1);
            self.rises.entry(count).or_default().push(number);
            mined.push(Mined {
                succeeded: ran.result.is_success(),
                gas_used: ran.result.tx_gas_used(),
                logs: ran.result.logs().to_vec(),
                created,
                tx: pooled.tx,
            });
        }
        waiting.retain(|pooled| pooled.tx.envelope.nonce() >= Self::nonce(&state, pooled.tx.from));
        self.pool = waiting;
        let mut preimage = Vec::new();
        preimage.extend_from_slice(parent.hash.as_slice());
        preimage.extend_from_slice(&number.to_be_bytes());
        preimage.extend_from_slice(&timestamp.to_be_bytes());
        preimage.extend_from_slice(&self.forks.to_be_bytes());
        for tx in &mined {
            preimage.extend_from_slice(tx.tx.hash.as_slice());
        }
        let hash = keccak256(&preimage);
        let summary = format!(
            "block {number} {} at {timestamp} fee {base_fee} txs {}",
            &hash.to_string()[..10],
            mined
                .iter()
                .map(|tx| format!(
                    "{}{}",
                    &tx.tx.hash.to_string()[..10],
                    if tx.succeeded { "" } else { "!" }
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        self.shaky = !mined.is_empty();
        self.blocks.push(Block {
            number,
            hash,
            parent: parent.hash,
            timestamp,
            base_fee,
            mined,
            state,
        });
        self.trace.push(summary);
    }

    fn reorg(&mut self, depth: u64) {
        let head = self.head().number;
        let depth = depth.min(head.saturating_sub(self.floor));
        if depth == 0 {
            return;
        }
        self.forks += 1;
        let dropped = self.blocks.split_off((head - depth + 1) as usize);
        self.note(format!("reorg of {depth} from {head}"));
        let back = dropped.first().expect("a dropped block").number - 1;
        let mut again: Vec<Pooled> = dropped
            .iter()
            .flat_map(|block| &block.mined)
            .map(|mined| Pooled {
                tx: mined.tx.clone(),
                arrived: back,
                expires: mined.tx.private.then_some(back + PROTECT),
                replayed: true,
            })
            .collect();
        again.append(&mut self.pool);
        self.pool = again;
        for block in &dropped {
            self.build(block.timestamp);
        }
    }

    fn resolve(&self, at: Option<BlockId>, stale: bool) -> Result<&Block, Refusal> {
        let head = self.head();
        let behind = |blocks: u64| {
            let number = head.number.saturating_sub(blocks);
            &self.blocks[number as usize]
        };
        match at.unwrap_or(BlockId::latest()) {
            BlockId::Hash(hash) => self
                .blocks
                .iter()
                .find(|block| block.hash == hash.block_hash)
                .ok_or_else(|| Refusal::new("header not found")),
            BlockId::Number(BlockNumberOrTag::Number(number)) => self
                .blocks
                .get(number as usize)
                .ok_or_else(|| Refusal::new("header not found")),
            BlockId::Number(BlockNumberOrTag::Earliest) => Ok(&self.blocks[0]),
            BlockId::Number(BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized) => {
                Ok(behind(self.weather.reorg_depth + 1))
            }
            BlockId::Number(_) => Ok(behind(u64::from(stale))),
        }
    }

    fn call_env(&self, request: &TransactionRequest) -> TxEnv {
        TxEnv {
            caller: request.from.unwrap_or_default(),
            gas_limit: request.gas.unwrap_or(GAS_LIMIT),
            gas_price: 0,
            kind: request.to.unwrap_or(TxKind::Create),
            value: request.value.unwrap_or_default(),
            data: request.input.input().cloned().unwrap_or_default(),
            chain_id: Some(self.chain_id),
            ..Default::default()
        }
    }

    fn simulate(
        &self,
        request: &TransactionRequest,
        block: &Block,
        gas: Option<u64>,
    ) -> Result<ExecutionResult, Refusal> {
        let mut tx = self.call_env(request);
        if let Some(gas) = gas {
            tx.gas_limit = gas;
        }
        let block_env = env(block.number, block.timestamp, block.base_fee);
        run(&block.state, self.chain_id, &block_env, tx, true)
            .map(|ran| ran.result)
            .map_err(Refusal::new)
    }

    fn reverted(result: ExecutionResult) -> Result<ExecutionResult, Refusal> {
        match result {
            ExecutionResult::Success { .. } => Ok(result),
            ExecutionResult::Revert { output, .. } => Err(Refusal {
                code: 3,
                message: "execution reverted".into(),
                data: Some(output),
            }),
            ExecutionResult::Halt { reason, .. } => {
                Err(Refusal::new(format!("execution halted: {reason:?}")))
            }
        }
    }

    fn estimate(&self, request: &TransactionRequest, block: &Block) -> Result<u64, Refusal> {
        let ceiling = request.gas.unwrap_or(GAS_LIMIT);
        let whole = Self::reverted(self.simulate(request, block, Some(ceiling))?)?;
        let (mut low, mut high) = (whole.tx_gas_used().saturating_sub(1), ceiling);
        while high - low > 1 {
            let middle = low + (high - low) / 2;
            match self.simulate(request, block, Some(middle)) {
                Ok(result) if result.is_success() => high = middle,
                _ => low = middle,
            }
        }
        Ok(high)
    }

    fn take(&mut self, raw: &Bytes, private: Option<u64>) -> Result<B256, Refusal> {
        let envelope = TxEnvelope::decode_2718(&mut raw.as_ref())
            .map_err(|e| Refusal::new(format!("invalid transaction: {e}")))?;
        let from = envelope
            .recover_signer()
            .map_err(|_| Refusal::new("invalid signature"))?;
        let hash = *envelope.tx_hash();
        if envelope.chain_id() != Some(self.chain_id) {
            return Err(Refusal::new("invalid chain id"));
        }
        let head = self.head();
        if self.pool.iter().any(|pooled| pooled.tx.hash == hash) {
            return Err(Refusal::new("already known"));
        }
        let held = Self::nonce(&head.state, from);
        if envelope.nonce() < held {
            return Err(Refusal::new(format!(
                "nonce too low: next nonce {held}, tx nonce {}",
                envelope.nonce()
            )));
        }
        let balance = head
            .state
            .basic_ref(from)
            .ok()
            .flatten()
            .map_or(U256::ZERO, |account| account.balance);
        let most = U256::from(envelope.max_fee_per_gas()) * U256::from(envelope.gas_limit())
            + envelope.value();
        if balance < most {
            return Err(Refusal::new("insufficient funds for gas * price + value"));
        }
        let rival = self.pool.iter().position(|pooled| {
            pooled.tx.from == from
                && pooled.tx.envelope.nonce() == envelope.nonce()
                && pooled.tx.private == private.is_some()
        });
        if let Some(rival) = rival {
            let old = &self.pool[rival].tx.envelope;
            let outbid = |new: u128, old: u128| new >= old + old / 10;
            if !outbid(envelope.max_fee_per_gas(), old.max_fee_per_gas())
                || !outbid(
                    envelope.max_priority_fee_per_gas().unwrap_or(0),
                    old.max_priority_fee_per_gas().unwrap_or(0),
                )
            {
                return Err(Refusal::new("replacement transaction underpriced"));
            }
            self.pool.remove(rival);
        }
        let number = self.head().number;
        let tx = Tx {
            hash,
            from,
            envelope,
            private: private.is_some(),
            sent: number,
        };
        if private.is_none() {
            let key = u64::from_be_bytes(hash[..8].try_into().unwrap());
            self.ambushed |= self.seed.chance("ambush", key, self.weather.ambush);
            self.published.push(Published {
                hash,
                from,
                to: tx.envelope.to(),
                input: tx.envelope.input().clone(),
                at: number,
                landed: None,
            });
        }
        self.note(format!(
            "{} tx {} from {} nonce {}",
            if tx.private { "private" } else { "public" },
            &hash.to_string()[..10],
            &from.to_string()[..8],
            tx.envelope.nonce()
        ));
        self.seen.insert(hash, tx.clone());
        self.pool.push(Pooled {
            tx,
            arrived: number,
            expires: private.map(|blocks| number + blocks),
            replayed: false,
        });
        Ok(hash)
    }

    fn outs(&self, from: Address, nonce: u64) -> impl Iterator<Item = u64> + '_ {
        let sent = self.published.iter().filter(move |sent| {
            let numbered = self.seen.get(&sent.hash);
            sent.landed.is_none()
                && sent.from == from
                && numbered.is_some_and(|tx| tx.envelope.nonce() == nonce)
        });
        sent.map(|sent| sent.at)
    }

    fn located(&self, hash: B256) -> Option<(&Block, usize)> {
        self.blocks.iter().rev().find_map(|block| {
            let index = block.mined.iter().position(|tx| tx.tx.hash == hash)?;
            Some((block, index))
        })
    }

    fn logs_of(block: &Block) -> Vec<Value> {
        let mut out = Vec::new();
        for (index, mined) in block.mined.iter().enumerate() {
            for log in &mined.logs {
                out.push(json!({
                    "address": log.address,
                    "topics": log.data.topics(),
                    "data": log.data.data,
                    "blockHash": block.hash,
                    "blockNumber": quantity(block.number),
                    "blockTimestamp": quantity(block.timestamp),
                    "transactionHash": mined.tx.hash,
                    "transactionIndex": quantity(index),
                    "logIndex": quantity(out.len()),
                    "removed": false,
                }));
            }
        }
        out
    }

    fn receipt(&self, hash: B256) -> Value {
        let Some((block, index)) = self.located(hash) else {
            return Value::Null;
        };
        let mined = &block.mined[index];
        let envelope = &mined.tx.envelope;
        let tip = envelope
            .max_priority_fee_per_gas()
            .unwrap_or(0)
            .min(envelope.max_fee_per_gas() - u128::from(block.base_fee));
        let logs: Vec<Value> = Self::logs_of(block)
            .into_iter()
            .filter(|log| log["transactionHash"] == json!(hash))
            .collect();
        let cumulative: u64 = block.mined[..=index].iter().map(|tx| tx.gas_used).sum();
        json!({
            "transactionHash": hash,
            "transactionIndex": quantity(index),
            "blockHash": block.hash,
            "blockNumber": quantity(block.number),
            "from": mined.tx.from,
            "to": envelope.to(),
            "cumulativeGasUsed": quantity(cumulative),
            "gasUsed": quantity(mined.gas_used),
            "contractAddress": mined.created,
            "logs": logs,
            "logsBloom": Bloom::ZERO,
            "type": quantity(envelope.ty()),
            "status": quantity(u8::from(mined.succeeded)),
            "effectiveGasPrice": quantity(u128::from(block.base_fee) + tip),
        })
    }

    fn block_json(block: &Block) -> Value {
        json!({
            "hash": block.hash,
            "parentHash": block.parent,
            "sha3Uncles": B256::ZERO,
            "miner": Address::ZERO,
            "stateRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO,
            "receiptsRoot": B256::ZERO,
            "logsBloom": Bloom::ZERO,
            "difficulty": "0x0",
            "number": quantity(block.number),
            "gasLimit": quantity(GAS_LIMIT),
            "gasUsed": quantity(block.mined.iter().map(|tx| tx.gas_used).sum::<u64>()),
            "timestamp": quantity(block.timestamp),
            "extraData": "0x",
            "mixHash": B256::ZERO,
            "nonce": "0x0000000000000000",
            "baseFeePerGas": quantity(block.base_fee),
            "size": "0x0",
            "uncles": [],
            "transactions": block.mined.iter().map(|tx| tx.tx.hash).collect::<Vec<_>>(),
        })
    }

    fn answer(
        &mut self,
        method: &str,
        params: Value,
        stale: bool,
    ) -> Result<Box<RawValue>, Refusal> {
        let arg = |index: usize| params.get(index).cloned().unwrap_or(Value::Null);
        fn read<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Refusal> {
            serde_json::from_value(value).map_err(Refusal::params)
        }
        let at = |index: usize| -> Result<Option<BlockId>, Refusal> {
            match arg(index) {
                Value::Null => Ok(None),
                value => read(value).map(Some),
            }
        };
        let pending = |index: usize| arg(index) == json!("pending");
        Ok(raw(&match method {
            "eth_chainId" => quantity(self.chain_id),
            "eth_blockNumber" => quantity(self.resolve(None, stale)?.number),
            "eth_gasPrice" => quantity(self.head().base_fee + TIP),
            "eth_maxPriorityFeePerGas" => quantity(TIP),
            "eth_feeHistory" => {
                let head = self.head();
                json!({
                    "oldestBlock": quantity(head.number),
                    "baseFeePerGas": [quantity(head.base_fee), quantity(self.next_fee(head.number + 1))],
                    "gasUsedRatio": [0.5],
                    "reward": [[quantity(TIP)]],
                })
            }
            "eth_getBlockByNumber" => match self.resolve(at(0)?, stale) {
                Ok(block) => Self::block_json(block),
                Err(_) => Value::Null,
            },
            "eth_getBlockByHash" => {
                let hash: B256 = read(arg(0))?;
                self.blocks
                    .iter()
                    .find(|block| block.hash == hash)
                    .map_or(Value::Null, Self::block_json)
            }
            "eth_getBalance" => {
                let who: Address = read(arg(0))?;
                let block = self.resolve(at(1)?, stale)?;
                let account = block.state.basic_ref(who).ok().flatten();
                quantity(account.map_or(U256::ZERO, |account| account.balance))
            }
            "eth_getCode" => {
                let who: Address = read(arg(0))?;
                let block = self.resolve(at(1)?, stale)?;
                let code = block
                    .state
                    .basic_ref(who)
                    .ok()
                    .flatten()
                    .and_then(|account| account.code)
                    .map_or(Bytes::new(), |code| code.original_bytes());
                json!(code)
            }
            "eth_getTransactionCount" => {
                let who: Address = read(arg(0))?;
                if pending(1) {
                    let mut next = Self::nonce(&self.head().state, who);
                    let mut queued: Vec<u64> = self
                        .pool
                        .iter()
                        .filter(|pooled| pooled.tx.from == who && !pooled.tx.private)
                        .map(|pooled| pooled.tx.envelope.nonce())
                        .collect();
                    queued.sort_unstable();
                    for nonce in queued {
                        if nonce == next {
                            next += 1;
                        }
                    }
                    quantity(next)
                } else {
                    quantity(Self::nonce(&self.resolve(at(1)?, stale)?.state, who))
                }
            }
            "eth_call" => {
                let request: TransactionRequest = read(arg(0))?;
                let block = match pending(1) {
                    true => self.head(),
                    false => self.resolve(at(1)?, stale)?,
                };
                let result = Self::reverted(self.simulate(&request, block, None)?)?;
                let returned = format!("\"{}\"", result.into_output().unwrap_or_default());
                return Ok(RawValue::from_string(returned).expect("a JSON string"));
            }
            "eth_estimateGas" => {
                let request: TransactionRequest = read(arg(0))?;
                let block = match pending(1) {
                    true => self.head(),
                    false => self.resolve(at(1)?, stale)?,
                };
                quantity(self.estimate(&request, block)?)
            }
            "eth_sendRawTransaction" => {
                let raw: Bytes = read(arg(0))?;
                json!(self.take(&raw, None)?)
            }
            "eth_getTransactionReceipt" => self.receipt(read(arg(0))?),
            "eth_getTransactionByHash" => {
                let hash: B256 = read(arg(0))?;
                let Some(tx) = self.seen.get(&hash) else {
                    return Ok(raw(&Value::Null));
                };
                let located = self.located(hash);
                if located.is_none()
                    && (tx.private || !self.pool.iter().any(|pooled| pooled.tx.hash == hash))
                {
                    return Ok(raw(&Value::Null));
                }
                let shown = RpcTransaction {
                    inner: Recovered::new_unchecked(tx.envelope.clone(), tx.from),
                    block_hash: located.map(|(block, _)| block.hash),
                    block_number: located.map(|(block, _)| block.number),
                    transaction_index: located.map(|(_, index)| index as u64),
                    effective_gas_price: None,
                };
                serde_json::to_value(shown).map_err(Refusal::params)?
            }
            "eth_getLogs" => {
                let filter: Filter = read(arg(0))?;
                let head = self.head().number;
                let (from, to) = match filter.block_option.as_block_hash() {
                    Some(hash) => {
                        let block = self.resolve(Some(BlockId::hash(*hash)), false)?;
                        (block.number, block.number)
                    }
                    None => (
                        filter.get_from_block().unwrap_or(head),
                        filter.get_to_block().unwrap_or(head).min(head),
                    ),
                };
                let mut out = Vec::new();
                for block in self
                    .blocks
                    .iter()
                    .filter(|b| (from..=to).contains(&b.number))
                {
                    for (index, mined) in block.mined.iter().enumerate() {
                        let before = block.mined[..index].iter().map(|tx| tx.logs.len());
                        let first: usize = before.sum();
                        for (offset, log) in mined.logs.iter().enumerate() {
                            if filter.matches(log) {
                                out.push(Self::logs_of(block)[first + offset].clone());
                            }
                        }
                    }
                }
                Value::Array(out)
            }
            other => {
                return Err(Refusal {
                    code: -32601,
                    message: format!("the method {other} does not exist"),
                    data: None,
                });
            }
        }))
    }
}

impl Node {
    pub fn new(seed: Seed, chain_id: u64, genesis_time: u64, weather: Weather) -> Self {
        let genesis = Block {
            number: 0,
            hash: keccak256(b"genesis"),
            parent: B256::ZERO,
            timestamp: genesis_time,
            base_fee: 2 * GWEI,
            mined: Vec::new(),
            state: CacheDB::new(EmptyDB::new()),
        };
        Self(Arc::new(Mutex::new(Inner {
            seed,
            chain_id,
            weather,
            blocks: vec![genesis],
            pool: Vec::new(),
            seen: BTreeMap::new(),
            published: Vec::new(),
            rises: BTreeMap::new(),
            fees: vec![2 * GWEI],
            forks: 0,
            floor: 0,
            clears: u64::MAX,
            shaky: false,
            ambushed: false,
            eases: false,
            trace: Vec::new(),
        })))
    }

    fn setup<T>(&self, change: impl FnOnce(&mut Inner, &mut State) -> T) -> T {
        let mut inner = self.0.lock().unwrap();
        assert_eq!(inner.blocks.len(), 1, "setup is for the genesis state only");
        let mut state = std::mem::replace(&mut inner.blocks[0].state, CacheDB::new(EmptyDB::new()));
        let out = change(&mut inner, &mut state);
        inner.blocks[0].state = state;
        out
    }

    pub fn fund(&self, who: Address, wei: U256) {
        self.setup(|_, state| {
            let mut info = state.basic_ref(who).ok().flatten().unwrap_or_default();
            info.balance = wei;
            state.insert_account_info(who, info);
        });
    }

    pub fn transact(
        &self,
        from: Address,
        to: Option<Address>,
        data: Bytes,
    ) -> (Bytes, Option<Address>) {
        self.setup(|inner, state| {
            let genesis = &inner.blocks[0];
            let block_env = env(0, genesis.timestamp, genesis.base_fee);
            let tx = TxEnv {
                caller: from,
                gas_limit: GAS_LIMIT,
                kind: to.map_or(TxKind::Create, TxKind::Call),
                data,
                nonce: Inner::nonce(state, from),
                chain_id: Some(inner.chain_id),
                ..Default::default()
            };
            let ran = run(state, inner.chain_id, &block_env, tx, true).expect("a setup call runs");
            assert!(
                ran.result.is_success(),
                "a setup call failed: {:?}",
                ran.result
            );
            state.commit(ran.state);
            let created = match &ran.result {
                ExecutionResult::Success {
                    output: Output::Create(_, address),
                    ..
                } => *address,
                _ => None,
            };
            (ran.result.into_output().unwrap_or_default(), created)
        })
    }

    pub fn place(&self, from: Address, at: Address) {
        self.setup(|_, state| {
            let info = state
                .basic_ref(from)
                .ok()
                .flatten()
                .expect("a deployed contract");
            state.insert_account_info(
                at,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 1,
                    code_hash: info.code_hash,
                    code: info.code,
                    ..Default::default()
                },
            );
        });
    }

    pub fn view(&self, to: Address, data: Bytes) -> Result<Bytes, String> {
        let head = self.head().0;
        self.view_at(head, to, data)
    }

    pub fn view_at(&self, number: u64, to: Address, data: Bytes) -> Result<Bytes, String> {
        let inner = self.0.lock().unwrap();
        let request = TransactionRequest::default().to(to).input(data.into());
        let block = &inner.blocks[number as usize];
        let result = inner
            .simulate(&request, block, None)
            .map_err(|refusal| refusal.message)?;
        match result {
            ExecutionResult::Success { .. } => Ok(result.into_output().unwrap_or_default()),
            other => Err(format!("{other:?}")),
        }
    }

    pub fn tick(&self, timestamp: u64) {
        let mut inner = self.0.lock().unwrap();
        if timestamp >= inner.clears {
            inner.clears = u64::MAX;
            inner.weather = Weather::CALM;
            inner.note("the weather cleared".into());
        }
        let slot = timestamp / SLOT;
        if inner
            .seed
            .chance("missed-slot", slot, inner.weather.missed_slot)
        {
            inner.note(format!("missed the slot at {timestamp}"));
            return;
        }
        let shaken = std::mem::take(&mut inner.shaky)
            && inner
                .seed
                .chance("reorg-after-tx", slot, inner.weather.reorg_after_tx);
        if std::mem::take(&mut inner.ambushed) {
            let depth = inner.weather.reorg_depth;
            inner.reorg(depth);
        } else if shaken || inner.seed.chance("reorg", slot, inner.weather.reorg) {
            let depth =
                inner
                    .seed
                    .between("reorg-depth", slot, 1, inner.weather.reorg_depth.max(1));
            inner.reorg(depth);
        }
        inner.build(timestamp);
    }

    pub fn clear_from(&self, timestamp: u64) {
        self.0.lock().unwrap().clears = timestamp;
    }

    pub fn ease_fees(&self) {
        self.0.lock().unwrap().eases = true;
    }

    pub fn head(&self) -> (u64, u64) {
        let inner = self.0.lock().unwrap();
        (inner.head().number, inner.head().timestamp)
    }

    pub fn published(&self) -> Vec<Published> {
        self.0.lock().unwrap().published.clone()
    }

    pub fn trace(&self) -> Vec<String> {
        self.0.lock().unwrap().trace.clone()
    }

    pub fn logs(&self) -> Vec<Logged> {
        let inner = self.0.lock().unwrap();
        let mut logs = Vec::new();
        for block in &inner.blocks {
            for mined in &block.mined {
                logs.extend(mined.logs.iter().map(|log| Logged {
                    block: block.number,
                    tx: mined.tx.hash,
                    log: log.clone(),
                }));
            }
        }
        logs
    }

    pub fn note(&self, what: String) {
        self.0.lock().unwrap().note(what);
    }

    pub(crate) fn nonce_at(&self, who: Address, back: u64) -> u64 {
        let inner = self.0.lock().unwrap();
        let number = inner.head().number.saturating_sub(back);
        Inner::nonce(&inner.blocks[number as usize].state, who)
    }

    pub(crate) fn holds_privately(&self, who: Address, nonce: u64) -> bool {
        self.0.lock().unwrap().pool.iter().any(|pooled| {
            let tx = &pooled.tx;
            tx.private && tx.from == who && tx.envelope.nonce() == nonce
        })
    }

    pub(crate) fn missed(&self, who: &str, method: &str, since: u64) -> bool {
        self.0.lock().unwrap().trace.iter().any(|line| {
            let told = line.strip_prefix('#').and_then(|line| line.split_once(' '));
            told.is_some_and(|(at, what)| {
                at.parse().is_ok_and(|at: u64| at >= since)
                    && what.split(' ').next() == Some(who)
                    && what.ends_with(method)
            })
        })
    }

    pub(crate) fn rose(&self, who: Address, count: u64, after: u64) -> Option<u64> {
        let inner = self.0.lock().unwrap();
        let rises = inner.rises.get(&(who, count))?;
        rises.iter().copied().filter(|at| *at > after).min()
    }

    pub(crate) fn known(&self, hash: B256) -> Option<Known> {
        let inner = self.0.lock().unwrap();
        let tx = inner.seen.get(&hash)?;
        Some(Known {
            from: tx.from,
            nonce: tx.envelope.nonce(),
            private: tx.private,
            sent: tx.sent,
            mined: inner.located(hash).map(|(block, _)| block.number),
            pooled: inner.pool.iter().any(|pooled| pooled.tx.hash == hash),
        })
    }

    pub(crate) fn out_since(&self, from: Address, nonce: u64) -> Option<u64> {
        self.0.lock().unwrap().outs(from, nonce).min()
    }

    pub(crate) fn waiting(&self, from: Address, nonce: u64) -> Option<Waiting> {
        let inner = self.0.lock().unwrap();
        let pooled = inner.pool.iter().find(|pooled| {
            let tx = &pooled.tx;
            tx.from == from && !tx.private && tx.envelope.nonce() == nonce
        })?;
        let envelope = &pooled.tx.envelope;
        Some(Waiting {
            to: envelope.to(),
            input: envelope.input().clone(),
            value: envelope.value(),
            gas: envelope.gas_limit(),
            max_fee: envelope.max_fee_per_gas(),
            tip: envelope.max_priority_fee_per_gas().unwrap_or(0),
        })
    }

    pub(crate) fn queues(&self) -> Vec<Queue> {
        let inner = self.0.lock().unwrap();
        let open = inner.pool.iter().filter(|pooled| !pooled.tx.private);
        let mut senders: Vec<Address> = open.clone().map(|pooled| pooled.tx.from).collect();
        senders.sort_unstable();
        senders.dedup();
        let queued = senders.into_iter().filter_map(|from| {
            let behind = Inner::nonce(&inner.head().state, from);
            let level = inner.blocks.iter().rev();
            let level = level.take_while(|block| Inner::nonce(&block.state, from) == behind);
            let since = level.last()?.number;
            let later = open
                .clone()
                .filter(|pooled| pooled.tx.from == from && pooled.tx.envelope.nonce() > behind);
            let spans = later.map(|pooled| {
                let outs = inner.outs(from, pooled.tx.envelope.nonce());
                let outs: Vec<u64> = outs.filter(|at| *at >= since).collect();
                let first = outs.iter().min().copied().unwrap_or(0);
                outs.iter().max().map_or(0, |last| last - first)
            });
            Some(Queue {
                from,
                behind,
                repriced: spans.max()?,
            })
        });
        queued.collect()
    }

    pub(crate) fn weather(&self) -> (Seed, Weather) {
        let inner = self.0.lock().unwrap();
        (inner.seed, inner.weather)
    }

    pub(crate) fn answer(
        &self,
        method: &str,
        params: Value,
        stale: bool,
    ) -> Result<Box<RawValue>, Refusal> {
        self.0.lock().unwrap().answer(method, params, stale)
    }

    pub(crate) fn take_privately(&self, raw: &Bytes, blocks: u64) -> Result<B256, Refusal> {
        self.0.lock().unwrap().take(raw, Some(blocks))
    }
}
