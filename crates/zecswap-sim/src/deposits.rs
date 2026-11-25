mod payout;
mod relayer;
mod wallet;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256, address};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep, sleep_until};
use zecswap_api::relayer::deposit_fee;
use zecswap_chain::evm::deposits::{Call, Used};
use zecswap_chain::evm::{Settlement, simulate_wall};
use zecswap_client::FeeCap;
use zecswap_core::{NetworkType, derive_deposit_keys};
use zecswap_railgun::Keys as RailgunKeys;

use crate::net::{Line, Net};
use crate::node::{Node, SLOT, Weather};
use crate::relayer::{FEE as SWAP_FEE, Funder};
use crate::run::{Report, connect, lives, notes, paused, ticks};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, Chain, EPOCH, RAILGUN, Skewed, key};
use payout::Paid;
use relayer::{Desk, Door, LANDING, Relayer};
pub use relayer::{Offer, Service};
use wallet::{PAYOUT, RESCAN, Request, Wallet};

const HORIZON: u64 = 60 * 60;
const SETTLED: u64 = 25 * 60;
const RETURNS_UNTIL: u64 = 25 * 60;
const MOST_RETURNS: u32 = 2;
const REACH: u32 = 80;
const GAP: u32 = 20;
const DEPTH: u64 = 3;
const TOLERANCE: u64 = 150;
const TREASURY: Address = address!("0000000000000000000000000000000000007ea5");
const RICH: u128 = 1_000_000_000_000_000_000_000;
const FEE: u128 = 100_000;
const FEE_BPS: u32 = 10;
const MINIMUM: u128 = 2_000_000;
const DAY: u64 = 24 * 60 * 60;
const ROOMY: FeeCap = FeeCap {
    fee: 150_000,
    fee_bps: 15,
};

sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
    event DepositShielded(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
    event Rescued(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
    function vaultOf(address from) external view returns (address);
    function deposited(address from) external view returns (bool);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Shown,
    Reused(u64),
    Ahead(u32),
    Far(u32),
    Index(u32),
}

#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub at: u64,
    pub to: Where,
    pub amount: u128,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub weather: Weather,
    pub line: Line,
    pub service: Service,
    pub offer: Offer,
    pub reprice: Option<(u64, u128)>,
    pub cap: FeeCap,
    pub multicall: bool,
    pub arrivals: Vec<Arrival>,
    pub returned: u64,
    pub crashes: Vec<(u64, u64)>,
    pub skew: i64,
    pub clears: u64,
    pub payout: Option<Payout>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Payout {
    pub reverse: bool,
    pub returns: u32,
}

impl Plan {
    pub fn plain() -> Self {
        Self {
            weather: Weather::CALM,
            line: Line::CLEAR,
            service: Service::Honest,
            offer: Offer {
                fee: FEE,
                fee_bps: FEE_BPS,
                min_value: MINIMUM,
                max_validity: DAY,
            },
            reprice: None,
            cap: ROOMY,
            multicall: true,
            arrivals: vec![Arrival {
                at: 60,
                to: Where::Shown,
                amount: 25_000_000,
            }],
            returned: 0,
            crashes: Vec::new(),
            skew: 0,
            clears: 0,
            payout: None,
        }
    }

    pub fn from_seed(seed: Seed) -> Self {
        let roll = |what: &str, bound: u64| seed.below(&format!("deposit-{what}"), 0, bound);
        let calm = roll("sky", 100) < 20;
        let up_to = |what: &str, most: u64| if calm { 0 } else { roll(what, most + 1) };
        let pick =
            |what: &str, n: u64, bound: u64| seed.below(&format!("deposit-{what}"), n, bound);
        let arrival = |n: u64| Arrival {
            at: 20 + pick("arrives", n, 15 * 60 - 20),
            to: match pick("where", n, 100) {
                0..55 => Where::Shown,
                55..75 => Where::Reused(seed.roll("deposit-reused", n)),
                75..90 => Where::Ahead(1 + pick("ahead", n, 19) as u32),
                _ => Where::Far(pick("far", n, 20) as u32),
            },
            amount: u128::from(match pick("amount-kind", n, 100) {
                0..70 => 2_500_000 + pick("amount", n, 57_500_000),
                70..88 => 200_000 + pick("dust", n, 1_700_000),
                _ => 2_000_000 + pick("edge", n, 2),
            }),
        };
        let mut crashes: Vec<(u64, u64)> = (0..up_to("crashes", 3))
            .map(|n| {
                (
                    30 + pick("crash-at", n, 30 * 60),
                    5 + pick("crash-rest", n, 296),
                )
            })
            .collect();
        if up_to("away", 7) == 1 {
            let left = 3 * 60 + roll("left-at", 5 * 60);
            crashes.retain(|(at, _)| *at < left);
            crashes.push((left, 10 * 60 + roll("back-at", 15 * 60) - left));
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
                private_lost: 0,
                fee_swing: up_to("fee-swing", 600),
                rpc_error: up_to("rpc-error", 40),
                rpc_stall: up_to("rpc-stall", 4),
                stale_read: up_to("stale-read", 150),
            },
            line: Line {
                busy: up_to("busy", 150),
                hang: up_to("hang", 8),
                unheard: up_to("unheard", 40),
                unanswered: up_to("unanswered", 40),
            },
            service: match roll("service", 100) {
                0..40 => Service::Honest,
                40..46 => Service::Overcharges(roll("lie", 2)),
                46..52 => Service::AnotherDeployment(roll("lie", 3)),
                52..55 => Service::TakesNone,
                55..60 => Service::LowMinimum,
                60..66 => Service::StaleTerms(30 + roll("terms-lag", 211)),
                66..76 => Service::Sits(60 + roll("sits", 11 * 60)),
                76..81 => Service::NeverSends,
                81..88 => Service::LosesAnswers,
                88..94 => Service::Tampers(roll("lie", 3)),
                _ => Service::NamesAnother(roll("lie", 2)),
            },
            offer: Offer {
                fee: FEE,
                fee_bps: FEE_BPS,
                min_value: MINIMUM,
                max_validity: match roll("validity", 5) {
                    0 => 1_100 + roll("short-validity", 400),
                    _ => DAY,
                },
            },
            reprice: match roll("reprice", 8) {
                0 => Some((
                    300 + roll("reprice-at", 15 * 60),
                    FEE + u128::from(roll("reprice-by", 50_001)),
                )),
                1 => Some((300 + roll("reprice-at", 15 * 60), 2 * FEE)),
                _ => None,
            },
            cap: match roll("cap", 4) {
                0 => FeeCap {
                    fee: FEE,
                    fee_bps: FEE_BPS,
                },
                _ => ROOMY,
            },
            multicall: roll("multicall", 4) != 0,
            arrivals: (0..1 + roll("arrivals", 4)).map(arrival).collect(),
            returned: up_to("returned", 300),
            crashes,
            skew: if calm {
                0
            } else {
                roll("skew", 101) as i64 - 50
            },
            clears: 5 * 60 + roll("clears", 15 * 60),
            payout: (roll("payout", 4) == 0).then(|| Payout {
                reverse: roll("payout-way", 2) == 1,
                returns: 1 + roll("payout-returns", 2) as u32,
            }),
        };
        let focus = roll("focus", 16);
        if focus < 12 {
            if focus != 5 {
                plan.payout = None;
            }
            plan.crashes.clear();
            plan.reprice = None;
            plan.returned = 0;
            plan.offer.max_validity = DAY;
            plan.cap = ROOMY;
            plan.weather.lost = 0;
            plan.weather.rpc_stall = 0;
        }
        let first = 30 + roll("focus-first", 90);
        let whole = |n: u64| 2_500_000 + u128::from(pick("focus-amount", n, 40_000_000));
        let arrive = |after: u64, to: Where, amount: u128| Arrival {
            at: first + after,
            to,
            amount,
        };
        let later = 360 + roll("focus-later", 240);
        match focus {
            0 => {
                plan.service = Service::Sits(6 * 60 + roll("focus-sits", 8 * 60));
                plan.offer.max_validity = 1_100 + roll("focus-validity", 300);
                plan.arrivals = vec![arrive(0, Where::Shown, whole(0))];
            }
            1 => {
                plan.service = Service::StaleTerms(150 + roll("focus-terms-lag", 151));
                plan.reprice = Some((first, 110_000 + u128::from(roll("focus-reprice", 40_001))));
                plan.arrivals = vec![arrive(0, Where::Shown, whole(0))];
            }
            2 => {
                plan.service = Service::NamesAnother(0);
                plan.arrivals = vec![
                    arrive(0, Where::Index(0), whole(0)),
                    arrive(later, Where::Index(0), whole(1)),
                ];
            }
            3 => {
                plan.service = Service::NamesAnother(1);
                plan.arrivals = vec![
                    arrive(0, Where::Shown, whole(0)),
                    arrive(later, Where::Shown, whole(1)),
                ];
            }
            4 => {
                plan.service = Service::Honest;
                plan.arrivals = vec![
                    arrive(0, Where::Index(3), whole(0)),
                    arrive(60, Where::Index(47), whole(1)),
                    arrive(600 + roll("focus-gap", 300), Where::Index(22), whole(2)),
                ];
            }
            5 => {
                plan.service = match roll("focus-returns", 2) {
                    0 => Service::Honest,
                    _ => Service::LosesAnswers,
                };
                plan.returned = 1000;
                plan.arrivals = vec![
                    arrive(0, Where::Index(0), whole(0)),
                    arrive(later, Where::Index(0), whole(1)),
                ];
                if roll("focus-rescue-fee", 2) == 0 {
                    plan.reprice = Some((first + 150 + roll("focus-rescue-reprice", 100), 2 * FEE));
                }
            }
            6 => match roll("focus-minimum", 2) {
                0 => {
                    plan.service = Service::LowMinimum;
                    let crumbs = 50_000 + u128::from(roll("focus-crumbs", 50_000));
                    plan.arrivals = vec![
                        arrive(0, Where::Shown, crumbs),
                        arrive(later, Where::Shown, whole(1)),
                    ];
                }
                _ => {
                    plan.service = Service::Honest;
                    let dust = 1_000_000 + u128::from(roll("focus-dust", 900_000));
                    plan.arrivals = vec![
                        arrive(0, Where::Index(0), dust),
                        arrive(later, Where::Index(0), dust),
                    ];
                }
            },
            7 => {
                plan.service = match roll("focus-flight", 2) {
                    0 => Service::Honest,
                    _ => Service::Sits(60 + roll("focus-flight-sits", 120)),
                };
                plan.weather.lost = 300;
                plan.arrivals = vec![
                    arrive(0, Where::Index(0), whole(0)),
                    arrive(3 + roll("focus-flight-gap", 43), Where::Index(0), whole(1)),
                ];
            }
            8 => {
                plan.service = Service::Overcharges(roll("focus-lie-kind", 2));
                plan.arrivals = vec![arrive(0, Where::Shown, whole(0))];
            }
            9 => {
                plan.service = Service::AnotherDeployment(roll("focus-lie-kind", 3));
                plan.arrivals = vec![arrive(0, Where::Shown, whole(0))];
            }
            10 => {
                plan.service = Service::Tampers(roll("focus-lie-kind", 3));
                plan.arrivals = vec![arrive(0, Where::Shown, whole(0))];
            }
            11 => {
                plan.service = match roll("focus-silent", 2) {
                    0 => Service::NeverSends,
                    _ => Service::LosesAnswers,
                };
                plan.weather.lost = 300;
                plan.arrivals = vec![
                    arrive(0, Where::Shown, whole(0)),
                    arrive(later, Where::Shown, whole(1)),
                ];
            }
            _ => {}
        }
        plan
    }

    fn delivers(&self) -> bool {
        let fee = self.reprice.map_or(self.offer.fee, |(_, fee)| fee);
        self.service.delivers() && fee <= self.cap.fee && self.offer.fee_bps <= self.cap.fee_bps
    }

    fn settled(&self) -> bool {
        let sits = match self.service {
            Service::Sits(seconds) => seconds,
            Service::StaleTerms(lag) => self.reprice.map_or(0, |(at, _)| at + lag),
            _ => 0,
        };
        let back = self.crashes.iter().map(|(at, rest)| at + rest);
        let arrived = self
            .arrivals
            .iter()
            .map(|arrival| arrival.at + RESCAN.as_secs());
        let returns = self.returned > 0 || self.payout.is_some();
        let busy = [self.clears, sits, RETURNS_UNTIL * u64::from(returns)];
        let last = busy.into_iter().chain(back).chain(arrived).max();
        last.unwrap_or(0) + SETTLED <= HORIZON
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    pub arrived: u128,
    pub private: u128,
    pub waiting: u128,
    pub returned: u128,
    pub believed: usize,
    pub audited: bool,
}

pub(crate) struct Landing {
    pub block: u64,
    pub tx: B256,
    pub from: Address,
    pub fee: u128,
    pub relayer: Address,
    pub note: B256,
    pub rescue: bool,
}

pub(crate) fn landings(node: &Node, deposits: Address) -> Vec<Landing> {
    let logs = node.logs().into_iter();
    logs.filter(|logged| logged.log.address == deposits)
        .filter_map(|logged| {
            let (from, fee, relayer, note, rescue) = match logged.log.topics().first() {
                Some(&DepositShielded::SIGNATURE_HASH) => {
                    let event = DepositShielded::decode_log(&logged.log).ok()?;
                    (event.from, event.fee, event.relayer, event.note, false)
                }
                Some(&Rescued::SIGNATURE_HASH) => {
                    let event = Rescued::decode_log(&logged.log).ok()?;
                    (event.from, event.fee, event.relayer, event.note, true)
                }
                _ => return None,
            };
            Some(Landing {
                block: logged.block,
                tx: logged.tx,
                from,
                fee: fee.saturating_to(),
                relayer,
                note,
                rescue,
            })
        })
        .collect()
}

struct Moved {
    block: u64,
    tx: B256,
    from: Address,
    to: Address,
    value: u128,
}

fn transfers(node: &Node, token: Address) -> Vec<Moved> {
    let logs = node.logs().into_iter();
    logs.filter(|logged| logged.log.address == token)
        .filter_map(|logged| {
            let event = Transfer::decode_log(&logged.log).ok()?;
            Some(Moved {
                block: logged.block,
                tx: logged.tx,
                from: event.from,
                to: event.to,
                value: event.value.saturating_to(),
            })
        })
        .collect()
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
    deposits: Address,
    relayer: Arc<Relayer>,
    wallet: Arc<Wallet>,
    user_seed: [u8; 64],
    addresses: Vec<Address>,
    vaults: Vec<Address>,
    paid: Option<Paid>,
}

async fn simulate(seed: Seed, plan: Plan) -> Report<Plan, Ended> {
    simulate_wall(Some(EPOCH as i64));
    let start = Instant::now();
    let node = Node::new(seed, CHAIN_ID, EPOCH - SLOT, plan.weather);
    node.ease_fees();
    let chain = Chain::deploy(&node, plan.multicall);
    let deposits = chain.deploy_deposits();
    let [relayer_key, payer_key, screener_key] =
        ["deposit-relayer", "payer", "screener"].map(|who| key(seed, who));
    for who in [&relayer_key, &payer_key, &screener_key] {
        node.fund(who.address(), U256::from(RICH));
    }
    node.clear_from(EPOCH + plan.clears);
    let user_seed = seed.bytes::<64>("user-seed");
    let paid = plan.payout.map(|payout| {
        payout::settle(
            &chain,
            seed,
            &user_seed,
            relayer_key.address(),
            payout.reverse,
        )
    });
    let desk = Desk {
        service: plan.service,
        offer: plan.offer,
        reprice: plan.reprice,
        cap: plan.cap,
        deposits,
        start,
    };
    let relayer = Relayer::new(
        desk,
        chain.clone(),
        connect(&node, &chain, "relayer", relayer_key.clone(), false),
        Funder::new(&node, "forcer", relayer_key),
    );
    let addresses: Vec<Address> = (0..REACH)
        .map(|index| {
            let keys = derive_deposit_keys(&user_seed, NetworkType::Test, 0, index);
            keys.expect("deposit keys").key.address().into()
        })
        .collect();
    let vault = |from: &Address| {
        let out = node.view(deposits, vaultOfCall { from: *from }.abi_encode().into());
        vaultOfCall::abi_decode_returns(&out.expect("the contract answers vaultOf"))
            .expect("a vault's address")
    };
    let vaults: Vec<Address> = addresses.iter().map(vault).collect();
    let wallet = Arc::new(Wallet {
        user_seed,
        chain: chain.clone(),
        deposits,
        vaults: vaults.clone(),
        relayer: Net::to(
            seed,
            "relayer",
            plan.line,
            start + Duration::from_secs(plan.clears),
            Arc::new(Door(relayer.clone())),
            notes(&node),
        ),
        cap: plan.cap,
        paid: paid.clone(),
        max_fee: 2 * SWAP_FEE,
        http: reqwest::Client::new(),
        start,
        saved: Arc::default(),
        kept: Arc::default(),
        believed: Arc::default(),
        said: Arc::default(),
    });
    let world = Arc::new(World {
        seed,
        plan: plan.clone(),
        deposits,
        relayer,
        wallet: wallet.clone(),
        user_seed,
        vaults,
        addresses,
        chain: chain.clone(),
        paid,
    });
    let payer = connect(&node, &chain, "payer", payer_key, false);
    let screener = connect(&node, &chain, "screener", screener_key, false);
    let background = [
        spawn_local(ticks(node.clone(), start)),
        spawn_local(world.clone().pay(payer, start)),
        spawn_local(world.clone().screen(screener, start)),
    ];
    lives(&*notes(&node), start, HORIZON, &plan.crashes, || {
        spawn_local(Skewed::new(plan.skew, wallet.clone().live()))
    })
    .await;
    let scanned = world.scan().await;
    for task in background {
        task.abort();
    }
    world.report(scanned)
}

type Scan = (Vec<(u32, Used)>, u32);

impl World {
    async fn mint(&self, sender: &Settlement, to: Address, amount: u128) {
        for _ in 0..6 {
            match sender.mint_test_token(self.chain.token, to, amount).await {
                Err(e) if e.nothing_sent() => sleep(Duration::from_secs(20)).await,
                _ => return,
            }
        }
    }

    async fn pay(self: Arc<Self>, payer: Settlement, start: Instant) {
        let mut arrivals = self.plan.arrivals.clone();
        arrivals.sort_by_key(|arrival| arrival.at);
        for arrival in arrivals {
            sleep_until(start + Duration::from_secs(arrival.at)).await;
            let shown = self.wallet.saved().shown;
            let index = match arrival.to {
                Where::Shown => shown,
                Where::Reused(pick) => (pick % (u64::from(shown) + 1)) as u32,
                Where::Ahead(by) => shown + by,
                Where::Far(by) => (shown / GAP + 2) * GAP + by,
                Where::Index(index) => index,
            };
            let index = index.min(REACH - 1);
            let note = format!("{} arrive for address #{index}", arrival.amount);
            self.chain.node.note(note);
            let to = self.addresses[index as usize];
            self.mint(&payer, to, arrival.amount).await;
        }
    }

    async fn screen(self: Arc<Self>, screener: Settlement, start: Instant) {
        let mut judged = BTreeSet::new();
        let mut sent_back: BTreeMap<Address, u32> = BTreeMap::new();
        let swap = self.paid.as_ref().map(|paid| paid.vault);
        while start.elapsed().as_secs() < RETURNS_UNTIL {
            sleep(Duration::from_secs(20)).await;
            let settled = self.chain.node.head().0.saturating_sub(DEPTH);
            let mut twins: BTreeMap<(Address, B256), u64> = BTreeMap::new();
            for (from, npk, value) in self.chain.notes_at(settled) {
                let index = self.vaults.iter().position(|vault| *vault == from);
                let (whose, odds, most) = match index {
                    Some(index) => (format!("#{index}"), self.plan.returned, MOST_RETURNS),
                    None if swap == Some(from) => {
                        let most = self.plan.payout.map_or(0, |payout| payout.returns);
                        ("the swap".to_owned(), 1000, most)
                    }
                    None => continue,
                };
                let twin = twins.entry((from, npk)).or_default();
                *twin += 1;
                if !judged.insert((from, npk, *twin)) {
                    continue;
                }
                let word = u64::from_be_bytes(npk[..8].try_into().expect("eight bytes"));
                let roll = word.wrapping_add(*twin);
                let refused = self.seed.chance("railgun-refuses", roll, odds);
                let returns = sent_back.entry(from).or_default();
                if !refused || *returns >= most {
                    continue;
                }
                *returns += 1;
                let wait = self.seed.between("railgun-returns-after", roll, 20, 240);
                sleep(Duration::from_secs(wait)).await;
                let note = format!("Railgun sends {value} back to the vault of {whose}");
                self.chain.node.note(note);
                self.mint(&screener, from, value).await;
            }
        }
    }

    fn used(&self, index: u32) -> Used {
        let from = self.addresses[index as usize];
        let out = self
            .chain
            .node
            .view(self.deposits, depositedCall { from }.abi_encode().into());
        Used {
            balance: self.chain.token_balance(from),
            deposited: depositedCall::abi_decode_returns(
                &out.expect("the contract answers deposited"),
            )
            .expect("a flag"),
        }
    }

    fn truth(&self) -> Vec<(u32, Used)> {
        let mut used = Vec::new();
        for window in 0..REACH / GAP {
            let found: Vec<(u32, Used)> = (window * GAP..(window + 1) * GAP)
                .map(|index| (index, self.used(index)))
                .filter(|(_, used)| used.balance > 0 || used.deposited)
                .collect();
            if found.is_empty() {
                break;
            }
            used.extend(found);
        }
        used
    }

    async fn scan(&self) -> Option<Scan> {
        let depositor = self.wallet.depositor("auditor");
        let before = self.truth();
        let found = depositor.recover().await.ok()?;
        let shown = depositor.showing().await.ok()?;
        (before == self.truth()).then_some((found, shown))
    }

    fn report(&self, scanned: Option<Scan>) -> Report<Plan, Ended> {
        let node = &self.chain.node;
        let (kept, believed) = (self.wallet.kept(), self.wallet.believed());
        let (heard, named) = (self.relayer.heard(), self.relayer.named());
        let (asks, cap) = (self.relayer.asks(), self.plan.cap);
        let landings = landings(node, self.deposits);
        let transfers = transfers(node, self.chain.token);
        let published = node.published();
        let head = node.head().0;
        let place = |all: &[Address], one: Address| {
            let found = all.iter().position(|each| *each == one);
            found.map(|index| index as u32)
        };
        let mut failures = Vec::new();
        let mut failed = BTreeSet::new();
        let mut fail = |check: u8, what: String| {
            if failed.insert(check) {
                failures.push(what);
            }
        };

        for moved in &transfers {
            let Some(index) = place(&self.addresses, moved.from) else {
                continue;
            };
            let sent = published.iter().find(|sent| sent.hash == moved.tx);
            let signed = match sent.and_then(|sent| Call::decode(&sent.input)) {
                Some(Call::Shield {
                    from,
                    value,
                    fee,
                    valid_before,
                }) => {
                    let signed = |kept: &wallet::Kept| match &kept.request {
                        Request::Shield(shield) => {
                            kept.index == index
                                && (shield.value, shield.fee, shield.valid_before)
                                    == (value, fee, valid_before)
                        }
                        Request::Rescue(_) | Request::Payout(_) => false,
                    };
                    moved.to == self.deposits
                        && (from, value) == (moved.from, moved.value)
                        && kept.iter().any(signed)
                }
                _ => false,
            };
            if !signed {
                fail(
                    1,
                    format!(
                        "block {}: {} left address #{index} for {} with no signature of the wallet's for it",
                        moved.block, moved.value, moved.to
                    ),
                );
            }
        }
        for landing in &landings {
            let Some(index) = place(&self.addresses, landing.from) else {
                continue;
            };
            let signed = kept.iter().any(|kept| {
                kept.index == index
                    && kept.request.note() == landing.note
                    && kept.request.fee() == landing.fee
                    && matches!(kept.request, Request::Rescue(_)) == landing.rescue
            });
            if !signed {
                fail(
                    2,
                    format!(
                        "block {}: a deposit of address #{index} landed paying {} to {}, to a note and fee the wallet never signed together",
                        landing.block, landing.fee, landing.relayer
                    ),
                );
            }
        }
        for (nth, landing) in landings.iter().enumerate() {
            let twice = |other: &Landing| (other.from, other.note) == (landing.from, landing.note);
            if landings[..nth].iter().any(twice) {
                fail(
                    22,
                    format!(
                        "block {}: a request of {} landed a second time",
                        landing.block, landing.from
                    ),
                );
            }
        }
        for moved in &transfers {
            let Some(index) = place(&self.vaults, moved.from) else {
                continue;
            };
            let owner = self.addresses[index as usize];
            let fee = landings.iter().any(|landing| {
                (landing.tx, landing.from, landing.relayer, landing.fee)
                    == (moved.tx, owner, moved.to, moved.value)
            });
            if !fee && moved.to != RAILGUN && moved.to != TREASURY {
                fail(
                    3,
                    format!(
                        "block {}: {} left the vault of #{index} for {}, neither Railgun nor a fee the wallet signed",
                        moved.block, moved.value, moved.to
                    ),
                );
            }
        }
        let railgun = RailgunKeys::from_seed(&self.user_seed, 0);
        let shielded = self.chain.notes_at(head);
        for (from, npk, value) in &shielded {
            let Some(index) = place(&self.vaults, *from) else {
                continue;
            };
            let ours = kept.iter().any(|kept| {
                let note = kept.request.shield_note();
                kept.index == index && B256::from(note.npk) == *npk && railgun.open(&note).is_some()
            });
            if !ours {
                fail(
                    4,
                    format!(
                        "{value} was shielded from the vault of #{index} to a note that is not the wallet's"
                    ),
                );
            }
        }

        let deposits = kept.iter().any(|kept| kept.index != PAYOUT);
        if self.plan.service.lies() && deposits {
            fail(
                5,
                format!(
                    "the wallet signed a deposit for a relayer that {:?}",
                    self.plan.service
                ),
            );
        }
        for kept in &kept {
            let (index, until) = (kept.index, kept.request.until());
            match &kept.request {
                Request::Shield(shield) => {
                    let most = deposit_fee(cap.fee, cap.fee_bps, shield.value);
                    if shield.fee > most || shield.fee >= shield.value {
                        fail(
                            6,
                            format!(
                                "the wallet signed a fee of {} on a deposit of {} from #{index}, over its cap of {most} or leaving nothing",
                                shield.fee, shield.value
                            ),
                        );
                    }
                    if shield.value < asks.min_value {
                        fail(
                            7,
                            format!(
                                "the wallet signed a deposit of {} from #{index}, under the relayer's minimum of {}",
                                shield.value, asks.min_value
                            ),
                        );
                    }
                }
                Request::Rescue(rescue) => {
                    if rescue.fee > cap.fee {
                        fail(
                            6,
                            format!(
                                "the wallet signed a rescue fee of {} for #{index}, over its cap of {}",
                                rescue.fee, cap.fee
                            ),
                        );
                    }
                    if kept.held <= rescue.fee {
                        fail(
                            8,
                            format!(
                                "block {}: the wallet signed a rescue for #{index}, whose vault held no more than the fee of {}",
                                kept.block, rescue.fee
                            ),
                        );
                    }
                }
                Request::Payout(_) => continue,
            }
            if until > kept.wall + self.plan.offer.max_validity {
                fail(
                    9,
                    format!(
                        "the wallet signed for #{index} until {until}, {}s from its clock, past the {}s the relayer takes",
                        until - kept.wall,
                        self.plan.offer.max_validity
                    ),
                );
            }
        }
        for heard in &heard {
            let index = place(&self.addresses, heard.from);
            let what = match heard.rescue {
                true => "rescue",
                false => "deposit",
            };
            let saved = kept
                .iter()
                .any(|kept| kept.key == heard.key && kept.at <= heard.at);
            if !saved {
                fail(
                    10,
                    format!(
                        "the relayer heard a {what} for #{index:?} at {} that the wallet had not saved first",
                        heard.time
                    ),
                );
            }
            if heard.until + TOLERANCE < heard.time + LANDING {
                fail(
                    11,
                    format!(
                        "the relayer heard a {what} for #{index:?} at {} with {}s left to land",
                        heard.time,
                        heard.until.saturating_sub(heard.time)
                    ),
                );
            }
        }
        for believed in &believed {
            let (index, tx) = (believed.index, believed.tx);
            if !believed.evidence {
                fail(
                    12,
                    format!(
                        "the wallet believed {tx} a deposit of #{index}, which the contract's events in it do not show"
                    ),
                );
            }
            if believed.asked != Some(believed.note) && !named.contains(&tx) {
                fail(
                    13,
                    format!(
                        "the wallet believed {tx} was its request for #{index}, though it shields another note and the relayer named no conflict"
                    ),
                );
            }
        }

        let vault_holds = |index: u32| self.chain.token_balance(self.vaults[index as usize]);
        if let Some((found, shown)) = &scanned {
            let truth = self.truth();
            if *found != truth {
                fail(
                    14,
                    format!("a scan from the seed found {found:?}, and the chain holds {truth:?}"),
                );
            }
            let unused = (0..REACH).find(|index| !self.used(*index).deposited);
            if Some(*shown) != unused {
                fail(
                    15,
                    format!(
                        "the wallet shows address #{shown}, and the first never deposited from is {unused:?}"
                    ),
                );
            }
            if self.plan.delivers() && self.plan.settled() {
                for (index, used) in &truth {
                    let worth = used.balance >= asks.min_value
                        && deposit_fee(asks.fee, asks.fee_bps, used.balance) < used.balance;
                    if worth {
                        fail(16, format!("{} is stuck at address #{index}", used.balance));
                    }
                    if used.deposited && vault_holds(*index) > asks.fee {
                        fail(
                            16,
                            format!("{} is stuck in the vault of #{index}", vault_holds(*index)),
                        );
                    }
                }
            }
        }

        if let Some(paid) = &self.paid {
            let contract = self.chain.contract;
            let rescued = payout::rescues(node, contract, paid.id);
            for (tx, relayer, fee) in &rescued {
                let signed = kept.iter().any(|kept| {
                    matches!(kept.request, Request::Payout(_)) && kept.request.fee() == *fee
                });
                if !signed {
                    fail(
                        17,
                        format!(
                            "{tx} rescued the swap's payout paying {fee} to {relayer}, a fee the wallet never signed"
                        ),
                    );
                }
            }
            let next = payout::nonce(node, contract, paid.id);
            if next != rescued.len() as u64 {
                fail(
                    18,
                    format!(
                        "the swap's next rescue nonce is {next} after {} rescues",
                        rescued.len()
                    ),
                );
            }
            for moved in transfers.iter().filter(|moved| moved.from == paid.vault) {
                let fee = rescued.contains(&(moved.tx, moved.to, moved.value));
                if !fee && moved.to != RAILGUN && moved.to != TREASURY {
                    fail(
                        19,
                        format!(
                            "block {}: {} left the swap's vault for {}, neither Railgun nor a fee the wallet signed",
                            moved.block, moved.value, moved.to
                        ),
                    );
                }
            }
            let strange = shielded
                .iter()
                .any(|(from, npk, _)| *from == paid.vault && *npk != B256::from(paid.note.npk));
            if strange {
                fail(
                    20,
                    "the swap's vault shielded to a note that is not the wallet's".into(),
                );
            }
            let waits = self.chain.token_balance(paid.vault);
            if self.plan.delivers() && self.plan.settled() && waits > SWAP_FEE {
                fail(21, format!("{waits} is stuck in the swap's vault"));
            }
        }

        let minted = |to: &[Address]| -> u128 {
            let mints = transfers
                .iter()
                .filter(|moved| moved.from == Address::ZERO && to.contains(&moved.to));
            mints.map(|moved| moved.value).sum()
        };
        let held = |by: &[Address]| -> u128 {
            by.iter().map(|each| self.chain.token_balance(*each)).sum()
        };
        let ended = Ended {
            arrived: minted(&self.addresses),
            private: shielded
                .iter()
                .filter(|(from, ..)| self.vaults.contains(from))
                .map(|(.., value)| value)
                .sum(),
            waiting: held(&self.addresses) + held(&self.vaults),
            returned: minted(&self.vaults),
            believed: believed.len(),
            audited: scanned.is_some(),
        };
        Report {
            seed: self.seed.0,
            plan: self.plan.clone(),
            ended: Some(ended),
            stage: None,
            failures,
            trace: node.trace(),
        }
    }
}
