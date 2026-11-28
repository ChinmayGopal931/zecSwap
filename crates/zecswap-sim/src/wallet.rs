use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use zecswap_api::Network;
use zecswap_chain::evm::judge::READY_DEPTH;
use zecswap_chain::evm::{Settlement, Stage, wall_now};
use zecswap_client::{
    FeeNotHonoured, MakerApi, Quoted, RelayerApi, Route, ServiceError, User, UserSwap, Wire,
};
use zecswap_core::NetworkType;

use crate::alerts::Alarms;
use crate::seed::Seed;
use crate::world::Chain;
use crate::zcash::{PaymentId, Zcash};

const CHAIN_POLL: Duration = Duration::from_secs(15);
const FEE_POLL: Duration = Duration::from_secs(5);
const FEE_RESEND: u32 = 6;
const PAYMENT_FEE: u64 = 10_000;
const DEPOSIT_PATIENCE: u32 = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    Paid(u128),
    SweptBack(u64),
    Cancelled,
    Abandoned(String),
}

#[derive(Clone, Default)]
pub struct Saved {
    pub next_index: u32,
    pub quoted: Option<Quoted>,
    pub fee: Option<PaymentId>,
    pub swap: Option<UserSwap>,
    pub joint: Option<String>,
    pub deposit: Option<PaymentId>,
    pub deposited_at: Option<u64>,
    pub deposited_shallow: bool,
    pub ended: Option<End>,
    pub runs: u32,
    pub lingered: bool,
}

#[derive(Clone)]
pub enum Way {
    Account {
        key: PrivateKeySigner,
        private: bool,
    },
    Railgun {
        max_fee: u128,
    },
}

pub struct Wallet {
    pub seed: Seed,
    pub user_seed: [u8; 64],
    pub chain: Chain,
    pub zcash: Zcash,
    pub maker: Arc<dyn Wire>,
    pub relayer: Arc<dyn Wire>,
    pub way: Way,
    pub units: u32,
    pub max_open_fee: u64,
    pub funds: u64,
    pub linger_verify: Option<i64>,
    pub linger_claim: Option<i64>,
    pub saved: Arc<Mutex<Saved>>,
    pub(crate) alarms: Arc<Alarms>,
}

pub(crate) fn refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServiceError>()
        .is_some_and(ServiceError::refused)
}

impl Wallet {
    pub fn saved(&self) -> Saved {
        self.saved.lock().unwrap().clone()
    }

    fn save(&self, change: impl FnOnce(&mut Saved)) {
        change(&mut self.saved.lock().unwrap());
    }

    fn end(&self, end: End) {
        self.chain.node.note(format!("wallet ended: {end:?}"));
        self.save(|saved| saved.ended = Some(end));
    }

    fn user(&self, run: u32) -> User {
        let node = &self.chain.node;
        let name = format!("wallet-{run}");
        let maker = MakerApi::over("http://maker.sim", self.maker.clone());
        let (settlement, route) = match &self.way {
            Way::Account { key, private } => {
                let mut settlement =
                    Settlement::over(node.rpc(&name), self.chain.contract, Some(key.clone()));
                settlement.alerts_to(self.alarms.sink(&name, key));
                if *private {
                    settlement.broadcast_privately_over(node.private_rpc(&name, 25), 25);
                }
                (settlement, Route::Account)
            }
            Way::Railgun { max_fee } => (
                Settlement::over(node.rpc(&name), self.chain.contract, None),
                Route::Railgun {
                    relayer: RelayerApi::over("http://relayer.sim", self.relayer.clone()),
                    max_fee: *max_fee,
                },
            ),
        };
        User::new(
            &self.user_seed,
            &self.user_seed,
            Network::Testnet,
            settlement,
            maker,
            self.chain.token,
            route,
        )
        .with_max_open_fee(self.max_open_fee)
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
                .between("wallet-rest", u64::from(saved.runs), 20, 240);
            tokio::time::sleep(Duration::from_secs(rest)).await;
        }
    }

    async fn run(&self, run: u32) -> Result<()> {
        let user = self.user(run);
        let resumed = self.saved().swap.is_some();
        let Some(swap) = self.open(&user).await? else {
            return Ok(());
        };
        if let (false, Some(shift)) = (resumed, self.linger_verify) {
            let until = swap.t0 as i64 - zecswap_client::MIN_TIME_TO_T0 as i64 + shift;
            self.linger(until).await;
        }
        let saved = self.saved();
        match saved.deposit {
            None => {
                if resumed && user.state(&swap).await?.stage == Stage::Refunded {
                    self.end(End::Cancelled);
                    return Ok(());
                }
                let joint = user.verify(&swap).await?;
                let address = joint.unified_address(NetworkType::Test);
                let id = self.zcash.build(&address, swap.quote.deposit_zat);
                let (block, head) = self.chain.node.head();
                let below = block.saturating_sub(READY_DEPTH);
                let shallow = self.chain.recorded_at(swap.swap_id, below).stage == 0;
                self.save(|saved| {
                    saved.joint = Some(address);
                    saved.deposit = Some(id);
                    saved.deposited_at = Some(head);
                    saved.deposited_shallow = shallow;
                });
                self.zcash.broadcast(id);
                self.chain.node.note("wallet deposited".into());
            }
            Some(id) if self.zcash.confirmations(id) == 0 => self.zcash.broadcast(id),
            Some(_) => {}
        }
        loop {
            let chain = user.state(&swap).await?;
            let now = user.settlement().now().await?;
            match chain.stage {
                Stage::Ready | Stage::Claimed => return self.claim(&user, &swap).await,
                Stage::Open if now >= chain.t0 => return self.claim(&user, &swap).await,
                Stage::Refunded => return self.sweep_back(&user, &swap).await,
                Stage::Open => {}
            }
            tokio::time::sleep(CHAIN_POLL).await;
        }
    }

    async fn open(&self, user: &User) -> Result<Option<UserSwap>> {
        let saved = self.saved();
        if let Some(swap) = saved.swap {
            return Ok(Some(swap));
        }
        let quoted = match saved.quoted {
            Some(quoted) => quoted,
            None => {
                let index = saved.next_index;
                self.save(|saved| saved.next_index = index + 1);
                let quoted = user.quote(index, self.units).await?;
                self.save(|saved| saved.quoted = Some(quoted.clone()));
                quoted
            }
        };
        let give_up = |why: String| {
            self.end(End::Abandoned(why));
            Ok(None)
        };
        let resumed = saved.fee.is_some();
        let fee = match saved.fee {
            Some(id) => Some(id),
            None => match quoted.fee_due(wall_now()) {
                Ok(None) => None,
                Ok(Some(due)) => {
                    let needed = due
                        .zat
                        .saturating_add(quoted.quote.deposit_zat)
                        .saturating_add(2 * PAYMENT_FEE);
                    if self.funds < needed {
                        return give_up("the wallet is short: nothing was paid".into());
                    }
                    let id = self.zcash.build(&due.address, due.zat);
                    self.save(|saved| saved.fee = Some(id));
                    Some(id)
                }
                Err(e) => return give_up(format!("{e:#}: nothing was paid")),
            },
        };
        if let Some(id) = fee {
            let mut waited = None;
            while self.zcash.confirmations(id) == 0 {
                if wall_now() >= quoted.quote.expires_at {
                    return give_up("the quote expired with its open fee unmined".into());
                }
                if waited.is_none_or(|polls| polls >= FEE_RESEND) {
                    match quoted.fee_due(wall_now()) {
                        Ok(_) => self.zcash.broadcast(id),
                        Err(_) if waited.is_some() || resumed => {}
                        Err(e) => return give_up(format!("{e:#}: nothing was paid")),
                    }
                    waited = Some(0);
                }
                waited = waited.map(|polls| polls + 1);
                tokio::time::sleep(FEE_POLL).await;
            }
        }
        let swap = match user.accept(&quoted).await {
            Ok(swap) => swap,
            Err(e) if e.is::<FeeNotHonoured>() || refused(&e) => {
                return give_up(format!("{e:#}"));
            }
            Err(e) => return Err(e),
        };
        self.save(|saved| saved.swap = Some(swap.clone()));
        Ok(Some(swap))
    }

    async fn linger(&self, until: i64) {
        let left = u64::try_from(until - wall_now() as i64).unwrap_or(0);
        self.chain.node.note(format!("the wallet lingers {left}s"));
        tokio::time::sleep(Duration::from_secs(left)).await;
    }

    async fn claim(&self, user: &User, swap: &UserSwap) -> Result<()> {
        if let (false, Some(early)) = (self.saved().lingered, self.linger_claim) {
            self.linger(swap.t1 as i64 - early).await;
            self.save(|saved| saved.lingered = true);
        }
        let paid = user.claim(swap).await?;
        self.end(End::Paid(paid.amount));
        Ok(())
    }

    async fn sweep_back(&self, user: &User, swap: &UserSwap) -> Result<()> {
        let saved = self.saved();
        let (Some(joint), Some(deposit)) = (saved.joint, saved.deposit) else {
            self.end(End::Cancelled);
            return Ok(());
        };
        user.refund_key(swap).await?;
        let mut polls = 0;
        while self.zcash.confirmations(deposit) == 0 {
            polls += 1;
            if polls > DEPOSIT_PATIENCE {
                self.zcash.void(deposit);
                self.end(End::Cancelled);
                return Ok(());
            }
            tokio::time::sleep(CHAIN_POLL).await;
        }
        self.end(End::SweptBack(self.zcash.sweep(&joint)));
        Ok(())
    }
}
