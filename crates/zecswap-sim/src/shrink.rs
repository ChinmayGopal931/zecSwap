use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use zecswap_client::FeeCap;

use crate::deposits::{self, Arrival, Offer, Where};
use crate::guards::{self, Ask, Funded, Relayed, Request, Stance};
use crate::hunt::{Kind, Labels, Outcome, Told, Tree, named, signature, spread, workspace};
use crate::lightwallet::{self, Ahead, Era, Errand, Home, Sky, Trick};
use crate::maker::{Fee, Mood};
use crate::net::Line;
use crate::node::Weather;
use crate::run::Report;
use crate::seed::Seed;
use crate::{Plan, cli, reverse};

const LONG: usize = 4;
const SPARE: u64 = 50_000;
const CHECK: &str = "plan=";

pub struct Step<P> {
    name: String,
    plan: P,
    calms: bool,
}

pub trait Shrinks: Clone + Debug + Send + Sync + 'static {
    type Ended: Debug + Send + 'static;

    fn plain() -> Self;

    fn of(seed: u64) -> Self;

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended>;

    fn calm(&self) -> bool;

    fn breaks(&self) -> Vec<&'static str>;

    fn simpler(&self) -> Vec<Step<Self>>;
}

struct Steps<P> {
    from: P,
    plain: P,
    out: Vec<Step<P>>,
}

fn shorter<T: Clone>(list: &[T], least: usize) -> Vec<(String, Vec<T>)> {
    let count = list.len();
    if count > LONG {
        let mut kept = vec![least.max(1), count.div_ceil(2)];
        kept.dedup();
        let kept = kept.into_iter();
        return kept
            .map(|keep| (format!("[..{keep}]"), list[..keep].to_vec()))
            .collect();
    }
    let dropped = (0..count).rev().filter(|_| count > least);
    dropped
        .map(|index| {
            let mut rest = list.to_vec();
            rest.remove(index);
            (format!("-{index}"), rest)
        })
        .collect()
}

impl<P: Shrinks> Steps<P> {
    fn of(from: &P) -> Self {
        Self {
            from: from.clone(),
            plain: P::plain(),
            out: Vec::new(),
        }
    }

    fn step(&mut self, name: String, calms: bool, change: impl FnOnce(&mut P)) {
        let mut plan = self.from.clone();
        change(&mut plan);
        self.out.push(Step { name, plan, calms });
    }

    fn set<T: Clone + Debug>(&mut self, name: &str, calms: bool, at: impl Fn(&mut P) -> &mut T) {
        let (plain, now) = (at(&mut self.plain).clone(), at(&mut self.from).clone());
        let told = format!("{plain:?}").replace(' ', "");
        if format!("{now:?}") != format!("{plain:?}") {
            self.step(format!("{name}={told}"), calms, |plan| *at(plan) = plain);
        }
    }

    fn fewer<T: Clone>(
        &mut self,
        name: &str,
        least: usize,
        calms: bool,
        at: impl Fn(&mut P) -> &mut Vec<T>,
        eased: impl Fn(&T) -> Vec<(String, T)>,
    ) {
        let list = at(&mut self.from).clone();
        for (index, each) in list.iter().enumerate().rev() {
            for (how, simpler) in eased(each) {
                let name = format!("{name}.{index}.{how}");
                self.step(name, false, |plan| at(plan)[index] = simpler);
            }
        }
        for (how, rest) in shorter(&list, least) {
            self.step(format!("{name}{how}"), calms, |plan| *at(plan) = rest);
        }
    }
}

macro_rules! each {
    ($steps:ident, $plan:ident => $part:expr, $type:path {
        calms: [$($calm:ident),* $(,)?],
        also: [$($also:ident),* $(,)?],
        apart: [$($apart:ident),* $(,)?] $(,)?
    }) => {{
        let _every_field = {
            let $plan = &$steps.from;
            let part = &$part;
            $type {
                $($calm: part.$calm.clone(),)*
                $($also: part.$also.clone(),)*
                $($apart: part.$apart.clone(),)*
            }
        };
        let part = stringify!($part).split_once('.');
        let part = part.map_or(String::new(), |(_, part)| format!("{part}."));
        $($steps.set(&format!("{part}{}", stringify!($calm)), true, |$plan| &mut $part.$calm);)*
        $($steps.set(&format!("{part}{}", stringify!($also)), false, |$plan| &mut $part.$also);)*
    }};
}

fn broken(rules: &[(bool, &'static str)]) -> Vec<&'static str> {
    let broken = rules.iter().filter(|(kept, _)| !kept);
    broken.map(|(_, rule)| *rule).collect()
}

fn whole<T>(_: &T) -> Vec<(String, T)> {
    Vec::new()
}

impl Shrinks for Plan {
    type Ended = crate::End;

    fn plain() -> Self {
        Self::plain()
    }

    fn of(seed: u64) -> Self {
        Self::from_seed(Seed(seed))
    }

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended> {
        crate::run_with(seed, plan)
    }

    fn calm(&self) -> bool {
        self.calm()
    }

    fn breaks(&self) -> Vec<&'static str> {
        let asked = match self.fee {
            Fee::Asked(fee) => fee <= self.max_open_fee,
            Fee::None | Fee::Transparent(_) => false,
        };
        let kept = self.mood != Mood::KeepsFee || asked;
        broken(&[(kept, "a maker that keeps the fee asks one under the cap")])
    }

    fn simpler(&self) -> Vec<Step<Self>> {
        let mut steps = Steps::of(self);
        each!(steps, plan => plan, Plan {
            calms: [skew, linger_verify, linger_claim, zcash_loss],
            also: [mood, service, fee, max_open_fee, railgun, private, multicall, clears, units],
            apart: [weather, maker_line, relayer_line, crashes],
        });
        steps.fewer("crashes", 0, true, |plan| &mut plan.crashes, whole);
        each!(steps, plan => plan.weather, Weather {
            calms: [
                reorg, reorg_after_tx, missed_slot, delayed, lost, private_lost, fee_swing,
                rpc_error, rpc_stall, stale_read,
            ],
            also: [ambush, fork_delay, reorg_depth, delay_blocks],
            apart: [],
        });
        each!(steps, plan => plan.maker_line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        each!(steps, plan => plan.relayer_line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        steps.out
    }
}

impl Shrinks for reverse::Plan {
    type Ended = reverse::Ended;

    fn plain() -> Self {
        Self::plain()
    }

    fn of(seed: u64) -> Self {
        Self::from_seed(Seed(seed))
    }

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended> {
        reverse::run_with(seed, plan)
    }

    fn calm(&self) -> bool {
        self.calm()
    }

    fn breaks(&self) -> Vec<&'static str> {
        Vec::new()
    }

    fn simpler(&self) -> Vec<Step<Self>> {
        let mut steps = Steps::of(self);
        each!(steps, plan => plan, reverse::Plan {
            calms: [skew, patience, regret, linger_ready, linger_refund, zcash_loss],
            also: [mood, service, private, clears, units],
            apart: [weather, maker_line, relayer_line, crashes],
        });
        steps.fewer("crashes", 0, true, |plan| &mut plan.crashes, whole);
        each!(steps, plan => plan.weather, Weather {
            calms: [
                reorg, reorg_after_tx, ambush, fork_delay, missed_slot, delayed, lost,
                private_lost, fee_swing, rpc_error, rpc_stall, stale_read,
            ],
            also: [reorg_depth, delay_blocks],
            apart: [],
        });
        each!(steps, plan => plan.maker_line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        each!(steps, plan => plan.relayer_line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        steps.out
    }
}

fn shown(arrival: &Arrival) -> Vec<(String, Arrival)> {
    let Arrival { at, to, amount } = *arrival;
    let shown = Arrival {
        at,
        to: Where::Shown,
        amount,
    };
    match to {
        Where::Shown => Vec::new(),
        Where::Reused(_) | Where::Ahead(_) | Where::Far(_) | Where::Index(_) => {
            vec![("to=Shown".to_owned(), shown)]
        }
    }
}

impl Shrinks for deposits::Plan {
    type Ended = deposits::Ended;

    fn plain() -> Self {
        Self::plain()
    }

    fn of(seed: u64) -> Self {
        Self::from_seed(Seed(seed))
    }

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended> {
        deposits::run_with(seed, plan)
    }

    fn calm(&self) -> bool {
        false
    }

    fn breaks(&self) -> Vec<&'static str> {
        Vec::new()
    }

    fn simpler(&self) -> Vec<Step<Self>> {
        let mut steps = Steps::of(self);
        each!(steps, plan => plan, deposits::Plan {
            calms: [returned, skew],
            also: [service, reprice, multicall, clears, payout, fees_stay],
            apart: [weather, line, offer, cap, arrivals, crashes],
        });
        steps.fewer("crashes", 0, true, |plan| &mut plan.crashes, whole);
        steps.fewer("arrivals", 1, false, |plan| &mut plan.arrivals, shown);
        each!(steps, plan => plan.offer, Offer {
            calms: [],
            also: [fee, fee_bps, min_value, max_validity],
            apart: [],
        });
        each!(steps, plan => plan.cap, FeeCap {
            calms: [],
            also: [fee, fee_bps],
            apart: [],
        });
        each!(steps, plan => plan.weather, Weather {
            calms: [
                reorg, reorg_after_tx, ambush, fork_delay, missed_slot, delayed, lost,
                private_lost, fee_swing, rpc_error, rpc_stall, stale_read,
            ],
            also: [reorg_depth, delay_blocks],
            apart: [],
        });
        each!(steps, plan => plan.line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        steps.out
    }
}

fn eased(errand: &Errand, own: bool) -> Vec<(String, Errand)> {
    let mut out = Vec::new();
    match errand {
        Errand::Receive {
            at,
            chained,
            notes,
            sweep,
            patience,
        } => {
            let receive = |chained: bool, notes: &[(u64, u64)], sweep: Option<Home>| {
                let (at, notes, patience) = (*at, notes.to_vec(), *patience);
                Errand::Receive {
                    at,
                    chained,
                    notes,
                    sweep,
                    patience,
                }
            };
            if *chained {
                out.push(("chained=false".to_owned(), receive(false, notes, *sweep)));
            }
            if own && *sweep == Some(Home::Stranger) {
                let home = Some(Home::Own);
                out.push((format!("sweep={home:?}"), receive(*chained, notes, home)));
            }
            for (how, rest) in shorter(notes, 1) {
                out.push((format!("notes{how}"), receive(*chained, &rest, *sweep)));
            }
        }
        Errand::Pay { at, amount, ahead } => {
            let pay = |ahead: Option<Ahead>| Errand::Pay {
                at: *at,
                amount: *amount,
                ahead,
            };
            if let Some(Ahead { hold, then, crash }) = *ahead {
                if crash.is_some() {
                    let crash = None;
                    let ahead = Ahead { hold, then, crash };
                    out.push(("ahead.crash=None".to_owned(), pay(Some(ahead))));
                }
                out.push(("ahead=None".to_owned(), pay(None)));
            }
        }
        Errand::Fee {
            at: _,
            index: _,
            value: _,
        } => {}
        Errand::Sign {
            at,
            notes,
            trick,
            view,
            proves,
        } => {
            let sign = |notes: &[(u64, u64)], trick: Trick| {
                let (at, notes, view, proves) = (*at, notes.to_vec(), *view, *proves);
                Errand::Sign {
                    at,
                    notes,
                    trick,
                    view,
                    proves,
                }
            };
            if *trick != Trick::None {
                let honest = Trick::None;
                out.push((format!("trick={honest:?}"), sign(notes, honest)));
            }
            if *trick != Trick::Partial {
                for (how, rest) in shorter(notes, 1) {
                    out.push((format!("notes{how}"), sign(&rest, *trick)));
                }
            }
        }
    }
    out
}

impl Shrinks for lightwallet::Plan {
    type Ended = lightwallet::Ended;

    fn plain() -> Self {
        Self::plain()
    }

    fn of(seed: u64) -> Self {
        Self::from_seed(Seed(seed))
    }

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended> {
        lightwallet::run_with(seed, plan)
    }

    fn calm(&self) -> bool {
        self.calm()
    }

    fn breaks(&self) -> Vec<&'static str> {
        let (sky, service) = (&self.sky, &self.service);
        let watched = |errand: &Errand| matches!(errand, Errand::Receive { sweep: None, .. });
        let joint = |errand: &Errand| match errand {
            Errand::Receive { sweep, .. } => *sweep != Some(Home::Own),
            Errand::Sign { .. } => true,
            Errand::Pay { .. } | Errand::Fee { .. } => false,
        };
        let backwards =
            sky.steady == 0 && sky.reorg + sky.reorg_after_tx == 0 && service.lags >= 600;
        let dropped =
            sky.dropped == 1000 && sky.reorg_after_tx >= 800 && self.errands.iter().all(watched);
        let alone = !self.inventory && self.errands.iter().all(joint);
        broken(&[
            (
                !service.backwards || backwards,
                "a tip that goes back comes with a fresh wallet, no reorgs and lags",
            ),
            (
                sky.dropped == 0 || dropped,
                "payments lost with their blocks come with reorgs after them and a watched account alone",
            ),
            (
                self.era != Era::After || sky.lost == 0,
                "no transaction is lost after the upgrade",
            ),
            (
                self.own != self.funds.is_empty(),
                "funds come with an account of the wallet's own",
            ),
            (
                self.own || alone,
                "a wallet of joint accounts alone receives and signs only",
            ),
        ])
    }

    fn simpler(&self) -> Vec<Step<Self>> {
        let mut steps = Steps::of(self);
        each!(steps, plan => plan, lightwallet::Plan {
            calms: [],
            also: [mainnet, era, confirmations, poll, own, inventory, clears],
            apart: [sky, service, funds, errands, crashes, blocks],
        });
        let least = steps.plain.blocks;
        let half = least + self.blocks.saturating_sub(least) / 2;
        let mut shorter_runs = vec![least, half];
        shorter_runs.dedup();
        for blocks in shorter_runs
            .into_iter()
            .filter(|blocks| *blocks < self.blocks)
        {
            steps.step(format!("blocks={blocks}"), false, |plan| {
                plan.blocks = blocks
            });
        }
        steps.fewer("crashes", 0, false, |plan| &mut plan.crashes, whole);
        let own = self.own;
        let simpler = |errand: &Errand| eased(errand, own);
        steps.fewer("errands", 1, false, |plan| &mut plan.errands, simpler);
        let owed = self.errands.iter().map(|errand| match errand {
            Errand::Pay { amount, .. } => amount + SPARE,
            Errand::Receive { .. } | Errand::Fee { .. } | Errand::Sign { .. } => 0,
        });
        let owed: u64 = owed.sum();
        for (how, rest) in shorter(&self.funds, 1) {
            if rest.iter().map(|(_, value)| value).sum::<u64>() >= owed {
                steps.step(format!("funds{how}"), false, |plan| plan.funds = rest);
            }
        }
        each!(steps, plan => plan.sky, Sky {
            calms: [reorg, reorg_after_tx, dropped, delayed, lost, quick, rival],
            also: [steady, reorg_depth, traffic],
            apart: [],
        });
        each!(steps, plan => plan.service, lightwallet::Service {
            calms: [errors, cuts, deaf, hangups, misplaces],
            also: [slow, stalls, lags, backwards, another, quiet],
            apart: [],
        });
        steps.out
    }
}

fn plainer(request: &Request) -> Vec<(String, Request)> {
    let mut out = Vec::new();
    if request.twice {
        let once = Request {
            twice: false,
            ..*request
        };
        out.push(("twice=false".to_owned(), once));
    }
    let honest = match request.ask {
        Ask::Relayed(Relayed::Private) | Ask::Funded(Funded::Honest) => None,
        Ask::Relayed(_) => Some(Ask::Relayed(Relayed::Private)),
        Ask::Funded(_) => Some(Ask::Funded(Funded::Honest)),
    };
    if let Some(ask) = honest {
        let plain = Request { ask, ..*request };
        out.push((format!("ask={ask:?}"), plain));
    }
    out
}

impl Shrinks for guards::Plan {
    type Ended = guards::Ended;

    fn plain() -> Self {
        Self::plain()
    }

    fn of(seed: u64) -> Self {
        Self::from_seed(Seed(seed))
    }

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended> {
        guards::run_with(seed, plan)
    }

    fn calm(&self) -> bool {
        self.calm()
    }

    fn breaks(&self) -> Vec<&'static str> {
        let low = self.stance.eth < Stance::SOUND.eth;
        broken(&[(
            low == (self.stance.top_up > 0),
            "a relayer low on ETH is topped up, and only one",
        )])
    }

    fn simpler(&self) -> Vec<Step<Self>> {
        let mut steps = Steps::of(self);
        each!(steps, plan => plan, guards::Plan {
            calms: [],
            also: [multicall, clears],
            apart: [weather, stance, requests, crashes],
        });
        steps.fewer("crashes", 0, true, |plan| &mut plan.crashes, whole);
        steps.fewer("requests", 1, false, |plan| &mut plan.requests, plainer);
        each!(steps, plan => plan.stance, Stance {
            calms: [cap, eth, top_up, config, adapter],
            also: [priced],
            apart: [],
        });
        each!(steps, plan => plan.weather, Weather {
            calms: [
                reorg, reorg_after_tx, ambush, fork_delay, missed_slot, delayed, lost,
                private_lost, fee_swing, rpc_error, rpc_stall, stale_read,
            ],
            also: [reorg_depth, delay_blocks],
            apart: [],
        });
        steps.out
    }
}

fn calmer(kill: &cli::Kill) -> Vec<(String, cli::Kill)> {
    let short = 30;
    match kill.rest > short {
        true => vec![(
            format!("rest={short}"),
            cli::Kill {
                rest: short,
                ..*kill
            },
        )],
        false => Vec::new(),
    }
}

impl Shrinks for cli::Plan {
    type Ended = cli::Ended;

    fn plain() -> Self {
        Self::plain()
    }

    fn of(seed: u64) -> Self {
        Self::from_seed(Seed(seed))
    }

    fn run(seed: u64, plan: Self) -> Report<Self, Self::Ended> {
        cli::run_with(seed, plan)
    }

    fn calm(&self) -> bool {
        self.calm()
    }

    fn breaks(&self) -> Vec<&'static str> {
        let asked = match self.fee {
            Fee::Asked(fee) => fee <= self.max_open_fee,
            Fee::None | Fee::Transparent(_) => false,
        };
        let kept = self.mood != Mood::KeepsFee || asked;
        broken(&[(kept, "a maker that keeps the fee asks one under the cap")])
    }

    fn simpler(&self) -> Vec<Step<Self>> {
        let mut steps = Steps::of(self);
        each!(steps, plan => plan, cli::Plan {
            calms: [skew],
            also: [
                mood, service, fee, max_open_fee, railgun, multicall, units, funds, swaps, stale,
                clears, deposits,
            ],
            apart: [weather, maker_line, relayer_line, sky, lightwalletd, kills],
        });
        steps.fewer("kills", 0, true, |plan| &mut plan.kills, calmer);
        each!(steps, plan => plan.weather, Weather {
            calms: [
                reorg, reorg_after_tx, ambush, fork_delay, missed_slot, delayed, lost,
                private_lost, fee_swing, rpc_error, rpc_stall, stale_read,
            ],
            also: [reorg_depth, delay_blocks],
            apart: [],
        });
        each!(steps, plan => plan.maker_line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        each!(steps, plan => plan.relayer_line, Line {
            calms: [busy, hang, unheard, unanswered],
            also: [],
            apart: [],
        });
        each!(steps, plan => plan.sky, Sky {
            calms: [reorg, reorg_after_tx, dropped, delayed, lost, quick, rival],
            also: [steady, reorg_depth, traffic],
            apart: [],
        });
        each!(steps, plan => plan.lightwalletd, lightwallet::Service {
            calms: [errors, cuts, deaf, hangups, misplaces],
            also: [slow, stalls, lags, backwards, another, quiet],
            apart: [],
        });
        steps.out
    }
}

struct Withheld {
    calming: Vec<String>,
    outside: Vec<String>,
}

fn offered<P: Shrinks>(plan: &P) -> (Vec<Step<P>>, Withheld) {
    let mut withheld = Withheld {
        calming: Vec::new(),
        outside: Vec::new(),
    };
    let kept = plan.breaks();
    let mut steps = plan.simpler();
    steps.retain(|step| {
        let strays =
            !plan.calm() && step.plan.calm() && step.plan.simpler().iter().any(|next| next.calms);
        let breaks = step.plan.breaks().iter().any(|rule| !kept.contains(rule));
        let list = match (!breaks, strays) {
            (false, _) => &mut withheld.outside,
            (true, true) => &mut withheld.calming,
            (true, false) => return true,
        };
        list.push(step.name.clone());
        false
    });
    (steps, withheld)
}

fn mark<P: Debug>(plan: &P) -> String {
    named(&format!("{plan:?}"))
}

pub fn stepped<P: Shrinks>(mut plan: P, steps: &str) -> Result<P, String> {
    for step in steps.split_whitespace() {
        if let Some(shrunk) = step.strip_prefix(CHECK) {
            let ours = mark(&plan);
            if ours != shrunk {
                return Err(format!(
                    "the steps were taken to plan {shrunk} and lead to plan {ours} here, {plan:?}: this tree plans the seed otherwise (leave {step} out to run it all the same)"
                ));
            }
            continue;
        }
        let mut simpler = plan.simpler().into_iter();
        let Some(next) = simpler.find(|next| next.name == step) else {
            return Err(format!("no step {step} leads on from plan {plan:?}"));
        };
        plan = next.plan;
    }
    Ok(plan)
}

struct Search {
    seed: u64,
    tried: BTreeMap<String, Outcome>,
    runs: usize,
}

impl Search {
    fn run<P: Shrinks>(&mut self, plans: &[P]) -> Vec<Outcome>
    where
        Report<P, P::Ended>: Told,
    {
        let texts: Vec<String> = plans.iter().map(|plan| format!("{plan:?}")).collect();
        let mut fresh = Vec::new();
        let mut new = BTreeSet::new();
        for (index, text) in texts.iter().enumerate() {
            if !self.tried.contains_key(text) && new.insert(text) {
                fresh.push(index as u64);
            }
        }
        let seed = self.seed;
        let run = |index: u64| {
            let plan = plans[index as usize].clone();
            (index, Outcome::of(&P::run(seed, plan)))
        };
        spread(&fresh, run, |(index, outcome)| {
            self.tried.insert(texts[index as usize].clone(), outcome);
        });
        self.runs += fresh.len();
        texts.iter().map(|text| self.tried[text].clone()).collect()
    }
}

fn ways(outcome: &Outcome) -> BTreeMap<String, &str> {
    let mut ways = BTreeMap::new();
    for failure in &outcome.failures {
        let way = named(&signature(failure));
        ways.entry(way).or_insert(failure.as_str());
    }
    ways
}

struct Other {
    failure: String,
    steps: Vec<String>,
}

struct Simplest<P> {
    plan: P,
    held: Outcome,
    steps: Vec<String>,
    rounds: usize,
    left: Vec<String>,
    withheld: Withheld,
    others: BTreeMap<String, Other>,
}

fn taken<P: Shrinks>(plan: &P, steps: &[&Step<P>]) -> (Vec<String>, P) {
    let (mut names, mut plan) = (Vec::new(), plan.clone());
    for step in steps {
        let mut open = offered(&plan).0.into_iter();
        if let Some(next) = open.find(|next| next.name == step.name) {
            names.push(next.name);
            plan = next.plan;
        }
    }
    (names, plan)
}

fn simplest<P: Shrinks>(search: &mut Search, way: &str, first: &Outcome) -> Simplest<P>
where
    Report<P, P::Ended>: Told,
{
    let told = format!("shrink {} {} [{way}]", first.kind.name(), first.seed);
    let known = ways(first);
    let mut found = Simplest {
        plan: P::of(search.seed),
        held: first.clone(),
        steps: Vec::new(),
        rounds: 0,
        left: Vec::new(),
        withheld: Withheld {
            calming: Vec::new(),
            outside: Vec::new(),
        },
        others: BTreeMap::new(),
    };
    loop {
        let (simpler, withheld) = offered(&found.plan);
        let plans: Vec<P> = simpler.iter().map(|step| step.plan.clone()).collect();
        let outcomes = search.run(&plans);
        found.rounds += 1;
        found.left = simpler.iter().map(|step| step.name.clone()).collect();
        found.withheld = withheld;
        let mut other = |outcome: &Outcome, steps: &[String], more: &[String]| {
            for (met, failure) in ways(outcome) {
                if !known.contains_key(&met) {
                    found.others.entry(met).or_insert_with(|| Other {
                        failure: failure.to_owned(),
                        steps: [steps, more].concat(),
                    });
                }
            }
        };
        let mut good = Vec::new();
        for (step, outcome) in simpler.iter().zip(&outcomes) {
            other(outcome, &found.steps, std::slice::from_ref(&step.name));
            if ways(outcome).contains_key(way) {
                good.push(step);
            }
        }
        eprintln!(
            "{told}: round {}: {} of {} simpler plans fail the same way, {} runs so far",
            found.rounds,
            good.len(),
            simpler.len(),
            search.runs
        );
        let halves = std::iter::successors(Some(good.len()), |take| Some(take / 2));
        let mut halves = halves.take_while(|take| *take > 0);
        let next = halves.find_map(|take| {
            let (names, plan) = taken(&found.plan, &good[..take]);
            let outcome = search.run(std::slice::from_ref(&plan)).remove(0);
            if ways(&outcome).contains_key(way) {
                return Some((names, plan, outcome));
            }
            other(&outcome, &found.steps, &names);
            None
        });
        let Some((names, plan, outcome)) = next.filter(|(names, ..)| !names.is_empty()) else {
            return found;
        };
        eprintln!("{told}: took {}", names.join(" "));
        found.steps.extend(names);
        found.plan = plan;
        found.held = outcome;
    }
}

pub struct Shrunk {
    pub way: String,
    pub kept: PathBuf,
    pub text: String,
    pub earlier: bool,
}

fn replay(kind: Kind, seed: u64, steps: &[String], plan: &str) -> String {
    let mut steps = steps.to_vec();
    steps.push(format!("{CHECK}{}", named(plan)));
    kind.replay(seed, &steps)
}

fn shrunk<P: Shrinks>(seed: u64, way: Option<&str>, folder: &Path) -> std::io::Result<Vec<Shrunk>>
where
    Report<P, P::Ended>: Told,
{
    let kind = <Report<P, P::Ended>>::KIND;
    let tree = Tree::read(workspace());
    let home = folder.join(tree.name());
    let kept = |way: &str| home.join(format!("shrunk-{}-{seed}-{way}.txt", kind.name()));
    let earlier = |way: &str| {
        let text = fs::read_to_string(kept(way)).ok()?;
        Some(Shrunk {
            way: way.to_owned(),
            kept: kept(way),
            text,
            earlier: true,
        })
    };
    if let Some(shrunk) = way.and_then(earlier) {
        return Ok(vec![shrunk]);
    }
    let mut search = Search {
        seed,
        tried: BTreeMap::new(),
        runs: 0,
    };
    let first = search.run(&[P::of(seed)]).remove(0);
    let fails = ways(&first);
    let wanted = fails.keys();
    let wanted = wanted.filter(|name| way.is_none_or(|way| way == name.as_str()));
    let labels = Labels::read(folder);
    let mut all = Vec::new();
    for way in wanted {
        if let Some(shrunk) = earlier(way) {
            all.push(shrunk);
            continue;
        }
        let (started, before) = (Instant::now(), search.runs);
        let found: Simplest<P> = simplest(&mut search, way, &first);
        let mut text = format!(
            "{} seed {seed} shrunk for [{way}] {} on tree {tree}\n  {}\n{} steps in {} rounds, {} runs and {:.0} s\n",
            kind.name(),
            labels.told(way),
            signature(fails[way]),
            found.steps.len(),
            found.rounds,
            1 + search.runs - before,
            started.elapsed().as_secs_f64(),
        );
        let _ = writeln!(text, "\nbefore\n  {}\n  plan {}", first.line, first.plan);
        let held = &found.held;
        let _ = writeln!(
            text,
            "after\n  {}\n  plan {}\n  digest {}",
            held.line, held.plan, held.digest
        );
        for failure in &held.failures {
            let _ = writeln!(text, "  [{}] {failure}", named(&signature(failure)));
        }
        let _ = writeln!(text, "steps");
        for step in &found.steps {
            let _ = writeln!(text, "  {step}");
        }
        let again = replay(kind, seed, &found.steps, &held.plan);
        let _ = writeln!(text, "replay\n  {again}");
        if !found.left.is_empty() {
            let _ = writeln!(
                text,
                "left, as one step on the plan fails otherwise or not at all\n  {}",
                found.left.join(" ")
            );
        }
        let withheld = [
            (
                &found.withheld.calming,
                "each would leave a calm plan with faults no calm plan has",
            ),
            (
                &found.withheld.outside,
                "each would break a rule every plan of a seed keeps",
            ),
        ];
        for (steps, why) in withheld {
            if !steps.is_empty() {
                let _ = writeln!(text, "not tried, as {why}\n  {}", steps.join(" "));
            }
        }
        if !found.others.is_empty() {
            let _ = writeln!(
                text,
                "simpler plans that failed another way: a second finding, or a plan no seed gives"
            );
        }
        for (other, met) in &found.others {
            let _ = writeln!(
                text,
                "  [{other}] {}\n      {}\n      {}\n      {}",
                labels.told(other),
                signature(&met.failure),
                met.failure,
                kind.replay(seed, &met.steps),
            );
        }
        fs::create_dir_all(&home)?;
        fs::write(home.join("tree.txt"), format!("{tree}\n"))?;
        fs::write(kept(way), &text)?;
        all.push(Shrunk {
            way: way.clone(),
            kept: kept(way),
            text,
            earlier: false,
        });
    }
    Ok(all)
}

pub fn shrink(
    kind: Kind,
    seed: u64,
    way: Option<&str>,
    folder: &Path,
) -> std::io::Result<Vec<Shrunk>> {
    match kind {
        Kind::Forward => shrunk::<Plan>(seed, way, folder),
        Kind::Reverse => shrunk::<reverse::Plan>(seed, way, folder),
        Kind::Deposits => shrunk::<deposits::Plan>(seed, way, folder),
        Kind::Zcash => shrunk::<lightwallet::Plan>(seed, way, folder),
        Kind::Guards => shrunk::<guards::Plan>(seed, way, folder),
        Kind::Cli => shrunk::<cli::Plan>(seed, way, folder),
    }
}
