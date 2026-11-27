use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{B256, U256};
use alloy::signers::local::PrivateKeySigner;
use tokio::task::{JoinHandle, LocalSet, spawn_local};
use tokio::time::{Instant, sleep, sleep_until};
use zecswap_chain::evm::{Reveal, Settlement, Stage, Window, Windows, simulate_wall};
use zecswap_client::MIN_TIME_TO_T0;
use zecswap_core::{NetworkType, SecretShare, derive_user_keys};
use zecswap_railgun::Keys as RailgunKeys;

use crate::alerts::Alarms;
use crate::maker::{Fee, Maker, MakerDoor, Mood, T0_TO_T1, TO_T0};
use crate::net::{Line, Net};
use crate::node::{Node, PROTECT, SLOT, Weather};
use crate::panics::{self, Named};
use crate::relayer::{FEE, Relayer, RelayerDoor, Service};
use crate::seed::Seed;
use crate::wallet::{End, Saved, Wallet, Way};
use crate::world::{CHAIN_ID, Chain, EPOCH, LATE_WINDOW, MIN_WINDOW, Shares, Skewed, UNIT, key};
use crate::zcash::Zcash;

const ZCASH_BLOCK: Duration = Duration::from_secs(25);
pub(crate) const SETTLE: Duration = Duration::from_secs(8 * 60);
const HORIZON: u64 = zecswap_client::MAX_TIME_TO_T0 + T0_TO_T1 + LATE_WINDOW + 30 * 60;
const OPEN_FEE: u64 = 40_000;
const CLAIMED: u8 = 3;
const REFUNDED: u8 = 4;

#[derive(Clone, Debug)]
pub struct Plan {
    pub weather: Weather,
    pub maker_line: Line,
    pub relayer_line: Line,
    pub mood: Mood,
    pub service: Service,
    pub fee: Fee,
    pub max_open_fee: u64,
    pub railgun: bool,
    pub private: bool,
    pub multicall: bool,
    pub crashes: Vec<(u64, u64)>,
    pub skew: i64,
    pub linger_verify: Option<i64>,
    pub linger_claim: Option<i64>,
    pub clears: u64,
    pub zcash_loss: u64,
    pub units: u32,
}

impl Plan {
    pub fn plain() -> Self {
        Self {
            weather: Weather::CALM,
            maker_line: Line::CLEAR,
            relayer_line: Line::CLEAR,
            mood: Mood::Honest,
            service: Service::Honest,
            fee: Fee::None,
            max_open_fee: 0,
            railgun: true,
            private: false,
            multicall: true,
            crashes: Vec::new(),
            skew: 0,
            linger_verify: None,
            linger_claim: None,
            clears: 0,
            zcash_loss: 0,
            units: 1,
        }
    }

    pub fn from_seed(seed: Seed) -> Self {
        let roll = |what: &str, bound: u64| seed.below(what, 0, bound);
        let calm = roll("sky", 100) < 25;
        let up_to = |what: &str, most: u64| if calm { 0 } else { roll(what, most + 1) };
        let line = |who: &str| Line {
            busy: up_to(&format!("{who}-busy"), 150),
            hang: up_to(&format!("{who}-hang"), 8),
            unheard: up_to(&format!("{who}-unheard"), 40),
            unanswered: up_to(&format!("{who}-unanswered"), 40),
        };
        let mood = match roll("mood", 100) {
            0..55 => Mood::Honest,
            55..63 => Mood::NeverReadies,
            63..70 => Mood::Vanishes,
            70..78 => Mood::CancelsEarly,
            78..83 => Mood::KeepsFee,
            83..88 => Mood::OpensLess,
            88..92 => Mood::OpensAnotherNote,
            92..96 => Mood::OpensFar,
            _ => Mood::LiesAboutDeadlines,
        };
        let (fee, max_open_fee) = match roll("fee", 10) {
            _ if mood == Mood::KeepsFee => (Fee::Asked(OPEN_FEE), OPEN_FEE),
            0..5 => (Fee::None, 0),
            5..8 => (Fee::Asked(OPEN_FEE), OPEN_FEE + 10_000),
            8 => (Fee::Asked(OPEN_FEE), OPEN_FEE - 1),
            _ => (Fee::Transparent(OPEN_FEE), OPEN_FEE),
        };
        let mut crashes: Vec<(u64, u64)> = (0..up_to("crashes", 3))
            .map(|n| {
                (
                    seed.between("crash-at", n, 30, TO_T0 + T0_TO_T1),
                    seed.between("crash-rest", n, 5, 300),
                )
            })
            .collect();
        let away = match up_to("away", 7) {
            1 => Some(TO_T0 + seed.between("back-near-t0", 0, 0, 6 * 60) - 3 * 60),
            2 | 3 => Some(TO_T0 + T0_TO_T1 + seed.between("back-near-t1", 0, 0, 11 * 60) - 9 * 60),
            _ => None,
        };
        if let Some(back) = away {
            let left = seed.between("left-at", 0, 3 * 60, 12 * 60);
            crashes.retain(|(at, _)| *at < left);
            crashes.push((left, back - left));
        }
        let mut plan = Self {
            weather: Weather {
                reorg: up_to("reorg", 25),
                reorg_after_tx: up_to("reorg-after-tx", 450),
                ambush: up_to("ambush", 400),
                reorg_depth: 2,
                fork_delay: up_to("fork-delay", 900),
                missed_slot: up_to("missed-slot", 60),
                delayed: up_to("delayed", 200),
                delay_blocks: 3,
                lost: up_to("lost", 40),
                private_lost: up_to("private-lost", 40),
                fee_swing: up_to("fee-swing", 600),
                rpc_error: up_to("rpc-error", 40),
                rpc_stall: up_to("rpc-stall", 4),
                stale_read: up_to("stale-read", 150),
            },
            maker_line: line("maker"),
            relayer_line: line("relayer"),
            mood,
            service: match roll("service", 5) {
                0 => Service::SkipsPayouts,
                _ => Service::Honest,
            },
            fee,
            max_open_fee,
            railgun: roll("route", 4) != 0,
            private: roll("private", 2) == 0,
            multicall: roll("multicall", 2) == 0,
            crashes,
            skew: if calm {
                0
            } else {
                roll("skew", 181) as i64 - 90
            },
            linger_verify: (up_to("linger-verify", 7) == 1)
                .then(|| roll("linger-verify-shift", 481) as i64 - 240),
            linger_claim: match up_to("linger-claim", 7) {
                1 | 2 => Some(roll("linger-claim-near", 46) as i64),
                3 => Some(roll("linger-claim-early", 421) as i64),
                _ => None,
            },
            clears: seed.between("clears", 0, 10 * 60, TO_T0 + 30 * 60),
            zcash_loss: up_to("zcash-loss", 100),
            units: 1 + roll("units", 2) as u32,
        };
        let focus = roll("focus", 10);
        if focus < 2 {
            plan.private = false;
            plan.crashes.clear();
            plan.linger_verify = None;
            plan.linger_claim = None;
            plan.service = Service::Honest;
            (plan.fee, plan.max_open_fee) = (Fee::None, 0);
            plan.weather.reorg_depth = 2;
            plan.weather.delay_blocks = 3;
            plan.weather.lost = 0;
            plan.weather.rpc_stall = 0;
        }
        match focus {
            0 => {
                plan.mood = match roll("focus-mood", 3) {
                    0 => Mood::Honest,
                    1 => Mood::NeverReadies,
                    _ => Mood::Vanishes,
                };
                plan.linger_claim = Some(roll("focus-linger", 46) as i64);
                plan.weather.delayed = 600;
            }
            1 => {
                plan.mood = Mood::Honest;
                plan.weather.ambush = 1000;
                plan.weather.fork_delay = 900;
            }
            _ => {}
        }
        plan
    }

    pub(crate) fn calm(&self) -> bool {
        self.weather.rpc_error == 0
            && self.weather.delayed == 0
            && self.weather.reorg == 0
            && self.maker_line.busy == 0
            && self.crashes.is_empty()
            && self.linger_verify.is_none()
            && self.linger_claim.is_none()
    }
}

#[derive(Debug)]
pub struct Report<P = Plan, E = End> {
    pub seed: u64,
    pub plan: P,
    pub ended: Option<E>,
    pub stage: Option<u8>,
    pub failures: Vec<String>,
    pub trace: Vec<String>,
}

impl<P: std::fmt::Debug, E: std::fmt::Debug> Report<P, E> {
    pub fn story(&self) -> String {
        let tail = self.trace.len().saturating_sub(60);
        format!(
            "seed {}\nplan {:?}\nended {:?} stage {:?}\nfailures:\n  {}\nlast events:\n  {}",
            self.seed,
            self.plan,
            self.ended,
            self.stage,
            self.failures.join("\n  "),
            self.trace[tail..].join("\n  ")
        )
    }
}

pub fn run(seed: u64) -> Report {
    run_with(seed, Plan::from_seed(Seed(seed)))
}

pub fn run_with(seed: u64, plan: Plan) -> Report {
    paused(seed, plan, simulate)
}

pub(crate) fn paused<P: Clone, E, F: Future<Output = Report<P, E>>>(
    seed: u64,
    plan: P,
    simulate: impl FnOnce(Seed, P) -> F,
) -> Report<P, E> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a runtime");
    let simulation = simulate(Seed(seed), plan.clone());
    let (report, panics) = panics::caught(|| LocalSet::new().block_on(&runtime, simulation));
    simulate_wall(None);
    let mut report = report.unwrap_or(Report {
        seed,
        plan,
        ended: None,
        stage: None,
        failures: Vec::new(),
        trace: Vec::new(),
    });
    report.failures.extend(panics);
    report
}

pub(crate) fn connect(
    node: &Node,
    chain: &Chain,
    name: &str,
    signer: PrivateKeySigner,
    private: bool,
    alarms: &Arc<Alarms>,
) -> Settlement {
    let sink = alarms.sink(name, &signer);
    let mut settlement = Settlement::over(node.rpc(name), chain.contract, Some(signer));
    settlement.alerts_to(sink);
    if private {
        settlement.broadcast_privately_over(node.private_rpc(name, PROTECT), PROTECT);
    }
    settlement
}

pub(crate) async fn ticks(node: Node, start: Instant) {
    loop {
        sleep(Duration::from_secs(SLOT)).await;
        node.tick(EPOCH + start.elapsed().as_secs());
    }
}

pub(crate) async fn zcash_blocks(zcash: Zcash, seed: Seed, loss: u64, clears: Instant) {
    loop {
        sleep(ZCASH_BLOCK).await;
        let height = zcash.height();
        let cleared = Instant::now() >= clears;
        zcash.mine(|id| !cleared && seed.chance("zcash-lost", id.0 as u64 * 4096 + height, loss));
    }
}

pub(crate) async fn lives(
    who: &'static str,
    note: &dyn Fn(String),
    start: Instant,
    horizon: impl Fn() -> Instant,
    crashes: &[(u64, u64)],
    live: impl Fn() -> JoinHandle<()>,
) -> Option<Named> {
    let mut crashes = crashes.to_vec();
    crashes.sort_unstable();
    let mut rested = start;
    let mut life = live();
    loop {
        let end = horizon();
        let crash = crashes
            .first()
            .map(|(at, _)| (start + Duration::from_secs(*at)).max(rested));
        tokio::select! {
            biased;
            () = sleep_until(end) => if horizon() == end { break },
            () = sleep_until(crash.unwrap_or(end)), if crash.is_some() => {
                let (_, rest) = crashes.remove(0);
                panics::end([(who, life)]).await;
                note(format!("{who} crashed, away {rest}s"));
                sleep(Duration::from_secs(rest)).await;
                rested = Instant::now();
                note(format!("{who} is back"));
                life = live();
            }
            ended = &mut life => {
                panics::judge(who, ended);
                sleep(SETTLE).await;
                return None;
            }
        }
    }
    life.abort();
    Some((who, life))
}

struct World {
    seed: Seed,
    plan: Plan,
    chain: Chain,
    zcash: Zcash,
    maker: Arc<Maker>,
    wallet: Arc<Wallet>,
    user_seed: [u8; 64],
    shares: Shares,
    disagreements: Arc<Mutex<Vec<String>>>,
    alarms: Arc<Alarms>,
}

async fn simulate(seed: Seed, plan: Plan) -> Report {
    simulate_wall(Some(EPOCH as i64));
    let start = Instant::now();
    let node = Node::new(seed, CHAIN_ID, EPOCH - SLOT, plan.weather);
    let chain = Chain::deploy(&node, plan.multicall);
    let (maker_key, relayer_key, user_key) =
        (key(seed, "maker"), key(seed, "relayer"), key(seed, "user"));
    for who in [&maker_key, &relayer_key, &user_key] {
        chain.fund(who.address());
    }
    chain.stock(maker_key.address(), 100 * UNIT);
    node.clear_from(EPOCH + plan.clears);
    let zcash = Zcash::default();
    let (alarms, mut operator) = Alarms::new(&node, seed);
    let maker = Maker::new(
        seed,
        plan.mood,
        plan.fee,
        chain.clone(),
        zcash.clone(),
        connect(&node, &chain, "maker", maker_key, plan.private, &alarms),
    );
    let relayer = Relayer::new(
        plan.service,
        chain.clone(),
        connect(&node, &chain, "relayer", relayer_key, plan.private, &alarms),
    );
    operator.restarts("maker", {
        let maker = maker.clone();
        move || maker.settlement().forget_sends()
    });
    operator.restarts("relayer", {
        let relayer = relayer.clone();
        move || relayer.settlement().forget_sends()
    });
    let log = notes(&node);
    let clears = start + Duration::from_secs(plan.clears);
    let user_seed = seed.bytes::<64>("user-seed");
    let wallet = Arc::new(Wallet {
        seed,
        user_seed,
        chain: chain.clone(),
        zcash: zcash.clone(),
        maker: Net::to(
            seed,
            "maker",
            plan.maker_line,
            clears,
            Arc::new(MakerDoor(maker.clone())),
            log.clone(),
        ),
        relayer: Net::to(
            seed,
            "relayer",
            plan.relayer_line,
            clears,
            Arc::new(RelayerDoor(relayer)),
            log,
        ),
        way: match plan.railgun {
            true => Way::Railgun { max_fee: 2 * FEE },
            false => Way::Account {
                key: user_key,
                private: plan.private,
            },
        },
        units: plan.units,
        max_open_fee: plan.max_open_fee,
        funds: 100_000_000,
        linger_verify: plan.linger_verify,
        linger_claim: plan.linger_claim,
        saved: Arc::default(),
        alarms: alarms.clone(),
    });
    let world = Arc::new(World {
        seed,
        plan: plan.clone(),
        chain: chain.clone(),
        zcash: zcash.clone(),
        maker: maker.clone(),
        wallet: wallet.clone(),
        user_seed,
        shares: Shares::default(),
        disagreements: Arc::default(),
        alarms,
    });

    let background = [
        ("the chain", spawn_local(ticks(node.clone(), start))),
        ("the operator", spawn_local(operator.tend())),
        (
            "the Zcash chain",
            spawn_local(zcash_blocks(zcash.clone(), seed, plan.zcash_loss, clears)),
        ),
        ("the maker", spawn_local(maker.tend())),
        (
            "the observer",
            spawn_local({
                let world = world.clone();
                async move { world.observe().await }
            }),
        ),
    ];
    let horizon = || world.alarms.until(start + Duration::from_secs(HORIZON));
    let life = lives(
        "the wallet",
        &*notes(&node),
        start,
        horizon,
        &plan.crashes,
        || spawn_local(Skewed::new(plan.skew, wallet.clone().live())),
    )
    .await;
    panics::end(background.into_iter().chain(life)).await;
    world.report()
}

impl World {
    fn z(&self, saved: &Saved) -> Option<SecretShare> {
        let index = saved.quoted.as_ref()?.index;
        self.shares.of(&self.user_seed, index)
    }

    async fn observe(&self) {
        let windows = Windows {
            min: MIN_WINDOW,
            late: LATE_WINDOW,
        };
        loop {
            sleep(Duration::from_secs(SLOT)).await;
            let (number, now) = self.chain.node.head();
            let Some(z) = self.z(&self.wallet.saved()) else {
                continue;
            };
            for swap in self.maker.swaps() {
                let stage = match self.chain.recorded(swap.id).stage {
                    1 => Stage::Open,
                    2 => Stage::Ready,
                    _ => continue,
                };
                let deadlines = (swap.opened.t0, swap.opened.t1);
                let judged = |reveal| windows.window(reveal, false, stage, deadlines, now);
                let e = self.maker.share(swap.nonce);
                let claim = self.chain.would_claim(swap.id, &swap.opened, &z);
                let refund = self.chain.would_refund(swap.id, &swap.opened, &e);
                let mut wrong = Vec::new();
                if (judged(Reveal::Claim) != Window::Shut) != claim {
                    wrong.push(format!(
                        "the client judges a claim {:?}, the contract takes one: {claim}",
                        judged(Reveal::Claim)
                    ));
                }
                if (judged(Reveal::Refund) != Window::Shut) != refund {
                    wrong.push(format!(
                        "the client judges a refund {:?}, the contract takes one: {refund}",
                        judged(Reveal::Refund)
                    ));
                }
                if claim && refund {
                    wrong.push("both sides may reveal at once".into());
                }
                let wasted = self.chain.node.published().into_iter().any(|tx| {
                    tx.landed == Some(false)
                        && tx.at + 1 >= number
                        && tx.input.windows(32).any(|window| window == z.to_be_bytes())
                });
                if wasted {
                    wrong.push("a transaction carrying the user's share reverted".into());
                }
                for what in wrong {
                    self.disagreements
                        .lock()
                        .unwrap()
                        .push(format!("block {number} at {now}, {stage:?}: {what}"));
                }
            }
        }
    }

    fn report(&self) -> Report {
        let saved = self.wallet.saved();
        let published = self.chain.node.published();
        let shown = |secret: &SecretShare| {
            let word = secret.to_be_bytes();
            published
                .iter()
                .any(|tx| tx.input.windows(32).any(|window| window == word))
        };
        let mut failures = self.disagreements.lock().unwrap().clone();
        failures.truncate(5);
        failures.extend(self.alarms.failures(&[]));
        let mut fail = |what: String| failures.push(what);

        let fee_payments = self.zcash.payments_to(&self.maker.fee_address);
        let fees_paid: u64 = fee_payments
            .iter()
            .filter(|(_, _, mined)| *mined)
            .map(|(_, zat, _)| zat)
            .sum();
        if fee_payments.len() > 1 {
            fail(format!(
                "{} open fees were built for one swap",
                fee_payments.len()
            ));
        }
        if fees_paid > self.plan.max_open_fee {
            fail(format!(
                "an open fee of {fees_paid} zat was paid over the cap of {}",
                self.plan.max_open_fee
            ));
        }
        if matches!(self.plan.fee, Fee::Transparent(_))
            && (saved.fee.is_some() || saved.swap.is_some())
        {
            fail("the wallet went on with a fee payable outside Orchard".into());
        }

        let swaps = self.maker.swaps();
        let swap = swaps.first();
        let recorded = swap.map(|swap| self.chain.recorded(swap.id));
        let stage = recorded.map(|recorded| recorded.stage);
        let deposit = saved.deposit.filter(|id| self.zcash.confirmations(*id) > 0);
        let settled = self.plan.clears + SETTLE.as_secs() + LATE_WINDOW < HORIZON;

        if saved.deposit.is_some() && self.plan.mood.cheats_on_terms() {
            fail(format!(
                "the wallet deposited though its maker was {:?}",
                self.plan.mood
            ));
        }
        if let (Some(swap), Some(at)) = (swap, saved.deposited_at)
            && swap.opened.t0.saturating_sub(at) + 2 * SLOT < MIN_TIME_TO_T0
        {
            fail(format!(
                "the wallet deposited {}s before t0",
                swap.opened.t0.saturating_sub(at)
            ));
        }
        if let (Some(swap), Some(recorded), Some(z)) = (swap, recorded, self.z(&saved)) {
            let e = self.maker.share(swap.nonce);
            if shown(&z) && recorded.stage != CLAIMED {
                fail(format!(
                    "the user's share is public and the swap is at stage {}: the maker can take the ZEC and keep the tokens",
                    recorded.stage
                ));
            }
            let carrying: Vec<_> = published
                .iter()
                .filter(|tx| tx.input.windows(32).any(|window| window == z.to_be_bytes()))
                .collect();
            for tx in &carrying {
                let stage = self.chain.recorded_at(swap.id, tx.at).stage;
                if tx.landed == Some(false) && stage != CLAIMED {
                    fail(format!(
                        "block {}: a transaction carrying the user's share reverted with the swap at stage {stage}",
                        tx.at
                    ));
                    break;
                }
            }
            let head = self.chain.node.head().0;
            let exposed = carrying
                .iter()
                .map(|tx| tx.at + 1)
                .min()
                .unwrap_or(u64::MAX);
            for block in exposed..=head {
                if self.chain.would_refund_at(swap.id, &swap.opened, &e, block) {
                    fail(format!(
                        "block {block}: the user's share is public while the maker may refund"
                    ));
                    break;
                }
            }
            if recorded.stage == CLAIMED && recorded.secret != U256::from_be_bytes(z.to_be_bytes())
            {
                fail("the swap was claimed with another secret than the user's share".into());
            }
            if recorded.stage == REFUNDED && recorded.secret != U256::from_be_bytes(e.to_be_bytes())
            {
                fail("the swap was refunded with another secret than the maker's share".into());
            }
            if deposit.is_some() && settled {
                match recorded.stage {
                    CLAIMED => self.paid(swap.opened.amount, &saved, recorded.paid_out, &mut fail),
                    REFUNDED => {
                        if saved.ended != Some(End::SweptBack(swap.deposit_zat)) {
                            fail(format!(
                                "the swap was refunded and the wallet ended {:?}, its deposit not swept back",
                                saved.ended
                            ));
                        }
                    }
                    other => fail(format!(
                        "the deposit is stuck: the swap is at stage {other}"
                    )),
                }
            }
        }
        if self.plan.calm() {
            let fine = match (self.plan.mood, self.plan.fee, &saved.ended) {
                (_, Fee::Transparent(_), None) => true,
                (_, Fee::Asked(fee), None) if fee > self.plan.max_open_fee => true,
                (_, Fee::Transparent(_), _) => false,
                (_, Fee::Asked(fee), _) if fee > self.plan.max_open_fee => false,
                (
                    Mood::Honest | Mood::NeverReadies | Mood::Vanishes | Mood::OpensFar,
                    _,
                    Some(End::Paid(_)),
                ) => true,
                (Mood::CancelsEarly, _, Some(End::Cancelled | End::SweptBack(_))) => true,
                (Mood::KeepsFee, _, Some(End::Abandoned(_))) => true,
                (mood, _, None | Some(End::Cancelled)) if mood.cheats_on_terms() => true,
                _ => false,
            };
            if !fine {
                fail(format!(
                    "in calm weather a {:?} maker asking {:?} left the wallet at {:?}",
                    self.plan.mood, self.plan.fee, saved.ended
                ));
            }
        }
        Report {
            seed: self.seed.0,
            plan: self.plan.clone(),
            ended: saved.ended,
            stage,
            failures,
            trace: self.chain.node.trace(),
        }
    }

    fn paid(&self, amount: u128, saved: &Saved, paid_out: bool, fail: &mut impl FnMut(String)) {
        if !matches!(saved.ended, Some(End::Paid(_))) {
            fail(format!(
                "the swap was claimed and the wallet ended {:?}",
                saved.ended
            ));
        }
        if !self.plan.railgun {
            let held = self.chain.token_balance(key(self.seed, "user").address());
            if held != amount {
                fail(format!(
                    "the user's account holds {held} of the {amount} claimed"
                ));
            }
            return;
        }
        let Some(index) = saved.quoted.as_ref().map(|quoted| quoted.index) else {
            return;
        };
        let (shielded, held) = shielded_to(&self.chain, &self.user_seed, index, amount);
        if !paid_out || !held {
            fail(format!(
                "the swap was claimed and {shielded} never reached the user's note (paid out: {paid_out})"
            ));
        }
    }
}

pub(crate) fn shielded_to(
    chain: &Chain,
    user_seed: &[u8; 64],
    index: u32,
    amount: u128,
) -> (u128, bool) {
    let keys = derive_user_keys(user_seed, NetworkType::Test, 0, index).expect("keys");
    let note = RailgunKeys::from_seed(user_seed, 0)
        .note(&keys.note_entropy)
        .expect("the user's note");
    let value = amount - FEE;
    let shielded = value - value * 25 / 10_000;
    let ours = (B256::from(note.npk), shielded);
    (shielded, chain.shielded().contains(&ours))
}

pub(crate) fn notes(node: &Node) -> Arc<dyn Fn(String) + Send + Sync> {
    let node = node.clone();
    Arc::new(move |what| node.note(what))
}
