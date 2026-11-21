//! The user side of a swap, resumable: progress is saved after every step.

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
/// An open fee has only its quote's few minutes to be mined in.
const FEE_POLL: Duration = Duration::from_secs(5);
/// The polls an open fee in no block is given before it is sent again: half a minute. A node can
/// take a payment and drop it, and the quote ends meanwhile.
const FEE_RESEND: u32 = 6;
/// What a payment of one note and its change pays the network under ZIP 317.
const PAYMENT_FEE: u64 = 10_000;

pub(crate) struct SwapArgs {
    pub(crate) maker: String,
    pub(crate) rpc: String,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) units: u32,
    pub(crate) payee: Payee,
    pub(crate) tokens: Option<std::sync::Arc<zecswap_client::Tokens>>,
    pub(crate) max_open_fee: u64,
}

/// Where the swap pays.
pub(crate) enum Payee {
    /// An account the user holds the key to, which sends its own transactions.
    Account(PrivateKeySigner),
    /// The Railgun wallet of this seed, through a relayer.
    Railgun { relayer: String, max_fee: u128 },
}

#[derive(Serialize, Deserialize)]
struct Run {
    progress: Progress,
    joint_account: Option<AccountUuid>,
    deposit: Option<[u8; 32]>,
    finished: bool,
}

#[derive(Serialize, Deserialize)]
enum Progress {
    /// A quote taken and, once built, the payment of its open fee, each saved before the next
    /// step: a resumed run neither pays the fee twice nor takes the index again.
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

/// The maker's side of opening a swap, as `User` has it.
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

/// The wallet's side of opening a swap, as `Session` has it.
trait FeeWallet {
    fn store(&self) -> &Store;
    /// The zatoshis the wallet holds, as its server now shows the chain.
    async fn holds(&mut self) -> Result<u64>;
    /// Builds and stores a payment. Nothing is sent.
    async fn prepare(&mut self, to: &ZcashAddress, zatoshis: u64) -> Result<TxId>;
    /// Sends a stored payment; sending it again is no error.
    async fn send(&mut self, txid: TxId) -> Result<()>;
    /// Whether a block has the payment, as the wallet's server now shows the chain.
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

pub(crate) async fn run(ctx: &mut Session, args: SwapArgs) -> Result<()> {
    let spending = |api: MakerApi| match &args.tokens {
        Some(tokens) => api.with_tokens(tokens.clone()),
        None => api,
    };
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
    let maker = spending(MakerApi::new(args.maker)?);
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

    let mut run = start(&ctx.store, &user, args.units).await?;
    let resumed = matches!(run.progress, Progress::Accepted(_));
    let swap = open(ctx, &user, &mut run).await?;
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
            Stage::Ready | Stage::Claimed => return claim(ctx, &user, &mut run, &swap).await,
            Stage::Open if now >= chain.t0 => return claim(ctx, &user, &mut run, &swap).await,
            Stage::Refunded => return sweep_back(ctx, &user, &mut run, &swap).await,
            Stage::Open => println!("  waiting for the maker to confirm the deposit"),
        }
        tokio::time::sleep(CHAIN_POLL).await;
    }
}

/// The run to carry on: the unfinished one, or a new one with a quote for the next index.
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
    // The index is spent the moment anything derived from it leaves the device.
    let index = store.load::<u32>(NEXT_INDEX)?.unwrap_or(0);
    store.save(NEXT_INDEX, &(index + 1))?;
    let run = Run {
        progress: Progress::Quoted {
            quoted: maker.quote(index, units).await?,
            fee: None,
        },
        joint_account: None,
        deposit: None,
        finished: false,
    };
    store.save(STATE, &run)?;
    Ok(run)
}

/// Brings the run to a swap its maker accepted: pays its quote's open fee, once, waits for a
/// block to have it, and accepts. A run that can no longer get there is ended, whatever it paid
/// lost, and the next begins another swap.
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
                // A fee buys nothing if the deposit cannot follow it.
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
                // Saved before it is sent, so a resumed run never pays twice.
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
        // The polls since this run last sent the payment, or decided not to.
        let mut waited = None;
        while !wallet.mined(txid).await? {
            if wall_now() >= quoted.quote.expires_at {
                let unmined = anyhow::anyhow!(
                    "the quote expired with its open fee unmined: the fee is lost if {txid} is mined after all"
                );
                return Err(end(wallet.store(), run, unmined));
            }
            if waited.is_none_or(|polls| polls >= FEE_RESEND) {
                // The time left is judged again each time: building the payment can outlast
                // what it was built within, and a resumed run comes back at any time.
                match quoted.fee_due(wall_now()) {
                    // Sent again by a resumed run, whose first send may never have left, and
                    // by this one while no block has it.
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
                    // What an earlier run sent may still be mined in time.
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
        // Turned down, nothing opens the quote's swap any more; any other failure may have
        // opened it, and the next run asks again.
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

/// Ends a run that will open no swap, so that the next starts a new one: the error to stop with.
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
    // A wallet that paid its open fee from its only note has the change to wait for.
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
    // Saved before broadcasting, so a resumed run never pays twice.
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
    // A request that timed out may have paid, in a transaction no answer named.
    match (paid.amount, paid.tx) {
        (0, _) => println!("claimed; nothing was left to pay"),
        (_, Some(tx)) => println!("claimed; {to} in {tx}"),
        (_, None) => println!("claimed; {to}"),
    }
    finish(ctx, run)
}

/// The maker refunded and revealed its share, so the deposit is ours to take back.
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
    // Nothing left means an earlier run already swept it.
    if ctx.wallet.funds(account)?.total > 0 {
        let home: ZcashAddress = ctx.wallet.fresh_address(ctx.account()?)?.parse()?;
        let txid = ctx.wallet.sweep(&ctx.prover, account, &key, &home)?;
        ctx.broadcast(txid).await?;
        println!("swept the deposit back in {txid}; waiting for it to be mined");
        ctx.sync_until(|ctx| Ok(ctx.wallet.is_mined(txid)?)).await?;
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

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use alloy_primitives::B256;
    use anyhow::bail;
    use zcash_address::unified::{self, Encoding, Receiver, Revision, Uitem};
    use zcash_protocol::consensus::NetworkType;
    use zecswap_api::{OpenFee, Quote};
    use zecswap_client::OPEN_FEE_MARGIN;
    use zecswap_core::{ShareProof, derive_maker_share};

    use super::*;

    const FEE: u64 = 40_000;

    /// How a maker ends an accept short of a swap.
    enum Refusal {
        /// The quote expired with its fee unseen.
        Unseen,
        Refused,
        /// No answer: the accept may have opened the swap.
        Silent,
    }

    /// A maker whose quotes last `life` seconds, ask a fee or none, and whose accepts end as
    /// `refusals` says, in turn, before they succeed.
    struct Maker {
        life: u64,
        fee: bool,
        refusals: RefCell<VecDeque<Refusal>>,
        quotes: Cell<u32>,
        accepts: Cell<u32>,
    }

    impl Maker {
        fn new(life: u64, fee: bool, refusals: impl IntoIterator<Item = Refusal>) -> Self {
            Self {
                life,
                fee,
                refusals: RefCell::new(refusals.into_iter().collect()),
                quotes: Cell::new(0),
                accepts: Cell::new(0),
            }
        }
    }

    impl Opener for Maker {
        async fn quote(&self, index: u32, _units: u32) -> Result<Quoted> {
            self.quotes.set(self.quotes.get() + 1);
            let quote = Quote {
                quote_id: B256::repeat_byte(index as u8),
                maker: Address::repeat_byte(1),
                maker_share: derive_maker_share(&[9; 32], index.into())?.public(),
                maker_proof: ShareProof::from_bytes([0; 64]),
                chain_id: 11_155_111,
                contract: Address::repeat_byte(2),
                token: Address::repeat_byte(3),
                amount: 1_000_000,
                deposit_zat: 100_000,
                expires_at: wall_now() + self.life,
                network_cost: None,
                open_fee: self.fee.then(|| OpenFee {
                    zat: FEE,
                    address: fee_address(),
                }),
            };
            Ok(Quoted { index, quote })
        }

        async fn accept(&self, quoted: &Quoted) -> Result<UserSwap> {
            self.accepts.set(self.accepts.get() + 1);
            match self.refusals.borrow_mut().pop_front() {
                Some(Refusal::Unseen) => Err(FeeNotHonoured.into()),
                Some(Refusal::Refused) => Err(ServiceError {
                    service: "maker",
                    status: http::StatusCode::NOT_FOUND,
                    body: "unknown quote".into(),
                }
                .into()),
                Some(Refusal::Silent) => bail!("the connection was reset"),
                None => Ok(UserSwap {
                    index: quoted.index,
                    quote: quoted.quote.clone(),
                    swap_id: B256::repeat_byte(0xaa),
                    t0: 1_790_003_600,
                    t1: 1_790_007_200,
                    ticket: None,
                }),
            }
        }
    }

    /// A wallet that records what it is asked to pay and send. A payment is in a block
    /// `blocks_away` polls after it was sent, or never.
    struct Wallet {
        _dir: tempfile::TempDir,
        store: Store,
        paid: Vec<(String, u64)>,
        sent: Vec<TxId>,
        /// Sends still to fail, as for a run cut off between saving its payment and sending it.
        failing_sends: u32,
        /// Sends of a payment the network takes and drops before one reaches a block.
        dropped: usize,
        blocks_away: Option<u32>,
        held: u64,
        /// How long building a payment takes, by the wall clock a quote's life is judged by.
        building: Duration,
    }

    impl Wallet {
        fn new(blocks_away: Option<u32>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            Self {
                store: Store::new(dir.path()),
                _dir: dir,
                paid: Vec::new(),
                sent: Vec::new(),
                failing_sends: 0,
                dropped: 0,
                blocks_away,
                held: u64::MAX,
                building: Duration::ZERO,
            }
        }

        fn next_index(&self) -> Option<u32> {
            self.store.load(NEXT_INDEX).unwrap()
        }

        fn saved(&self) -> Run {
            self.store.load(STATE).unwrap().unwrap()
        }
    }

    impl FeeWallet for Wallet {
        fn store(&self) -> &Store {
            &self.store
        }

        async fn holds(&mut self) -> Result<u64> {
            Ok(self.held)
        }

        async fn prepare(&mut self, to: &ZcashAddress, zatoshis: u64) -> Result<TxId> {
            std::thread::sleep(self.building);
            self.paid.push((to.encode(), zatoshis));
            Ok(TxId::from_bytes([self.paid.len() as u8; 32]))
        }

        async fn send(&mut self, txid: TxId) -> Result<()> {
            if self.failing_sends > 0 {
                self.failing_sends -= 1;
                bail!("lightwalletd is unreachable");
            }
            self.sent.push(txid);
            Ok(())
        }

        async fn mined(&mut self, txid: TxId) -> Result<bool> {
            let Some(blocks_away) = &mut self.blocks_away else {
                return Ok(false);
            };
            if self.sent.iter().filter(|sent| **sent == txid).count() <= self.dropped {
                return Ok(false);
            }
            let mined = *blocks_away == 0;
            *blocks_away = blocks_away.saturating_sub(1);
            Ok(mined)
        }
    }

    fn fee_address() -> String {
        let orchard = Uitem::Data(Receiver::Orchard([1; 43]));
        unified::Address::try_from_items(Revision::R0, vec![orchard])
            .unwrap()
            .encode(&NetworkType::Test)
    }

    /// One run of the CLI, up to its swap being accepted.
    async fn run(wallet: &mut Wallet, maker: &Maker) -> Result<UserSwap> {
        let mut run = start(&wallet.store, maker, 1).await?;
        open(wallet, maker, &mut run).await
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_cut_off_before_its_fee_was_sent_resumes_without_paying_again() {
        let maker = Maker::new(600, true, []);
        let mut wallet = Wallet::new(Some(1));
        wallet.failing_sends = 1;
        let cut = run(&mut wallet, &maker).await.unwrap_err().to_string();
        assert!(cut.contains("unreachable"), "{cut}");
        assert_eq!(wallet.paid, [(fee_address(), FEE)]);
        assert!(wallet.sent.is_empty() && !wallet.saved().finished);

        let swap = run(&mut wallet, &maker).await.unwrap();
        assert_eq!(
            wallet.paid.len(),
            1,
            "the payment saved was sent, not built again"
        );
        assert_eq!(wallet.sent, [TxId::from_bytes([1; 32])]);
        assert_eq!((maker.quotes.get(), maker.accepts.get()), (1, 1));
        assert_eq!((swap.index, wallet.next_index()), (0, Some(1)));

        // Accepted, a run asks its maker nothing more and pays nothing.
        let again = run(&mut wallet, &maker).await.unwrap();
        assert_eq!(again.swap_id, swap.swap_id);
        assert_eq!((maker.quotes.get(), maker.accepts.get()), (1, 1));
        assert_eq!((wallet.paid.len(), wallet.sent.len()), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_payment_the_network_dropped_is_sent_again_and_its_swap_opens() {
        let maker = Maker::new(600, true, []);
        let mut wallet = Wallet::new(Some(0));
        wallet.dropped = 1;
        let swap = run(&mut wallet, &maker).await.unwrap();
        let fee = TxId::from_bytes([1; 32]);
        assert_eq!(wallet.sent, [fee, fee], "the same payment, sent twice");
        assert_eq!(wallet.paid.len(), 1, "and built once");
        assert_eq!((swap.index, maker.accepts.get()), (0, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_fee_the_maker_never_honoured_ends_the_swap_and_the_next_run_starts_another() {
        for refusal in [Refusal::Unseen, Refusal::Refused] {
            let maker = Maker::new(600, true, [refusal]);
            let mut wallet = Wallet::new(Some(0));
            let lost = run(&mut wallet, &maker).await.unwrap_err();
            let lost = format!("{lost:#}");
            assert!(
                lost.contains("is lost") && lost.contains("run again"),
                "{lost}"
            );
            assert!(wallet.saved().finished);

            let swap = run(&mut wallet, &maker).await.unwrap();
            assert_eq!((swap.index, wallet.next_index()), (1, Some(2)));
            assert_eq!(maker.quotes.get(), 2);
            assert_eq!(wallet.paid.len(), 2, "the new swap's own fee");
            assert_eq!(wallet.sent.len(), 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_accept_that_went_unanswered_is_asked_again_with_nothing_more_paid() {
        let maker = Maker::new(600, true, [Refusal::Silent]);
        let mut wallet = Wallet::new(Some(0));
        let unanswered = run(&mut wallet, &maker).await.unwrap_err().to_string();
        assert!(unanswered.contains("reset"), "{unanswered}");
        assert!(!wallet.saved().finished);

        let swap = run(&mut wallet, &maker).await.unwrap();
        assert_eq!((swap.index, wallet.next_index()), (0, Some(1)));
        assert_eq!((maker.quotes.get(), maker.accepts.get()), (1, 2));
        assert_eq!((wallet.paid.len(), wallet.sent.len()), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_quote_too_near_its_end_is_given_up_with_nothing_paid() {
        let maker = Maker::new(OPEN_FEE_MARGIN - 1, true, []);
        let mut wallet = Wallet::new(Some(0));
        let late = run(&mut wallet, &maker).await.unwrap_err();
        let late = format!("{late:#}");
        assert!(late.contains("nothing was paid"), "{late}");
        assert!(wallet.paid.is_empty() && wallet.saved().finished);
        assert_eq!(maker.accepts.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fee_still_unmined_when_its_quote_expires_is_not_sent_or_waited_for() {
        let maker = Maker::new(600, true, []);
        let mut wallet = Wallet::new(None);
        wallet.failing_sends = 1;
        run(&mut wallet, &maker).await.unwrap_err();
        // The CLI is next run after the quote's end.
        let mut saved = wallet.saved();
        let Progress::Quoted { quoted, fee } = &mut saved.progress else {
            panic!("the quote was accepted");
        };
        assert_eq!(*fee, Some([1; 32]));
        quoted.quote.expires_at = wall_now() - 1;
        wallet.store.save(STATE, &saved).unwrap();

        let expired = run(&mut wallet, &maker).await.unwrap_err();
        let expired = format!("{expired:#}");
        assert!(expired.contains("unmined"), "{expired}");
        assert!(wallet.sent.is_empty() && wallet.saved().finished);
        assert_eq!((maker.accepts.get(), wallet.paid.len()), (0, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wallet_that_could_not_also_deposit_pays_no_fee() {
        let maker = Maker::new(600, true, []);
        let mut wallet = Wallet::new(Some(0));
        // The fake maker's deposit is 100,000 zat.
        wallet.held = FEE + 100_000 + 2 * PAYMENT_FEE - 1;
        let short = run(&mut wallet, &maker).await.unwrap_err();
        let short = format!("{short:#}");
        assert!(short.contains("nothing was paid"), "{short}");
        assert!(wallet.paid.is_empty() && wallet.saved().finished);
        assert_eq!(maker.accepts.get(), 0);

        wallet.held += 1;
        run(&mut wallet, &maker).await.unwrap();
        assert_eq!((wallet.paid.len(), wallet.sent.len()), (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_payment_whose_building_outlasted_the_time_to_mine_it_is_not_sent() {
        let maker = Maker::new(OPEN_FEE_MARGIN, true, []);
        let mut wallet = Wallet::new(Some(0));
        wallet.building = Duration::from_millis(1_100);
        let late = run(&mut wallet, &maker).await.unwrap_err();
        let late = format!("{late:#}");
        assert!(late.contains("nothing was paid"), "{late}");
        assert_eq!(wallet.paid.len(), 1, "it was built");
        assert!(wallet.sent.is_empty() && wallet.saved().finished);
        assert_eq!(maker.accepts.get(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_resumed_run_too_near_its_quotes_end_sends_its_saved_fee_no_more() {
        let maker = Maker::new(600, true, []);
        let mut wallet = Wallet::new(None);
        wallet.failing_sends = 1;
        run(&mut wallet, &maker).await.unwrap_err();
        // The CLI is next run with a second of the quote left.
        let mut saved = wallet.saved();
        let Progress::Quoted { quoted, .. } = &mut saved.progress else {
            panic!("the quote was accepted");
        };
        quoted.quote.expires_at = wall_now() + 1;
        wallet.store.save(STATE, &saved).unwrap();
        wallet.failing_sends = 0;

        let expired = run(&mut wallet, &maker).await.unwrap_err();
        let expired = format!("{expired:#}");
        assert!(expired.contains("unmined"), "{expired}");
        assert!(wallet.sent.is_empty() && wallet.saved().finished);
        assert_eq!((maker.accepts.get(), wallet.paid.len()), (0, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_quote_without_a_fee_is_accepted_with_nothing_paid() {
        let maker = Maker::new(600, false, []);
        let mut wallet = Wallet::new(None);
        let swap = run(&mut wallet, &maker).await.unwrap();
        assert_eq!((swap.index, maker.accepts.get()), (0, 1));
        assert!(wallet.paid.is_empty() && wallet.sent.is_empty());
    }

    #[tokio::test]
    async fn a_state_file_of_another_version_stops_the_run_before_anything_is_asked() {
        let maker = Maker::new(600, true, []);
        let mut wallet = Wallet::new(Some(0));
        let old = serde_json::json!({"swap": {"index": 4}, "deposit": null, "finished": true});
        wallet.store.save(STATE, &old).unwrap();
        let stopped = run(&mut wallet, &maker).await.unwrap_err().to_string();
        assert!(stopped.contains("another version's"), "{stopped}");
        assert_eq!((maker.quotes.get(), wallet.next_index()), (0, None));
    }
}
