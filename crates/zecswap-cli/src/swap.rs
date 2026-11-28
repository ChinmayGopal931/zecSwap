use std::time::Duration;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zcash_address::ZcashAddress;
use zcash_protocol::consensus::Parameters;
use zecswap_chain::evm::{PrivateKeySigner, Settlement, Stage, wall_now};
use zecswap_chain::zcash::{AccountUuid, TxId};
use zecswap_client::{
    FeeNotHonoured, MakerApi, Quoted, RelayerApi, Route, ServiceError, User, UserSwap,
};
use zecswap_core::JointAccount;

use crate::Session;
use crate::store::Store;

const STATE: &str = "swap.json";
const NEXT_INDEX: &str = "next-swap-index.json";
const CHAIN_POLL: Duration = Duration::from_secs(15);
const FEE_POLL: Duration = Duration::from_secs(5);
const FEE_RESEND: u32 = 6;
const PAYMENT_FEE: u64 = 10_000;

pub struct SwapArgs {
    pub maker: String,
    pub rpc: String,
    pub contract: Address,
    pub token: Address,
    pub units: u32,
    pub payee: Payee,
    pub max_open_fee: u64,
}

pub enum Payee {
    Account(PrivateKeySigner),
    Railgun { relayer: String, max_fee: u128 },
}

#[derive(Serialize, Deserialize)]
struct Run {
    progress: Progress,
    joint_account: Option<AccountUuid>,
    deposit: Option<[u8; 32]>,
    #[serde(default)]
    sweep: Option<[u8; 32]>,
    finished: bool,
}

#[derive(Serialize, Deserialize)]
enum Progress {
    Quoted {
        quoted: Quoted,
        fee: Option<[u8; 32]>,
    },
    Accepted(UserSwap),
}

impl Run {
    fn index(&self) -> u32 {
        match &self.progress {
            Progress::Quoted { quoted, .. } => quoted.index,
            Progress::Accepted(swap) => swap.index,
        }
    }
}

trait Opener {
    async fn quote(&self, index: u32, units: u32) -> Result<Quoted>;
    async fn accept(&self, quoted: &Quoted) -> Result<UserSwap>;
}

impl Opener for User {
    async fn quote(&self, index: u32, units: u32) -> Result<Quoted> {
        User::quote(self, index, units).await
    }

    async fn accept(&self, quoted: &Quoted) -> Result<UserSwap> {
        User::accept(self, quoted).await
    }
}

trait FeeWallet {
    fn store(&self) -> &Store;
    async fn holds(&mut self) -> Result<u64>;
    async fn prepare(&mut self, to: &ZcashAddress, zatoshis: u64) -> Result<TxId>;
    async fn send(&mut self, txid: TxId) -> Result<()>;
    async fn mined(&mut self, txid: TxId) -> Result<bool>;
}

impl FeeWallet for Session {
    fn store(&self) -> &Store {
        &self.store
    }

    async fn holds(&mut self) -> Result<u64> {
        self.sync().await?;
        Ok(self.wallet.funds(self.account()?)?.total)
    }

    async fn prepare(&mut self, to: &ZcashAddress, zatoshis: u64) -> Result<TxId> {
        self.sync().await?;
        self.pay(to, zatoshis)
    }

    async fn send(&mut self, txid: TxId) -> Result<()> {
        self.broadcast(txid).await
    }

    async fn mined(&mut self, txid: TxId) -> Result<bool> {
        self.sync().await?;
        Ok(self.wallet.is_mined(txid)?)
    }
}

pub async fn run(ctx: &mut Session, args: SwapArgs) -> Result<()> {
    let (settlement, route) = match args.payee {
        Payee::Account(key) => (
            Settlement::connect(&args.rpc, args.contract, key)?,
            Route::Account,
        ),
        Payee::Railgun { relayer, max_fee } => (
            Settlement::read_only(&args.rpc, args.contract)?,
            Route::Railgun {
                relayer: RelayerApi::new(relayer)?,
                max_fee,
            },
        ),
    };
    let maker = MakerApi::new(args.maker)?;
    let seed = ctx.store.seed()?;
    let user = User::new(
        &seed,
        &seed,
        ctx.network,
        settlement,
        maker,
        args.token,
        route,
    )
    .with_max_open_fee(args.max_open_fee);
    drive(ctx, &user, args.units).await
}

pub async fn drive(ctx: &mut Session, user: &User, units: u32) -> Result<()> {
    let mut run = start(&ctx.store, user, units).await?;
    let resumed = matches!(run.progress, Progress::Accepted(_));
    let swap = open(ctx, user, &mut run).await?;
    if resumed && run.deposit.is_none() && user.state(&swap).await?.stage == Stage::Refunded {
        println!("the maker cancelled before anything was deposited");
        return finish(ctx, &mut run);
    }

    match run.deposit.map(TxId::from_bytes) {
        None => {
            let joint = user.verify(&swap).await?;
            deposit(ctx, &mut run, &swap, &joint).await?;
        }
        Some(txid) if !ctx.wallet.is_mined(txid)? => {
            if let Err(e) = ctx.broadcast(txid).await {
                println!("  rebroadcasting the deposit: {e:#}");
            }
        }
        Some(_) => {}
    }

    loop {
        let chain = user.state(&swap).await?;
        let now = user.settlement().now().await?;
        match chain.stage {
            Stage::Ready | Stage::Claimed => return claim(ctx, user, &mut run, &swap).await,
            Stage::Open if now >= chain.t0 => return claim(ctx, user, &mut run, &swap).await,
            Stage::Refunded => return sweep_back(ctx, user, &mut run, &swap).await,
            Stage::Open => println!("  waiting for the maker to confirm the deposit"),
        }
        tokio::time::sleep(CHAIN_POLL).await;
    }
}

async fn start(store: &Store, maker: &impl Opener, units: u32) -> Result<Run> {
    let saved = store.load::<Run>(STATE).with_context(|| {
        format!("{STATE} is another version's: rename it if its swap is finished")
    })?;
    if let Some(run) = saved
        && !run.finished
    {
        println!("resuming swap #{}", run.index());
        return Ok(run);
    }
    let index = store.load::<u32>(NEXT_INDEX)?.unwrap_or(0);
    store.save(NEXT_INDEX, &(index + 1))?;
    let run = Run {
        progress: Progress::Quoted {
            quoted: maker.quote(index, units).await?,
            fee: None,
        },
        joint_account: None,
        deposit: None,
        sweep: None,
        finished: false,
    };
    store.save(STATE, &run)?;
    Ok(run)
}

async fn open(wallet: &mut impl FeeWallet, maker: &impl Opener, run: &mut Run) -> Result<UserSwap> {
    let (quoted, fee) = match &run.progress {
        Progress::Accepted(swap) => return Ok(swap.clone()),
        Progress::Quoted { quoted, fee } => (quoted.clone(), fee.map(TxId::from_bytes)),
    };
    let resumed = fee.is_some();
    let fee = match fee {
        Some(txid) => Some(txid),
        None => match quoted.fee_due(wall_now()) {
            Ok(None) => None,
            Ok(Some(due)) => {
                let needed = due
                    .zat
                    .saturating_add(quoted.quote.deposit_zat)
                    .saturating_add(2 * PAYMENT_FEE);
                let held = wallet.holds().await?;
                if held < needed {
                    let short = anyhow::anyhow!(
                        "the wallet holds {held} zat, and the open fee, the deposit and their transaction fees take {needed}: nothing was paid"
                    );
                    return Err(end(wallet.store(), run, short));
                }
                println!("paying the maker's open fee of {} zat", due.zat);
                let to = due.address.parse().context("the open fee's address")?;
                let txid = wallet.prepare(&to, due.zat).await?;
                run.progress = Progress::Quoted {
                    quoted: quoted.clone(),
                    fee: Some(*txid.as_ref()),
                };
                wallet.store().save(STATE, run)?;
                Some(txid)
            }
            Err(e) => return Err(end(wallet.store(), run, e.context("nothing was paid"))),
        },
    };
    if let Some(txid) = fee {
        let mut waited = None;
        while !wallet.mined(txid).await? {
            if wall_now() >= quoted.quote.expires_at {
                let unmined = anyhow::anyhow!(
                    "the quote expired with its open fee unmined: the fee is lost if {txid} is mined after all"
                );
                return Err(end(wallet.store(), run, unmined));
            }
            if waited.is_none_or(|polls| polls >= FEE_RESEND) {
                match quoted.fee_due(wall_now()) {
                    Ok(_) => {
                        wallet.send(txid).await?;
                        match waited {
                            None => {
                                println!("  open fee sent in {txid}; waiting for it to be mined")
                            }
                            Some(_) => println!("  open fee sent again"),
                        }
                    }
                    Err(_) if waited.is_some() => {}
                    Err(e) if resumed => println!("  not sending the open fee again: {e:#}"),
                    Err(e) => return Err(end(wallet.store(), run, e.context("nothing was paid"))),
                }
                waited = Some(0);
            }
            waited = waited.map(|polls| polls + 1);
            tokio::time::sleep(FEE_POLL).await;
        }
    }
    let swap = match maker.accept(&quoted).await {
        Ok(swap) => swap,
        Err(e) if e.is::<FeeNotHonoured>() || refused(&e) => {
            let e = match fee {
                Some(txid) => e.context(format!("the open fee paid in {txid} is lost")),
                None => e,
            };
            return Err(end(wallet.store(), run, e));
        }
        Err(e) => return Err(e),
    };
    println!("swap #{} opened: {}", swap.index, swap.swap_id);
    run.progress = Progress::Accepted(swap.clone());
    wallet.store().save(STATE, run)?;
    Ok(swap)
}

fn refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServiceError>()
        .is_some_and(ServiceError::refused)
}

fn end(store: &Store, run: &mut Run, why: anyhow::Error) -> anyhow::Error {
    run.finished = true;
    match store.save(STATE, run) {
        Ok(()) => why.context("this swap is over; run again to start a new one"),
        Err(e) => e.context(why),
    }
}

async fn deposit(
    ctx: &mut Session,
    run: &mut Run,
    swap: &UserSwap,
    joint: &JointAccount,
) -> Result<()> {
    if run.joint_account.is_none() {
        let account = ctx
            .wallet
            .import_joint(&mut ctx.client, joint, "swap")
            .await?;
        run.joint_account = Some(account);
        ctx.store.save(STATE, run)?;
    }
    let (own, deposit_zat) = (ctx.account()?, swap.quote.deposit_zat);
    ctx.sync_until(|ctx| {
        let funds = ctx.wallet.funds(own)?;
        Ok(funds.spendable >= deposit_zat || funds.spendable == funds.total)
    })
    .await?;
    let to: ZcashAddress = joint
        .unified_address(ctx.wallet.network().network_type())
        .parse()?;
    let txid = ctx.pay(&to, deposit_zat)?;
    run.deposit = Some(*txid.as_ref());
    ctx.store.save(STATE, run)?;
    ctx.broadcast(txid).await?;
    println!("deposited {deposit_zat} zat to {to}\n  tx {txid}");
    Ok(())
}

async fn claim(ctx: &mut Session, user: &User, run: &mut Run, swap: &UserSwap) -> Result<()> {
    let paid = user.claim(swap).await?;
    let to = match user.settlement().account() {
        Some(account) => format!("paid {} to {account}", paid.amount),
        None => format!("shielded {} into {}", paid.amount, user.railgun().address()),
    };
    match (paid.amount, paid.tx) {
        (0, _) => println!("claimed; nothing was left to pay"),
        (_, Some(tx)) => println!("claimed; {to} in {tx}"),
        (_, None) => println!("claimed; {to}"),
    }
    finish(ctx, run)
}

async fn sweep_back(ctx: &mut Session, user: &User, run: &mut Run, swap: &UserSwap) -> Result<()> {
    let (Some(account), Some(deposit)) = (run.joint_account, run.deposit.map(TxId::from_bytes))
    else {
        println!("the maker cancelled before anything was deposited");
        return finish(ctx, run);
    };
    let key = user.refund_key(swap).await?;
    println!("the maker refunded; waiting for the deposit to confirm");
    ctx.sync_until(|ctx| {
        let funds = ctx.wallet.funds(account)?;
        Ok(ctx.wallet.is_mined(deposit)? && funds.spendable == funds.total)
    })
    .await?;
    let txid = match run.sweep.map(TxId::from_bytes) {
        Some(txid) if !ctx.wallet.is_expired(txid)? => Some(txid),
        _ if ctx.wallet.funds(account)?.total > 0 => {
            let home: ZcashAddress = ctx.wallet.fresh_address(ctx.account()?)?.parse()?;
            let txid = ctx.wallet.sweep(&ctx.prover, account, &key, &home)?;
            run.sweep = Some(*txid.as_ref());
            ctx.store.save(STATE, run)?;
            Some(txid)
        }
        _ => None,
    };
    if let Some(txid) = txid {
        if !ctx.wallet.is_mined(txid)? {
            ctx.broadcast(txid).await?;
        }
        println!("swept the deposit back in {txid}; waiting for it to be mined");
        ctx.sync_until(|ctx| Ok(ctx.wallet.is_mined(txid)? || ctx.wallet.is_expired(txid)?))
            .await?;
        anyhow::ensure!(
            ctx.wallet.is_mined(txid)?,
            "the sweep back {txid} expired unmined: type swap again to sweep afresh"
        );
    }
    finish(ctx, run)
}

fn finish(ctx: &mut Session, run: &mut Run) -> Result<()> {
    run.finished = true;
    ctx.store.save(STATE, run)?;
    if let Some(account) = run.joint_account {
        ctx.wallet.forget(account)?;
    }
    Ok(())
}
