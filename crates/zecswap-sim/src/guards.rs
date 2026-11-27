mod railgun;
mod relayer;
mod wallet;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::eips::Encodable2718;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes, U256, address};
use alloy::providers::MULTICALL3_ADDRESS;
use alloy::providers::bindings::IMulticall3;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use serde_json::json;
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep, sleep_until};
use zecswap_chain::evm::funding::FundingPolicy;
use zecswap_chain::evm::railgun::{IRailgunSmartWallet, IRelayAdapt, SendPolicy, Transaction};
use zecswap_chain::evm::{IErc20, IZecSwap, simulate_wall};
use zecswap_railgun::{Keys as RailgunKeys, OutputCiphertext};

use crate::node::{Landed, Node, SLOT, Weather};
use crate::panics;
use crate::run::{Report, lives, notes, paused, ticks};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, EPOCH, LATE_WINDOW, MIN_WINDOW, artifact, key};
use railgun::{Circuit, Coin, Draft, Out, Reads, root};
use relayer::{Desk, MAX_GAS, RATE, Relayer, Reply};
use wallet::{AMOUNT, COINS, FLOOR, Ground, coins};

const HORIZON: u64 = 90 * 60;
const LAST_POST: u64 = 80 * 60;
const SETTLE: u64 = 20 * 60;
const GWEI: u128 = 1_000_000_000;
const ETHER: u128 = 1_000_000_000_000_000_000;
const TOP_UP: u128 = ETHER;
const HELD: u128 = 7_000_000;
const DEPLOYER: Address = address!("d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0");
const FUNDER: Address = address!("f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0");

sol! {
    struct CommitmentCiphertext {
        bytes32[4] ciphertext;
        bytes32 blindedSenderViewingKey;
        bytes32 blindedReceiverViewingKey;
        bytes annotationData;
        bytes memo;
    }

    event Transact(uint256 treeNumber, uint256 startPosition, bytes32[] hash, CommitmentCiphertext[] ciphertext);
    function mint(address to, uint256 amount) external;
    function balanceOf(address owner) external view returns (uint256);
    function nullifiers(uint256 treeNumber, bytes32 nullifier) external view returns (bool);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relayed {
    Private,
    Withdrawal,
    Batch,
    Twin,
    Rival,
    OtherChain,
    OtherProxy,
    WithValue,
    Oversized,
    TooManyTransactions,
    TooManyNullifiers,
    TooManyCommitments,
    LongAnnotation,
    LongMemo,
    ExtraCiphertext,
    Selector,
    Offset,
    Padding,
    Trailing,
    Truncated,
    DirtyWord,
    Empty,
    NoNullifiers,
    NoCommitments,
    BoundChain,
    BoundAdapt,
    Redirect,
    OtherUnshield,
    MinGasPrice,
    FeeShort,
    FeeToken,
    FeeReceiver,
    FeeGarbled,
    FeeClaims,
    FeeSecond,
    FeeUnderGas,
    Spent,
    Doubled,
    Forged,
    UnknownRoot,
}

impl Relayed {
    pub const HONEST: [Self; 5] = [
        Self::Private,
        Self::Withdrawal,
        Self::Batch,
        Self::Twin,
        Self::Rival,
    ];
    pub const LIES: [Self; 35] = [
        Self::OtherChain,
        Self::OtherProxy,
        Self::WithValue,
        Self::Oversized,
        Self::TooManyTransactions,
        Self::TooManyNullifiers,
        Self::TooManyCommitments,
        Self::LongAnnotation,
        Self::LongMemo,
        Self::ExtraCiphertext,
        Self::Selector,
        Self::Offset,
        Self::Padding,
        Self::Trailing,
        Self::Truncated,
        Self::DirtyWord,
        Self::Empty,
        Self::NoNullifiers,
        Self::NoCommitments,
        Self::BoundChain,
        Self::BoundAdapt,
        Self::Redirect,
        Self::OtherUnshield,
        Self::MinGasPrice,
        Self::FeeShort,
        Self::FeeToken,
        Self::FeeReceiver,
        Self::FeeGarbled,
        Self::FeeClaims,
        Self::FeeSecond,
        Self::FeeUnderGas,
        Self::Spent,
        Self::Doubled,
        Self::Forged,
        Self::UnknownRoot,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Funded {
    Honest,
    Again,
    Short,
    Late,
    Rival,
    OtherAdapter,
    WithValue,
    Selector,
    DirtyBool,
    ExtraCall,
    MissingCall,
    NoSuccess,
    MinGas,
    CallValue,
    Reordered,
    OtherContract,
    OtherToken,
    InnerTrailing,
    InnerSelector,
    FeeShort,
    FeeElsewhere,
    OtherMaker,
    TermsToken,
    NoUser,
    NoAmount,
    NoNote,
    Deadlines,
    Spender,
    ApproveAmount,
    TwoShields,
    Dust,
    DustToken,
    DustNpk,
    MakerShare,
    UserShare,
    SwapId,
    ShortSignature,
    Signer,
    NoTransactions,
    NoNullifiers,
    BoundChain,
    BoundAdapter,
    BoundParams,
    MinGasPrice,
    Redirect,
    UnshieldNpk,
    UnshieldToken,
    NoUnshield,
}

impl Funded {
    pub const HONEST: [Self; 2] = [Self::Honest, Self::Again];
    pub const LIES: [Self; 46] = [
        Self::Short,
        Self::Late,
        Self::Rival,
        Self::OtherAdapter,
        Self::WithValue,
        Self::Selector,
        Self::DirtyBool,
        Self::ExtraCall,
        Self::MissingCall,
        Self::NoSuccess,
        Self::MinGas,
        Self::CallValue,
        Self::Reordered,
        Self::OtherContract,
        Self::OtherToken,
        Self::InnerTrailing,
        Self::InnerSelector,
        Self::FeeShort,
        Self::FeeElsewhere,
        Self::OtherMaker,
        Self::TermsToken,
        Self::NoUser,
        Self::NoAmount,
        Self::NoNote,
        Self::Deadlines,
        Self::Spender,
        Self::ApproveAmount,
        Self::TwoShields,
        Self::Dust,
        Self::DustToken,
        Self::DustNpk,
        Self::MakerShare,
        Self::UserShare,
        Self::SwapId,
        Self::ShortSignature,
        Self::Signer,
        Self::NoTransactions,
        Self::NoNullifiers,
        Self::BoundChain,
        Self::BoundAdapter,
        Self::BoundParams,
        Self::MinGasPrice,
        Self::Redirect,
        Self::UnshieldNpk,
        Self::UnshieldToken,
        Self::NoUnshield,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    Relayed(Relayed),
    Funded(Funded),
}

impl Ask {
    pub fn honest(self) -> bool {
        match self {
            Ask::Relayed(relayed) => Relayed::HONEST.contains(&relayed),
            Ask::Funded(funded) => Funded::HONEST.contains(&funded),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub at: u64,
    pub ask: Ask,
    pub twice: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Config {
    Sound,
    Unset,
    MakerIsRelayer,
    FeeElsewhere,
    NoGas,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adapter {
    Checked,
    OtherRailgun,
    NoCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stance {
    pub cap: u128,
    pub eth: u128,
    pub top_up: u64,
    pub priced: bool,
    pub config: Config,
    pub adapter: Adapter,
}

impl Stance {
    pub const SOUND: Self = Self {
        cap: 100 * GWEI,
        eth: 10 * ETHER,
        top_up: 0,
        priced: true,
        config: Config::Sound,
        adapter: Adapter::Checked,
    };

    pub fn sponsors(&self) -> bool {
        self.config == Config::Sound && self.adapter == Adapter::Checked
    }

    fn sound(&self) -> bool {
        self.cap == Self::SOUND.cap && self.eth == Self::SOUND.eth && self.sponsors()
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub weather: Weather,
    pub stance: Stance,
    pub requests: Vec<Request>,
    pub crashes: Vec<(u64, u64)>,
    pub multicall: bool,
    pub clears: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Must {
    Serve,
    Refuse,
    Fail,
}

#[derive(Clone, Debug)]
pub struct Post {
    pub chain_id: u64,
    pub to: Address,
    pub value: u128,
    pub data: Bytes,
    pub swap: Option<B256>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ended {
    pub served: usize,
    pub refused: usize,
    pub landed: usize,
    pub reverted: usize,
}

const RETRIED: &[&str] = &[
    "the network's gas price is above the relayer's cap",
    "the relayer is low on ETH",
    "network gas price exceeds sponsorship limit",
    "funding simulation failed",
    "cannot read funding adapter's Railgun deployment",
];

impl Plan {
    pub fn plain() -> Self {
        Self {
            weather: Weather::CALM,
            stance: Stance::SOUND,
            requests: vec![
                Request {
                    at: 60,
                    ask: Ask::Relayed(Relayed::Private),
                    twice: false,
                },
                Request {
                    at: 120,
                    ask: Ask::Funded(Funded::Honest),
                    twice: false,
                },
            ],
            crashes: Vec::new(),
            multicall: true,
            clears: 0,
        }
    }

    pub fn from_seed(seed: Seed) -> Self {
        let roll = |what: &str, bound: u64| seed.below(what, 0, bound);
        let calm = roll("guard-sky", 100) < 20;
        let up_to = |what: &str, most: u64| if calm { 0 } else { roll(what, most + 1) };
        let low = !calm && roll("guard-eth", 10) < 2;
        let stance = Stance {
            cap: match !calm && roll("guard-cap", 10) < 3 {
                true => u128::from(seed.between("guard-cap-gwei", 0, 8, 30)) * GWEI,
                false => Stance::SOUND.cap,
            },
            eth: match low {
                true => u128::from(seed.between("guard-eth-low", 0, 0, 3_000)) * 1_000_000_000_000,
                false => Stance::SOUND.eth,
            },
            top_up: match low {
                true => seed.between("guard-top-up", 0, 5 * 60, 40 * 60),
                false => 0,
            },
            priced: roll("guard-priced", 4) != 0,
            config: match up_to("guard-config", 19) {
                1 => Config::Unset,
                2 => Config::MakerIsRelayer,
                3 => Config::FeeElsewhere,
                4 => Config::NoGas,
                _ => Config::Sound,
            },
            adapter: match up_to("guard-adapter", 19) {
                1 => Adapter::OtherRailgun,
                2 => Adapter::NoCode,
                _ => Adapter::Checked,
            },
        };
        let count = seed.between("guard-requests", 0, 4, 9);
        let requests = (0..count)
            .map(|n| {
                let honest = calm || seed.chance("guard-honest", n, 350);
                let funded = seed.chance("guard-funded", n, 450);
                let pick =
                    |list: &[Ask]| list[seed.below("guard-ask", n, list.len() as u64) as usize];
                let ask = match (honest, funded) {
                    (true, false) => pick(&Relayed::HONEST.map(Ask::Relayed)),
                    (true, true) => pick(&Funded::HONEST.map(Ask::Funded)),
                    (false, false) => pick(&Relayed::LIES.map(Ask::Relayed)),
                    (false, true) => pick(&Funded::LIES.map(Ask::Funded)),
                };
                Request {
                    at: seed.between("guard-at", n, 60, 40 * 60),
                    ask,
                    twice: !calm && seed.chance("guard-twice", n, 120),
                }
            })
            .collect();
        let crashes = (0..up_to("guard-crashes", 2))
            .map(|n| {
                (
                    seed.between("guard-crash-at", n, 60, 45 * 60),
                    seed.between("guard-crash-rest", n, 5, 300),
                )
            })
            .collect();
        let mut plan = Self {
            weather: Weather {
                reorg: up_to("guard-reorg", 25),
                reorg_after_tx: up_to("guard-reorg-after-tx", 450),
                ambush: up_to("guard-ambush", 300),
                reorg_depth: 2,
                fork_delay: up_to("guard-fork-delay", 900),
                missed_slot: up_to("guard-missed-slot", 60),
                delayed: up_to("guard-delayed", 200),
                delay_blocks: 3,
                lost: up_to("guard-lost", 60),
                private_lost: 0,
                fee_swing: up_to("guard-fee-swing", 600),
                rpc_error: up_to("guard-rpc-error", 40),
                rpc_stall: up_to("guard-rpc-stall", 4),
                stale_read: up_to("guard-stale-read", 150),
            },
            stance,
            requests,
            crashes,
            multicall: roll("guard-multicall", 2) == 0,
            clears: seed.between("guard-clears", 0, 10 * 60, 45 * 60),
        };
        if calm {
            plan.weather = Weather::CALM;
        }
        match up_to("guard-focus", 9) {
            1 => {
                plan.stance.cap = 10 * GWEI;
                plan.weather.fee_swing = 1000;
            }
            2 => {
                plan.weather.rpc_error = 200;
                plan.weather.lost = 300;
            }
            3 => {
                plan.weather.reorg_after_tx = 1000;
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
            && self.weather.reorg_after_tx == 0
            && self.weather.ambush == 0
            && self.weather.lost == 0
            && self.weather.fee_swing == 0
            && self.weather.rpc_stall == 0
            && self.crashes.is_empty()
            && self.stance.sound()
    }

    fn settled(&self) -> u64 {
        let back = self.crashes.iter().map(|(at, rest)| at + rest).max();
        let posted = self.requests.iter().map(|request| request.at).max();
        let quiet = [
            self.clears,
            self.stance.top_up,
            back.unwrap_or(0),
            posted.unwrap_or(0),
        ];
        quiet.into_iter().max().unwrap_or(0) + SETTLE
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
    node: Node,
    ground: Ground,
    relayer: Arc<Relayer>,
    posts: Vec<Vec<(Post, Must)>>,
    replies: Mutex<BTreeMap<(usize, usize), Vec<Reply>>>,
    funder: Address,
}

fn create(node: &Node, code: Vec<u8>) -> Address {
    node.transact(DEPLOYER, None, code.into())
        .1
        .expect("a deployed contract")
}

fn deployed(node: &Node, name: &str, args: &[u8]) -> Address {
    let mut code = artifact(name);
    code.extend_from_slice(args);
    create(node, code)
}

fn minted(node: &Node, token: Address, to: Address, amount: u128) {
    let call = mintCall {
        to,
        amount: U256::from(amount),
    };
    node.transact(DEPLOYER, Some(token), call.abi_encode().into());
}

fn setup(seed: Seed, plan: &Plan, node: &Node) -> (Ground, Address) {
    let circuit = Circuit::new(seed);
    let prover = circuit.verifier().abi_encode();
    let token = deployed(node, "TestToken", &[]);
    let other_token = deployed(node, "TestToken", &[]);
    let railgun = deployed(node, "SimRailgun", &prover);
    let stray = deployed(node, "SimRailgun", &prover);
    let windows =
        |railgun: Address| (U256::from(MIN_WINDOW), U256::from(LATE_WINDOW), railgun).abi_encode();
    let contract = deployed(node, "ZecSwap", &windows(railgun));
    let other_contract = deployed(node, "ZecSwap", &windows(railgun));
    let adapt = deployed(node, "SimRelayAdapt", &railgun.abi_encode());
    let other_adapt = deployed(node, "SimRelayAdapt", &railgun.abi_encode());
    let stray_adapt = deployed(node, "SimRelayAdapt", &stray.abi_encode());
    if plan.multicall {
        node.place(
            create(node, IMulticall3::BYTECODE.to_vec()),
            MULTICALL3_ADDRESS,
        );
    }
    let relayer = key(seed, "guard-relayer").address();
    let relayer_keys = RailgunKeys::from_seed(&seed.bytes::<64>("guard-relayer-railgun"), 0);
    let user_seed = seed.bytes::<64>("guard-user-seed");
    let own = RailgunKeys::from_seed(&user_seed, 0).receiver();
    let other = RailgunKeys::from_seed(&seed.bytes::<64>("guard-other-seed"), 0).receiver();
    let count = plan.requests.len();
    let mut ground = Ground {
        seed,
        contract,
        other_contract,
        token,
        other_token,
        railgun,
        adapt,
        other_adapt,
        relayer,
        relayer_receiver: relayer_keys.receiver(),
        maker: key(seed, "guard-maker").address(),
        user_seed,
        own,
        other,
        cap: plan.stance.cap,
        honest_fee: FLOOR.max(u128::from(MAX_GAS) * plan.stance.cap * RATE / ETHER + 1),
        circuit,
        coins: (0..count)
            .map(|n| coins(seed, &n.to_string(), COINS, token))
            .collect(),
        odd: (0..count)
            .flat_map(|n| coins(seed, &format!("odd-{n}"), 1, other_token))
            .collect(),
        spent: (0..count)
            .flat_map(|n| coins(seed, &format!("spent-{n}"), 1, token))
            .collect(),
    };
    let mut all: Vec<Coin> = ground.coins.iter().flatten().copied().collect();
    all.extend(&ground.odd);
    all.extend(&ground.spent);
    for coin in &all {
        ground.circuit.mint(*coin);
    }
    let held = |token: Address| {
        all.iter()
            .filter(|coin| coin.token == token)
            .map(|coin| coin.value)
            .sum()
    };
    minted(node, token, railgun, held(token));
    minted(node, other_token, railgun, held(other_token));
    for (at, chunk) in ground.spent.chunks(13).enumerate() {
        let value: u128 = chunk.iter().map(|coin| coin.value).sum();
        let draft = Draft {
            spends: chunk.to_vec(),
            outs: vec![Out {
                to: other,
                token,
                value,
                reads: Reads::Clean,
            }],
            exit: None,
            chain_id: CHAIN_ID,
            adapt: Address::ZERO,
            min_gas_price: 0,
            root: root(),
            annotation: 0,
            memo: 0,
        };
        let mut spending = ground
            .circuit
            .build(seed, &format!("guard-spent-{at}"), &draft);
        ground.circuit.prove(&mut spending, &draft);
        let call = IRailgunSmartWallet::transactCall {
            _transactions: vec![spending],
        };
        node.transact(DEPLOYER, Some(railgun), call.abi_encode().into());
    }
    for (number, request) in plan.requests.iter().enumerate() {
        if request.ask == Ask::Funded(Funded::Rival) {
            minted(node, token, FUNDER, AMOUNT);
            for (to, data) in ground.rival(number, request.at) {
                node.transact(FUNDER, Some(to), data.into());
            }
        }
    }
    node.fund(relayer, U256::from(plan.stance.eth));
    minted(node, token, relayer, HELD);
    (ground, stray_adapt)
}

fn policies(ground: &Ground, plan: &Plan, stray_adapt: Address) -> (SendPolicy, FundingPolicy) {
    let send = SendPolicy {
        railgun: ground.railgun,
        token: ground.token,
        fee: FLOOR,
        max_gas_limit: MAX_GAS,
        max_gas_price_wei: plan.stance.cap,
    };
    let mut funding = FundingPolicy {
        relay_adapt: match plan.stance.adapter {
            Adapter::Checked => ground.adapt,
            Adapter::OtherRailgun => stray_adapt,
            Adapter::NoCode => address!("00000000000000000000000000000000000ad0a7"),
        },
        token: ground.token,
        maker: ground.maker,
        max_gas_limit: MAX_GAS,
        max_gas_price_wei: plan.stance.cap,
        fee: wallet::FUND_FEE,
        fee_recipient: ground.relayer,
    };
    match plan.stance.config {
        Config::Sound => {}
        Config::Unset => funding.token = Address::ZERO,
        Config::MakerIsRelayer => funding.maker = ground.relayer,
        Config::FeeElsewhere => funding.fee_recipient = ground.maker,
        Config::NoGas => funding.max_gas_limit = 0,
    }
    (send, funding)
}

async fn simulate(seed: Seed, plan: Plan) -> Report<Plan, Ended> {
    simulate_wall(Some(EPOCH as i64));
    let start = Instant::now();
    let node = Node::new(seed, CHAIN_ID, EPOCH - SLOT, plan.weather);
    let (ground, stray_adapt) = setup(seed, &plan, &node);
    let (send_policy, funding_policy) = policies(&ground, &plan, stray_adapt);
    let signer = key(seed, "guard-relayer");
    node.watch(signer.address());
    let funder = key(seed, "guard-funder");
    node.fund(funder.address(), U256::from(10 * ETHER));
    node.clear_from(EPOCH + plan.clears);
    node.ease_fees_to(GWEI as u64 / 10);
    let posts = plan
        .requests
        .iter()
        .enumerate()
        .map(|(number, request)| {
            ground.build(number, request, plan.stance.priced, plan.stance.sponsors())
        })
        .collect();
    let relayer = Relayer::new(
        node.clone(),
        ground.contract,
        Desk {
            signer,
            keys: RailgunKeys::from_seed(&seed.bytes::<64>("guard-relayer-railgun"), 0),
            send_policy,
            funding_policy,
            priced: plan.stance.priced,
        },
        start,
    );
    let world = Arc::new(World {
        seed,
        plan: plan.clone(),
        node: node.clone(),
        ground,
        relayer: relayer.clone(),
        posts,
        replies: Mutex::default(),
        funder: funder.address(),
    });
    let mut background = vec![("the chain", spawn_local(ticks(node.clone(), start)))];
    for number in 0..plan.requests.len() {
        let world = world.clone();
        background.push(("a wallet", spawn_local(world.post(number, start))));
    }
    if plan.stance.top_up > 0 {
        let world = world.clone();
        background.push(("the operator", spawn_local(world.top_up(funder, start))));
    }
    let horizon = || start + Duration::from_secs(HORIZON);
    let life = lives(
        "the relayer",
        &*notes(&node),
        start,
        horizon,
        &plan.crashes,
        || spawn_local(relayer.clone().live()),
    )
    .await;
    panics::end(background.into_iter().chain(life)).await;
    world.report()
}

fn retried(why: &str) -> bool {
    RETRIED.iter().any(|reason| why.starts_with(reason))
}

impl World {
    async fn top_up(self: Arc<Self>, funder: PrivateKeySigner, start: Instant) {
        sleep_until(start + Duration::from_secs(self.plan.stance.top_up)).await;
        let envelope = TransactionRequest::default()
            .with_to(self.relayer.account())
            .with_value(U256::from(TOP_UP))
            .with_nonce(0)
            .with_gas_limit(21_000)
            .with_gas_price(400 * GWEI + 1)
            .with_chain_id(CHAIN_ID)
            .build(&EthereumWallet::from(funder))
            .await
            .expect("a signed top-up");
        let raw = Bytes::from(envelope.encoded_2718());
        while self.node.nonce_at(self.funder, 0) == 0 {
            self.node.note("the operator tops up the relayer".into());
            let _ = self
                .node
                .answer("eth_sendRawTransaction", json!([raw]), false);
            sleep(Duration::from_secs(60)).await;
        }
    }

    fn landed(&self, data: &Bytes) -> bool {
        let relayer = self.relayer.account();
        self.node
            .landed_from(relayer)
            .iter()
            .any(|landed| landed.succeeded && landed.input == *data)
    }

    fn escrow(&self, swap: B256) -> Option<B256> {
        let call = IZecSwap::reverseFundingCall { id: swap }.abi_encode();
        let out = self.node.view(self.ground.contract, call.into()).ok()?;
        let funding = IZecSwap::reverseFundingCall::abi_decode_returns(&out).ok()?;
        (funding.blockNumber > 0).then_some(funding.refundNote)
    }

    fn done(&self, post: &Post) -> bool {
        match post.swap {
            Some(swap) => self.escrow(swap).is_some(),
            None => self.landed(&post.data),
        }
    }

    async fn post(self: Arc<Self>, number: usize, start: Instant) {
        let request = self.plan.requests[number];
        sleep_until(start + Duration::from_secs(request.at)).await;
        for (index, (post, must)) in self.posts[number].iter().enumerate() {
            self.node
                .note(format!("request {number}.{index} {:?} posted", request.ask));
            if request.twice {
                let world = self.clone();
                let post = post.clone();
                spawn_local(async move {
                    let reply = world.relayer.serve(number, &post).await;
                    world.replied(number, index, reply);
                });
            }
            self.until(number, index, post, *must, start).await;
            if request.ask == Ask::Funded(Funded::Again) && self.done(post) {
                self.node
                    .note(format!("request {number}.{index} posted again"));
                let reply = self.relayer.serve(number, post).await;
                self.replied(number, index, reply);
            }
        }
    }

    fn replied(&self, number: usize, index: usize, reply: Reply) {
        let mut replies = self.replies.lock().unwrap();
        replies.entry((number, index)).or_default().push(reply);
    }

    async fn until(&self, number: usize, index: usize, post: &Post, must: Must, start: Instant) {
        loop {
            if start.elapsed().as_secs() > LAST_POST {
                return;
            }
            let reply = self.relayer.serve(number, post).await;
            self.replied(number, index, reply.clone());
            let wait = match reply {
                Reply::Sent(_) if must != Must::Serve => return,
                Reply::Sent(_) => {
                    let watched = Instant::now();
                    while !self.done(post) && watched.elapsed() < Duration::from_secs(10 * 60) {
                        sleep(Duration::from_secs(SLOT)).await;
                    }
                    if self.done(post) {
                        return;
                    }
                    0
                }
                Reply::Rejected(why) if must == Must::Serve && retried(&why) => 60,
                Reply::Rejected(_) | Reply::Spent(_) => return,
                Reply::Busy(seconds) => seconds,
                Reply::Internal(_) | Reply::Lost => 30,
            };
            sleep(Duration::from_secs(wait)).await;
        }
    }

    fn report(&self) -> Report<Plan, Ended> {
        let mut failures = self.relayer.wrong();
        failures.truncate(5);
        let relayer = self.relayer.account();
        let landed = self.node.landed_from(relayer);
        let simulated: Vec<Bytes> = self
            .node
            .simulated()
            .into_iter()
            .filter(|(from, _)| *from == relayer)
            .map(|(_, input)| input)
            .collect();
        let replies = self.replies.lock().unwrap().clone();
        let settled = self.plan.settled() <= HORIZON;
        let mut ended = Ended::default();
        let mut fail = |what: String| failures.push(what);
        for (number, posts) in self.posts.iter().enumerate() {
            let ask = self.plan.requests[number].ask;
            for (index, (post, must)) in posts.iter().enumerate() {
                let told = format!("request {number}.{index} {ask:?}");
                let sent = landed.iter().any(|landed| landed.input == post.data);
                let heard = replies.get(&(number, index)).cloned().unwrap_or_default();
                for reply in &heard {
                    match reply {
                        Reply::Sent(_) => ended.served += 1,
                        Reply::Rejected(_) | Reply::Spent(_) => ended.refused += 1,
                        _ => {}
                    }
                }
                match must {
                    Must::Refuse if simulated.contains(&post.data) => fail(format!(
                        "{told}: the relayer simulated what its policy must refuse"
                    )),
                    Must::Refuse | Must::Fail if sent => {
                        fail(format!("{told}: the relayer sent what it must refuse"))
                    }
                    Must::Serve => {
                        for reply in &heard {
                            if let Reply::Rejected(why) = reply
                                && !retried(why)
                            {
                                fail(format!("{told}: an honest request was refused: {why}"));
                            }
                        }
                        let due = settled
                            && match post.swap {
                                Some(_) => {
                                    self.plan.requests[number].at + 50 * 60 > self.plan.settled()
                                }
                                None => true,
                            };
                        if (self.plan.calm() || due) && !self.done(post) {
                            fail(format!("{told}: an honest request never landed"));
                        }
                    }
                    Must::Refuse | Must::Fail => {}
                }
            }
        }
        self.landings(&landed, &mut fail, &mut ended);
        Report {
            seed: self.seed.0,
            plan: self.plan.clone(),
            ended: Some(ended),
            stage: None,
            failures,
            trace: self.node.trace(),
        }
    }

    fn asked(&self, input: &Bytes) -> Option<&Post> {
        let posts = self.posts.iter().flatten();
        posts.map(|(post, _)| post).find(|post| post.data == *input)
    }

    fn view(&self, block: u64, to: Address, data: Vec<u8>) -> Option<Bytes> {
        self.node.view_at(block, to, data.into()).ok()
    }

    fn spent_by(&self, block: u64, transactions: &[Transaction]) -> bool {
        transactions.iter().any(|transaction| {
            transaction.nullifiers.iter().any(|nullifier| {
                let call = nullifiersCall {
                    treeNumber: U256::from(transaction.boundParams.treeNumber),
                    nullifier: *nullifier,
                };
                let out = self.view(block, self.ground.railgun, call.abi_encode());
                out.and_then(|out| nullifiersCall::abi_decode_returns(&out).ok()) == Some(true)
            })
        })
    }

    fn token_balance(&self, token: Address, owner: Address) -> u128 {
        let out = self
            .node
            .view(token, balanceOfCall { owner }.abi_encode().into());
        let out = out.expect("the token answers balanceOf");
        balanceOfCall::abi_decode_returns(&out)
            .expect("a balance")
            .to()
    }

    fn landings(&self, landed: &[Landed], fail: &mut impl FnMut(String), ended: &mut Ended) {
        let relayer = self.relayer.account();
        let keys = RailgunKeys::from_seed(&self.seed.bytes::<64>("guard-relayer-railgun"), 0);
        let counted = self.relayer.counted();
        let mut spent_gas = U256::ZERO;
        let mut fees_in = 0u128;
        let mut times: BTreeMap<&Bytes, usize> = BTreeMap::new();
        for landing in landed {
            ended.landed += 1;
            ended.reverted += usize::from(!landing.succeeded);
            spent_gas += U256::from(landing.gas_used) * U256::from(landing.price);
            *times.entry(&landing.input).or_default() += 1;
            let told = format!("block {}: the relayer's {}", landing.block, landing.hash);
            if landing.price > self.plan.stance.cap {
                fail(format!(
                    "{told} paid {} wei a gas, over its cap",
                    landing.price
                ));
            }
            let Some(post) = self.asked(&landing.input) else {
                fail(format!("{told} carries calldata nobody asked it to send"));
                continue;
            };
            match post.swap {
                None => {
                    let Ok(call) = IRailgunSmartWallet::transactCall::abi_decode(&landing.input)
                    else {
                        fail(format!("{told} is no transact call"));
                        continue;
                    };
                    if call.abi_encode() != landing.input.to_vec() {
                        fail(format!("{told} sent calldata in a noncanonical encoding"));
                    }
                    if !landing.succeeded {
                        if !self.spent_by(landing.block, &call._transactions) {
                            fail(format!(
                                "{told} reverted though nothing it spends was spent"
                            ));
                        }
                        continue;
                    }
                    let read = self.read_fees(&keys, landing);
                    let most = counted
                        .iter()
                        .filter(|counted| counted.data == landing.input);
                    if let Some(fee) = most.map(|counted| counted.fee).max()
                        && read < fee.max(FLOOR)
                    {
                        fail(format!(
                            "{told} counted a fee of {fee} and its keys read {read}"
                        ));
                    }
                }
                Some(swap) => {
                    let Ok(call) = IRelayAdapt::relayCall::abi_decode(&landing.input) else {
                        fail(format!("{told} is no relay call"));
                        continue;
                    };
                    if call.abi_encode() != landing.input.to_vec() {
                        fail(format!("{told} sent calldata in a noncanonical encoding"));
                    }
                    if !landing.succeeded {
                        let block = landing.block;
                        let open = self.opened_by(block, swap);
                        if !open && !self.spent_by(block, &call._transactions) {
                            fail(format!(
                                "{told} reverted though nothing it spends was spent"
                            ));
                        }
                        continue;
                    }
                    let paid = call._actionData.calls.iter().find_map(|call| {
                        let transfer = IErc20::transferCall::abi_decode(&call.data).ok()?;
                        (call.to == self.ground.token && transfer.to == relayer)
                            .then(|| transfer.amount.to::<u128>())
                    });
                    fees_in += paid.unwrap_or(0);
                    if self.escrow(swap).is_none() {
                        fail(format!("{told} landed and no escrow is open for {swap}"));
                    }
                }
            }
        }
        for (input, count) in times {
            if count > 1 {
                let post = self.asked(input).map(|post| post.swap);
                fail(format!(
                    "the relayer's calldata {post:?} was mined {count} times"
                ));
            }
        }
        let topped = self
            .node
            .landed_from(self.funder)
            .iter()
            .filter(|landed| landed.succeeded)
            .count();
        let start = U256::from(self.plan.stance.eth) + U256::from(TOP_UP) * U256::from(topped);
        let balance = self.node.balance(relayer);
        if balance + spent_gas != start {
            fail(format!(
                "the relayer holds {balance} wei: {start} less {spent_gas} for gas is another sum"
            ));
        }
        let held = self.token_balance(self.ground.token, relayer);
        if held != HELD + fees_in {
            fail(format!(
                "the relayer holds {held} of its token, not {HELD} and the {fees_in} it was paid"
            ));
        }
        let left = self.token_balance(self.ground.token, self.ground.adapt);
        if left > 0 {
            fail(format!("Relay Adapt was left holding {left} of the token"));
        }
    }

    fn opened_by(&self, block: u64, swap: B256) -> bool {
        let call = IZecSwap::reverseFundingCall { id: swap }.abi_encode();
        let out = self.view(block, self.ground.contract, call);
        let funding =
            out.and_then(|out| IZecSwap::reverseFundingCall::abi_decode_returns(&out).ok());
        funding.is_some_and(|funding| funding.blockNumber > 0)
    }

    fn read_fees(&self, keys: &RailgunKeys, landing: &Landed) -> u128 {
        let token = self.ground.token.into_word().0;
        let events = landing
            .logs
            .iter()
            .filter(|log| log.address == self.ground.railgun);
        let events = events.filter_map(|log| Transact::decode_log(log).ok());
        events
            .flat_map(|event| {
                let notes = event.hash.clone().into_iter().zip(event.ciphertext.clone());
                notes
                    .filter_map(|(commitment, ciphertext)| {
                        let output = OutputCiphertext {
                            ciphertext: ciphertext.ciphertext.map(|word| word.0),
                            blinded_sender_viewing_key: ciphertext.blindedSenderViewingKey.0,
                            memo: ciphertext.memo.to_vec(),
                        };
                        let received = keys.receive(&commitment.0, &output)?;
                        (received.token == token).then_some(received.value)
                    })
                    .collect::<Vec<_>>()
            })
            .sum()
    }
}
