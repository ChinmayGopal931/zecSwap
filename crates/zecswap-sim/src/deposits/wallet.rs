use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, keccak256};
use anyhow::{Result, ensure};
use tokio::time::Instant;
use zecswap_api::Network;
use zecswap_api::relayer::{Note, Rescue as PayoutRescue, RescueDeposit, ShieldDeposit};
use zecswap_chain::evm::deposits::Used;
use zecswap_chain::evm::{Settlement, wall_now};
use zecswap_client::{Deposit, DepositDeployment, Depositor, FeeCap, RelayerApi, Wire};
use zecswap_core::{Domain, NetworkType, RescueAuthorization, derive_user_keys};
use zecswap_railgun::ShieldNote;

use super::landings;
use super::payout::{Paid, SWAP};
use crate::world::{CHAIN_ID, Chain};

const POLL: Duration = Duration::from_secs(30);
pub const RESCAN: Duration = Duration::from_secs(15 * 60);
pub const PAYOUT: u32 = u32::MAX;
const PAYOUT_VALIDITY: u64 = 60 * 60;
const RELAYER: &str = "http://relayer.sim";

#[derive(Clone, Default)]
pub struct Saved {
    pub pending: BTreeMap<u32, ShieldDeposit>,
    pub deposited: BTreeSet<u32>,
    pub shown: u32,
    pub runs: u32,
}

#[derive(Clone, Debug)]
pub enum Request {
    Shield(ShieldDeposit),
    Rescue(RescueDeposit),
    Payout(Box<PayoutRescue>),
}

impl Request {
    pub fn shield_note(&self) -> ShieldNote {
        ShieldNote::from(match self {
            Self::Shield(shield) => &shield.note,
            Self::Rescue(rescue) => &rescue.note,
            Self::Payout(rescue) => &rescue.note,
        })
    }

    pub fn note(&self) -> B256 {
        self.shield_note().commitment().into()
    }

    pub fn fee(&self) -> u128 {
        match self {
            Self::Shield(shield) => shield.fee,
            Self::Rescue(rescue) => rescue.fee,
            Self::Payout(rescue) => rescue.fee,
        }
    }

    pub fn until(&self) -> u64 {
        match self {
            Self::Shield(shield) => shield.valid_before,
            Self::Rescue(rescue) => rescue.deadline,
            Self::Payout(rescue) => rescue.deadline,
        }
    }
}

pub fn commitment(note: &Note) -> B256 {
    ShieldNote::from(note).commitment().into()
}

#[derive(Clone, Debug)]
pub struct Kept {
    pub at: u64,
    pub wall: u64,
    pub block: u64,
    pub index: u32,
    pub key: B256,
    pub held: u128,
    pub request: Request,
}

#[derive(Clone, Debug)]
pub struct Believed {
    pub index: u32,
    pub tx: B256,
    pub note: B256,
    pub asked: Option<B256>,
    pub evidence: bool,
}

pub struct Wallet {
    pub user_seed: [u8; 64],
    pub chain: Chain,
    pub deposits: Address,
    pub vaults: Vec<Address>,
    pub relayer: Arc<dyn Wire>,
    pub cap: FeeCap,
    pub paid: Option<Paid>,
    pub max_fee: u128,
    pub http: reqwest::Client,
    pub start: Instant,
    pub saved: Arc<Mutex<Saved>>,
    pub kept: Arc<Mutex<Vec<Kept>>>,
    pub believed: Arc<Mutex<Vec<Believed>>>,
    pub said: Arc<Mutex<String>>,
}

impl Wallet {
    pub fn saved(&self) -> Saved {
        self.saved.lock().unwrap().clone()
    }

    pub fn kept(&self) -> Vec<Kept> {
        self.kept.lock().unwrap().clone()
    }

    pub fn believed(&self) -> Vec<Believed> {
        self.believed.lock().unwrap().clone()
    }

    fn relayer(&self) -> RelayerApi {
        RelayerApi::over(RELAYER, self.relayer.clone())
    }

    pub fn depositor(&self, name: &str) -> Depositor {
        Depositor::new(
            &self.user_seed,
            &self.user_seed,
            Network::Testnet,
            Settlement::over(self.chain.node.rpc(name), self.deposits, None),
            self.relayer(),
            DepositDeployment {
                chain_id: CHAIN_ID,
                contract: self.deposits,
                token: self.chain.token,
            },
            self.cap,
        )
    }

    fn save<T>(&self, change: impl FnOnce(&mut Saved) -> T) -> T {
        change(&mut self.saved.lock().unwrap())
    }

    fn say(&self, what: String) {
        let mut said = self.said.lock().unwrap();
        if *said != what {
            self.chain.node.note(what.clone());
            *said = what;
        }
    }

    fn keep(&self, index: u32, key: B256, request: Request) {
        let block = self.chain.node.head().0;
        let held = |back: u64| {
            let vault = self.vaults[index as usize];
            self.chain
                .token_balance_at(vault, block.saturating_sub(back))
        };
        let held = match request {
            Request::Rescue(_) => (0..=2).map(held).max().unwrap_or(0),
            _ => 0,
        };
        self.kept.lock().unwrap().push(Kept {
            at: self.start.elapsed().as_millis() as u64,
            wall: wall_now(),
            block,
            index,
            key,
            held,
            request,
        });
    }

    fn keep_shield(&self, index: u32) -> impl FnMut(&ShieldDeposit) -> Result<()> + '_ {
        move |request| {
            self.save(|saved| {
                saved.pending.insert(index, request.clone());
            });
            let key = keccak256(request.signature);
            self.keep(index, key, Request::Shield(request.clone()));
            Ok(())
        }
    }

    fn keep_rescue(&self, index: u32) -> impl FnMut(&RescueDeposit) -> Result<()> + '_ {
        move |request| {
            let key = keccak256(request.signature);
            self.keep(index, key, Request::Rescue(request.clone()));
            Ok(())
        }
    }

    fn believe(&self, depositor: &Depositor, index: u32, deposit: &Deposit, rescue: bool) {
        let asked = match rescue {
            true => {
                let kept = self.kept.lock().unwrap();
                let last = kept
                    .iter()
                    .rev()
                    .find(|kept| kept.index == index && matches!(kept.request, Request::Rescue(_)));
                last.map(|kept| kept.request.note())
            }
            false => {
                let pending = self.saved().pending;
                pending.get(&index).map(|request| commitment(&request.note))
            }
        };
        let from = depositor.address(index).expect("a deposit address");
        let evidence = landings(&self.chain.node, self.deposits)
            .iter()
            .any(|landing| landing.tx == deposit.tx && landing.from == from);
        self.chain.node.note(format!(
            "the wallet believes {} from #{index} is private in {}",
            deposit.shielded.value,
            &deposit.tx.to_string()[..10]
        ));
        self.believed.lock().unwrap().push(Believed {
            index,
            tx: deposit.tx,
            note: deposit.shielded.note.commitment().into(),
            asked,
            evidence,
        });
    }

    pub async fn live(self: Arc<Self>) {
        let run = self.save(|saved| {
            saved.runs += 1;
            saved.runs
        });
        let depositor = self.depositor(&format!("wallet-{run}"));
        let node = &self.chain.node;
        let swaps = Settlement::over(node.rpc(&format!("swaps-{run}")), self.chain.contract, None);
        self.recover(&depositor).await;
        let mut scanned = Instant::now();
        for poll in 1u64.. {
            tokio::time::sleep(POLL).await;
            self.resume(&depositor).await;
            if scanned.elapsed() >= RESCAN {
                self.recover(&depositor).await;
                scanned = Instant::now();
                continue;
            }
            let saved = self.saved();
            match depositor.scan(saved.shown, 1).await {
                Ok(used) if used[0].used() => self.tend(&depositor, saved.shown, used[0]).await,
                Ok(_) => {}
                Err(e) => self.say(format!("no look at #{}: {e:#}", saved.shown)),
            }
            if poll % 2 == 0 {
                for index in saved.deposited {
                    self.rescue(&depositor, index).await;
                }
                if let Err(e) = self.rescue_payout(&swaps).await {
                    self.say(format!("no rescue of the swap's payout: {e:#}"));
                }
            }
        }
    }

    async fn rescue_payout(&self, swaps: &Settlement) -> Result<()> {
        let Some(paid) = &self.paid else {
            return Ok(());
        };
        let terms = self.relayer().terms().await?;
        ensure!(
            (terms.chain_id, terms.contract) == (CHAIN_ID, self.chain.contract),
            "the relayer serves another deployment"
        );
        ensure!(
            terms.fee <= self.max_fee,
            "the relayer asks a fee of {}, over the {} this wallet pays",
            terms.fee,
            self.max_fee
        );
        if swaps.token_balance(self.chain.token, paid.vault).await? <= terms.fee {
            return Ok(());
        }
        let authorization = RescueAuthorization {
            nonce: swaps.rescue_nonce(paid.id).await?,
            deadline: wall_now() + PAYOUT_VALIDITY,
        };
        let keys = derive_user_keys(&self.user_seed, NetworkType::Test, 0, SWAP)?;
        let domain = Domain {
            chain_id: CHAIN_ID,
            contract: self.chain.contract.into(),
        };
        let digest = domain.rescue(
            &paid.id.0,
            &paid.note.commitment(),
            &terms.relayer.into(),
            terms.fee,
            authorization,
        );
        let request = PayoutRescue {
            swap_id: paid.id,
            terms: (&paid.terms).into(),
            note: (&paid.note).into(),
            fee: terms.fee,
            nonce: authorization.nonce,
            deadline: authorization.deadline,
            signature: keys.auth.sign(&digest).into(),
        };
        let key = keccak256(request.signature);
        self.keep(PAYOUT, key, Request::Payout(Box::new(request.clone())));
        let path = match paid.reverse {
            true => "/v1/reverse/rescue",
            false => "/v1/rescue",
        };
        let post = self.http.post(format!("{RELAYER}{path}")).json(&request);
        let answer = self.relayer.exchange(post.build()?).await?;
        let status = answer.status();
        ensure!(
            status.is_success(),
            "the relayer answered {status}: {}",
            answer.text().await?
        );
        Ok(())
    }

    async fn recover(&self, depositor: &Depositor) {
        match depositor.recover().await {
            Ok(used) => {
                for (index, used) in used {
                    self.tend(depositor, index, used).await;
                }
            }
            Err(e) => self.say(format!("no scan: {e:#}")),
        }
        self.show(depositor).await;
    }

    async fn show(&self, depositor: &Depositor) {
        match depositor.showing().await {
            Ok(shown) => self.save(|saved| saved.shown = shown),
            Err(e) => self.say(format!("no address to show: {e:#}")),
        }
    }

    async fn resume(&self, depositor: &Depositor) {
        for (index, request) in self.saved().pending {
            let value = request.value;
            let posting = depositor.post_shield(index, request, self.keep_shield(index));
            match posting.await {
                Ok(deposit) => self.shielded(depositor, index, &deposit).await,
                Err(e) => {
                    self.say(format!("no deposit from #{index}: {e:#}"));
                    let held = depositor.scan(index, 1).await;
                    if held.is_ok_and(|held| held[0].balance < value) {
                        self.save(|saved| saved.pending.remove(&index));
                    }
                }
            }
        }
    }

    async fn shielded(&self, depositor: &Depositor, index: u32, deposit: &Deposit) {
        self.believe(depositor, index, deposit, false);
        let shown = self.save(|saved| {
            saved.pending.remove(&index);
            saved.deposited.insert(index);
            saved.shown
        });
        if shown == index {
            self.show(depositor).await;
        }
    }

    async fn tend(&self, depositor: &Depositor, index: u32, used: Used) {
        if used.deposited {
            self.save(|saved| saved.deposited.insert(index));
        }
        if used.balance > 0 && !self.saved().pending.contains_key(&index) {
            match depositor.shield(index, self.keep_shield(index)).await {
                Ok(Some(deposit)) => self.shielded(depositor, index, &deposit).await,
                Ok(None) => {}
                Err(e) => self.say(format!("no deposit from #{index}: {e:#}")),
            }
        }
    }

    async fn rescue(&self, depositor: &Depositor, index: u32) {
        match depositor.rescue(index, self.keep_rescue(index)).await {
            Ok(Some(deposit)) => self.believe(depositor, index, &deposit, true),
            Ok(None) => {}
            Err(e) => self.say(format!("no rescue for #{index}: {e:#}")),
        }
    }
}
