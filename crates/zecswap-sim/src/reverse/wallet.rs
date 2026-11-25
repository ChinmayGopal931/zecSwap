use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::Result;
use zecswap_api::Network;
use zecswap_api::reverse::Funding;
use zecswap_chain::evm::railgun::IRelayAdapt;
use zecswap_chain::evm::{Settlement, wall_now};
use zecswap_client::reverse::{ReverseSwap, ReverseUser};
use zecswap_client::{MakerApi, RelayerApi, Wire};
use zecswap_core::{JointAccount, NetworkType, derive_user_keys};

use crate::seed::Seed;
use crate::wallet::refused;
use crate::world::{CHAIN_ID, Chain};
use crate::zcash::Zcash;

pub const SPENDABLE_CONFIRMATIONS: u64 = 10;
pub const READY_LIFE: u64 = 120;
const POLL: Duration = Duration::from_secs(15);
const FUNDING_GRACE: u64 = 5 * 60;
const REFUND_LEAD: u64 = 60;
pub const OPEN: u8 = 1;
pub const READY: u8 = 2;
pub const CLAIMED: u8 = 3;
pub const REFUNDED: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Received(u64),
    Refunded,
    Abandoned(String),
}

#[derive(Clone, Default)]
pub struct Saved {
    pub next_index: u32,
    pub swap: Option<ReverseSwap>,
    pub accepted: bool,
    pub joint: Option<String>,
    pub funding: Option<Funding>,
    pub funded_at: Option<u64>,
    pub confirmed_at: Option<u64>,
    pub refund_wanted: bool,
    pub ended: Option<Ended>,
    pub runs: u32,
    pub lingered_ready: bool,
    pub lingered_refund: bool,
}

#[derive(Clone, Debug)]
pub struct Signed {
    pub at: u64,
    pub deadline: u64,
    pub spendable: u64,
    pub refunding: bool,
}

#[derive(Clone, Debug)]
pub struct Asked {
    pub at: u64,
    pub time: u64,
}

pub struct Wallet {
    pub seed: Seed,
    pub user_seed: [u8; 64],
    pub chain: Chain,
    pub zcash: Zcash,
    pub maker: Arc<dyn Wire>,
    pub relayer: Arc<dyn Wire>,
    pub units: u32,
    pub max_fee: u128,
    pub patience: Option<u64>,
    pub regret: Option<u64>,
    pub linger_ready: Option<i64>,
    pub linger_refund: Option<i64>,
    pub saved: Arc<Mutex<Saved>>,
    pub signed: Arc<Mutex<Vec<Signed>>>,
    pub asked: Arc<Mutex<Vec<Asked>>>,
    pub said: Arc<Mutex<String>>,
}

impl Wallet {
    pub fn saved(&self) -> Saved {
        self.saved.lock().unwrap().clone()
    }

    pub fn signed(&self) -> Vec<Signed> {
        self.signed.lock().unwrap().clone()
    }

    pub fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    fn save(&self, change: impl FnOnce(&mut Saved)) {
        change(&mut self.saved.lock().unwrap());
    }

    fn end(&self, end: Ended) {
        self.chain.node.note(format!("wallet ended: {end:?}"));
        self.save(|saved| saved.ended = Some(end));
    }

    fn say(&self, what: String) {
        let mut said = self.said.lock().unwrap();
        if *said != what {
            self.chain.node.note(what.clone());
            *said = what;
        }
    }

    fn relayer(&self) -> RelayerApi {
        RelayerApi::over("http://relayer.sim", self.relayer.clone())
    }

    fn user(&self, run: u32) -> ReverseUser {
        let name = format!("wallet-{run}");
        ReverseUser::new(
            &self.user_seed,
            &self.user_seed,
            Network::Testnet,
            Settlement::over(self.chain.node.rpc(&name), self.chain.contract, None),
            MakerApi::over("http://maker.sim", self.maker.clone()),
            self.relayer(),
            self.chain.token,
            self.max_fee,
        )
    }

    pub async fn live(self: Arc<Self>) {
        loop {
            let saved = self.saved();
            if saved.ended.is_some() {
                return;
            }
            self.save(|saved| saved.runs += 1);
            if let Err(e) = self.run(saved.runs).await {
                self.chain.node.note(format!("wallet stopped: {e:#}"));
            }
            let rest = self
                .seed
                .between("reverse-wallet-rest", u64::from(saved.runs), 20, 240);
            tokio::time::sleep(Duration::from_secs(rest)).await;
        }
    }

    async fn run(&self, run: u32) -> Result<()> {
        let user = self.user(run);
        let Some(mut swap) = self.open(&user).await? else {
            return Ok(());
        };
        if !self.fund(&user, &swap).await? {
            return Ok(());
        }
        loop {
            if self.step(&user, &mut swap).await? {
                return Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn open(&self, user: &ReverseUser) -> Result<Option<ReverseSwap>> {
        let saved = self.saved();
        let mut swap = match saved.swap {
            Some(swap) => swap,
            None => {
                let index = saved.next_index;
                self.save(|saved| saved.next_index = index + 1);
                let swap = user.quote(index, self.units).await?;
                self.save(|saved| saved.swap = Some(swap.clone()));
                swap
            }
        };
        if !saved.accepted {
            if let Err(e) = user.accept(&mut swap).await {
                if refused(&e) || wall_now() >= swap.quote.funding_deadline {
                    self.end(Ended::Abandoned(format!("{e:#}")));
                    return Ok(None);
                }
                return Err(e);
            }
            self.save(|saved| {
                saved.swap = Some(swap.clone());
                saved.accepted = true;
            });
        }
        if saved.joint.is_none() {
            let keys = derive_user_keys(&self.user_seed, NetworkType::Test, 0, swap.index)?;
            let joint = JointAccount::derive(
                &swap.quote.terms.maker_share,
                &keys.share.public(),
                &keys.viewing,
            )?
            .unified_address(NetworkType::Test);
            self.save(|saved| saved.joint = Some(joint));
        }
        Ok(Some(swap))
    }

    async fn fund(&self, user: &ReverseUser, swap: &ReverseSwap) -> Result<bool> {
        loop {
            if self.chain.recorded(swap.swap_id).stage != 0 {
                let now = wall_now();
                self.save(|saved| {
                    saved.funded_at.get_or_insert(now);
                });
                return Ok(true);
            }
            if wall_now() >= swap.quote.funding_deadline + FUNDING_GRACE {
                self.end(Ended::Abandoned("the escrow was never funded".into()));
                return Ok(false);
            }
            let funding = match self.saved().funding {
                Some(funding) => funding,
                None => {
                    let funding = relayed(swap.swap_id, user.funding_calls(swap).await?);
                    self.save(|saved| saved.funding = Some(funding.clone()));
                    funding
                }
            };
            if let Err(e) = self
                .relayer()
                .fund_reverse(&funding, swap.ticket.as_deref())
                .await
            {
                self.say(format!("the funding was not taken: {e:#}"));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn step(&self, user: &ReverseUser, swap: &mut ReverseSwap) -> Result<bool> {
        let recorded = self.chain.recorded(swap.swap_id);
        let now = self.chain.node.head().1;
        let (t0, t1) = (swap.quote.ready_deadline, swap.quote.refund_after);
        match recorded.stage {
            CLAIMED => {
                user.receive_key(swap).await?;
                let joint = self
                    .saved()
                    .joint
                    .expect("the joint account is saved first");
                self.end(Ended::Received(self.zcash.sweep(&joint)));
                return Ok(true);
            }
            REFUNDED if recorded.paid_out => {
                self.end(Ended::Refunded);
                return Ok(true);
            }
            REFUNDED => self.refund(user, swap).await,
            READY if now + REFUND_LEAD >= t1 => {
                if let (false, Some(shift)) = (self.saved().lingered_refund, self.linger_refund) {
                    self.linger(t1 as i64 + shift).await;
                    self.save(|saved| saved.lingered_refund = true);
                }
                self.refund(user, swap).await;
            }
            OPEN => {
                if now < t0 {
                    self.ready(user, swap).await;
                }
                if self.wants_refund(swap, now) {
                    self.refund(user, swap).await;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn spendable(&self) -> u64 {
        let joint = self
            .saved()
            .joint
            .expect("the joint account is saved first");
        self.zcash.received(&joint, SPENDABLE_CONFIRMATIONS)
    }

    async fn ready(&self, user: &ReverseUser, swap: &mut ReverseSwap) {
        let saved = self.saved();
        if let (false, Some(shift)) = (saved.lingered_ready, self.linger_ready)
            && self.spendable() >= swap.quote.terms.deposit_zat
        {
            self.linger(swap.quote.ready_deadline as i64 - READY_LIFE as i64 + shift)
                .await;
            self.save(|saved| saved.lingered_ready = true);
        }
        let spendable = self.spendable();
        if spendable >= swap.quote.terms.deposit_zat {
            let now = wall_now();
            self.save(|saved| {
                saved.confirmed_at.get_or_insert(now);
            });
        }
        let asked = !self.asked.lock().unwrap().is_empty();
        let readied = user
            .ready_with_spendable(swap, spendable, |signed| {
                self.signed.lock().unwrap().push(Signed {
                    at: self.chain.node.head().0,
                    deadline: signed.ready_signed_until.unwrap_or_default(),
                    spendable,
                    refunding: signed.refunding || asked,
                });
                self.save(|saved| saved.swap = Some(signed.clone()));
                Ok(())
            })
            .await;
        if let Err(e) = readied {
            self.say(format!("no ready: {e:#}"));
        }
    }

    fn wants_refund(&self, swap: &ReverseSwap, now: u64) -> bool {
        let saved = self.saved();
        let wall = wall_now();
        let after = |wait: Option<u64>, from: Option<u64>| {
            wait.zip(from)
                .is_some_and(|(wait, from)| wall >= from + wait)
        };
        let wanted = saved.refund_wanted
            || now + READY_LIFE >= swap.quote.ready_deadline
            || after(self.patience, saved.funded_at)
            || after(self.regret, saved.confirmed_at);
        if wanted && !saved.refund_wanted {
            self.chain
                .node
                .note("the wallet wants its USDC back".into());
            self.save(|saved| saved.refund_wanted = true);
        }
        wanted
    }

    async fn refund(&self, user: &ReverseUser, swap: &mut ReverseSwap) {
        let refunded = user
            .refund(swap, |asked| {
                let (at, time) = self.chain.node.head();
                self.asked.lock().unwrap().push(Asked { at, time });
                self.save(|saved| saved.swap = Some(asked.clone()));
                Ok(())
            })
            .await;
        if let Err(e) = refunded {
            self.say(format!("no refund: {e:#}"));
        }
    }

    async fn linger(&self, until: i64) {
        let left = u64::try_from(until - wall_now() as i64).unwrap_or(0);
        self.chain.node.note(format!("the wallet lingers {left}s"));
        tokio::time::sleep(Duration::from_secs(left)).await;
    }
}

fn relayed(swap_id: B256, calls: [(Address, Vec<u8>); 2]) -> Funding {
    let calls = calls
        .into_iter()
        .map(|(to, data)| IRelayAdapt::Call {
            to,
            data: data.into(),
            value: U256::ZERO,
        })
        .collect();
    let relay = IRelayAdapt::relayCall {
        _transactions: Vec::new(),
        _actionData: IRelayAdapt::ActionData {
            random: Default::default(),
            requireSuccess: true,
            minGasLimit: U256::ZERO,
            calls,
        },
    };
    Funding {
        swap_id,
        chain_id: CHAIN_ID,
        to: Network::Testnet
            .relay_adapt(CHAIN_ID)
            .expect("Sepolia has a Relay Adapt"),
        data: relay.abi_encode().into(),
        value: 0,
    }
}
