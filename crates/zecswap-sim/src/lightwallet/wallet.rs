use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use tokio::time::{Instant, sleep};
use zcash_address::ZcashAddress;
use zcash_protocol::consensus::{Network, NetworkType, Parameters};
use zecswap_chain::Error;
use zecswap_chain::alert::Alert;
use zecswap_chain::zcash::{
    AccountUuid, Funds, Lightwalletd, ProvedPayment, SWEEP_FEE_CEILING, TxId, UnifiedSpendingKey,
    Wallet, address_at,
};
use zecswap_core::{JointAccount, SecretShare, SpendKey, ViewingKeys};

use super::ledger::Ledger;
use super::server::Server;
use super::signer::{Trick, View};
use super::{PROVER, receiver, unified};
use crate::seed::Seed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Home {
    Own,
    Stranger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    Send(u32),
    Release(u32),
    Late,
    WrongKey(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ahead {
    pub hold: u32,
    pub then: Then,
    pub crash: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum Errand {
    Receive {
        at: u64,
        chained: bool,
        notes: Vec<(u64, u64)>,
        sweep: Option<Home>,
        patience: u64,
    },
    Pay {
        at: u64,
        amount: u64,
        ahead: Option<Ahead>,
    },
    Fee {
        at: u64,
        index: u64,
        value: u64,
    },
    Sign {
        at: u64,
        notes: Vec<(u64, u64)>,
        trick: Trick,
        view: View,
        proves: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    Swept(u64),
    Watched(u64),
    Refused(u64),
    Paid(u64),
    Seen(u64),
    Signed(u64),
    Balked,
}

#[derive(Clone, Debug)]
pub enum Step {
    Waiting,
    Imported {
        account: AccountUuid,
    },
    Held {
        since: u32,
    },
    Sent {
        txid: [u8; 32],
        account: Option<AccountUuid>,
        value: u64,
        out: bool,
    },
    Done(End),
}

#[derive(Clone, Debug)]
pub enum Made {
    Sweep { home: Home, held: u64 },
    Payment { amount: u64 },
}

#[derive(Clone, Debug)]
pub struct Built {
    pub errand: usize,
    pub txid: [u8; 32],
    pub made: Made,
    pub tip: u32,
    pub settled: bool,
    pub quoted: Option<u64>,
    pub home: Option<orchard::Address>,
}

#[derive(Default)]
pub struct Saved {
    pub lives: u64,
    pub crashed: bool,
    pub locked: Option<u64>,
    pub served: bool,
    pub account: Option<AccountUuid>,
    pub fresh: Vec<String>,
    pub tries: Vec<u32>,
    pub earlier: Option<Option<AccountUuid>>,
    pub own: Option<(String, Instant)>,
    pub steps: Vec<Step>,
    pub imported: Vec<Option<Instant>>,
    pub plain: Vec<bool>,
    pub built: Vec<Built>,
    pub refused: Vec<(usize, u64, u64)>,
    pub said: BTreeMap<String, String>,
    pub born: Option<u32>,
}

pub struct Joint {
    pub account: JointAccount,
    pub key: SpendKey,
}

const TRIES: u32 = 40;
const AGAIN: u64 = 4;

pub const OWN: usize = 0;
pub const STRANGER: usize = 1;
pub const JOINTS: usize = super::ledger::JOINT_KEYS;

pub struct Keys {
    pub own: [u8; 32],
    pub other: [u8; 32],
    pub usk: UnifiedSpendingKey,
    pub joints: Vec<Joint>,
    pub stranger: FullViewingKey,
    network: Network,
}

impl Keys {
    pub fn new(seed: Seed, network: Network, errands: usize) -> Self {
        let rng = |what: &str| ChaCha20Rng::from_seed(seed.bytes(what));
        let joints = (0..errands)
            .map(|index| {
                let e = SecretShare::random(rng(&format!("joint-e-{index}")));
                let z = SecretShare::random(rng(&format!("joint-z-{index}")));
                let viewing = ViewingKeys::random(rng(&format!("joint-viewing-{index}")));
                let account =
                    JointAccount::derive(&e.public(), &z.public(), &viewing).expect("a joint key");
                let key = account.spend_key(&e, &z).expect("its spend key");
                Joint { account, key }
            })
            .collect();
        let stranger = SpendingKey::from_bytes(seed.bytes("stranger"));
        let stranger: SpendingKey = stranger.into_option().expect("a spending key");
        let own = seed.bytes("own-seed");
        Self {
            own,
            other: seed.bytes("another-seed"),
            usk: UnifiedSpendingKey::from_seed(&network, &own, zip32::AccountId::ZERO)
                .expect("the wallet's key"),
            joints,
            stranger: FullViewingKey::from(&stranger),
            network,
        }
    }

    pub fn spending(&self) -> UnifiedSpendingKey {
        UnifiedSpendingKey::from_seed(&self.network, &self.own, zip32::AccountId::ZERO)
            .expect("the wallet's key")
    }

    pub fn all(&self) -> Vec<FullViewingKey> {
        let ufvk = self.usk.to_unified_full_viewing_key();
        let own = ufvk.orchard().expect("an Orchard key").clone();
        let joints = self.joints.iter().map(|joint| joint.account.fvk().clone());
        [own, self.stranger.clone()]
            .into_iter()
            .chain(joints)
            .collect()
    }

    pub fn fee_address(&self, index: u64) -> orchard::Address {
        let ufvk = self.usk.to_unified_full_viewing_key();
        let address = address_at(&ufvk, self.network, index).expect("an address");
        receiver(&address, self.network.network_type())
    }

    pub fn stranger_address(&self) -> orchard::Address {
        self.stranger.address_at(0u32, Scope::External)
    }
}

pub struct Actor {
    pub network: Network,
    pub confirmations: u32,
    pub poll: Duration,
    pub seasons: u32,
    pub owns: bool,
    pub inventory: bool,
    pub errands: Vec<Errand>,
    pub ledger: Ledger,
    pub server: Rc<Server>,
    pub keys: Rc<Keys>,
    pub path: PathBuf,
    pub start: Instant,
    pub saved: Rc<RefCell<Saved>>,
    pub failures: Rc<RefCell<Vec<(u8, String)>>>,
    pub heard: Arc<Mutex<Vec<Alert>>>,
}

struct Life {
    actor: Rc<Actor>,
    wallet: Wallet,
    client: Lightwalletd,
    own: Option<(AccountUuid, UnifiedSpendingKey)>,
    proved: BTreeMap<usize, ProvedPayment>,
    fresh: bool,
    polls: u64,
    tried: BTreeMap<usize, u64>,
}

fn brief(error: &Error) -> String {
    match error {
        Error::Lightwalletd(status) => format!("lightwalletd answered {:?}", status.code()),
        Error::Rejected { message, .. } => format!("the node refused it: {message}"),
        Error::SweepFee { fee, ceiling } => format!("a sweep fee of {fee} is over {ceiling}"),
        Error::Config(why) => why.clone(),
        other => {
            let text = other.to_string();
            let plain = text.split_whitespace().filter(|word| {
                let digits = word.chars().filter(char::is_ascii_hexdigit).count();
                word.len() < 24 || digits * 2 < word.len()
            });
            let words: Vec<&str> = plain.take(14).collect();
            words.join(" ")
        }
    }
}

impl Actor {
    pub fn fail(&self, check: u8, what: String) {
        let mut failures = self.failures.borrow_mut();
        if !failures.iter().any(|(held, _)| *held == check) {
            failures.push((check, what));
        }
    }

    fn kind(&self) -> NetworkType {
        self.network.network_type()
    }

    fn say(&self, who: &str, what: String) {
        let mut saved = self.saved.borrow_mut();
        if saved.said.get(who) != Some(&what) {
            self.ledger.note(format!("{who}: {what}"));
            saved.said.insert(who.to_owned(), what);
        }
    }

    fn step(&self, index: usize) -> Step {
        self.saved.borrow().steps[index].clone()
    }

    fn set(&self, index: usize, step: Step) {
        self.saved.borrow_mut().steps[index] = step;
    }

    fn name(&self, txid: &[u8; 32]) -> String {
        match self.ledger.ordinal(txid) {
            Some(ordinal) => format!("transaction {ordinal}"),
            None => "a transaction not yet out".to_owned(),
        }
    }

    fn open(&self) -> Result<Wallet, Error> {
        let confirmations = NonZeroU32::new(self.confirmations).expect("a depth");
        let mut wallet = Wallet::open(&self.path, self.network)?.with_confirmations(confirmations);
        let heard = self.heard.clone();
        wallet.alerts_to(Arc::new(move |alert: &Alert| {
            heard.lock().unwrap().push(alert.clone());
        }));
        Ok(wallet)
    }

    fn birth(&self) {
        let born = self.server.told().0 + 1;
        self.saved.borrow_mut().born.get_or_insert(born);
    }

    pub async fn live(self: Rc<Self>) {
        self.saved.borrow_mut().lives += 1;
        let wallet = match self.open() {
            Ok(wallet) => wallet,
            Err(e) => {
                self.fail(1, format!("the wallet would not open: {}", brief(&e)));
                return std::future::pending().await;
            }
        };
        let mut life = Life {
            client: self.server.connect(),
            actor: self.clone(),
            wallet,
            own: None,
            proved: BTreeMap::new(),
            fresh: true,
            polls: 0,
            tried: BTreeMap::new(),
        };
        loop {
            life.poll().await;
            let done = |step: &Step| matches!(step, Step::Done(_));
            if self.saved.borrow().steps.iter().all(done) {
                return self.ledger.note("the wallet has nothing left to do".into());
            }
            sleep(self.poll).await;
        }
    }

    pub async fn audit(self: Rc<Self>) -> bool {
        let Ok(wallet) = self.open() else {
            return false;
        };
        let mut life = Life {
            client: self.server.connect(),
            actor: self,
            wallet,
            own: None,
            proved: BTreeMap::new(),
            fresh: false,
            polls: 0,
            tried: BTreeMap::new(),
        };
        if let Ok(Some(account)) = life.wallet.derived_account() {
            life.own = Some((account, life.actor.keys.spending()));
        }
        life.actor.server.clear();
        if let Err(e) = life.sync().await {
            let actor = life.actor.clone();
            actor.say("the wallet", format!("no sync at the end: {}", brief(&e)));
            return false;
        }
        let empty = life
            .wallet
            .accounts()
            .is_ok_and(|accounts| accounts.is_empty());
        let settled = life.actor.ledger.tip() == life.actor.server.told();
        if settled {
            life.observe(true);
        }
        settled || empty
    }
}

impl Life {
    fn elapsed(&self) -> u64 {
        self.actor.start.elapsed().as_secs()
    }

    async fn sync(&mut self) -> Result<(), Error> {
        let actor = self.actor.clone();
        let before = actor.heard.lock().unwrap().len();
        let misplaced = actor.server.misplaced();
        let synced = self.wallet.sync(&mut self.client).await;
        if synced.is_ok() && actor.server.misplaced() > misplaced {
            actor.fail(
                26,
                "the wallet stepped back on the tree state of another height".into(),
            );
        }
        let heard = actor.heard.lock().unwrap().clone();
        let (earlier, raised) = heard.split_at(before);
        let refused = synced
            .as_ref()
            .is_err_and(|e| e.to_string().contains("A rewind for your wallet"));
        if refused && raised.is_empty() {
            actor.fail(
                24,
                "the wallet could not step back after a reorg, and raised no alert".into(),
            );
        }
        for alert in raised {
            if earlier.iter().all(|told| told.key() != alert.key()) {
                actor
                    .ledger
                    .note(format!("alert from the wallet: {}", alert.name()));
            }
            let born = actor.saved.borrow().born;
            let fine = match alert {
                Alert::WalletRewindRefused {
                    reorg_at, rewind, ..
                } => {
                    synced.is_err()
                        && actor.ledger.reorged(*reorg_at)
                        && *rewind == reorg_at.saturating_sub(10)
                        && born.is_some_and(|born| actor.ledger.reorged(born))
                }
                _ => false,
            };
            if !fine {
                actor.fail(
                    25,
                    format!("a false alert from the wallet: {}", alert.name()),
                );
            }
        }
        synced
    }

    async fn poll(&mut self) {
        let actor = self.actor.clone();
        if self.own.is_none() && actor.owns {
            let seed = actor.keys.own;
            let made = match actor.inventory {
                true => self.wallet.inventory_account(&mut self.client, &seed).await,
                false => {
                    self.wallet
                        .seed_account(&mut self.client, &seed, "own")
                        .await
                }
            };
            match made {
                Ok((account, usk)) => {
                    let known = actor.saved.borrow().own.is_some();
                    actor.birth();
                    let before = actor.saved.borrow_mut().account.replace(account);
                    if before.is_some_and(|before| before != account) {
                        actor.fail(23, "the wallet made its own account a second time".into());
                    }
                    if !known {
                        let Ok(address) = self.wallet.address(account) else {
                            return;
                        };
                        actor.ledger.note("the wallet has its own account".into());
                        actor.saved.borrow_mut().own = Some((address, Instant::now()));
                    }
                    self.own = Some((account, usk));
                    self.strangers().await;
                }
                Err(e) => return actor.say("the wallet", format!("no account: {}", brief(&e))),
            }
        }
        if !actor.owns && actor.seasons > 0 && !self.seasoned().await {
            return;
        }
        if let Err(e) = self.sync().await {
            return actor.say("the wallet", format!("no sync: {}", brief(&e)));
        }
        actor.say("the wallet", "synced".into());
        let accounts = self
            .wallet
            .accounts()
            .is_ok_and(|accounts| !accounts.is_empty());
        actor.saved.borrow_mut().served |= accounts;
        self.weaned();
        let settled = actor.ledger.tip() == actor.server.told();
        self.observe(settled);
        for index in 0..actor.errands.len() {
            self.advance(index, settled).await;
        }
        self.fresh = false;
        self.polls += 1;
    }

    async fn seasoned(&mut self) -> bool {
        let actor = self.actor.clone();
        let earlier = actor.saved.borrow().earlier;
        let old = &actor.keys.joints[actor.errands.len()].account;
        match earlier {
            None => {
                let imported = self
                    .wallet
                    .import_joint(&mut self.client, old, "an earlier swap")
                    .await;
                match &imported {
                    Ok(account) => {
                        actor
                            .ledger
                            .note("the wallet has an earlier swap's account".into());
                        actor.birth();
                        actor.saved.borrow_mut().earlier = Some(Some(*account));
                    }
                    Err(e) => actor.say("the wallet", format!("no account: {}", brief(e))),
                }
                imported.is_ok()
            }
            Some(_) => true,
        }
    }

    fn weaned(&mut self) {
        let actor = self.actor.clone();
        let Some(Some(account)) = actor.saved.borrow().earlier else {
            return;
        };
        let age = actor.ledger.tip().0 - actor.ledger.base();
        if age > actor.seasons + 1 && self.wallet.forget(account).is_ok() {
            actor
                .ledger
                .note("the wallet forgot the earlier swap".into());
            actor.saved.borrow_mut().earlier = Some(None);
        }
    }

    async fn strangers(&mut self) {
        let actor = self.actor.clone();
        if !actor.inventory {
            return;
        }
        let count = |wallet: &Wallet| wallet.accounts().map(|accounts| accounts.len()).ok();
        let before = count(&self.wallet);
        let other = actor.keys.other;
        let taken = self
            .wallet
            .inventory_account(&mut self.client, &other)
            .await;
        if taken.is_ok() || before != count(&self.wallet) {
            actor.fail(2, "the wallet took another seed for its inventory's".into());
        }
    }

    fn tracked(&self) -> Vec<(String, AccountUuid, usize, bool, bool)> {
        let actor = &self.actor;
        let saved = actor.saved.borrow();
        let tip = actor.ledger.tip().0;
        let flying = |txid: &[u8; 32]| actor.ledger.mined(txid).is_none_or(|at| at > tip);
        let mut tracked = Vec::new();
        if let Some((account, _)) = &self.own {
            let steps = saved.steps.iter().zip(&actor.errands);
            let mine = steps.filter(|(_, errand)| matches!(errand, Errand::Pay { .. }));
            let (mut in_flight, mut held) = (false, false);
            for (step, _) in mine {
                match step {
                    Step::Sent { txid, .. } => in_flight |= flying(txid),
                    Step::Held { .. } => held = true,
                    _ => {}
                }
            }
            let incoming = saved.steps.iter().any(|step| match step {
                Step::Sent { txid, account, .. } => account.is_some() && flying(txid),
                _ => false,
            });
            let facts = actor.ledger.facts();
            let afloat = saved.built.iter().any(|built| {
                let ours = match built.made {
                    Made::Payment { .. } => true,
                    Made::Sweep { home, .. } => home == Home::Own,
                };
                let signed = matches!(actor.errands[built.errand], Errand::Sign { .. });
                let known = actor.ledger.ordinal(&built.txid).map(|n| &facts[n]);
                ours && !signed
                    && known.is_some_and(|fact| fact.mined.is_none() && tip <= fact.expiry)
            });
            let busy = in_flight || incoming || afloat;
            tracked.push(("its own account".to_owned(), *account, OWN, busy, held));
        }
        for (index, step) in saved.steps.iter().enumerate() {
            let (account, in_flight) = match step {
                Step::Imported { account } => (*account, false),
                Step::Sent {
                    txid,
                    account: Some(account),
                    ..
                } => (*account, flying(txid)),
                _ => continue,
            };
            tracked.push((
                format!("joint {index}"),
                account,
                JOINTS + index,
                in_flight,
                false,
            ));
        }
        tracked
    }

    fn observe(&mut self, settled: bool) {
        let actor = self.actor.clone();
        let (tip, depth) = (actor.ledger.tip().0, actor.confirmations);
        let Ok(all) = self.wallet.funds_by_account() else {
            return actor.fail(3, "the wallet could not read its funds".into());
        };
        for (label, account, key, in_flight, held) in self.tracked() {
            let Ok(funds) = self.wallet.funds(account) else {
                actor.fail(3, format!("the wallet could not read the funds of {label}"));
                continue;
            };
            if all.get(&account).copied().unwrap_or_default() != funds {
                actor.fail(
                    4,
                    format!(
                        "the funds of {label} read two ways: {funds:?} alone, another among all"
                    ),
                );
            }
            actor.say(
                &label,
                format!("holds {}, {} to spend", funds.total, funds.spendable),
            );
            if !settled {
                continue;
            }
            let notes = actor.ledger.holdings(key, tip);
            let unspent = notes.iter().filter(|note| note.spent.is_none());
            let total: u64 = unspent.clone().map(|note| note.value).sum();
            let ripe = unspent.filter(|note| tip - note.mined + 1 >= depth);
            let spendable: u64 = ripe.map(|note| note.value).sum();
            if !in_flight && !actor.ledger.limbo(key) && funds.total != total {
                actor.fail(
                    5,
                    format!(
                        "block {}: the wallet says {label} holds {}, and the chain holds {total} for its key",
                        tip - actor.ledger.base(),
                        funds.total
                    ),
                );
            }
            let exact = !in_flight && !held;
            if funds.spendable > spendable || (exact && funds.spendable != spendable) {
                actor.fail(
                    6,
                    format!(
                        "block {}: the wallet says {label} may spend {}, and the chain holds {spendable} for its key {depth} deep",
                        tip - actor.ledger.base(),
                        funds.spendable
                    ),
                );
            }
        }
        if settled {
            self.claims(tip);
            self.fees(tip);
        }
    }

    fn claims(&mut self, tip: u32) {
        let actor = self.actor.clone();
        let depth = actor.confirmations;
        let built: Vec<[u8; 32]> = {
            let saved = actor.saved.borrow();
            let kept = saved.built.iter().filter(|built| {
                let gone = matches!(saved.steps[built.errand], Step::Done(_));
                let ours = !matches!(actor.errands[built.errand], Errand::Sign { .. });
                ours && (!gone || matches!(built.made, Made::Payment { .. }))
            });
            kept.map(|built| built.txid).collect()
        };
        let facts = actor.ledger.facts();
        for txid in built {
            let name = actor.name(&txid);
            let mined = actor.ledger.mined(&txid);
            let id = TxId::from_bytes(txid);
            let says = (self.wallet.is_mined(id), self.wallet.is_confirmed(id));
            let (Ok(says_mined), Ok(says_confirmed)) = says else {
                actor.fail(
                    7,
                    format!("the wallet could not say whether {name} is mined"),
                );
                continue;
            };
            let confirmed = mined.is_some_and(|at| tip - at >= depth - 1);
            if says_mined != mined.is_some() || says_confirmed != confirmed {
                actor.fail(
                    8,
                    format!(
                        "block {}: the wallet says {name} is mined: {says_mined}, confirmed: {says_confirmed}; the chain has it at {:?}, {depth} deep: {confirmed}",
                        tip - actor.ledger.base(),
                        mined.map(|at| at - actor.ledger.base())
                    ),
                );
            }
            let Some(ordinal) = actor.ledger.ordinal(&txid) else {
                continue;
            };
            let expiry = facts[ordinal].expiry;
            let expired = mined.is_none() && expiry != 0 && tip > expiry;
            if self.wallet.is_expired(id).ok() != Some(expired) {
                actor.fail(
                    9,
                    format!(
                        "block {}: the wallet says {name} is expired: {:?}; it expires after block {} and the chain has it at {:?}",
                        tip - actor.ledger.base(),
                        self.wallet.is_expired(id).ok(),
                        expiry - actor.ledger.base(),
                        mined.map(|at| at - actor.ledger.base())
                    ),
                );
            }
        }
    }

    fn fees(&mut self, tip: u32) {
        let actor = self.actor.clone();
        let Some((own, _)) = &self.own else {
            return;
        };
        let notes = actor.ledger.holdings(OWN, tip);
        for (index, errand) in actor.errands.iter().enumerate() {
            let Errand::Fee {
                index: at, value, ..
            } = errand
            else {
                continue;
            };
            let asked = [at - 1, *at, at + 1, *at];
            let truth = asked.map(|index| {
                let address = actor.keys.fee_address(index);
                let paid = notes.iter().filter(|note| note.to == address);
                paid.map(|note| note.value).sum::<u64>()
            });
            let Ok(seen) = self.wallet.received_at(*own, &asked) else {
                actor.fail(
                    10,
                    "the wallet could not read what its addresses received".into(),
                );
                continue;
            };
            if seen != truth {
                actor.fail(
                    10,
                    format!(
                        "block {}: the wallet says addresses {asked:?} received {seen:?}, and the chain holds {truth:?}",
                        tip - actor.ledger.base()
                    ),
                );
            }
            if seen[1] >= *value && matches!(actor.step(index), Step::Waiting) {
                actor
                    .ledger
                    .note(format!("the wallet saw the fee at address {at}"));
                actor.set(index, Step::Done(End::Seen(seen[1])));
            }
        }
    }

    async fn advance(&mut self, index: usize, settled: bool) {
        let actor = self.actor.clone();
        match actor.errands[index].clone() {
            Errand::Receive {
                at,
                chained,
                notes,
                sweep,
                patience,
            } => {
                let done = |step: &Step| matches!(step, Step::Done(_));
                let free = actor.saved.borrow().steps[..index].iter().all(done);
                let warm = actor.saved.borrow().earlier != Some(None);
                let free = free && !(warm && !actor.owns && actor.seasons > 0);
                if !chained || free {
                    self.receive(index, at, &notes, sweep, patience, settled)
                        .await;
                }
            }
            Errand::Pay { at, amount, ahead } => {
                let ahead = ahead.filter(|_| !actor.saved.borrow().plain[index]);
                self.pay(index, at, amount, ahead, settled).await;
            }
            Errand::Fee { .. } | Errand::Sign { .. } => {}
        }
    }

    async fn receive(
        &mut self,
        index: usize,
        at: u64,
        notes: &[(u64, u64)],
        sweep: Option<Home>,
        patience: u64,
        settled: bool,
    ) {
        let actor = self.actor.clone();
        let joint = &actor.keys.joints[index];
        let label = format!("joint {index}");
        match actor.step(index) {
            Step::Waiting if self.elapsed() >= at => {
                let name = format!("swap {index}");
                let imported = self
                    .wallet
                    .import_joint(&mut self.client, &joint.account, &name)
                    .await;
                let account = match imported {
                    Ok(account) => account,
                    Err(e) => return actor.say(&label, format!("not imported: {}", brief(&e))),
                };
                let again = self
                    .wallet
                    .import_joint(&mut self.client, &joint.account, &name)
                    .await;
                let other = &actor.keys.joints[(index + 1) % actor.keys.joints.len()];
                let wrong = actor.keys.joints.len() > 1
                    && self.wallet.tracks_joint(account, &other.account).ok() != Some(false);
                let right = self.wallet.tracks_joint(account, &joint.account).ok() == Some(true);
                if again.ok() != Some(account) || wrong || !right {
                    actor.fail(11, format!("the wallet does not know {label} by its key"));
                }
                actor.ledger.note(format!("the wallet imported {label}"));
                actor.birth();
                actor.saved.borrow_mut().imported[index] = Some(Instant::now());
                actor.set(index, Step::Imported { account });
                if let Err(e) = self.sync().await {
                    actor.say("the wallet", format!("no sync: {}", brief(&e)));
                }
            }
            Step::Imported { account } => {
                let Ok(funds) = self.wallet.funds(account) else {
                    return;
                };
                if actor.saved.borrow().tries[index] >= TRIES {
                    return;
                }
                let expected: u64 = notes.iter().map(|(_, value)| value).sum();
                let since = actor.saved.borrow().imported[index].expect("an import");
                let patient = since.elapsed().as_secs() < patience;
                if funds.total == 0 || funds.spendable != funds.total {
                    return;
                }
                if funds.total < expected && patient {
                    return;
                }
                let Some(home) = sweep else {
                    return self.forget(index, account, End::Watched(funds.total));
                };
                let to = match home {
                    Home::Stranger => unified(actor.keys.stranger_address(), actor.kind()),
                    Home::Own => {
                        let Some((own, _)) = &self.own else {
                            return;
                        };
                        match self.wallet.fresh_address(*own) {
                            Ok(address) => {
                                let mut saved = actor.saved.borrow_mut();
                                let shown = saved.own.as_ref().map(|(shown, _)| shown);
                                if shown == Some(&address) || saved.fresh.contains(&address) {
                                    actor.fail(
                                        22,
                                        format!(
                                            "the wallet named a used address to sweep {label} to"
                                        ),
                                    );
                                }
                                saved.fresh.push(address.clone());
                                address
                            }
                            Err(e) => {
                                return actor.say(&label, format!("no address: {}", brief(&e)));
                            }
                        }
                    }
                };
                let home_address = receiver(&to, actor.kind());
                let to: ZcashAddress = to.parse().expect("an address");
                let quoted = self.wallet.sweep_fee(account, &to).ok();
                match self.wallet.sweep(&PROVER, account, &joint.key, &to) {
                    Ok(txid) => {
                        let txid: [u8; 32] = *txid.as_ref();
                        actor.saved.borrow_mut().built.push(Built {
                            errand: index,
                            txid,
                            made: Made::Sweep {
                                home,
                                held: funds.total,
                            },
                            tip: actor.ledger.tip().0,
                            settled,
                            quoted,
                            home: Some(home_address),
                        });
                        actor.set(
                            index,
                            Step::Sent {
                                txid,
                                account: Some(account),
                                value: funds.total,
                                out: false,
                            },
                        );
                        actor.ledger.note(format!("the wallet swept {label}"));
                        self.send(index).await;
                    }
                    Err(Error::SweepFee { fee, ceiling }) => {
                        actor.ledger.note(format!(
                            "the wallet refused to sweep {label} for a fee of {fee}"
                        ));
                        let after = self.wallet.funds(account).ok();
                        actor
                            .saved
                            .borrow_mut()
                            .refused
                            .push((index, fee, quoted.unwrap_or(0)));
                        if after != Some(funds) || fee <= ceiling {
                            actor.fail(
                                12,
                                format!("a sweep of {label} refused for its fee left {after:?} of {funds:?}"),
                            );
                        }
                        actor.set(index, Step::Done(End::Refused(fee)));
                    }
                    Err(e) => {
                        actor.say(&label, format!("not swept: {}", brief(&e)));
                        if quoted.is_some_and(|fee| fee > SWEEP_FEE_CEILING) {
                            actor.fail(
                                12,
                                format!(
                                    "a sweep of {label} for a fee of {quoted:?} was refused, and not for its fee: {}",
                                    brief(&e)
                                ),
                            );
                            actor.set(index, Step::Done(End::Refused(quoted.unwrap_or(0))));
                        }
                        self.tried(index);
                    }
                }
            }
            Step::Sent { .. } => self.sent(index).await,
            _ => {}
        }
    }

    fn tried(&self, index: usize) {
        self.actor.saved.borrow_mut().tries[index] += 1;
    }

    fn forget(&mut self, index: usize, account: AccountUuid, end: End) {
        let actor = self.actor.clone();
        let before = self.wallet.accounts().map(|accounts| accounts.len()).ok();
        if let Err(e) = self.wallet.forget(account) {
            return actor.say(
                &format!("joint {index}"),
                format!("not forgotten: {}", brief(&e)),
            );
        }
        let after = self.wallet.accounts().map(|accounts| accounts.len()).ok();
        let emptied = self.wallet.funds(account).ok() == Some(Funds::default());
        if !emptied || before.map(|before| before - 1) != after {
            actor.fail(
                13,
                format!("the wallet still knows joint {index} once forgotten"),
            );
        }
        actor
            .ledger
            .note(format!("the wallet forgot joint {index}"));
        actor.set(index, Step::Done(end));
    }

    async fn send(&mut self, index: usize) {
        let actor = self.actor.clone();
        let Step::Sent {
            txid,
            account,
            value,
            ..
        } = actor.step(index)
        else {
            return;
        };
        self.tried.insert(index, self.polls);
        let faults = actor.server.faults();
        let sent = self
            .wallet
            .broadcast(&mut self.client, TxId::from_bytes(txid))
            .await;
        let held = actor.ledger.holds(&txid).is_some();
        let clean = faults == actor.server.faults();
        let name = actor.name(&txid);
        match &sent {
            Ok(()) if !held => actor.fail(
                14,
                format!("the wallet took {name} for broadcast, and the node does not hold it"),
            ),
            Err(Error::Rejected { .. }) if held && clean => actor.fail(
                15,
                format!("the wallet called {name} refused, and the node holds it"),
            ),
            _ => {}
        }
        if let Err(e) = &sent {
            actor.say(
                &format!("errand {index}"),
                format!("{name} not out: {}", brief(e)),
            );
        }
        actor.set(
            index,
            Step::Sent {
                txid,
                account,
                value,
                out: sent.is_ok(),
            },
        );
    }

    async fn sent(&mut self, index: usize) {
        let actor = self.actor.clone();
        let Step::Sent {
            txid,
            account,
            value,
            out,
        } = actor.step(index)
        else {
            return;
        };
        let id = TxId::from_bytes(txid);
        let name = actor.name(&txid);
        if self.wallet.is_mined(id).ok() == Some(true) {
            actor.ledger.note(format!("the wallet sees {name} mined"));
            return match account {
                Some(account) => self.forget(index, account, End::Swept(value)),
                None => actor.set(index, Step::Done(End::Paid(value))),
            };
        }
        if self.wallet.is_expired(id).ok() == Some(true) {
            actor
                .ledger
                .note(format!("the wallet sees {name} expired unmined"));
            actor.saved.borrow_mut().plain[index] = true;
            return match account {
                Some(account) => actor.set(index, Step::Imported { account }),
                None => actor.set(index, Step::Waiting),
            };
        }
        let waited = self.polls - self.tried.get(&index).copied().unwrap_or(0);
        if !out || self.fresh || waited >= AGAIN {
            self.send(index).await;
        }
    }

    async fn pay(
        &mut self,
        index: usize,
        at: u64,
        amount: u64,
        ahead: Option<Ahead>,
        settled: bool,
    ) {
        let actor = self.actor.clone();
        let label = format!("payment {index}");
        let Some((own, usk)) = self.own.clone() else {
            return;
        };
        let to = unified(actor.keys.stranger_address(), actor.kind());
        let payments: [(ZcashAddress, u64); 1] = [(to.parse().expect("an address"), amount)];
        let holder = [index as u8 + 1; 32];
        let tip = actor.ledger.tip().0;
        match actor.step(index) {
            Step::Waiting if self.elapsed() >= at => {
                let Ok(funds) = self.wallet.funds(own) else {
                    return;
                };
                if funds.spendable < amount && funds.spendable != funds.total {
                    return;
                }
                if actor.saved.borrow().tries[index] >= TRIES {
                    return;
                }
                let Some(ahead) = ahead else {
                    return match self.wallet.pay(&PROVER, own, &usk, &payments) {
                        Ok(txid) => self.built(index, txid, amount, settled).await,
                        Err(e) => {
                            actor.say(&label, format!("not built: {}", brief(&e)));
                            self.tried(index);
                        }
                    };
                };
                let proved = self
                    .wallet
                    .prove_payment(&PROVER, own, holder, &payments, ahead.hold);
                match proved {
                    Ok(payment) => {
                        let after = self.wallet.funds(own).map(|funds| funds.spendable).ok();
                        if after.is_none_or(|after| after + amount > funds.spendable) {
                            actor.fail(
                                16,
                                format!(
                                    "a payment of {amount} proved ahead left {after:?} of {} to spend",
                                    funds.spendable
                                ),
                            );
                        }
                        self.proved.insert(index, payment);
                        if let (true, Some(after)) = (settled, after) {
                            let locked = self.ripe().checked_sub(after);
                            actor.saved.borrow_mut().locked = locked;
                        }
                        actor
                            .ledger
                            .note(format!("the wallet proved {label} ahead"));
                        actor.set(index, Step::Held { since: tip });
                    }
                    Err(e) => {
                        actor.say(&label, format!("not proved: {}", brief(&e)));
                        self.tried(index);
                    }
                }
            }
            Step::Held { since } => {
                let Some(ahead) = ahead else {
                    return actor.set(index, Step::Waiting);
                };
                let blocks = tip.saturating_sub(since);
                let killed = actor.saved.borrow().crashed;
                if let (Some(rest), false) = (ahead.crash, killed) {
                    actor.saved.borrow_mut().crashed = true;
                    return self.reborn(rest).await;
                }
                let payment = match self.proved.remove(&index) {
                    Some(payment) => payment,
                    None => {
                        let again = self
                            .wallet
                            .prove_payment(&PROVER, own, holder, &payments, ahead.hold);
                        match again {
                            Ok(payment) => {
                                actor
                                    .ledger
                                    .note(format!("the wallet proved {label} again"));
                                let free = self.wallet.funds(own).map(|funds| funds.spendable);
                                let now = free.ok().and_then(|free| self.ripe().checked_sub(free));
                                let before = actor.saved.borrow().locked;
                                let more =
                                    before.zip(now).is_some_and(|(before, now)| now > before);
                                if settled && blocks < ahead.hold && more {
                                    actor.fail(
                                        17,
                                        format!(
                                            "{label}, lost in a restart, was proved again from other notes than it held: {now:?} are held where {before:?} were"
                                        ),
                                    );
                                }
                                actor.set(index, Step::Held { since: tip });
                                self.proved.insert(index, payment);
                                return;
                            }
                            Err(e) => {
                                if settled && blocks < ahead.hold && self.affords(amount) {
                                    actor.fail(
                                        17,
                                        format!(
                                            "{label}, lost in a restart {blocks} blocks into its hold of {}, could not be proved again: {}",
                                            ahead.hold,
                                            brief(&e)
                                        ),
                                    );
                                }
                                return actor
                                    .say(&label, format!("not proved again: {}", brief(&e)));
                            }
                        }
                    }
                };
                let due = match ahead.then {
                    Then::Send(after) | Then::Release(after) | Then::WrongKey(after) => {
                        blocks >= after
                    }
                    Then::Late => blocks > ahead.hold + 1 && settled,
                };
                if !due {
                    self.proved.insert(index, payment);
                    return;
                }
                let holds = self.wallet.holds(&payment).ok();
                match ahead.then {
                    Then::Release(_) => {
                        let released = self.wallet.release(payment);
                        actor.ledger.note(format!("the wallet released {label}"));
                        self.freed(index, &label, released.is_ok(), settled);
                    }
                    Then::WrongKey(_) => {
                        let other = UnifiedSpendingKey::from_seed(
                            &actor.network,
                            &actor.keys.other,
                            zip32::AccountId::ZERO,
                        )
                        .expect("another key");
                        let sent = self.wallet.send_proved(payment, &other);
                        actor
                            .ledger
                            .note(format!("the wallet signed {label} with another key"));
                        self.freed(index, &label, sent.is_err(), settled);
                    }
                    Then::Send(_) | Then::Late => match self.wallet.send_proved(payment, &usk) {
                        Ok(txid) => {
                            if holds != Some(true) || ahead.then == Then::Late {
                                actor.fail(
                                        18,
                                        format!(
                                            "the wallet sent {label} {blocks} blocks into its hold of {}, holding it: {holds:?}",
                                            ahead.hold
                                        ),
                                    );
                            }
                            self.built(index, txid, amount, settled).await;
                        }
                        Err(e) => {
                            actor
                                .ledger
                                .note(format!("the wallet would not send {label}: {}", brief(&e)));
                            self.freed(index, &label, true, settled);
                        }
                    },
                }
            }
            Step::Sent { .. } => self.sent(index).await,
            _ => {}
        }
    }

    async fn reborn(&mut self, rest: u64) {
        let actor = self.actor.clone();
        actor
            .ledger
            .note(format!("the wallet crashed, away {rest}s"));
        self.proved.clear();
        sleep(Duration::from_secs(rest)).await;
        match actor.open() {
            Ok(wallet) => self.wallet = wallet,
            Err(e) => actor.fail(1, format!("the wallet would not open: {}", brief(&e))),
        }
        self.client = actor.server.connect();
        (self.own, self.fresh) = (None, true);
        actor.ledger.note("the wallet is back".into());
    }

    fn ripe(&self) -> u64 {
        let actor = &self.actor;
        let tip = actor.ledger.tip().0;
        let notes = actor.ledger.holdings(OWN, tip);
        let ripe = notes
            .iter()
            .filter(|note| note.spent.is_none() && tip - note.mined + 1 >= actor.confirmations);
        ripe.map(|note| note.value).sum()
    }

    fn affords(&self, amount: u64) -> bool {
        self.ripe() >= amount + 100_000
    }

    fn freed(&mut self, index: usize, label: &str, gave_back: bool, settled: bool) {
        let actor = self.actor.clone();
        let Some((own, _)) = &self.own else {
            return;
        };
        let busy = self.tracked().first().is_some_and(|own| own.3);
        let others = actor
            .saved
            .borrow()
            .steps
            .iter()
            .enumerate()
            .any(|(other, step)| other != index && matches!(step, Step::Held { .. }));
        actor.saved.borrow_mut().plain[index] = true;
        actor.set(index, Step::Waiting);
        if !gave_back {
            return actor.fail(19, format!("{label} was not given up as asked"));
        }
        if !settled || busy || others {
            return;
        }
        let tip = actor.ledger.tip().0;
        let notes = actor.ledger.holdings(OWN, tip);
        let ripe = notes
            .iter()
            .filter(|note| note.spent.is_none() && tip - note.mined + 1 >= actor.confirmations);
        let truth: u64 = ripe.map(|note| note.value).sum();
        let free = self.wallet.funds(*own).map(|funds| funds.spendable).ok();
        if free != Some(truth) {
            actor.fail(
                20,
                format!("{label}, given up, left {free:?} to spend of the {truth} the chain holds"),
            );
        }
    }

    async fn built(&mut self, index: usize, txid: TxId, amount: u64, settled: bool) {
        let actor = self.actor.clone();
        let txid: [u8; 32] = *txid.as_ref();
        actor.saved.borrow_mut().built.push(Built {
            errand: index,
            txid,
            made: Made::Payment { amount },
            tip: actor.ledger.tip().0,
            settled,
            quoted: None,
            home: None,
        });
        actor.set(
            index,
            Step::Sent {
                txid,
                account: None,
                value: amount,
                out: false,
            },
        );
        actor
            .ledger
            .note(format!("the wallet built payment {index}"));
        self.send(index).await;
    }
}
