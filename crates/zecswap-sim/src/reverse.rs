mod wallet;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::U256;
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep};
use zecswap_chain::evm::{Reveal, Stage, Window, Windows, simulate_wall};
use zecswap_core::{NetworkType, SecretShare, derive_user_keys};

pub use crate::maker::ReverseMood;
use crate::maker::{
    Escrow, MAX_READY_WAIT, REVERSE_T0_TO_T1, REVERSE_TO_T0, ReverseMaker, ReverseMakerDoor,
};
use crate::net::{Line, Net};
use crate::node::{Node, Published, SLOT, Weather};
pub use crate::relayer::ReverseService;
use crate::relayer::{Errand, FEE, Relayer, ReverseRelayer, ReverseRelayerDoor, Service};
use crate::run::{Report, SETTLE, connect, lives, notes, paused, shielded_to, ticks, zcash_blocks};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, Chain, EPOCH, LATE_WINDOW, MIN_WINDOW, Skewed, UNIT, key};
use crate::zcash::Zcash;
pub use wallet::Ended;
use wallet::{CLAIMED, OPEN, READY, READY_LIFE, REFUNDED, Wallet};

const T0: u64 = REVERSE_TO_T0;
const T1: u64 = REVERSE_TO_T0 + REVERSE_T0_TO_T1;
const HORIZON: u64 = T1 + 75 * 60;
const REFUND_TIME: u64 = 20 * 60;

#[derive(Clone, Debug)]
pub struct Plan {
    pub weather: Weather,
    pub maker_line: Line,
    pub relayer_line: Line,
    pub mood: ReverseMood,
    pub service: ReverseService,
    pub private: bool,
    pub crashes: Vec<(u64, u64)>,
    pub skew: i64,
    pub patience: Option<u64>,
    pub regret: Option<u64>,
    pub linger_ready: Option<i64>,
    pub linger_refund: Option<i64>,
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
            mood: ReverseMood::Honest,
            service: ReverseService::Honest,
            private: false,
            crashes: Vec::new(),
            skew: 0,
            patience: None,
            regret: None,
            linger_ready: None,
            linger_refund: None,
            clears: 0,
            zcash_loss: 0,
            units: 1,
        }
    }

    pub fn from_seed(seed: Seed) -> Self {
        let roll = |what: &str, bound: u64| seed.below(&format!("reverse-{what}"), 0, bound);
        let calm = roll("sky", 100) < 20;
        let up_to = |what: &str, most: u64| if calm { 0 } else { roll(what, most + 1) };
        let line = |who: &str| Line {
            busy: up_to(&format!("{who}-busy"), 150),
            hang: up_to(&format!("{who}-hang"), 8),
            unheard: up_to(&format!("{who}-unheard"), 40),
            unanswered: up_to(&format!("{who}-unanswered"), 40),
        };
        let mood = match roll("mood", 100) {
            0..36 => ReverseMood::Honest,
            36..42 => ReverseMood::NeverDeposits,
            42..48 => ReverseMood::DepositsLess,
            48..54 => ReverseMood::DepositsLate(roll("deposit-lead", 15 * 60)),
            54..60 => ReverseMood::NeverClaims,
            60..64 => ReverseMood::ClaimsLate(10 + roll("claim-lateness", 120)),
            64..67 => ReverseMood::ClaimsAtTheWire,
            67..70 => ReverseMood::LocksLong,
            70..73 => ReverseMood::LiesAboutDeadlines(roll("lie", 4)),
            73..77 => ReverseMood::LiesAboutNote,
            77..80 => ReverseMood::LiesAboutUser,
            80..84 => ReverseMood::LiesAboutAmounts(roll("lie", 3)),
            84..88 => ReverseMood::LiesAboutDeployment(roll("lie", 3)),
            88..93 => ReverseMood::BadProof,
            _ => ReverseMood::ReportsAnotherSwap,
        };
        let mut crashes: Vec<(u64, u64)> = (0..up_to("crashes", 3))
            .map(|n| {
                (
                    seed.between("reverse-crash-at", n, 30, T1),
                    seed.between("reverse-crash-rest", n, 5, 300),
                )
            })
            .collect();
        let away = match up_to("away", 7) {
            1 => Some(T0 + seed.between("reverse-back-near-t0", 0, 0, 6 * 60) - 3 * 60),
            2 | 3 => Some(T1 + seed.between("reverse-back-near-t1", 0, 0, 6 * 60) - 3 * 60),
            4 => Some(T1 + LATE_WINDOW + seed.between("reverse-back-late", 0, 0, 5 * 60)),
            _ => None,
        };
        if let Some(back) = away {
            let left = seed.between("reverse-left-at", 0, 3 * 60, 12 * 60);
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
            service: match roll("service", 10) {
                0 => ReverseService::SkipsPayouts,
                1 => ReverseService::SkipsOpen,
                2 | 3 => ReverseService::Careless,
                4 => ReverseService::BusyWithRefunds,
                5 => ReverseService::LosesReadyAnswers,
                _ => ReverseService::Honest,
            },
            private: roll("private", 2) == 0,
            crashes,
            skew: if calm {
                0
            } else {
                roll("skew", 181) as i64 - 90
            },
            patience: (up_to("patience", 5) == 1).then(|| roll("patience-after", 30 * 60)),
            regret: (up_to("regret", 7) == 1).then(|| roll("regret-after", 180)),
            linger_ready: (up_to("linger-ready", 7) == 1)
                .then(|| roll("linger-ready-shift", 121) as i64 - 60),
            linger_refund: (up_to("linger-refund", 7) == 1)
                .then(|| roll("linger-refund-shift", 121) as i64 - 60),
            clears: seed.between("reverse-clears", 0, 10 * 60, T1 + 20 * 60),
            zcash_loss: up_to("zcash-loss", 100),
            units: 1 + roll("units", 2) as u32,
        };
        let focus = roll("focus", 12);
        if focus < 7 {
            plan.crashes.clear();
            plan.patience = None;
            plan.regret = None;
            plan.linger_ready = None;
            plan.linger_refund = None;
            plan.weather.lost = 0;
            plan.weather.rpc_stall = 0;
        }
        match focus {
            0 => {
                plan.mood = ReverseMood::Honest;
                plan.service = match roll("focus-relayer", 3) {
                    0 => ReverseService::Careless,
                    _ => ReverseService::LosesReadyAnswers,
                };
                plan.regret = Some(roll("focus-regret", 61));
                plan.weather.delayed = 600;
            }
            1 => {
                plan.mood = ReverseMood::Honest;
                plan.service = ReverseService::BusyWithRefunds;
                plan.patience = Some(30 + roll("focus-patience", 171));
            }
            2 => {
                plan.mood = ReverseMood::DepositsLate(READY_LIFE + 130 + roll("focus-lead", 241));
                plan.linger_ready = (roll("focus-linger", 2) == 0)
                    .then(|| roll("focus-linger-shift", 121) as i64 - 60);
            }
            3 => {
                plan.mood = match roll("focus-short", 2) {
                    0 => ReverseMood::DepositsLess,
                    _ => ReverseMood::NeverDeposits,
                };
            }
            4 => {
                plan.mood = ReverseMood::Honest;
                plan.service = ReverseService::Careless;
                plan.weather.ambush = 1000;
                plan.weather.fork_delay = 900;
            }
            5 => {
                plan.mood = match roll("focus-wire", 3) {
                    0 => ReverseMood::ClaimsAtTheWire,
                    _ => ReverseMood::ClaimsLate(5 + roll("focus-lateness", 36)),
                };
                plan.linger_refund = Some(roll("focus-linger-refund", 61) as i64 - 30);
                plan.weather.delayed = 600;
                plan.weather.reorg_after_tx = 450;
            }
            6 => {
                let kind = || roll("focus-lie-kind", 3);
                plan.mood = match roll("focus-lie", 10) {
                    0 | 1 => ReverseMood::LiesAboutDeadlines(kind()),
                    2 => ReverseMood::LiesAboutNote,
                    3 => ReverseMood::LiesAboutUser,
                    4 => ReverseMood::LiesAboutAmounts(kind()),
                    5 => ReverseMood::LiesAboutDeployment(kind()),
                    6 => ReverseMood::BadProof,
                    7 => ReverseMood::ReportsAnotherSwap,
                    _ => ReverseMood::LocksLong,
                };
            }
            _ => {}
        }
        plan
    }

    fn calm(&self) -> bool {
        self.weather.rpc_error == 0
            && self.weather.delayed == 0
            && self.weather.reorg == 0
            && self.weather.reorg_after_tx == 0
            && self.weather.ambush == 0
            && self.maker_line.busy == 0
            && self.relayer_line.busy == 0
            && self.relayer_line.unheard == 0
            && self.crashes.is_empty()
            && self.patience.is_none()
            && self.regret.is_none()
            && self.linger_ready.is_none()
            && self.linger_refund.is_none()
    }
}

pub fn run(seed: u64) -> Report<Plan, Ended> {
    run_with(seed, Plan::from_seed(Seed(seed)))
}

pub fn run_with(seed: u64, plan: Plan) -> Report<Plan, Ended> {
    paused(simulate(Seed(seed), plan))
}

struct World {
    seed: Seed,
    plan: Plan,
    chain: Chain,
    maker: Arc<ReverseMaker>,
    relayer: Arc<ReverseRelayer>,
    wallet: Arc<Wallet>,
    user_seed: [u8; 64],
    disagreements: Arc<Mutex<Vec<String>>>,
}

async fn simulate(seed: Seed, plan: Plan) -> Report<Plan, Ended> {
    simulate_wall(Some(EPOCH as i64));
    let start = Instant::now();
    let node = Node::new(seed, CHAIN_ID, EPOCH - SLOT, plan.weather);
    let chain = Chain::deploy(&node, true);
    let (maker_key, relayer_key, funder_key) = (
        key(seed, "maker"),
        key(seed, "relayer"),
        key(seed, "funder"),
    );
    for who in [&maker_key, &relayer_key, &funder_key] {
        chain.fund(who.address());
    }
    chain.mint(funder_key.address(), 100 * UNIT);
    node.clear_from(EPOCH + plan.clears);
    let zcash = Zcash::default();
    let maker = ReverseMaker::new(
        seed,
        plan.mood,
        chain.clone(),
        zcash.clone(),
        connect(&node, &chain, "maker", maker_key, plan.private),
    );
    let relayer = ReverseRelayer::new(
        plan.service,
        Relayer::new(
            Service::Honest,
            chain.clone(),
            connect(&node, &chain, "relayer", relayer_key, plan.private),
        ),
        funder_key,
    );
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
            Arc::new(ReverseMakerDoor(maker.clone())),
            log.clone(),
        ),
        relayer: Net::to(
            seed,
            "relayer",
            plan.relayer_line,
            clears,
            Arc::new(ReverseRelayerDoor(relayer.clone())),
            log,
        ),
        units: plan.units,
        max_fee: 2 * FEE,
        patience: plan.patience,
        regret: plan.regret,
        linger_ready: plan.linger_ready,
        linger_refund: plan.linger_refund,
        saved: Arc::default(),
        signed: Arc::default(),
        asked: Arc::default(),
        said: Arc::default(),
    });
    let world = Arc::new(World {
        seed,
        plan: plan.clone(),
        chain: chain.clone(),
        maker: maker.clone(),
        relayer,
        wallet: wallet.clone(),
        user_seed,
        disagreements: Arc::default(),
    });
    let background = [
        spawn_local(ticks(node.clone(), start)),
        spawn_local(zcash_blocks(zcash, seed, plan.zcash_loss, clears)),
        spawn_local(maker.tend()),
        spawn_local({
            let world = world.clone();
            async move { world.observe().await }
        }),
    ];
    lives(&node, start, HORIZON, &plan.crashes, || {
        spawn_local(Skewed::new(plan.skew, wallet.clone().live()))
    })
    .await;
    for task in background {
        task.abort();
    }
    world.report()
}

fn carry(tx: &Published, secret: &SecretShare) -> bool {
    tx.input
        .windows(32)
        .any(|window| window == secret.to_be_bytes())
}

impl World {
    fn escrow(&self) -> Option<(Escrow, u32, SecretShare)> {
        let swap = self.wallet.saved().swap?;
        let escrow = self
            .maker
            .escrows()
            .into_iter()
            .find(|escrow| escrow.id == swap.swap_id)?;
        let keys = derive_user_keys(&self.user_seed, NetworkType::Test, 0, swap.index).ok()?;
        Some((escrow, swap.index, keys.share))
    }

    async fn observe(&self) {
        let windows = Windows {
            min: MIN_WINDOW,
            late: LATE_WINDOW,
        };
        loop {
            sleep(Duration::from_secs(SLOT)).await;
            let (number, now) = self.chain.node.head();
            let Some((escrow, _, z)) = self.escrow() else {
                continue;
            };
            let stage = match self.chain.recorded(escrow.id).stage {
                OPEN => Stage::Open,
                READY => Stage::Ready,
                _ => continue,
            };
            let deadlines = (escrow.terms.t0, escrow.terms.t1);
            let judged = |reveal| windows.window(reveal, true, stage, deadlines, now);
            let e = self.maker.share(escrow.nonce);
            let claim = self.chain.would_claim(escrow.id, &escrow.terms, &e);
            let refund = self.chain.would_refund(escrow.id, &escrow.terms, &z);
            let mut wrong = Vec::new();
            if (judged(Reveal::Claim) != Window::Shut) != claim {
                wrong.push(format!(
                    "the client judges the maker's claim {:?}, the contract takes one: {claim}",
                    judged(Reveal::Claim)
                ));
            }
            if (judged(Reveal::Refund) != Window::Shut) != refund {
                wrong.push(format!(
                    "the client judges the user's refund {:?}, the contract takes one: {refund}",
                    judged(Reveal::Refund)
                ));
            }
            if claim && refund {
                wrong.push("both sides may reveal at once".into());
            }
            let held = self
                .relayer
                .heard()
                .iter()
                .any(|heard| heard.id == escrow.id && heard.errand == Errand::Refund);
            if claim && held {
                wrong.push("the relayer holds the user's share while the maker may claim".into());
            }
            let published = self.chain.node.published();
            let wasted = |secret: &SecretShare| {
                published
                    .iter()
                    .any(|tx| tx.landed == Some(false) && tx.at + 1 >= number && carry(tx, secret))
            };
            if wasted(&z) {
                wrong.push("a transaction carrying the user's share reverted".into());
            }
            if self.plan.mood.keeps_its_margin() && wasted(&e) {
                wrong.push("a transaction carrying the maker's share reverted".into());
            }
            for what in wrong {
                self.disagreements
                    .lock()
                    .unwrap()
                    .push(format!("block {number} at {now}, {stage:?}: {what}"));
            }
        }
    }

    fn report(&self) -> Report<Plan, Ended> {
        let saved = self.wallet.saved();
        let heard = self.relayer.heard();
        let mut failures = self.disagreements.lock().unwrap().clone();
        failures.truncate(5);
        let mut fail = |what: String| failures.push(what);

        let funding: Vec<&Vec<_>> = heard
            .iter()
            .filter_map(|heard| match &heard.errand {
                Errand::Funding(to) => Some(to),
                _ => None,
            })
            .collect();
        if self.plan.mood.lies() && !funding.is_empty() {
            fail(format!(
                "the wallet asked to fund the quote of a maker that {:?}",
                self.plan.mood
            ));
        }
        if let Some(to) = funding
            .iter()
            .find(|to| to.as_slice() != [self.chain.token, self.chain.contract])
        {
            fail(format!(
                "the wallet's funding calls go to {to:?}, not to the deployment's token and contract"
            ));
        }

        let escrow = self.escrow();
        let stage = escrow
            .as_ref()
            .map(|(escrow, _, _)| self.chain.recorded(escrow.id).stage);
        if let Some((escrow, index, z)) = &escrow {
            self.judge(escrow, *index, z, &saved, &heard, &mut fail);
        }
        if self.plan.calm() {
            let fine = match (self.plan.mood, &saved.ended) {
                (ReverseMood::Honest, Some(Ended::Received(_))) => true,
                (
                    ReverseMood::NeverDeposits
                    | ReverseMood::DepositsLess
                    | ReverseMood::NeverClaims
                    | ReverseMood::ClaimsLate(_),
                    Some(Ended::Refunded),
                ) => true,
                (
                    ReverseMood::DepositsLate(_) | ReverseMood::ClaimsAtTheWire,
                    Some(Ended::Received(_) | Ended::Refunded),
                ) => true,
                (ReverseMood::LocksLong, Some(Ended::Received(_) | Ended::Abandoned(_))) => true,
                (mood, None | Some(Ended::Abandoned(_))) if mood.lies() => true,
                (ReverseMood::ReportsAnotherSwap, Some(Ended::Abandoned(_))) => true,
                _ => false,
            };
            if !fine {
                fail(format!(
                    "in calm weather a {:?} maker left the wallet at {:?}",
                    self.plan.mood, saved.ended
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

    fn judge(
        &self,
        escrow: &Escrow,
        index: u32,
        z: &SecretShare,
        saved: &wallet::Saved,
        heard: &[crate::relayer::Heard],
        fail: &mut impl FnMut(String),
    ) {
        let (id, terms) = (escrow.id, &escrow.terms);
        let e = self.maker.share(escrow.nonce);
        let recorded = self.chain.recorded(id);
        let open_at = |block: u64| self.chain.recorded_at(id, block).stage == OPEN;
        let signed = self.wallet.signed();
        for ready in &signed {
            if ready.spendable < escrow.deposit_zat {
                fail(format!(
                    "block {}: a ready was signed with {} of the {} zat deposit spendable",
                    ready.at, ready.spendable, escrow.deposit_zat
                ));
            }
            if ready.deadline >= terms.t0 {
                fail(format!(
                    "block {}: a ready was signed to last until {}, not before t0 at {}",
                    ready.at, ready.deadline, terms.t0
                ));
            }
            if ready.refunding {
                fail(format!(
                    "block {}: a ready was signed after a refund was asked for",
                    ready.at
                ));
            }
        }
        let landing = |time: u64, deadlines: &mut dyn Iterator<Item = u64>| {
            deadlines
                .filter(|until| time < terms.t0 && time <= *until)
                .max()
        };
        for asked in self.wallet.asked() {
            let deadlines = &mut signed.iter().map(|ready| ready.deadline);
            if let Some(until) = landing(asked.time, deadlines).filter(|_| open_at(asked.at)) {
                fail(format!(
                    "block {}: a refund was asked for at {} while a ready signed until {until} may still land",
                    asked.at, asked.time
                ));
            }
        }
        let mut refund_heard = None;
        let mut readies = Vec::new();
        let mut funded = false;
        for heard in heard.iter().filter(|heard| heard.id == id) {
            match heard.errand {
                Errand::Funding(_) if !funded => {
                    funded = true;
                    if terms.t1 > heard.time + MAX_READY_WAIT {
                        fail(format!(
                            "block {}: the wallet asked to fund an escrow that locks its USDC until {}, over a day away",
                            heard.at, terms.t1
                        ));
                    }
                }
                Errand::Ready(deadline) => {
                    if let Some(at) = refund_heard {
                        fail(format!(
                            "block {}: the relayer heard a ready after it heard a refund at block {at}",
                            heard.at
                        ));
                    }
                    if deadline >= terms.t0 {
                        fail(format!(
                            "block {}: the relayer heard a ready lasting until {deadline}, not before t0 at {}",
                            heard.at, terms.t0
                        ));
                    }
                    readies.push(deadline);
                }
                Errand::Refund => {
                    refund_heard.get_or_insert(heard.at);
                    let deadlines = &mut readies.iter().copied();
                    if let Some(until) =
                        landing(heard.time, deadlines).filter(|_| open_at(heard.at))
                    {
                        fail(format!(
                            "block {}: the relayer heard a refund at {} while a ready signed until {until} may still land",
                            heard.at, heard.time
                        ));
                    }
                }
                Errand::Funding(_) => {}
            }
        }

        let published = self.chain.node.published();
        let head = self.chain.node.head().0;
        let carrying = |secret: &SecretShare| -> Vec<&Published> {
            published.iter().filter(|tx| carry(tx, secret)).collect()
        };
        let user = carrying(z);
        if !user.is_empty() && recorded.stage != REFUNDED {
            fail(format!(
                "the user's share is public and the escrow is at stage {}: the maker can take the USDC and the ZEC",
                recorded.stage
            ));
        }
        if let Some(tx) = user.iter().find(|tx| {
            tx.landed == Some(false) && self.chain.recorded_at(id, tx.at).stage != REFUNDED
        }) {
            fail(format!(
                "block {}: a transaction carrying the user's share reverted with the escrow at stage {}",
                tx.at,
                self.chain.recorded_at(id, tx.at).stage
            ));
        }
        let exposed = user.iter().map(|tx| tx.at + 1).min().unwrap_or(u64::MAX);
        if let Some(block) =
            (exposed..=head).find(|block| self.chain.would_claim_at(id, terms, &e, *block))
        {
            fail(format!(
                "block {block}: the user's share is public while the maker may claim"
            ));
        }
        if self.plan.mood.keeps_its_margin() {
            let maker = carrying(&e);
            if !maker.is_empty() && recorded.stage != CLAIMED {
                fail(format!(
                    "the maker's share is public and the escrow is at stage {}: the user can take the ZEC and the USDC",
                    recorded.stage
                ));
            }
            let exposed = maker.iter().map(|tx| tx.at + 1).min().unwrap_or(u64::MAX);
            if let Some(block) =
                (exposed..=head).find(|block| self.chain.would_refund_at(id, terms, z, *block))
            {
                fail(format!(
                    "block {block}: the maker's share is public while the user may refund"
                ));
            }
        }

        if recorded.stage == CLAIMED && recorded.secret != U256::from_be_bytes(e.to_be_bytes()) {
            fail("the escrow was claimed with another secret than the maker's share".into());
        }
        if recorded.stage == REFUNDED && recorded.secret != U256::from_be_bytes(z.to_be_bytes()) {
            fail("the escrow was refunded with another secret than the user's share".into());
        }
        let settled = self.plan.clears + SETTLE.as_secs() + 30 * 60 < HORIZON
            && terms.t1 + REFUND_TIME < EPOCH + HORIZON;
        if recorded.stage == 0 || !settled {
            return;
        }
        let received =
            matches!(saved.ended, Some(Ended::Received(zat)) if zat >= escrow.deposit_zat);
        match recorded.stage {
            READY if received && !self.plan.mood.keeps_its_margin() => {}
            CLAIMED => {
                if !received {
                    fail(format!(
                        "the escrow was claimed and the wallet ended {:?}, not with the {} zat deposit",
                        saved.ended, escrow.deposit_zat
                    ));
                }
            }
            REFUNDED => {
                if saved.ended != Some(Ended::Refunded) {
                    fail(format!(
                        "the escrow was refunded and the wallet ended {:?}",
                        saved.ended
                    ));
                }
                let (shielded, held) =
                    shielded_to(&self.chain, &self.user_seed, index, terms.amount);
                if !recorded.paid_out || !held {
                    fail(format!(
                        "the escrow was refunded and {shielded} never reached the user's refund note (paid out: {})",
                        recorded.paid_out
                    ));
                }
            }
            other => fail(format!("the escrow is stuck at stage {other}")),
        }
    }
}
