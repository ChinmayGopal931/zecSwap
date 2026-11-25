mod ledger;
mod server;
mod signer;
mod wallet;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use orchard::ValuePool;
use orchard::keys::Scope;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep};
use zcash_address::unified::{self, Encoding, Receiver};
use zcash_protocol::consensus::{
    BlockHeight, BranchId, Network, NetworkType, NetworkUpgrade, Parameters,
};
use zecswap_chain::zcash::{Prover, SWEEP_FEE_CEILING};
use zecswap_core::{SweepIntent, sign_pczt_bytes};

use crate::panics;
use crate::run::{Report, lives, paused};
use crate::seed::Seed;
use crate::world::EPOCH;
pub use ledger::Sky;
use ledger::{Ledger, MARGINAL_FEE, Outcome};
use server::Server;
pub use server::Service;
pub use signer::{Trick, View};
use wallet::{Actor, Built, JOINTS, Keys, Made, OWN, STRANGER, Saved, Step};
pub use wallet::{Ahead, End, Errand, Home, Then};

const SPACING: u64 = 75;
const MOST_NOTES: usize = 64;
const SEASON: u32 = 13;
const STALE: Duration = Duration::from_secs(24 * 60 * 60);
const FUNDS: usize = usize::MAX;

static PROVER: LazyLock<Prover> = LazyLock::new(Prover::default);

fn unified(address: orchard::Address, network: NetworkType) -> String {
    let receiver = Receiver::Orchard(address.to_raw_address_bytes());
    unified::Address::try_from_items(unified::Revision::R0, vec![unified::Uitem::Data(receiver)])
        .expect("a lone Orchard receiver is a unified address")
        .encode(&network)
}

fn receiver(address: &str, network: NetworkType) -> orchard::Address {
    zecswap_core::SweepIntent::recipient(address, network).expect("an Orchard receiver")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    Before,
    Across(u32),
    After,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub mainnet: bool,
    pub era: Era,
    pub sky: Sky,
    pub service: Service,
    pub confirmations: u32,
    pub poll: u64,
    pub own: bool,
    pub inventory: bool,
    pub funds: Vec<(u64, u64)>,
    pub errands: Vec<Errand>,
    pub crashes: Vec<(u64, u64)>,
    pub clears: u64,
    pub blocks: u64,
}

impl Plan {
    pub fn plain() -> Self {
        Self {
            mainnet: false,
            era: Era::Before,
            sky: Sky::CALM,
            service: Service::HONEST,
            confirmations: 1,
            poll: 50,
            own: true,
            inventory: false,
            funds: vec![(0, 1_000_000)],
            errands: vec![Errand::Receive {
                at: 0,
                chained: false,
                notes: vec![(0, 500_000)],
                sweep: Some(Home::Own),
                patience: 600,
            }],
            crashes: Vec::new(),
            clears: 0,
            blocks: 16,
        }
    }

    pub fn from_seed(seed: Seed) -> Self {
        let roll = |what: &str, bound: u64| seed.below(what, 0, bound);
        let calm = roll("zcash-sky", 100) < 20;
        let up_to = |what: &str, most: u64| if calm { 0 } else { roll(what, most + 1) };
        let focus = roll("zcash-focus", 16);
        let era = match roll("era", 100) {
            0..55 => Era::Before,
            55..80 => Era::Across(seed.between("to-the-upgrade", 0, 5, 26) as u32),
            _ => Era::After,
        };
        let own = roll("own", 5) != 0;
        let proves = roll("proves", 100) < 40;
        let home = |n: u64| match own && seed.below("home", n, 3) != 0 {
            true => Home::Own,
            false => Home::Stranger,
        };
        let at = |n: u64| seed.between("errand-at", n, 0, 8 * SPACING);
        let notes = |n: u64, most: u64, low: u64, high: u64| -> Vec<(u64, u64)> {
            let count = seed.between("notes", n, 1, most);
            let note = |k: u64| {
                (
                    seed.between("note-delay", n * 128 + k, 0, 150),
                    seed.between("note-value", n * 128 + k, low, high),
                )
            };
            (0..count).map(note).collect()
        };
        let swept = |n: u64| Errand::Receive {
            at: at(n),
            chained: false,
            notes: notes(n, 3, 60_000, 900_000),
            sweep: Some(home(n)),
            patience: 900,
        };
        let watched = |n: u64, chained: bool| Errand::Receive {
            at: at(n),
            chained,
            notes: notes(n, 3, 60_000, 900_000),
            sweep: None,
            patience: 900,
        };
        let crowded = |n: u64| Errand::Receive {
            at: at(n),
            chained: false,
            notes: {
                let many = seed.between("crowd", n, MOST_NOTES as u64 + 1, MOST_NOTES as u64 + 4);
                let note = |k: u64| (seed.below("crowd-delay", k, 40), 10_000 + k);
                (0..many).map(note).collect()
            },
            sweep: Some(home(n)),
            patience: 900,
        };
        let paid = |n: u64, ahead: Option<Ahead>| Errand::Pay {
            at: at(n),
            amount: seed.between("amount", n, 60_000, 250_000),
            ahead,
        };
        let ahead = |n: u64| {
            let hold = seed.between("hold", n, 2, 6) as u32;
            let within = seed.between("held-for", n, 0, u64::from(hold)) as u32;
            let then = match seed.below("then", n, 6) {
                0 | 1 => Then::Send(within),
                2 => Then::Release(within),
                3 => Then::WrongKey(within),
                _ => Then::Late,
            };
            Ahead {
                hold,
                then,
                crash: None,
            }
        };
        let signed = |n: u64, trick: u64, proves: bool| {
            let trick = match trick {
                0 => Trick::Elsewhere,
                1 => Trick::Skims,
                2 => Trick::Partial,
                3 => Trick::Overpays,
                4 => Trick::Garbles,
                5 => Trick::Hides,
                6 => Trick::Presigned,
                7 => Trick::Forges,
                8 => Trick::AnothersKey,
                _ => Trick::None,
            };
            let mut paid = notes(n, 2, 60_000, 900_000);
            if trick == Trick::Partial {
                paid = vec![(0, 300_000 + n), (20, 400_000 + n)];
            }
            Errand::Sign {
                at: at(n),
                notes: paid,
                trick,
                view: match (seed.below("view", n, 3), trick) {
                    (0, _) => View::Whole,
                    (1, _) | (_, Trick::Garbles | Trick::Forges) => View::Signers,
                    _ => View::Compact,
                },
                proves: proves && trick == Trick::None,
            }
        };
        let mut errands = Vec::new();
        let mut proofs = 0;
        for n in 0..1 + roll("errands", 3) {
            let may = proves && proofs < 2;
            let errand = match seed.below("errand", n, 10) {
                0..3 if may => swept(n),
                3..5 if may && own => paid(n, None),
                5 if may && own => paid(n, Some(ahead(n))),
                6 if own => Errand::Fee {
                    at: at(n),
                    index: 4_000 + 5 * n + (n % 2) * (1 << 33),
                    value: seed.between("fee", n, 20_000, 60_000),
                },
                7 => crowded(n),
                8 => {
                    let proves = may && seed.below("sign-proves", n, 3) == 0;
                    signed(n, seed.below("trick", n, 14), proves)
                }
                _ => watched(n, false),
            };
            let proofless = matches!(
                errand,
                Errand::Fee { .. }
                    | Errand::Receive { sweep: None, .. }
                    | Errand::Sign { proves: false, .. }
            );
            proofs += u32::from(!proofless && may);
            errands.push(errand);
        }
        let mut crashes: Vec<(u64, u64)> = (0..up_to("zcash-crashes", 2))
            .map(|n| {
                (
                    seed.between("zcash-crash-at", n, 60, 20 * SPACING),
                    seed.between("zcash-crash-rest", n, 5, 300),
                )
            })
            .collect();
        let mut plan = Self {
            mainnet: roll("mainnet", 3) == 0,
            era,
            sky: Sky {
                steady: match roll("fresh", 80) {
                    0 => 0,
                    _ => SEASON,
                },
                reorg: up_to("zcash-reorg-odds", 50),
                reorg_after_tx: up_to("zcash-reorg-after-tx", 350),
                reorg_depth: 1 + roll("zcash-reorg-deep", 3),
                dropped: match roll("zcash-drops", 80) {
                    0 => up_to("zcash-dropped", 400),
                    _ => 0,
                },
                delayed: up_to("zcash-delayed", 300),
                lost: up_to("zcash-lost", 150),
                quick: up_to("zcash-quick", 300),
                traffic: roll("zcash-traffic-odds", 1001),
                rival: up_to("zcash-rival-odds", 300),
            },
            service: Service {
                errors: up_to("lwd-errors", 50),
                slow: up_to("lwd-slow", 100),
                stalls: up_to("lwd-stalls", 25),
                cuts: up_to("lwd-cuts", 150),
                lags: up_to("lwd-lags", 200),
                deaf: up_to("lwd-deaf", 300),
                hangups: up_to("lwd-hangups", 25),
                misplaces: up_to("lwd-misplaces", 900),
                backwards: roll("lwd-backwards", 80) == 0,
                another: roll("lwd-another", 40) == 0,
                quiet: roll("lwd-quiet", 2) == 0,
            },
            confirmations: [1, 1, 2, 3, 3, 10][roll("confirmations", 6) as usize],
            poll: seed.between("zcash-poll", 0, 20, 80),
            own,
            inventory: own && roll("inventory", 3) == 0,
            funds: match own {
                true => notes(100, 3, 500_000, 2_000_000),
                false => Vec::new(),
            },
            errands,
            crashes: Vec::new(),
            clears: up_to("zcash-clears", 28 * SPACING),
            blocks: 0,
        };
        match focus {
            0 => {
                plan.sky.reorg_after_tx = 800;
                plan.sky.reorg_depth = 3;
                plan.sky.dropped = 0;
                plan.sky.rival = 500 * roll("rivalled", 2);
                plan.confirmations = 1 + roll("shallow", 2) as u32;
                plan.errands = vec![swept(0)];
                plan.clears = plan.clears.max(12 * SPACING);
            }
            1 => {
                let depth = u64::from(plan.confirmations.min(3));
                plan.confirmations = depth as u32;
                plan.errands = match own && roll("upgrade-pays", 2) == 0 {
                    true => vec![paid(0, None)],
                    false => vec![swept(0)],
                };
                let built = at(0) / SPACING + depth + 3;
                plan.era = Era::Across((built + roll("upgrade-gap", 4)) as u32);
                plan.sky.delayed = 600;
                plan.sky.lost = 300;
                plan.clears = (built + 6) * SPACING;
            }
            2 => {
                plan.era = Era::Before;
                plan.confirmations = plan.confirmations.min(3);
                plan.errands = match own && roll("expiry-pays", 2) == 0 {
                    true => vec![paid(0, None)],
                    false => vec![swept(0)],
                };
                plan.sky.lost = 1000;
                plan.sky.reorg = 0;
                plan.sky.reorg_after_tx = 0;
                plan.clears = at(0) + (u64::from(plan.confirmations) + 6) * SPACING;
            }
            3 if own => {
                let hold = seed.between("hold", 0, 3, 6);
                let ahead = Ahead {
                    hold: hold as u32,
                    then: Then::Send(seed.between("held-for", 0, 2, 3) as u32),
                    crash: Some(match roll("rest", 3) {
                        0 => seed.between("long-rest", 0, hold + 2, hold + 6) * SPACING,
                        _ => seed.between("short-rest", 0, 5, 60),
                    }),
                };
                plan.errands = vec![paid(0, Some(ahead))];
            }
            4 => plan.errands = vec![crowded(0)],
            5 => {
                plan.own = false;
                plan.inventory = false;
                plan.funds = Vec::new();
                let chain = 2 + roll("chain", 2);
                plan.errands = (0..chain).map(|n| watched(n, n > 0)).collect();
            }
            6 => {
                plan.service.stalls = 250;
                plan.service.slow = 300;
                plan.clears = plan.clears.max(10 * SPACING);
            }
            7 => {
                plan.service.deaf = 500;
                plan.service.errors = 60;
                plan.service.quiet = roll("lwd-loud", 3) == 0;
                plan.sky.delayed = 600;
                plan.clears = plan.clears.max(12 * SPACING);
                if own {
                    plan.errands = vec![paid(0, None)];
                }
                let built = at(0) + (u64::from(plan.confirmations) + 2) * SPACING;
                crashes.push((built + seed.between("crash-after", 0, 0, 120), 30));
            }
            8 if own => {
                let hold = seed.between("hold", 0, 2, 5) as u32;
                let within = seed.between("held-for", 0, 0, u64::from(hold)) as u32;
                let then = match roll("then", 4) {
                    0 => Then::Send(within),
                    1 => Then::Release(within),
                    2 => Then::WrongKey(within),
                    _ => Then::Late,
                };
                let ahead = Ahead {
                    hold,
                    then,
                    crash: None,
                };
                plan.errands = vec![paid(0, Some(ahead))];
            }
            9 => {
                let count = 1 + roll("signs", 2);
                plan.errands = (0..count)
                    .map(|n| signed(n, seed.below("trick", n, 11), false))
                    .collect();
            }
            10 if own => {
                let fee = |n: u64| Errand::Fee {
                    at: at(n),
                    index: 7_000 + 4 * n,
                    value: seed.between("fee", n, 20_000, 60_000),
                };
                plan.errands = vec![fee(0), fee(1)];
                plan.sky.reorg_after_tx = 700;
                plan.sky.reorg_depth = 2;
                plan.sky.delayed = 850;
                plan.clears = plan.clears.max(14 * SPACING);
            }
            11 if own => {
                plan.inventory = roll("inventory", 2) == 0;
                crashes = (0..2)
                    .map(|n| {
                        (
                            seed.between("zcash-crash-at", n, 60, 12 * SPACING),
                            seed.between("zcash-crash-rest", n, 5, 200),
                        )
                    })
                    .collect();
            }
            _ => {}
        }
        if !plan.own {
            for errand in &mut plan.errands {
                if let Errand::Receive {
                    sweep: Some(home), ..
                } = errand
                {
                    *home = Home::Stranger;
                }
            }
            plan.errands
                .retain(|errand| matches!(errand, Errand::Receive { .. } | Errand::Sign { .. }));
        }
        if plan.errands.is_empty() {
            plan.errands = vec![watched(0, false)];
        }
        if plan.era == Era::After {
            plan.sky.lost = 0;
        }
        if plan.service.backwards {
            plan.service.lags = plan.service.lags.max(600);
            plan.sky.steady = 0;
            plan.sky.reorg = 0;
            plan.sky.reorg_after_tx = 0;
        }
        if plan.sky.dropped > 0 {
            plan.sky.dropped = 1000;
            plan.sky.reorg_after_tx = plan.sky.reorg_after_tx.max(800);
            plan.errands = vec![watched(0, false)];
        }
        plan.crashes = crashes;
        let depth = u64::from(plan.confirmations);
        let back = plan.crashes.iter().map(|(at, rest)| at + rest);
        let last = back.chain([plan.clears, 10 * SPACING]).max().unwrap_or(0);
        plan.blocks = last / SPACING
            + match (plan.era, plan.calm(), plan.service.another) {
                (_, _, true) => 4,
                (_, true, _) => 16 + 3 * depth,
                (Era::Before, false, _) => 78 + 3 * depth,
                (_, false, _) => 33 + 3 * depth,
            };
        plan
    }

    pub fn brief(&self) -> String {
        let errands = self.errands.iter().map(|errand| match errand {
            Errand::Receive { sweep: None, .. } => "watch".to_owned(),
            Errand::Receive { notes, sweep, .. } => format!("sweep {} to {sweep:?}", notes.len())
                .replace("Some(", "")
                .replace(')', ""),
            Errand::Pay { ahead: None, .. } => "pay".to_owned(),
            Errand::Pay {
                ahead: Some(ahead), ..
            } => format!(
                "pay held {} then {:?}{}",
                ahead.hold,
                ahead.then,
                if ahead.crash.is_some() { " killed" } else { "" }
            ),
            Errand::Fee { .. } => "fee".to_owned(),
            Errand::Sign {
                trick,
                view,
                proves,
                ..
            } => {
                format!(
                    "sign {trick:?} {view:?}{}",
                    if *proves { " proved" } else { "" }
                )
            }
        });
        let errands: Vec<String> = errands.collect();
        let wallet = match (self.own, self.inventory, self.sky.steady) {
            (_, _, 0) => "fresh",
            (false, _, _) => "joints only",
            (true, true, _) => "inventory",
            (true, false, _) => "own",
        };
        let marks = [
            (self.service.another, " another network"),
            (self.service.backwards, " tips backwards"),
            (self.sky.dropped > 0, " payments lost"),
        ];
        let server: String = marks
            .iter()
            .filter(|(set, _)| *set)
            .map(|(_, mark)| *mark)
            .collect();
        format!(
            "{:?} {} deep {wallet}{server} [{}]",
            self.era,
            self.confirmations,
            errands.join(", ")
        )
    }

    fn calm(&self) -> bool {
        let Sky {
            reorg,
            reorg_after_tx,
            dropped,
            delayed,
            lost,
            ..
        } = self.sky;
        reorg + reorg_after_tx + dropped + delayed + lost == 0
    }

    fn settles(&self) -> bool {
        let depth = u64::from(self.confirmations);
        let need = match (self.era, self.calm()) {
            (_, true) => 14 + 3 * depth,
            (Era::Before, false) => 75 + 3 * depth,
            (_, false) => 30 + 3 * depth,
        };
        let back = self.crashes.iter().map(|(at, rest)| at + rest);
        let starts = self.errands.iter().map(|errand| match errand {
            Errand::Receive { at, notes, .. } => {
                at + notes.iter().map(|(delay, _)| *delay).max().unwrap_or(0)
            }
            Errand::Sign { at, notes, .. } => {
                at + notes.iter().map(|(delay, _)| *delay).max().unwrap_or(0)
            }
            Errand::Pay { at, .. } | Errand::Fee { at, .. } => *at,
        });
        let funded = self.funds.iter().map(|(delay, _)| *delay);
        let last = back.chain(starts).chain(funded).chain([self.clears]).max();
        last.unwrap_or(0) + need * SPACING <= self.blocks * SPACING
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub errands: Vec<Option<End>>,
    pub own: u64,
    pub sent: usize,
    pub mined: usize,
    pub audited: bool,
}

struct Scratch(PathBuf);

impl Scratch {
    fn shaped(name: &str) -> bool {
        let numbers = name.strip_prefix("zecswap-sim-").unwrap_or_default();
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        numbers.split('-').count() == 3 && numbers.split('-').all(digits)
    }

    fn new(seed: Seed) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let ours = format!("zecswap-sim-{}-", std::process::id());
        let stale = |entry: &std::fs::DirEntry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let data = entry.metadata().ok().filter(|data| data.is_dir());
            let age = data.and_then(|data| data.modified().ok()?.elapsed().ok());
            Self::shaped(&name) && !name.starts_with(&ours) && age.is_some_and(|age| age > STALE)
        };
        let left = std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten();
        for entry in left.flatten().filter(stale) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
        let name = format!(
            "zecswap-sim-{}-{}-{}",
            std::process::id(),
            seed.0,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("a scratch directory");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn run(seed: u64) -> Report<Plan, Ended> {
    run_with(seed, Plan::from_seed(Seed(seed)))
}

pub fn run_with(seed: u64, plan: Plan) -> Report<Plan, Ended> {
    paused(seed, plan, simulate)
}

struct World {
    seed: Seed,
    plan: Plan,
    network: Network,
    ledger: Ledger,
    server: Rc<Server>,
    keys: Rc<Keys>,
    actor: Rc<Actor>,
    paid: RefCell<BTreeMap<(usize, usize), usize>>,
}

async fn simulate(seed: Seed, plan: Plan) -> Report<Plan, Ended> {
    let start = Instant::now();
    let network = match plan.mainnet {
        true => Network::MainNetwork,
        false => Network::TestNetwork,
    };
    let nu7 = network
        .activation_height(NetworkUpgrade::Nu7)
        .map(u32::from)
        .expect("a scheduled upgrade");
    let base = match plan.era {
        Era::Before => nu7 - 20_000,
        Era::Across(blocks) => nu7 - blocks - plan.sky.steady,
        Era::After => nu7 + 5_000,
    };
    let keys = Rc::new(Keys::new(seed, network, plan.errands.len() + 1));
    let clears = EPOCH + plan.clears;
    let ledger = Ledger::new(seed, network, keys.all(), base, EPOCH, plan.sky, clears);
    let clears = start + Duration::from_secs(plan.clears);
    let server = Server::new(ledger.clone(), seed, plan.service, clears);
    let scratch = Scratch::new(seed);
    let saved = Saved {
        steps: vec![Step::Waiting; plan.errands.len()],
        imported: vec![None; plan.errands.len()],
        plain: vec![false; plan.errands.len()],
        tries: vec![0; plan.errands.len()],
        ..Default::default()
    };
    let actor = Rc::new(Actor {
        network,
        confirmations: plan.confirmations,
        poll: Duration::from_secs(plan.poll),
        seasons: plan.sky.steady,
        owns: plan.own,
        inventory: plan.inventory,
        errands: plan.errands.clone(),
        ledger: ledger.clone(),
        server: server.clone(),
        keys: keys.clone(),
        path: scratch.0.join("wallet.sqlite"),
        start,
        saved: Rc::new(RefCell::new(saved)),
        failures: Rc::default(),
        heard: Arc::default(),
    });
    let world = Rc::new(World {
        seed,
        plan: plan.clone(),
        network,
        ledger: ledger.clone(),
        server,
        keys,
        actor: actor.clone(),
        paid: RefCell::default(),
    });
    let mut background = vec![
        ("the Zcash chain", spawn_local(world.clone().mine(start))),
        ("the payer", spawn_local(world.clone().fund())),
    ];
    for index in 0..plan.errands.len() {
        background.push(("the signer", spawn_local(world.clone().sign(index))));
    }
    let note = |what: String| ledger.note(what);
    let horizon = || start + Duration::from_secs(plan.blocks * SPACING);
    let life = lives(&note, start, horizon, &plan.crashes, || {
        spawn_local(actor.clone().live())
    })
    .await;
    panics::end(background.into_iter().chain(life)).await;
    let audited = spawn_local(actor.clone().audit()).await;
    world.report(panics::judge("the audit", audited).unwrap_or(false))
}

impl World {
    async fn mine(self: Rc<Self>, start: Instant) {
        let seasons = self.plan.sky.steady;
        let born = || {
            let saved = self.actor.saved.borrow();
            saved.account.is_some() || saved.earlier.is_some()
        };
        while seasons > 0 && !born() && start.elapsed().as_secs() < SPACING {
            sleep(Duration::from_secs(1)).await;
        }
        for _ in 0..seasons.min(u32::from(born()) * (seasons + 1)) {
            self.ledger.tick(EPOCH + start.elapsed().as_secs());
        }
        loop {
            let now = EPOCH + start.elapsed().as_secs();
            let wait = match self.ledger.quick(now) {
                true => self.seed.between("zcash-quick-gap", now, 2, 9),
                false => SPACING,
            };
            sleep(Duration::from_secs(wait)).await;
            self.ledger.tick(EPOCH + start.elapsed().as_secs());
        }
    }

    async fn fund(self: Rc<Self>) {
        let kind = self.network.network_type();
        loop {
            sleep(Duration::from_secs(5)).await;
            let mut due = Vec::new();
            {
                let saved = self.actor.saved.borrow();
                let own = saved.own.as_ref();
                for (n, (delay, value)) in self.plan.funds.iter().enumerate() {
                    let ripe = own.filter(|(_, since)| since.elapsed().as_secs() >= *delay);
                    if let Some((address, _)) = ripe {
                        due.push(((FUNDS, n), receiver(address, kind), *value));
                    }
                }
                for (index, errand) in self.plan.errands.iter().enumerate() {
                    match errand {
                        Errand::Receive { notes, .. } | Errand::Sign { notes, .. } => {
                            let Some(since) = saved.imported[index] else {
                                continue;
                            };
                            let to = self.keys.joints[index].account.deposit_address();
                            let ripe = notes.iter().enumerate();
                            let ripe =
                                ripe.filter(|(_, (delay, _))| since.elapsed().as_secs() >= *delay);
                            due.extend(ripe.map(|(n, (_, value))| ((index, n), to, *value)));
                        }
                        Errand::Fee {
                            at,
                            index: slot,
                            value,
                        } => {
                            let ripe = own.is_some() && self.actor.start.elapsed().as_secs() >= *at;
                            if ripe {
                                let to = self.keys.fee_address(*slot);
                                due.push(((index, 0), to, *value));
                            }
                        }
                        Errand::Pay { .. } => {}
                    }
                }
            }
            let mut paid = self.paid.borrow_mut();
            for (key, to, value) in due {
                paid.entry(key)
                    .or_insert_with(|| self.ledger.pay(to, value, ValuePool::Ironwood));
            }
        }
    }

    async fn sign(self: Rc<Self>, index: usize) {
        let Errand::Sign {
            at,
            notes,
            trick,
            view,
            proves,
        } = self.plan.errands[index].clone()
        else {
            return;
        };
        let (actor, ledger, keys) = (&self.actor, &self.ledger, &self.keys);
        let kind = self.network.network_type();
        let label = format!("joint {index}");
        sleep(Duration::from_secs(at)).await;
        actor.saved.borrow_mut().imported[index] = Some(Instant::now());
        ledger.note(format!("a sweep of {label} is wanted"));
        let expected: u64 = notes.iter().map(|(_, value)| value).sum();
        let asked = Instant::now();
        let held = loop {
            sleep(Duration::from_secs(20)).await;
            let held = ledger.everything(JOINTS + index);
            let unspent = held.iter().filter(|note| note.spent.is_none());
            let all: u64 = unspent.clone().map(|note| note.value).sum();
            if all >= expected || (all > 0 && asked.elapsed().as_secs() >= 900) {
                break (all, unspent.count());
            }
        };
        let (spent, count) = held;
        let joint = &keys.joints[index];
        let other = &keys.joints[self.plan.errands.len()];
        let (home, thief) = (keys.stranger_address(), other.account.deposit_address());
        let due = signer::fee(count, 1);
        let slack = 20_000;
        let first = ledger.everything(JOINTS + index);
        let first = first.iter().find(|note| note.spent.is_none());
        let first = first.map_or(0, |note| note.value);
        let outputs = match trick {
            Trick::Partial => vec![(home, first - signer::fee(1, 1))],
            Trick::Elsewhere | Trick::Forges => vec![(thief, spent - due)],
            Trick::Skims => vec![(home, spent - due - slack), (thief, slack)],
            Trick::Overpays => vec![(home, spent - due - slack)],
            _ => vec![(home, spent - due)],
        };
        let mut rng = ChaCha20Rng::from_seed(self.seed.bytes(&format!("sweep-{index}")));
        let owner = joint.account.fvk();
        let most = match trick {
            Trick::Partial => 1,
            _ => usize::MAX,
        };
        let key = JOINTS + index;
        let built = signer::sweep(ledger, self.network, key, owner, &outputs, most, &mut rng);
        let Some(sweep) = built else {
            return ledger.note(format!("no sweep of {label} could be built"));
        };
        let floor = spent - due - slack;
        let intent = SweepIntent::from_address(&unified(home, kind), kind, floor, due)
            .expect("a sweep's intent");
        let whole = sweep.pczt.clone();
        let seen = signer::shown(sweep.pczt, view);
        let key = match trick {
            Trick::AnothersKey => &other.key,
            _ => &joint.key,
        };
        let key = std::slice::from_ref(key);
        let bytes = match trick {
            Trick::Hides => signer::unvalued(seen).serialize().ok(),
            Trick::Garbles => signer::garbled(&seen, &keys.stranger),
            Trick::Forges => signer::forged(&seen, thief, home),
            Trick::Presigned => {
                let once = seen.serialize().ok();
                once.and_then(|once| sign_pczt_bytes(&once, key, &intent).ok())
            }
            _ => seen.serialize().ok(),
        };
        let Some(bytes) = bytes else {
            return ledger.note(format!("no sweep of {label} could be shown"));
        };
        let signed = match sign_pczt_bytes(&bytes, key, &intent) {
            Ok(signed) => signed,
            Err(e) => {
                let why = match e {
                    zecswap_core::Error::SweepIntent(why) => why,
                    zecswap_core::Error::UnsignedSpend { .. } => "no key for a spend".into(),
                    zecswap_core::Error::NothingToSign => "nothing to sign".into(),
                    _ => "an unreadable sweep".into(),
                };
                ledger.note(format!("the signer refused the sweep of {label}: {why}"));
                if trick == Trick::None {
                    actor.fail(
                        44,
                        format!("the signer refused an honest sweep of {label}: {why}"),
                    );
                }
                return actor.saved.borrow_mut().steps[index] = Step::Done(End::Balked);
            }
        };
        ledger.note(format!("the signer signed the sweep of {label}"));
        let ours = ledger.everything(JOINTS + index);
        let effects = signer::effects(&bytes, &keys.stranger, home);
        let fair = effects.is_some_and(|effects| {
            let spent = ours
                .iter()
                .filter(|note| effects.nullifiers.contains(&note.nullifier));
            let spent: u64 = spent.map(|note| note.value).sum();
            effects.received >= floor && spent.saturating_sub(effects.received) <= due
        });
        if !fair {
            actor.fail(
                45,
                format!(
                    "the signer signed a sweep of {label} that {trick:?}: its destination was to get {floor} of {spent} and the network at most {due}"
                ),
            );
        }
        if proves && trick == Trick::None {
            let Some((raw, txid)) = signer::finished(whole, &signed, &mut rng) else {
                return actor.fail(
                    46,
                    format!("the signed sweep of {label} made no transaction"),
                );
            };
            actor.saved.borrow_mut().built.push(Built {
                errand: index,
                txid,
                made: Made::Sweep {
                    home: Home::Stranger,
                    held: spent,
                },
                tip: ledger.tip().0,
                settled: true,
                quoted: Some(sweep.fee),
                home: Some(home),
            });
            loop {
                ledger.accept(&raw, true);
                sleep(Duration::from_secs(SPACING)).await;
                let fact = ledger.ordinal(&txid).map(|n| ledger.facts().swap_remove(n));
                let over = |fact: ledger::Fact| {
                    fact.mined.is_some() || fact.rivalled || ledger.tip().0 > fact.expiry
                };
                if fact.is_none_or(over) {
                    break;
                }
            }
        }
        actor.saved.borrow_mut().steps[index] = Step::Done(End::Signed(spent));
    }

    fn gone(&self, index: usize, notes: usize) -> bool {
        let paid = self.paid.borrow();
        (0..notes).any(|n| match paid.get(&(index, n)) {
            Some(id) => self.ledger.payment(*id).2,
            None => true,
        })
    }

    fn report(&self, audited: bool) -> Report<Plan, Ended> {
        let (ledger, keys, plan) = (&self.ledger, &self.keys, &self.plan);
        let saved = self.actor.saved.borrow();
        let facts = ledger.facts();
        let trace = ledger.trace();
        let base = ledger.base();
        let steady = !trace.iter().any(|line| line.starts_with("reorg of"));
        let mut failures: Vec<String> = Vec::new();
        let mut failed = BTreeSet::new();
        let mut fail = |check: u8, what: String| {
            if failed.insert(check) {
                failures.push(what);
            }
        };
        for (check, what) in self.actor.failures.borrow().iter() {
            fail(*check, what.clone());
        }
        for what in self.server.failures() {
            fail(30, what);
        }
        if plan.service.another && saved.served {
            fail(
                31,
                "the wallet took a lightwalletd of another network for its own".into(),
            );
        }
        for fact in &facts {
            let name = format!("transaction {}", fact.ordinal);
            if let Outcome::Refused(why) = fact.first
                && !why.stale()
            {
                fail(
                    32,
                    format!("{name} was refused when first broadcast: {why:?}"),
                );
            }
            let at_expiry = BranchId::for_height(&self.network, BlockHeight::from(fact.expiry));
            if fact.expiry == 0 || at_expiry != fact.branch {
                fail(
                    33,
                    format!(
                        "{name}, built for {:?}, may wait until block {}, which is {at_expiry:?}'s",
                        fact.branch,
                        i64::from(fact.expiry) - i64::from(base)
                    ),
                );
            }
            let due = MARGINAL_FEE * fact.actions.max(2) as u64;
            if fact.fee != due {
                fail(
                    34,
                    format!(
                        "{name}, of {} actions, pays the network {} where {due} is due",
                        fact.actions, fact.fee
                    ),
                );
            }
        }
        for built in &saved.built {
            let Some(ordinal) = ledger.ordinal(&built.txid) else {
                continue;
            };
            let fact = &facts[ordinal];
            let name = format!("transaction {ordinal}");
            match built.made {
                Made::Sweep { home, held } => {
                    let joint = &keys.joints[built.errand].account;
                    let ovks =
                        [Scope::External, Scope::Internal].map(|scope| joint.fvk().to_ovk(scope));
                    let readable = ledger.with_transaction(ordinal, |tx| {
                        [tx.orchard_bundle(), tx.ironwood_bundle()]
                            .into_iter()
                            .flatten()
                            .any(|bundle| !bundle.recover_outputs_with_ovks(&ovks).is_empty())
                    });
                    if readable {
                        fail(
                            35,
                            format!(
                                "{name}, a sweep, tells the other holder of the joint viewing key where it went"
                            ),
                        );
                    }
                    if fact.fee > SWEEP_FEE_CEILING || built.quoted != Some(fact.fee) {
                        fail(
                            36,
                            format!(
                                "{name}, a sweep, pays the network {}; the wallet had said {:?}",
                                fact.fee, built.quoted
                            ),
                        );
                    }
                    if fact.mined.is_none() {
                        continue;
                    }
                    let notes = ledger.everything(JOINTS + built.errand);
                    let spent = notes
                        .iter()
                        .filter(|note| fact.nullifiers.contains(&note.nullifier));
                    let spent: u64 = spent.map(|note| note.value).sum();
                    let key = match home {
                        Home::Own => OWN,
                        Home::Stranger => STRANGER,
                    };
                    let paid = ledger.paid_by(ordinal, key);
                    let paid = paid
                        .iter()
                        .filter(|(to, _)| Some(*to) == built.home)
                        .map(|(_, value)| value);
                    let paid: u64 = paid.sum();
                    let kept = ledger.paid_by(ordinal, JOINTS + built.errand).len();
                    if spent != held || paid + fact.fee != held || kept > 0 {
                        fail(
                            37,
                            format!(
                                "{name} swept {spent} of the {held} the wallet held, paid {paid} to its destination and {} to the network, and left {kept} notes behind",
                                fact.fee
                            ),
                        );
                    }
                    let left = notes.iter().filter(|note| {
                        note.mined <= built.tip
                            && note.spent.is_none_or(|at| at > built.tip)
                            && !fact.nullifiers.contains(&note.nullifier)
                    });
                    if steady && built.settled && left.count() > 0 {
                        fail(
                            38,
                            format!("{name}, a sweep, left notes of its account unspent"),
                        );
                    }
                }
                Made::Payment { amount } => {
                    if fact.mined.is_none() {
                        continue;
                    }
                    let paid = ledger.paid_by(ordinal, STRANGER);
                    let paid: u64 = paid.iter().map(|(_, value)| value).sum();
                    let notes = ledger.everything(OWN);
                    let spent = notes
                        .iter()
                        .filter(|note| fact.nullifiers.contains(&note.nullifier));
                    let spent: u64 = spent.map(|note| note.value).sum();
                    let change = ledger.paid_by(ordinal, OWN);
                    let change: u64 = change.iter().map(|(_, value)| value).sum();
                    if paid != amount || spent != amount + change + fact.fee {
                        fail(
                            39,
                            format!(
                                "{name}, a payment of {amount}, spent {spent}, paid {paid}, returned {change} and paid the network {}",
                                fact.fee
                            ),
                        );
                    }
                }
            }
        }
        for (index, _) in plan.errands.iter().enumerate() {
            let mined = saved.built.iter().filter(|built| {
                let ordinal = ledger.ordinal(&built.txid);
                built.errand == index && ordinal.is_some_and(|n| facts[n].mined.is_some())
            });
            if mined.count() > 1 {
                fail(
                    40,
                    format!("errand {index} was paid for by more than one mined transaction"),
                );
            }
        }
        for (index, fee, quoted) in &saved.refused {
            let notes = ledger.everything(JOINTS + index);
            let due = MARGINAL_FEE * notes.len().max(2) as u64;
            if *fee != due || quoted != fee || notes.len() <= MOST_NOTES {
                fail(
                    41,
                    format!(
                        "the wallet refused to sweep joint {index}, of {} notes, for a fee of {fee}, having said {quoted}; {due} is due",
                        notes.len()
                    ),
                );
            }
        }
        let ends: Vec<Option<End>> = saved
            .steps
            .iter()
            .map(|step| match step {
                Step::Done(end) => Some(end.clone()),
                _ => None,
            })
            .collect();
        let short = self.gone(FUNDS, plan.funds.len());
        if plan.settles() && !plan.service.another {
            if !audited {
                fail(42, "the wallet could not be read at the end".into());
            }
            let tip = ledger.tip().0;
            for (index, errand) in plan.errands.iter().enumerate() {
                let last = saved.built.iter().rfind(|built| built.errand == index);
                let last = last.and_then(|built| ledger.ordinal(&built.txid));
                let waits =
                    last.is_some_and(|n| facts[n].mined.is_none() && tip <= facts[n].expiry);
                let built = saved.built.iter().filter(|built| built.errand == index);
                let mut theirs = built.filter_map(|built| ledger.ordinal(&built.txid));
                let waits = waits || theirs.any(|n| facts[n].rivalled);
                let fine = match (errand, &ends[index]) {
                    (_, None) if waits => true,
                    (Errand::Receive { notes, .. }, _) if self.gone(index, notes.len()) => true,
                    (Errand::Receive { notes, sweep, .. }, Some(end)) => {
                        let all: u64 = notes.iter().map(|(_, value)| value).sum();
                        match (sweep, end) {
                            (None, End::Watched(seen)) => *seen == all,
                            (Some(_), End::Refused(_)) => notes.len() > MOST_NOTES,
                            (Some(_), End::Swept(swept)) => {
                                *swept == all && notes.len() <= MOST_NOTES
                            }
                            _ => false,
                        }
                    }
                    (Errand::Sign { notes, .. }, _) if self.gone(index, notes.len()) => true,
                    (Errand::Sign { notes, trick, .. }, Some(end)) => {
                        let all: u64 = notes.iter().map(|(_, value)| value).sum();
                        match trick {
                            Trick::None => *end == End::Signed(all),
                            _ => *end == End::Balked,
                        }
                    }
                    (Errand::Pay { amount, .. }, Some(End::Paid(paid))) => paid == amount,
                    (Errand::Pay { .. }, None) => short || !plan.own,
                    (Errand::Fee { .. }, None) => self.gone(index, 1) || !plan.own,
                    (Errand::Fee { value, .. }, Some(End::Seen(seen))) => seen >= value,
                    _ => false,
                };
                if !fine {
                    fail(
                        43,
                        format!(
                            "with time enough once the weather cleared, errand {index} ({errand:?}) ended {:?}",
                            ends[index]
                        ),
                    );
                }
            }
        }
        let held = ledger.everything(OWN);
        let held = held.iter().filter(|note| note.spent.is_none());
        Report {
            seed: self.seed.0,
            plan: plan.clone(),
            ended: Some(Ended {
                errands: ends,
                own: held.map(|note| note.value).sum(),
                sent: facts.len(),
                mined: facts.iter().filter(|fact| fact.mined.is_some()).count(),
                audited,
            }),
            stage: None,
            failures,
            trace,
        }
    }
}
