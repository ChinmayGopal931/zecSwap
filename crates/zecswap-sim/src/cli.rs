mod desk;
mod person;
mod seen;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::signers::local::PrivateKeySigner;
use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep, sleep_until};
use zcash_protocol::consensus::{Network, NetworkUpgrade, Parameters};
use zecswap_api::relayer::deposit_fee;
use zecswap_chain::evm::{Settlement, simulate_wall};
use zecswap_chain::zcash::UnifiedSpendingKey;
use zecswap_client::{FeeCap, MIN_TIME_TO_T0};
use zecswap_core::{NetworkType, SecretShare, derive_deposit_keys, derive_user_keys};

use crate::alerts::Alarms;
use crate::deposits::{self, Offer};
use crate::lightwallet::ledger::{Ledger, Sky};
use crate::lightwallet::server::Server;
use crate::lightwallet::{self, Scratch};
use crate::maker::{Fee, Maker, MakerDoor, Mood, T0_TO_T1, TO_T0};
use crate::net::{Line, Net};
use crate::node::{Node, SLOT, Weather};
use crate::panics;
use crate::relayer::{Funder, Relayer, RelayerDoor, Service};
use crate::run::{Report, SETTLE, connect, notes, paused, shielded_to, ticks};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, Chain, EPOCH, LATE_WINDOW, UNIT, UNIT_ZAT, key};
pub use desk::{Aim, Kill, Step, Tear};
use desk::{Desk, Kept};
use person::Person;
use seen::Seen;

const SPACING: u64 = 75;
const SEASON: u32 = 13;
const OPEN_FEE: u64 = 40_000;
const PAYMENT_FEE: u64 = 10_000;
const SWAP_TIME: u64 = TO_T0 + T0_TO_T1 + LATE_WINDOW + 30 * 60;
const UNTRUSTED: u64 = 10;
const OWN: usize = 0;
const CLAIMED: u8 = 3;
const REFUNDED: u8 = 4;
const SHORT_REST: u64 = 120;
const DEPOSIT_TIME: u64 = 40 * 60;
pub(crate) const MAX_FEE: FeeCap = FeeCap {
    fee: 1_000_000,
    fee_bps: 50,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Funds {
    One,
    Several,
    Short,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    Shown,
    Index(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrival {
    pub at: u64,
    pub to: Spot,
    pub amount: u128,
}

#[derive(Clone, Debug)]
pub struct Deposits {
    pub service: deposits::Service,
    pub offer: Offer,
    pub line: Line,
    pub arrivals: Vec<Arrival>,
    pub typed: Vec<u64>,
    pub kills: Vec<Kill>,
}

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
    pub multicall: bool,
    pub units: u32,
    pub sky: Sky,
    pub lightwalletd: lightwallet::Service,
    pub funds: Funds,
    pub swaps: u32,
    pub stale: bool,
    pub kills: Vec<Kill>,
    pub skew: i64,
    pub clears: u64,
    pub deposits: Option<Deposits>,
}

fn reachable(mood: Mood, fee: Fee, max_open_fee: u64) -> Vec<Step> {
    let paid = matches!(fee, Fee::Asked(fee) if fee <= max_open_fee);
    let refunded = mood == Mood::CancelsEarly;
    let opened = !mood.cheats_on_terms() && mood != Mood::KeepsFee;
    let steps = Step::ALL.into_iter().filter(|step| match step {
        Step::Index | Step::Quote | Step::Finish => true,
        Step::Fee | Step::FeeOut => paid,
        Step::Accept => paid || fee == Fee::None,
        Step::Joint | Step::Deposit | Step::DepositOut => opened,
        Step::SweepOut => refunded,
        Step::Pending | Step::Unpending => false,
    });
    steps.collect()
}

fn killing(seed: Seed, n: u64, steps: &[Step], short: bool, refunds: bool) -> Kill {
    let pick = |what: &str, bound: u64| seed.below(&format!("cli-{what}"), n, bound);
    let sends: Vec<Step> = steps
        .iter()
        .copied()
        .filter(|step| matches!(step, Step::FeeOut | Step::DepositOut | Step::SweepOut))
        .collect();
    let aim = match pick("aim", 20) {
        0..4 => Aim::At(seed.between("cli-kill-at", n, 5, 30 * 60)),
        4 | 5 if !sends.is_empty() => {
            Aim::Unsent(sends[pick("unsent-step", sends.len() as u64) as usize])
        }
        16 | 17 => Aim::Ready(pick("kill-ready", 91)),
        18 | 19 if refunds => Aim::Refunded(pick("kill-refunded", 121)),
        _ => Aim::Tear(Tear {
            step: steps[pick("kill-step", steps.len() as u64) as usize],
            after: pick("kill-after", 2) == 1,
        }),
    };
    let rest = match (short, pick("rest-kind", 4)) {
        (false, 0) => seed.between("cli-rest-long", n, 50 * 60, 70 * 60),
        _ => seed.between("cli-rest", n, 2, SHORT_REST),
    };
    Kill { aim, rest }
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
            max_open_fee: 500_000,
            railgun: true,
            multicall: true,
            units: 1,
            sky: Sky {
                steady: SEASON,
                ..Sky::CALM
            },
            lightwalletd: lightwallet::Service::HONEST,
            funds: Funds::One,
            swaps: 1,
            stale: false,
            kills: Vec::new(),
            skew: 0,
            clears: 0,
            deposits: None,
        }
    }

    pub fn from_seed(seed: Seed) -> Self {
        let roll = |what: &str, bound: u64| seed.below(&format!("cli-{what}"), 0, bound);
        let kind = roll("kind", 100);
        let calm = kind < 40;
        let up_to = |what: &str, most: u64| if calm { 0 } else { roll(what, most + 1) };
        let mood = match roll("mood", 100) {
            0..55 => Mood::Honest,
            55..62 => Mood::NeverReadies,
            62..68 => Mood::Vanishes,
            68..78 => Mood::CancelsEarly,
            78..83 => Mood::KeepsFee,
            83..88 => Mood::OpensLess,
            88..92 => Mood::OpensAnotherNote,
            92..96 => Mood::OpensFar,
            _ => Mood::LiesAboutDeadlines,
        };
        let (fee, max_open_fee) = match roll("fee", 10) {
            _ if mood == Mood::KeepsFee => (Fee::Asked(OPEN_FEE), OPEN_FEE),
            0..5 => (Fee::None, 500_000),
            5..8 => (Fee::Asked(OPEN_FEE), 500_000),
            8 => (Fee::Asked(OPEN_FEE), OPEN_FEE - 1),
            _ => (Fee::Transparent(OPEN_FEE), 500_000),
        };
        let line = |who: &str| Line {
            busy: up_to(&format!("{who}-busy"), 120),
            hang: up_to(&format!("{who}-hang"), 6),
            unheard: up_to(&format!("{who}-unheard"), 30),
            unanswered: up_to(&format!("{who}-unanswered"), 30),
        };
        let steps = reachable(mood, fee, max_open_fee);
        let refunds = mood == Mood::CancelsEarly;
        let kills = match kind {
            0..20 => 0,
            20..40 => 1 + roll("kills", 3),
            _ => up_to("kills", 3),
        };
        let kills = (0..kills)
            .map(|n| killing(seed, n, &steps, calm, refunds))
            .collect();
        let deposits = (roll("deposits", 4) == 0).then(|| Self::deposits(seed, calm));
        let mut plan = Self {
            weather: Weather {
                reorg: up_to("reorg", 20),
                reorg_after_tx: up_to("reorg-after-tx", 350),
                ambush: up_to("ambush", 300),
                reorg_depth: 2,
                fork_delay: up_to("fork-delay", 900),
                missed_slot: up_to("missed-slot", 60),
                delayed: up_to("delayed", 150),
                delay_blocks: 3,
                lost: up_to("lost", 30),
                private_lost: 0,
                fee_swing: up_to("fee-swing", 500),
                rpc_error: up_to("rpc-error", 30),
                rpc_stall: up_to("rpc-stall", 3),
                stale_read: up_to("stale-read", 120),
            },
            maker_line: line("maker"),
            relayer_line: line("relayer"),
            mood,
            service: match roll("service", 6) {
                0 => Service::SkipsPayouts,
                _ => Service::Honest,
            },
            fee,
            max_open_fee,
            railgun: roll("route", 4) != 0,
            multicall: roll("multicall", 2) == 0,
            units: 1 + roll("units", 2) as u32,
            sky: Sky {
                steady: SEASON,
                reorg: up_to("zcash-reorg", 30),
                reorg_after_tx: up_to("zcash-reorg-after-tx", 300),
                reorg_depth: 1 + roll("zcash-reorg-deep", 2),
                dropped: 0,
                delayed: up_to("zcash-delayed", 250),
                lost: up_to("zcash-lost", 120),
                quick: up_to("zcash-quick", 250),
                traffic: roll("zcash-traffic", 1001),
                rival: 0,
            },
            lightwalletd: lightwallet::Service {
                errors: up_to("lwd-errors", 30),
                slow: up_to("lwd-slow", 80),
                stalls: up_to("lwd-stalls", 10),
                cuts: up_to("lwd-cuts", 100),
                lags: up_to("lwd-lags", 150),
                deaf: up_to("lwd-deaf", 150),
                hangups: up_to("lwd-hangups", 15),
                misplaces: up_to("lwd-misplaces", 600),
                backwards: false,
                another: false,
                quiet: roll("lwd-quiet", 2) == 0,
            },
            funds: match roll("funds", 10) {
                0..5 => Funds::One,
                5..7 => Funds::Several,
                7 => Funds::Short,
                _ => Funds::Unconfirmed,
            },
            swaps: 1 + u32::from(roll("swaps", 4) == 0),
            stale: roll("stale", 12) == 0,
            kills,
            skew: match calm {
                true => 0,
                false => roll("skew", 121) as i64 - 60,
            },
            clears: match calm {
                true => 0,
                false => seed.between("cli-clears", 0, 10 * 60, 60 * 60),
            },
            deposits,
        };
        let focus = match kind {
            40.. => roll("focus", 20),
            _ => u64::MAX,
        };
        match focus {
            0 | 1 => {
                plan.mood = Mood::CancelsEarly;
                plan.sky.delayed = 600;
                plan.sky.lost = 250;
                plan.clears = plan.clears.max(80 * 60);
                let tear = Tear {
                    step: Step::Finish,
                    after: false,
                };
                let sweep = Tear {
                    step: Step::SweepOut,
                    after: false,
                };
                let aim = match (focus, roll("focus-sweep", 3)) {
                    (0, _) => Some(Aim::Tear(tear)),
                    (_, 0) => Some(Aim::Tear(sweep)),
                    (_, 1) => Some(Aim::Unsent(Step::SweepOut)),
                    _ => None,
                };
                let rest = seed.between("cli-focus-rest", 0, 2, SHORT_REST);
                plan.kills = aim.map(|aim| Kill { aim, rest }).into_iter().collect();
            }
            2 => {
                (plan.fee, plan.max_open_fee) = (Fee::Asked(OPEN_FEE), 500_000);
                plan.funds = Funds::Short;
                plan.mood = Mood::Honest;
            }
            3 => {
                (plan.fee, plan.max_open_fee) = (Fee::Asked(OPEN_FEE), 500_000);
                plan.mood = Mood::Honest;
                plan.funds = Funds::One;
                plan.kills.clear();
                plan.sky.lost = 1000;
                plan.sky.reorg = 0;
                plan.sky.reorg_after_tx = 0;
                plan.lightwalletd = lightwallet::Service::HONEST;
                plan.clears = plan.typed_at() + seed.between("cli-focus-clears", 0, 60, 180);
                if roll("focus-fee-refused", 2) == 0 {
                    plan.kills = vec![Kill {
                        aim: Aim::Unsent(Step::FeeOut),
                        rest: seed.between("cli-focus-rest", 0, 2, SHORT_REST),
                    }];
                }
            }
            _ => {}
        }
        plan
    }

    fn deposits(seed: Seed, calm: bool) -> Deposits {
        let pick =
            |what: &str, n: u64, bound: u64| seed.below(&format!("cli-deposit-{what}"), n, bound);
        let arrival = |n: u64| Arrival {
            at: 60 + pick("at", n, 25 * 60),
            to: match pick("where", n, 4) {
                0 => Spot::Index(pick("index", n, 4) as u32),
                _ => Spot::Shown,
            },
            amount: u128::from(match pick("amount", n, 10) {
                0 => 1_000_000 + pick("dust", n, 900_000),
                _ => 2_500_000 + pick("whole", n, 40_000_000),
            }),
        };
        let typed = (0..2 + pick("typed", 0, 3))
            .map(|n| 3 * 60 + pick("typed-at", n, 30 * 60))
            .collect();
        let steps = [Step::Pending, Step::Unpending];
        let kills = (0..pick("kills", 0, 3))
            .map(|n| {
                let tear = Tear {
                    step: steps[pick("kill-step", n, 2) as usize],
                    after: pick("kill-after", n, 2) == 1,
                };
                let aim = match pick("kill-aim", n, 4) {
                    0 => Aim::At(5 + pick("kill-at", n, 120)),
                    _ => Aim::Tear(tear),
                };
                let rest = match (calm, pick("rest-kind", n, 4)) {
                    (false, 0) => 50 * 60 + pick("rest-long", n, 20 * 60),
                    _ => 2 + pick("rest", n, SHORT_REST - 1),
                };
                Kill { aim, rest }
            })
            .collect();
        Deposits {
            service: match (calm, pick("service", 0, 10)) {
                (true, _) | (_, 0..6) => deposits::Service::Honest,
                (_, 6) => deposits::Service::Sits(60 + pick("sits", 0, 10 * 60)),
                (_, 7) => deposits::Service::LosesAnswers,
                (_, 8) => deposits::Service::NeverSends,
                _ => deposits::Service::StaleTerms(30 + pick("terms-lag", 0, 200)),
            },
            offer: Offer {
                fee: deposits::FEE,
                fee_bps: deposits::FEE_BPS,
                min_value: deposits::MINIMUM,
                max_validity: match pick("validity", 0, 5) {
                    0 => 1_100 + pick("short-validity", 0, 1_500),
                    _ => deposits::DAY,
                },
            },
            line: Line {
                busy: if calm { 0 } else { pick("busy", 0, 100) },
                hang: 0,
                unheard: if calm { 0 } else { pick("unheard", 0, 30) },
                unanswered: if calm { 0 } else { pick("unanswered", 0, 30) },
            },
            arrivals: (0..1 + pick("arrivals", 0, 3)).map(arrival).collect(),
            typed,
            kills,
        }
    }

    pub(crate) fn calm(&self) -> bool {
        let weather = self.weather;
        let sky = self.sky;
        let lightwalletd = self.lightwalletd;
        weather.reorg + weather.reorg_after_tx + weather.delayed + weather.lost == 0
            && weather.rpc_error + weather.rpc_stall == 0
            && sky.reorg + sky.reorg_after_tx + sky.delayed + sky.lost == 0
            && lightwalletd.errors + lightwalletd.stalls + lightwalletd.deaf + lightwalletd.hangups
                == 0
            && self.maker_line.busy + self.maker_line.unheard + self.maker_line.unanswered == 0
            && self.skew == 0
    }

    pub(crate) fn brief(&self) -> String {
        let kills: Vec<String> = self
            .kills
            .iter()
            .map(|kill| match kill.aim {
                Aim::At(at) => format!("at {at}s"),
                Aim::Tear(tear) => format!(
                    "{} {:?}",
                    if tear.after { "after" } else { "before" },
                    tear.step
                ),
                Aim::Ready(after) => format!("{after}s into the claim"),
                Aim::Refunded(after) => format!("{after}s into the sweep"),
                Aim::Unsent(step) => format!("{step:?} refused"),
            })
            .collect();
        format!(
            "{:?} {} fee {:?} funds {:?}{}{}{} kills [{}]",
            self.mood,
            if self.railgun { "railgun" } else { "account" },
            self.fee,
            self.funds,
            if self.swaps > 1 { " two swaps" } else { "" },
            if self.stale { " stale file" } else { "" },
            if self.deposits.is_some() {
                " deposits"
            } else {
                ""
            },
            kills.join(", ")
        )
    }

    pub(crate) fn need(&self) -> u64 {
        let fee = match self.fee {
            Fee::Asked(fee) => fee,
            Fee::None | Fee::Transparent(_) => 0,
        };
        u64::from(self.units) * UNIT_ZAT + fee + 2 * PAYMENT_FEE
    }

    fn notes(&self) -> Vec<u64> {
        let swaps = u64::from(self.swaps);
        let deposit = u64::from(self.units) * UNIT_ZAT;
        match self.funds {
            Funds::One | Funds::Unconfirmed => vec![swaps * self.need() + 500_000],
            Funds::Several => vec![self.need() * 11 / 20; 3 * swaps as usize],
            Funds::Short => match self.fee {
                Fee::Asked(_) => vec![deposit + 15_000],
                Fee::None | Fee::Transparent(_) => vec![deposit - 1],
            },
        }
    }

    fn typed_at(&self) -> u64 {
        match self.funds {
            Funds::Unconfirmed => 3 * SPACING,
            _ => (UNTRUSTED + 3) * SPACING,
        }
    }

    fn horizon(&self) -> u64 {
        let rests: u64 = self.kills.iter().map(|kill| kill.rest).sum();
        let swaps = u64::from(self.swaps) * SWAP_TIME;
        self.clears.max(self.typed_at()) + rests + swaps + SPACING * u64::from(SEASON)
    }

    pub(crate) fn twinned(&self) -> bool {
        let short = self.kills.iter().all(|kill| kill.rest <= SHORT_REST);
        let fixed = self.mood != Mood::CancelsEarly && self.deposits.is_none();
        self.calm() && !self.kills.is_empty() && short && fixed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finish {
    Paid(u128),
    SweptBack(u64),
    Cancelled,
    Abandoned,
    Unfinished,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub swaps: Vec<Finish>,
    pub skipped: u32,
    pub typed: u32,
    pub killed: u32,
    pub own: u64,
    pub shielded: u128,
}

pub fn run(seed: u64) -> Report<Plan, Ended> {
    run_with(seed, Plan::from_seed(Seed(seed)))
}

pub fn run_with(seed: u64, plan: Plan) -> Report<Plan, Ended> {
    let mut report = paused(seed, plan.clone(), simulate);
    if plan.twinned() && report.failures.is_empty() {
        let unkilled = Plan {
            kills: Vec::new(),
            ..plan.clone()
        };
        let twin = paused(seed, unkilled, simulate);
        let ends =
            |report: &Report<Plan, Ended>| report.ended.as_ref().map(|ended| ended.swaps.clone());
        if ends(&report) != ends(&twin) {
            report.failures.push(format!(
                "killed, the command ended {:?}; unkilled it ended {:?}",
                ends(&report),
                ends(&twin)
            ));
        }
    }
    report
}

pub(crate) struct World {
    pub seed: Seed,
    pub plan: Plan,
    pub network: Network,
    pub chain: Chain,
    pub ledger: Ledger,
    pub server: Rc<Server>,
    pub maker: Arc<Maker>,
    pub seen: Arc<Seen>,
    pub desk: Rc<Desk>,
    pub cli_seed: [u8; 32],
    pub own: orchard::Address,
    pub user_key: PrivateKeySigner,
    pub maker_wire: Arc<dyn zecswap_client::Wire>,
    pub relayer_wire: Arc<dyn zecswap_client::Wire>,
    pub depositing: Option<Depositing>,
    pub scratch: Scratch,
    pub start: Instant,
    pub born: Cell<bool>,
    pub typed: Cell<u32>,
    pub killed: Cell<u32>,
    pub settles: Cell<u64>,
}

pub(crate) struct Depositing {
    pub plan: Deposits,
    pub contract: Address,
    pub relayer: Arc<deposits::Relayer>,
    pub wire: Arc<dyn zecswap_client::Wire>,
    pub addresses: Vec<Address>,
    pub vaults: Vec<Address>,
}

const REACH: u32 = 40;

async fn simulate(seed: Seed, plan: Plan) -> Report<Plan, Ended> {
    simulate_wall(Some(EPOCH as i64));
    let start = Instant::now();
    let node = Node::new(seed, CHAIN_ID, EPOCH - SLOT, plan.weather);
    let chain = Chain::deploy(&node, plan.multicall);
    let (maker_key, relayer_key, user_key) =
        (key(seed, "maker"), key(seed, "relayer"), key(seed, "user"));
    let (deposit_key, payer_key) = (key(seed, "deposit-relayer"), key(seed, "payer"));
    for who in [
        &maker_key,
        &relayer_key,
        &user_key,
        &deposit_key,
        &payer_key,
    ] {
        chain.fund(who.address());
    }
    chain.stock(maker_key.address(), 100 * UNIT);
    node.clear_from(EPOCH + plan.clears);
    let (alarms, mut operator) = Alarms::new(&node, seed);
    let network = Network::TestNetwork;
    let cli_seed: [u8; 32] = seed.bytes("cli-seed");
    let usk = UnifiedSpendingKey::from_seed(&network, &cli_seed, zip32::AccountId::ZERO)
        .expect("the wallet's key");
    let own = usk
        .to_unified_full_viewing_key()
        .orchard()
        .expect("an Orchard key")
        .clone();
    let stranger = SpendingKey::from_bytes(seed.bytes("stranger"));
    let stranger: SpendingKey = stranger.into_option().expect("a spending key");
    let nu7 = network
        .activation_height(NetworkUpgrade::Nu7)
        .map(u32::from)
        .expect("a scheduled upgrade");
    let ledger = Ledger::new(
        seed,
        network,
        vec![own.clone(), FullViewingKey::from(&stranger)],
        nu7 - 20_000,
        EPOCH,
        plan.sky,
        EPOCH + plan.clears,
    );
    let clears = start + Duration::from_secs(plan.clears);
    let server = Server::new(ledger.clone(), seed, plan.lightwalletd, clears);
    let seen = Arc::new(Seen::new(ledger.clone()));
    let maker = Maker::new(
        seed,
        plan.mood,
        plan.fee,
        chain.clone(),
        seen.clone(),
        connect(&node, &chain, "maker", maker_key, false, &alarms),
    );
    let relayer = Relayer::new(
        plan.service,
        chain.clone(),
        connect(&node, &chain, "relayer", relayer_key, false, &alarms),
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
    let scratch = Scratch::new(seed);
    let _ = scratch.wallet(network);
    let desk = Rc::new(Desk {
        node: node.clone(),
        ledger: ledger.clone(),
        dir: scratch.0.clone(),
        start,
        armed: Default::default(),
        met: Default::default(),
        kept: Default::default(),
        pending: Default::default(),
    });
    let depositing = plan.deposits.clone().map(|plan| {
        let contract = chain.deploy_deposits();
        let desk = deposits::Desk {
            service: plan.service,
            offer: plan.offer,
            reprice: None,
            cap: MAX_FEE,
            deposits: contract,
            start,
        };
        let relayer = deposits::Relayer::new(
            desk,
            chain.clone(),
            connect(
                &node,
                &chain,
                "deposit-relayer",
                deposit_key.clone(),
                false,
                &alarms,
            ),
            Funder::new(&node, "forcer", deposit_key.clone()),
        );
        let addresses: Vec<Address> = (0..REACH)
            .map(|index| {
                let keys = derive_deposit_keys(&cli_seed, NetworkType::Test, 0, index);
                keys.expect("deposit keys").key.address().into()
            })
            .collect();
        let vaults = addresses
            .iter()
            .map(|from| deposits::vault_of(&node, contract, *from))
            .collect();
        Depositing {
            wire: Net::to(
                seed,
                "deposit relayer",
                plan.line,
                clears,
                Arc::new(deposits::Door(relayer.clone())),
                log.clone(),
            ),
            plan,
            contract,
            relayer,
            addresses,
            vaults,
        }
    });
    let world = Rc::new(World {
        seed,
        plan: plan.clone(),
        network,
        chain: chain.clone(),
        ledger: ledger.clone(),
        server,
        maker: maker.clone(),
        seen,
        desk,
        cli_seed,
        own: own.address_at(0u32, Scope::External),
        user_key,
        maker_wire: Net::to(
            seed,
            "maker",
            plan.maker_line,
            clears,
            Arc::new(MakerDoor(maker.clone())),
            log.clone(),
        ),
        relayer_wire: Net::to(
            seed,
            "relayer",
            plan.relayer_line,
            clears,
            Arc::new(RelayerDoor(relayer)),
            log,
        ),
        depositing,
        scratch,
        start,
        born: Cell::new(false),
        typed: Cell::new(0),
        killed: Cell::new(0),
        settles: Cell::new(0),
    });
    let payer = connect(&node, &chain, "payer", payer_key, false, &alarms);
    let mut background = vec![
        ("the chain", spawn_local(ticks(node.clone(), start))),
        ("the operator", spawn_local(operator.tend())),
        ("the maker", spawn_local(maker.tend())),
        ("the Zcash chain", spawn_local(world.clone().mine())),
        ("the payer", spawn_local(world.clone().fund())),
    ];
    if world.depositing.is_some() {
        background.push(("the arrivals", spawn_local(world.clone().arrive(payer))));
        background.push((
            "the depositor",
            spawn_local(Person::new(world.clone()).deposits()),
        ));
    }
    let horizon = start + Duration::from_secs(plan.horizon());
    let person = spawn_local(Person::new(world.clone()).swaps());
    let person = ("the person", person);
    let persons = 1 + u64::from(world.depositing.is_some());
    tokio::select! {
        biased;
        () = sleep_until(alarms.until(horizon)) => {}
        () = async {
            while world.settles.get() < persons {
                sleep(Duration::from_secs(5)).await;
            }
            sleep_until(clears.max(Instant::now())).await;
            sleep(SETTLE).await;
        } => {}
    }
    let settled = world.settles.get() == persons;
    let shown = match (&world.depositing, settled) {
        (Some(_), true) => {
            let last = Person::new(world.clone()).last_deposit();
            match tokio::time::timeout(Duration::from_secs(DEPOSIT_TIME), last).await {
                Ok(result) => Some(result),
                Err(_) => Some(Err("the last deposit command never ended".into())),
            }
        }
        _ => None,
    };
    panics::end(background.into_iter().chain([person])).await;
    let path = world.scratch.0.join("wallet.sqlite");
    let known =
        zecswap_chain::zcash::Wallet::open(path, network).and_then(|wallet| wallet.accounts());
    let mut report = world.report(settled, shown, known.ok().map(|known| known.len()));
    report.failures.extend(alarms.failures(&[]));
    report
}

impl World {
    async fn mine(self: Rc<Self>) {
        while !self.born.get() {
            sleep(Duration::from_secs(1)).await;
        }
        for _ in 0..self.plan.sky.steady {
            self.ledger.tick(EPOCH + self.start.elapsed().as_secs());
        }
        loop {
            let now = EPOCH + self.start.elapsed().as_secs();
            let wait = match self.ledger.quick(now) {
                true => self.seed.between("zcash-quick-gap", now, 2, 9),
                false => SPACING,
            };
            sleep(Duration::from_secs(wait)).await;
            self.ledger.tick(EPOCH + self.start.elapsed().as_secs());
        }
    }

    async fn fund(self: Rc<Self>) {
        while !self.born.get() {
            sleep(Duration::from_secs(1)).await;
        }
        let when = match self.plan.funds {
            Funds::Unconfirmed => self.plan.typed_at().saturating_sub(SPACING),
            _ => SPACING,
        };
        sleep_until(self.start + Duration::from_secs(when)).await;
        for value in self.plan.notes() {
            self.ledger
                .pay(self.own, value, orchard::ValuePool::Ironwood);
        }
    }

    async fn arrive(self: Rc<Self>, payer: Settlement) {
        let Some(depositing) = &self.depositing else {
            return;
        };
        let mut arrivals = depositing.plan.arrivals.clone();
        arrivals.sort_by_key(|arrival| arrival.at);
        for arrival in arrivals {
            sleep_until(self.start + Duration::from_secs(arrival.at)).await;
            let index = match arrival.to {
                Spot::Index(index) => index,
                Spot::Shown => (0..REACH)
                    .find(|index| !self.deposited(*index))
                    .unwrap_or(0),
            };
            let to = depositing.addresses[index as usize];
            self.chain
                .node
                .note(format!("{} arrive for address #{index}", arrival.amount));
            for _ in 0..6 {
                match payer
                    .mint_test_token(self.chain.token, to, arrival.amount)
                    .await
                {
                    Err(e) if e.nothing_sent() => sleep(Duration::from_secs(20)).await,
                    _ => break,
                }
            }
        }
    }

    fn deposited(&self, index: u32) -> bool {
        let Some(depositing) = &self.depositing else {
            return false;
        };
        deposits::deposited(
            &self.chain.node,
            depositing.contract,
            depositing.addresses[index as usize],
        )
    }

    fn share(&self, index: u32) -> SecretShare {
        derive_user_keys(&self.cli_seed, NetworkType::Test, 0, index)
            .expect("the user's keys")
            .share
    }

    fn report(
        &self,
        settled: bool,
        shown: Option<Result<(), String>>,
        known: Option<usize>,
    ) -> Report<Plan, Ended> {
        let mut failures = Vec::new();
        let mut failed = BTreeSet::new();
        let mut fail = |check: u8, what: String| {
            if failed.insert(check) {
                failures.push(what);
            }
        };
        let kept = self.desk.kept.borrow().clone();
        let tip = self.ledger.tip().0;
        let facts = self.ledger.facts();
        let published = self.chain.node.published();
        let maker_swaps = self.maker.swaps();
        let mut quotes: Vec<B256> = Vec::new();
        let mut by_quote: BTreeMap<B256, Vec<&Kept>> = BTreeMap::new();
        for each in &kept {
            let Some(id) = desk::quote_id(&each.run) else {
                continue;
            };
            if !quotes.contains(&id) {
                quotes.push(id);
            }
            by_quote.entry(id).or_default().push(each);
        }
        let mut indices: BTreeMap<u32, BTreeSet<B256>> = BTreeMap::new();
        for (id, saves) in &by_quote {
            for save in saves {
                if let Some(index) = desk::index_of(&save.run) {
                    indices.entry(index).or_default().insert(*id);
                }
            }
        }
        for (index, ids) in &indices {
            if ids.len() > 1 {
                fail(
                    1,
                    format!(
                        "{} swaps of one directory share the index {index}",
                        ids.len()
                    ),
                );
            }
        }
        let fee_key = self.seen.key(&self.maker.fee_address);
        let fees: Vec<u64> = fee_key.map_or(Vec::new(), |key| {
            let held = self.ledger.everything(key);
            held.iter().map(|note| note.value).collect()
        });
        let fee_quotes = by_quote
            .values()
            .filter(|saves| saves.iter().any(|save| desk::fee(&save.run).is_some()))
            .count();
        if fees.len() > fee_quotes {
            fail(
                2,
                format!(
                    "{} open fees were paid for the {fee_quotes} quotes the wallet paid one for",
                    fees.len()
                ),
            );
        }
        if let Some(over) = fees.iter().find(|fee| **fee > self.plan.max_open_fee) {
            fail(
                3,
                format!(
                    "an open fee of {over} zat was paid over the cap of {}",
                    self.plan.max_open_fee
                ),
            );
        }
        let refused_fee = match self.plan.fee {
            Fee::Transparent(_) => true,
            Fee::Asked(fee) => fee > self.plan.max_open_fee,
            Fee::None => false,
        };
        if refused_fee && (!fees.is_empty() || !kept.is_empty()) {
            fail(
                4,
                format!(
                    "the wallet went on with an open fee of {:?} under a cap of {}",
                    self.plan.fee, self.plan.max_open_fee
                ),
            );
        }
        let mut swaps = Vec::new();
        let mut shielded = 0;
        let mut paid_account = 0;
        for id in &quotes {
            let saves = &by_quote[id];
            let last = &saves.last().expect("a save").run;
            let swap_id = saves.iter().find_map(|save| desk::swap_id(&save.run));
            let opened =
                swap_id.and_then(|swap_id| maker_swaps.iter().find(|swap| swap.id == swap_id));
            let joint = opened.and_then(|opened| self.seen.key(&opened.joint));
            let index = desk::index_of(last).unwrap_or(u32::MAX);
            let received = joint.map_or(Vec::new(), |key| self.ledger.everything(key));
            let deposited = received.iter().filter(|note| note.mined <= tip).count();
            if deposited > 1 {
                fail(
                    5,
                    format!("swap #{index} was deposited to {deposited} times"),
                );
            }
            if self.plan.mood.cheats_on_terms() && deposited > 0 {
                fail(
                    6,
                    format!(
                        "the wallet deposited to swap #{index} though its maker was {:?}",
                        self.plan.mood
                    ),
                );
            }
            let built = saves
                .iter()
                .find(|save| desk::deposit(&save.run).is_some())
                .map(|save| save.wall);
            if let (Some(opened), Some(at)) = (opened, built)
                && opened.opened.t0.saturating_sub(at) + 2 * SLOT < MIN_TIME_TO_T0
            {
                fail(
                    7,
                    format!(
                        "the wallet built its deposit to swap #{index} {}s before t0",
                        opened.opened.t0.saturating_sub(at)
                    ),
                );
            }
            let recorded = swap_id.map(|swap_id| self.chain.recorded(swap_id));
            let stage = recorded.map(|recorded| recorded.stage);
            if let (Some(_), Some(recorded)) = (opened, recorded) {
                let z = self.share(index);
                let word = z.to_be_bytes();
                let shown = published
                    .iter()
                    .any(|tx| tx.input.windows(32).any(|window| window == word));
                if shown && recorded.stage != CLAIMED {
                    fail(
                        8,
                        format!(
                            "the user's share of swap #{index} is public and the swap is at stage {}",
                            recorded.stage
                        ),
                    );
                }
                if recorded.stage == CLAIMED && recorded.secret != U256::from_be_bytes(word) {
                    fail(
                        9,
                        format!(
                            "swap #{index} was claimed with another secret than the user's share"
                        ),
                    );
                }
            }
            let swept: u64 = joint.map_or(0, |key| {
                let notes = self.ledger.everything(key);
                let spends = facts.iter().filter(|fact| {
                    fact.mined.is_some()
                        && notes
                            .iter()
                            .any(|note| fact.nullifiers.contains(&note.nullifier))
                });
                spends
                    .map(|fact| {
                        let paid = self.ledger.paid_by(fact.ordinal, OWN);
                        paid.iter().map(|(_, value)| value).sum::<u64>()
                    })
                    .sum()
            });
            let left = joint.map_or(0, |key| self.seen.unspent(key, 1));
            let finish = match (desk::finished(last), stage) {
                (false, _) => Finish::Unfinished,
                (true, None) => Finish::Abandoned,
                (true, Some(CLAIMED)) => {
                    let amount = opened.map_or(0, |opened| opened.opened.amount);
                    Finish::Paid(amount)
                }
                (true, Some(REFUNDED)) if deposited > 0 => Finish::SweptBack(swept),
                (true, Some(REFUNDED)) => Finish::Cancelled,
                (true, Some(other)) => {
                    let what = match deposited {
                        0 => "with nothing deposited",
                        _ => "with its deposit in it",
                    };
                    fail(
                        10,
                        format!(
                            "swap #{index} is marked finished at stage {other} {what}: the run owes something"
                        ),
                    );
                    Finish::Unfinished
                }
            };
            if desk::finished(last) && stage != Some(CLAIMED) && left > 0 {
                fail(
                    11,
                    format!(
                        "swap #{index} is marked finished and its joint account still holds {left} the wallet no longer looks at"
                    ),
                );
            }
            if let Finish::SweptBack(swept) = finish {
                let deposit = opened.map_or(0, |opened| opened.deposit_zat);
                if swept == 0 || swept > deposit {
                    fail(
                        12,
                        format!(
                            "swap #{index} was refunded and {swept} of its {deposit} came back to the wallet's own account"
                        ),
                    );
                }
            }
            if let (Finish::Paid(amount), Some(opened)) = (&finish, opened) {
                match self.plan.railgun {
                    true => {
                        let (value, held) =
                            shielded_to(&self.chain, &self.cli_seed, index, *amount);
                        shielded += value;
                        if !held && self.plan.service != Service::SkipsPayouts {
                            fail(
                                13,
                                format!(
                                    "swap #{index} was claimed and {value} never reached the user's note"
                                ),
                            );
                        }
                    }
                    false => paid_account += opened.opened.amount,
                }
            }
            if settled && finish == Finish::Unfinished && !self.plan.mood.cheats_on_terms() {
                fail(
                    14,
                    format!(
                        "with time enough, swap #{index} was left unfinished at stage {stage:?}"
                    ),
                );
            }
            swaps.push(finish);
        }
        if !self.plan.railgun {
            let held = self.chain.token_balance(self.user_key.address());
            if held != paid_account {
                fail(
                    13,
                    format!("the user's account holds {held} of the {paid_account} claimed"),
                );
            }
        }
        for saves in by_quote.values() {
            let first = saves.iter().find(|save| desk::finished(&save.run));
            let later = first.map(|first| saves.iter().filter(|save| save.wall > first.wall));
            if later.is_some_and(|mut later| later.any(|save| !desk::finished(&save.run))) {
                fail(15, "a swap marked finished was taken up again".into());
            }
        }
        let calm_end =
            self.plan.calm() && self.plan.kills.is_empty() && self.plan.funds != Funds::Short;
        if calm_end && settled {
            let unconfirmed = self.plan.funds == Funds::Unconfirmed;
            let unpaid = unconfirmed && matches!(self.plan.fee, Fee::Asked(_));
            let fine = swaps.iter().all(|finish| match (self.plan.mood, finish) {
                (_, Finish::Abandoned) if unpaid => true,
                (mood, Finish::Unfinished) if !mood.cheats_on_terms() => false,
                (Mood::Honest | Mood::NeverReadies | Mood::Vanishes | Mood::OpensFar, end) => {
                    matches!(end, Finish::Paid(_))
                }
                (Mood::CancelsEarly, end) => {
                    matches!(end, Finish::Cancelled | Finish::SweptBack(_))
                }
                (Mood::KeepsFee, end) => *end == Finish::Abandoned,
                (_, end) => matches!(end, Finish::Cancelled | Finish::Unfinished),
            });
            let expected = match refused_fee {
                true => swaps.is_empty(),
                false => {
                    let all = swaps.len() as u32 == self.plan.swaps;
                    fine && (all || self.plan.mood.cheats_on_terms())
                }
            };
            if !expected {
                fail(
                    17,
                    format!(
                        "in calm weather a {:?} maker asking {:?} left the command at {swaps:?}",
                        self.plan.mood, self.plan.fee
                    ),
                );
            }
        }
        let needed = self.plan.need();
        for saves in by_quote.values() {
            let Some(paid) = saves.iter().find(|save| desk::fee(&save.run).is_some()) else {
                continue;
            };
            let notes = self.ledger.holdings(OWN, paid.tip);
            let held: u64 = notes
                .iter()
                .filter(|note| note.spent.is_none())
                .map(|note| note.value)
                .sum();
            if held < needed {
                fail(
                    19,
                    format!(
                        "the wallet paid an open fee holding {held}, where the fee, the deposit and their transaction fees take {needed}"
                    ),
                );
            }
            let txid = desk::fee(&paid.run).expect("a fee");
            let mined = self.ledger.mined(&txid).is_some();
            let expires = paid.run.quote().expires_at;
            let sent = self.desk.met.borrow().iter().find_map(|met| {
                let out = Tear {
                    step: Step::FeeOut,
                    after: true,
                };
                (met.tear == out && met.wall >= paid.wall).then_some(met.wall)
            });
            let cleared = EPOCH + self.plan.clears;
            if let Some(sent) = sent
                && !mined
                && self.plan.kills.is_empty()
                && expires > sent.max(cleared) + 4 * 60
            {
                fail(
                    26,
                    format!(
                        "an open fee sent with {}s of its quote left, {}s of them in clear weather, was never mined",
                        expires - sent,
                        expires - sent.max(cleared)
                    ),
                );
            }
        }
        let open_joints = quotes
            .iter()
            .filter(|id| {
                let saves = &by_quote[*id];
                let last = &saves.last().expect("a save").run;
                !desk::finished(last) && last.joint()
            })
            .count();
        if let Some(known) = known
            && known > 1 + open_joints
        {
            fail(
                24,
                format!(
                    "the wallet still knows {} joint accounts, where {open_joints} swaps are unfinished",
                    known - 1
                ),
            );
        }
        if self.plan.stale {
            let kept_aside = std::fs::read_dir(&self.scratch.0)
                .into_iter()
                .flatten()
                .flatten();
            let mut kept_aside = kept_aside.filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stale = std::fs::read(entry.path()).unwrap_or_default();
                name.starts_with("swap.json.finished-") && stale.starts_with(b"{\"version\": 1")
            });
            if kept_aside.next().is_none() {
                fail(25, "the other version's swap.json was lost".into());
            }
        }
        let used: BTreeSet<u32> = kept
            .iter()
            .filter_map(|save| desk::index_of(&save.run))
            .collect();
        let next = self.desk.read::<u32>(desk::NEXT_INDEX).unwrap_or(0);
        if used.iter().any(|index| *index >= next) {
            fail(
                18,
                format!("a swap used an index at or past the next one, {next}"),
            );
        }
        let skipped = next - used.len() as u32;
        if let Some(depositing) = &self.depositing {
            self.deposit_checks(depositing, settled, shown, &mut fail);
        }
        let own: u64 = {
            let held = self.ledger.everything(OWN);
            let left = held.iter().filter(|note| note.spent.is_none());
            left.map(|note| note.value).sum()
        };
        let stage = quotes.last().and_then(|id| {
            let saves = &by_quote[id];
            let swap_id = saves.iter().find_map(|save| desk::swap_id(&save.run))?;
            Some(self.chain.recorded(swap_id).stage)
        });
        let mut trace = self.chain.node.trace();
        trace.push("the Zcash chain:".into());
        trace.extend(self.ledger.trace());
        Report {
            seed: self.seed.0,
            plan: self.plan.clone(),
            ended: Some(Ended {
                swaps,
                skipped,
                typed: self.typed.get(),
                killed: self.killed.get(),
                own,
                shielded,
            }),
            stage,
            failures,
            trace,
        }
    }

    fn deposit_checks(
        &self,
        depositing: &Depositing,
        settled: bool,
        shown: Option<Result<(), String>>,
        fail: &mut impl FnMut(u8, String),
    ) {
        let saved = self.desk.pending.borrow().clone();
        let heard = depositing.relayer.heard();
        for heard in &heard {
            let first = saved.iter().any(|(at, pending)| {
                keccak256(pending.request.signature) == heard.key && *at <= heard.at
            });
            if !first && !heard.rescue {
                fail(
                    20,
                    format!(
                        "the relayer heard a deposit from {} at {} that the command had not saved first",
                        heard.from, heard.time
                    ),
                );
            }
        }
        let asks = depositing.relayer.asks();
        let worth = |balance: u128| {
            balance >= asks.min_value && deposit_fee(asks.fee, asks.fee_bps, balance) < balance
        };
        let delivers = depositing.plan.service.delivers();
        if let Some(pending) = self.desk.read::<desk::Pending>(desk::PENDING) {
            let request = &pending.request;
            let held = self.chain.token_balance(request.from);
            let now = zecswap_chain::evm::wall_now();
            let dead = request.valid_before <= now || held < request.value;
            if dead && worth(held) && settled && delivers {
                fail(
                    21,
                    format!(
                        "deposit.json holds a request that can no longer land, while its address holds {held}"
                    ),
                );
            }
        }
        if let Some(Err(e)) = &shown
            && settled
            && delivers
        {
            fail(
                22,
                format!("the last deposit command failed: {}", person::brief(e)),
            );
        }
        if settled && delivers && matches!(shown, Some(Ok(()))) {
            for (index, from) in depositing.addresses.iter().enumerate() {
                let held = self.chain.token_balance(*from);
                if worth(held) && (index as u32) < 20 {
                    fail(23, format!("{held} is stuck at deposit address #{index}"));
                }
                let vault = self.chain.token_balance(depositing.vaults[index]);
                if vault > asks.fee {
                    fail(23, format!("{vault} is stuck in the vault of #{index}"));
                }
            }
        }
    }
}
