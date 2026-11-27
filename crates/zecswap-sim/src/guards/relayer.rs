use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use tokio::sync::watch;
use tokio::time::{Instant, sleep};
use zecswap_chain::evm::Settlement;
use zecswap_chain::evm::funding::{FundingError, FundingPolicy};
use zecswap_chain::evm::railgun::{SendError, SendPolicy, Signed, Status};
use zecswap_core::Domain;
use zecswap_railgun::Keys;

use crate::node::{Node, SLOT};
use crate::world::{CHAIN_ID, EPOCH};

pub const MAX_GAS: u64 = 3_000_000;
pub const RATE: u128 = 1_000_000_000;
const ETHER: u128 = 1_000_000_000_000_000_000;
const UNHEARD: u64 = 60;
const BURIED: u64 = 12;
const REVERTED_HOLD: u64 = 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Sent(Vec<B256>),
    Rejected(String),
    Spent(Vec<B256>),
    Busy(u64),
    Internal(String),
    Lost,
}

#[derive(Clone, Debug)]
struct Record {
    signed: Vec<Signed>,
    nullifiers: Spends,
    unheard_since: Option<u64>,
    reverted_at: Option<u64>,
    forgotten: bool,
}

#[derive(Default)]
pub struct Journal {
    sends: BTreeMap<B256, Record>,
    fundings: BTreeMap<B256, B256>,
}

#[derive(Clone, Debug)]
pub struct Counted {
    pub data: Bytes,
    pub fee: u128,
}

type Spends = Vec<(u16, B256)>;

struct Flying<'a> {
    relayer: &'a Relayer,
    key: B256,
}

impl Drop for Flying<'_> {
    fn drop(&mut self) {
        let mut flying = self.relayer.flying.lock().unwrap();
        flying.retain(|(key, _)| *key != self.key);
    }
}

pub struct Relayer {
    node: Node,
    contract: Address,
    signer: alloy::signers::local::PrivateKeySigner,
    keys: Keys,
    pub send_policy: SendPolicy,
    pub funding_policy: FundingPolicy,
    priced: bool,
    start: Instant,
    journal: Mutex<Journal>,
    life: Mutex<Option<Arc<Settlement>>>,
    lives: watch::Sender<u64>,
    sponsors: Mutex<Option<Result<(), String>>>,
    flying: Mutex<Vec<(B256, Spends)>>,
    counted: Mutex<Vec<Counted>>,
    wrong: Mutex<Vec<String>>,
}

pub struct Desk {
    pub signer: alloy::signers::local::PrivateKeySigner,
    pub keys: Keys,
    pub send_policy: SendPolicy,
    pub funding_policy: FundingPolicy,
    pub priced: bool,
}

struct Alive(Arc<Relayer>);

impl Drop for Alive {
    fn drop(&mut self) {
        *self.0.life.lock().unwrap() = None;
        self.0.lives.send_modify(|life| *life += 1);
    }
}

fn says(error: &SendError) -> String {
    match error {
        SendError::Chain(e) => format!("chain: {e}"),
        other => other.to_string(),
    }
}

impl Relayer {
    pub fn new(node: Node, contract: Address, desk: Desk, start: Instant) -> Arc<Self> {
        Arc::new(Self {
            node,
            contract,
            signer: desk.signer,
            keys: desk.keys,
            send_policy: desk.send_policy,
            funding_policy: desk.funding_policy,
            priced: desk.priced,
            start,
            journal: Mutex::default(),
            life: Mutex::default(),
            lives: watch::channel(0).0,
            sponsors: Mutex::default(),
            flying: Mutex::default(),
            counted: Mutex::default(),
            wrong: Mutex::default(),
        })
    }

    pub fn account(&self) -> Address {
        self.signer.address()
    }

    pub fn counted(&self) -> Vec<Counted> {
        self.counted.lock().unwrap().clone()
    }

    pub fn wrong(&self) -> Vec<String> {
        self.wrong.lock().unwrap().clone()
    }

    fn now(&self) -> u64 {
        EPOCH + self.start.elapsed().as_secs()
    }

    fn note(&self, what: String) {
        self.node.note(what);
    }

    fn disagree(&self, what: String) {
        self.note(format!("the relayer read back {what}"));
        self.wrong.lock().unwrap().push(what);
    }

    pub async fn live(self: Arc<Self>) {
        let settlement = Settlement::over(
            self.node.rpc("relayer"),
            self.contract,
            Some(self.signer.clone()),
        );
        *self.life.lock().unwrap() = Some(Arc::new(settlement));
        *self.sponsors.lock().unwrap() = None;
        let _alive = Alive(self.clone());
        loop {
            sleep(Duration::from_secs(SLOT)).await;
            let Some(settlement) = self.settlement() else {
                return;
            };
            self.follow(&settlement).await;
        }
    }

    fn settlement(&self) -> Option<Arc<Settlement>> {
        self.life.lock().unwrap().clone()
    }

    pub async fn serve(&self, number: usize, post: &super::Post) -> Reply {
        let mut lives = self.lives.subscribe();
        let Some(settlement) = self.settlement() else {
            return Reply::Lost;
        };
        let serving = async {
            match post.swap {
                Some(swap) => self.fund(&settlement, number, swap, post).await,
                None => self.send(&settlement, number, post).await,
            }
        };
        tokio::select! {
            biased;
            _ = lives.changed() => Reply::Lost,
            reply = serving => reply,
        }
    }

    fn fly(&self, number: usize, key: B256, spends: Spends) -> Option<Flying<'_>> {
        let mut flying = self.flying.lock().unwrap();
        let shared = |(other, spent): &(B256, Spends)| {
            *other == key || spent.iter().any(|spent| spends.contains(spent))
        };
        if flying.iter().any(shared) {
            drop(flying);
            self.note(format!(
                "the relayer holds request {number}: a send of its notes has no outcome yet"
            ));
            return None;
        }
        flying.push((key, spends));
        Some(Flying { relayer: self, key })
    }

    fn refused(&self, number: usize, stage: &str, why: String) -> Reply {
        self.note(format!(
            "the relayer refused request {number} at {stage}: {why}"
        ));
        Reply::Rejected(why)
    }

    async fn send(&self, settlement: &Settlement, number: usize, post: &super::Post) -> Reply {
        if post.chain_id != CHAIN_ID {
            return self.refused(number, "terms", "wrong chain".into());
        }
        let id = keccak256(&post.data);
        let known = self.journal.lock().unwrap().sends.get(&id).cloned();
        if let Some(record) = known.filter(|record| !record.forgotten && !record.signed.is_empty())
        {
            let hash = record.signed.last().expect("a signed send").hash;
            self.note(format!("the relayer knows request {number}: {hash}"));
            return Reply::Sent(vec![hash]);
        }
        let policy = &self.send_policy;
        let transact = match policy.decode(CHAIN_ID, post.to, post.value, post.data.clone()) {
            Ok(transact) => transact,
            Err(e) => return self.refused(number, "decode", says(&e)),
        };
        let fee = match policy.check(&transact, &self.keys) {
            Ok(fee) => fee,
            Err(e) => return self.refused(number, "check", says(&e)),
        };
        let nullifiers = transact.nullifiers();
        let Some(_flying) = self.fly(number, id, nullifiers.clone()) else {
            return Reply::Busy(15);
        };
        let now = self.now();
        let mut rivals = Vec::new();
        for (other, record) in &self.journal.lock().unwrap().sends {
            let shared = record
                .nullifiers
                .iter()
                .any(|spent| nullifiers.contains(spent));
            if *other == id || record.forgotten || !shared {
                continue;
            }
            if let Some(at) = record.reverted_at {
                if now < at + REVERTED_HOLD {
                    self.note(format!(
                        "the relayer holds request {number}: its notes' send reverted"
                    ));
                    return Reply::Busy(at + REVERTED_HOLD - now);
                }
                continue;
            }
            rivals.extend(record.signed.iter().map(|signed| signed.hash));
        }
        if !rivals.is_empty() {
            self.note(format!(
                "the relayer refused request {number} at journal: its notes are in a send of its own"
            ));
            return Reply::Spent(rivals);
        }
        let priced = self.priced;
        let covers = |gas: u64, price: u128| {
            !priced
                || U256::from(fee) * U256::from(ETHER)
                    >= U256::from(gas) * U256::from(price) * U256::from(RATE)
        };
        let record = |signed: &Signed| {
            let mut journal = self.journal.lock().unwrap();
            let entry = journal.sends.entry(id).or_insert_with(|| Record {
                signed: Vec::new(),
                nullifiers: nullifiers.clone(),
                unheard_since: None,
                reverted_at: None,
                forgotten: false,
            });
            entry.forgotten = false;
            entry.signed.push(signed.clone());
            self.counted.lock().unwrap().push(Counted {
                data: post.data.clone(),
                fee,
            });
            Ok(())
        };
        match settlement
            .send_transact(policy, &transact, covers, record)
            .await
        {
            Ok(hash) => {
                self.note(format!("the relayer sent request {number} as {hash}"));
                Reply::Sent(vec![hash])
            }
            Err(SendError::Unknown(hash)) => {
                self.note(format!(
                    "the relayer sent request {number} as {hash}, unheard"
                ));
                Reply::Sent(vec![hash])
            }
            Err(SendError::Spent) => {
                self.note(format!(
                    "the relayer refused request {number} at send: spent"
                ));
                Reply::Spent(Vec::new())
            }
            Err(SendError::Chain(e)) => {
                self.note(format!("the relayer failed request {number}: {e}"));
                Reply::Internal(e.to_string())
            }
            Err(e) => self.refused(number, "send", says(&e)),
        }
    }

    async fn sponsors(&self, settlement: &Settlement) -> Result<(), Reply> {
        if let Some(known) = self.sponsors.lock().unwrap().clone() {
            return known.map_err(Reply::Rejected);
        }
        let policy = &self.funding_policy;
        let checked = match policy.validate_config(self.account()) {
            Ok(()) => settlement.check_funding_adapter(policy.relay_adapt).await,
            refused => refused,
        };
        let why = match checked {
            Ok(()) => {
                *self.sponsors.lock().unwrap() = Some(Ok(()));
                return Ok(());
            }
            Err(FundingError::Chain(e)) => return Err(Reply::Internal(e.to_string())),
            Err(FundingError::Rejected(why)) => why,
        };
        if !why.starts_with("cannot read") {
            *self.sponsors.lock().unwrap() = Some(Err(why.into()));
        }
        self.note(format!("the relayer sponsors no funding: {why}"));
        Err(Reply::Rejected(why.into()))
    }

    async fn fund(
        &self,
        settlement: &Settlement,
        number: usize,
        swap: B256,
        post: &super::Post,
    ) -> Reply {
        if post.chain_id != CHAIN_ID {
            return self.refused(number, "terms", "wrong chain".into());
        }
        match self.sponsors(settlement).await {
            Err(Reply::Rejected(why)) => return self.refused(number, "config", why),
            Err(other) => return other,
            Ok(()) => {}
        }
        let Some(_flying) = self.fly(number, swap, Vec::new()) else {
            return Reply::Busy(15);
        };
        let known = self.journal.lock().unwrap().fundings.get(&swap).copied();
        if let Some(hash) = known
            && matches!(
                settlement.transaction_status(hash).await,
                Ok(Status::Pending)
            )
        {
            self.note(format!("the relayer knows request {number}: {hash}"));
            return Reply::Sent(vec![hash]);
        }
        let policy = &self.funding_policy;
        let domain = Domain {
            chain_id: CHAIN_ID,
            contract: self.contract.into(),
        };
        let validated = match policy.validate(domain, swap, post.to, post.value, post.data.clone())
        {
            Ok(validated) => validated,
            Err(e) => return self.refused(number, "validate", e.to_string()),
        };
        match settlement.sponsor_reverse_funding(policy, &validated).await {
            Ok(Some(hash)) => {
                self.journal.lock().unwrap().fundings.insert(swap, hash);
                self.note(format!("the relayer sent request {number} as {hash}"));
                Reply::Sent(vec![hash])
            }
            Ok(None) => {
                self.note(format!("the relayer found request {number}'s escrow open"));
                Reply::Sent(Vec::new())
            }
            Err(FundingError::Rejected(why)) => self.refused(number, "sponsor", why.into()),
            Err(FundingError::Chain(e)) => {
                self.note(format!("the relayer failed request {number}: {e}"));
                Reply::Internal(e.to_string())
            }
        }
    }

    async fn follow(&self, settlement: &Settlement) {
        let account = self.account();
        let open: Vec<(B256, Record)> = {
            let journal = self.journal.lock().unwrap();
            let open = journal.sends.iter().filter(|(_, record)| !record.forgotten);
            open.map(|(id, record)| (*id, record.clone())).collect()
        };
        for (id, record) in open {
            let Some(last) = record.signed.last() else {
                continue;
            };
            let Ok(status) = settlement.transaction_status(last.hash).await else {
                continue;
            };
            let known = self.node.known(last.hash);
            let agrees = match status {
                Status::Mined { .. } => known.as_ref().is_some_and(|known| known.mined.is_some()),
                Status::Pending => known
                    .as_ref()
                    .is_some_and(|known| known.pooled || known.mined.is_some()),
                Status::Unknown => known
                    .as_ref()
                    .is_none_or(|known| !known.pooled && known.mined.is_none()),
            };
            if !agrees {
                self.disagree(format!(
                    "{} as {status:?}, the chain has it {known:?}",
                    last.hash
                ));
            }
            let now = self.now();
            match status {
                Status::Mined { succeeded: false } => {
                    let mut journal = self.journal.lock().unwrap();
                    if let Some(record) = journal.sends.get_mut(&id) {
                        record.reverted_at.get_or_insert(now);
                        record.unheard_since = None;
                    }
                }
                Status::Mined { succeeded: true } | Status::Pending => {
                    if let Some(record) = self.journal.lock().unwrap().sends.get_mut(&id) {
                        record.unheard_since = None;
                    }
                }
                Status::Unknown => self.unheard(settlement, id, &record, last, now).await,
            }
        }
        if let Ok((mined, sent)) = settlement.nonces().await {
            let pending = self.node.pending_count(account);
            if sent != pending || mined > sent {
                self.disagree(format!(
                    "nonces {mined} and {sent}, the chain counts {pending} pending"
                ));
            }
        }
    }

    async fn unheard(
        &self,
        settlement: &Settlement,
        id: B256,
        record: &Record,
        last: &Signed,
        now: u64,
    ) {
        let account = self.account();
        let Ok(taken) = settlement.nonce_taken(last.nonce, BURIED).await else {
            return;
        };
        let counted = |back: u64| self.node.nonce_at(account, back) > last.nonce;
        if taken && !counted(BURIED) || !taken && counted(BURIED + 2) {
            self.disagree(format!("nonce {} taken {BURIED} deep: {taken}", last.nonce));
        }
        let since = {
            let mut journal = self.journal.lock().unwrap();
            let Some(entry) = journal.sends.get_mut(&id) else {
                return;
            };
            entry.forgotten = taken;
            (!taken).then(|| *entry.unheard_since.get_or_insert(now))
        };
        let Some(since) = since else {
            self.note(format!(
                "the relayer forgot {}: its nonce went to another",
                last.hash
            ));
            return;
        };
        if now < since + UNHEARD || record.reverted_at.is_some() {
            return;
        }
        match settlement.rebroadcast(last).await {
            Ok(true) => {
                let known = self.node.known(last.hash);
                if !known.is_some_and(|known| known.pooled || known.mined.is_some()) {
                    self.disagree(format!(
                        "{} broadcast again, and no node holds it",
                        last.hash
                    ));
                }
                self.note(format!("the relayer broadcast {} again", last.hash));
                if let Some(entry) = self.journal.lock().unwrap().sends.get_mut(&id) {
                    entry.unheard_since = Some(now);
                }
            }
            Ok(false) => self.note(format!(
                "the relayer left {}: its nonce is not next",
                last.hash
            )),
            Err(_) => {}
        }
    }
}
