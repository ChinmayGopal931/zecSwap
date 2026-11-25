mod canonical;
pub mod deposits;
mod events;
pub mod funding;
pub mod judge;
mod meter;
pub mod railgun;
pub use events::{SwapEvent, SwapEventKind};
pub use meter::{RpcMeter, RpcReport, RpcUsage, endpoint, log_usage};
use railgun::Signed;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::contract::{CallBuilder, CallDecoder};
use alloy::eips::{BlockNumberOrTag, Encodable2718};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::keccak256;
use alloy::providers::bindings::IMulticall3;
use alloy::providers::utils::{EIP1559_BASE_FEE_MULTIPLIER, Eip1559Estimation};
use alloy::providers::{
    DynProvider, MULTICALL3_ADDRESS, PendingTransactionBuilder, Provider, ProviderBuilder,
};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::json_rpc::ErrorPayload;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::{RpcError, TransportError};
use tokio::sync::{Mutex, MutexGuard, Notify, OnceCell};
use tokio::time::Instant;
use tracing::{info, warn};
use zecswap_core::{PublicShare, ReverseOpen, SecretShare, Terms};
use zecswap_railgun::{ShieldCiphertext, ShieldNote};

use crate::Error;
use crate::alert::{self, Alert, Alerts, Unlanded};

pub use alloy::eips::BlockId;
pub use alloy::primitives::{Address, B256, U256};
pub use alloy::signers::local::PrivateKeySigner;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);
const SEND_ATTEMPTS: u32 = 4;
const SEND_RETRY_DELAY: Duration = Duration::from_secs(3);
const BATCH: usize = 100;
const PROTECT_BLOCKS: u64 = 25;
const PROTECT_MARGIN_BLOCKS: u64 = 5;
const REVEAL_GAS: u64 = 400_000;
const REPRICE_BLOCKS: u64 = 3;
const QUEUED_BLOCKS: u64 = 6;
const NONCE_TAKEN_READS: u32 = 3;
const LAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);

sol! {
    struct ReverseOpenWords {
        address maker;
        address user;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        uint64 t0;
        uint64 t1;
        bytes32 refundNote;
        uint64 deadline;
    }

    struct ShieldCiphertextWords {
        bytes32[3] encryptedBundle;
        bytes32 shieldKey;
    }

    struct TermsWords {
        address maker;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        address user;
        uint64 t0;
        uint64 t1;
        bytes32 payoutNote;
    }

    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    interface IZecSwap {
        event Opened(bytes32 indexed id, address indexed maker, address indexed user, address token, uint256 amount, uint256[2] makerKey, uint256[2] userKey, uint64 t0, uint64 t1, bytes32 payoutNote);
        event MarkedReady(bytes32 indexed id);
        event Claimed(bytes32 indexed id, uint256 userSecret);
        event PaidOut(bytes32 indexed id, address relayer, uint256 fee);
        event Rescued(bytes32 indexed id, address relayer, uint256 fee);
        event Refunded(bytes32 indexed id, uint256 makerSecret);
        struct Swap {
            bytes32 termsHash;
            uint8 stage;
            bool paidOut;
            uint256 secret;
        }

        function openReverse(ReverseOpenWords terms, bytes signature) external returns (bytes32);
        function reverseFunding(bytes32 id) external view returns (bytes32 refundNote, uint64 blockNumber);
        function readyWithSig(bytes32 id, TermsWords terms, uint64 deadline, bytes signature) external;
        function refundPayout(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function deposit(address token, uint256 amount) external;
        function withdraw(address token, uint256 amount, address to) external;
        function open(address token, uint128 amount, uint256[2] makerKey, uint256[2] userKey, address user, uint64 t0, uint64 t1, bytes32 payoutNote) external returns (bytes32 id);
        function ready(bytes32 id, TermsWords terms) external;
        function claim(bytes32 id, TermsWords terms, uint256 userSecret) external;
        function payout(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, bytes signature) external;
        function rescue(bytes32 id, TermsWords terms, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint64 nonce, uint64 deadline, bytes signature) external;
        function rescueNonces(bytes32 id) external view returns (uint64);
        function refund(bytes32 id, TermsWords terms, uint256 makerSecret) external;
        function getSwap(bytes32 id) external view returns (Swap memory);
        function vaultOf(bytes32 id) external view returns (address);
        function balanceOf(address maker, address token) external view returns (uint256);
        function MIN_WINDOW() external view returns (uint256);
        function LATE_WINDOW() external view returns (uint256);
        function RAILGUN() external view returns (address);
    }

    #[sol(rpc)]
    interface IRailgun {
        struct TokenData {
            uint8 tokenType;
            address tokenAddress;
            uint256 tokenSubID;
        }

        struct CommitmentPreimage {
            bytes32 npk;
            TokenData token;
            uint120 value;
        }

        event Shield(uint256 treeNumber, uint256 startPosition, CommitmentPreimage[] commitments, ShieldCiphertextWords[] shieldCiphertext, uint256[] fees);

        function tokenBlocklist(address token) external view returns (bool);
    }

    #[sol(rpc)]
    interface IErc20 {
        event Transfer(address indexed from, address indexed to, uint256 value);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
        function balanceOf(address owner) external view returns (uint256);
        function mint(address to, uint256 amount) external;
    }

    #[allow(clippy::too_many_arguments)]
    #[sol(rpc)]
    interface IDeposits {
        event DepositShielded(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
        event Rescued(address indexed from, uint256 value, uint256 fee, address relayer, bytes32 note);
        function shield(address from, uint256 value, uint256 validAfter, uint256 validBefore, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint8 v, bytes32 r, bytes32 s) external;
        function rescue(address from, bytes32 npk, ShieldCiphertextWords ciphertext, uint128 fee, uint64 nonce, uint64 deadline, bytes signature) external;
        function vaultOf(address from) external view returns (address);
        function deposited(address from) external view returns (bool);
        function rescueNonces(address from) external view returns (uint64);
        function TOKEN() external view returns (address);
        function RAILGUN() external view returns (address);
        function SANCTIONS() external view returns (address);
    }

    #[sol(rpc)]
    interface IEip3009 {
        function name() external view returns (string);
        function version() external view returns (string);
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function authorizationState(address authorizer, bytes32 nonce) external view returns (bool);
    }

    #[sol(rpc)]
    interface ISanctionsOracle {
        function isSanctioned(address addr) external view returns (bool);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Open,
    Ready,
    Claimed,
    Refunded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionFacts {
    pub sender: Address,
    pub succeeded: bool,
    pub block_number: u64,
    pub block_hash: B256,
    pub gas_used: u64,
    pub gas_price: u128,
    pub uses_railgun: bool,
    pub paid_to_sender: U256,
}

fn paid_to(
    sender: Address,
    token: Address,
    escrow: Address,
    logs: &[alloy::rpc::types::Log],
    input: Option<&[u8]>,
) -> U256 {
    let transferred = logs
        .iter()
        .filter(|log| {
            log.address() == token
                && log.topics().first() == Some(&IErc20::Transfer::SIGNATURE_HASH)
        })
        .filter_map(|log| IErc20::Transfer::decode_log_data(log.data()).ok())
        .filter(|transfer| transfer.to == sender)
        .fold(U256::ZERO, |total, transfer| {
            total.saturating_add(transfer.value)
        });
    let escrow_fees = logs
        .iter()
        .filter(|log| log.address() == escrow)
        .filter_map(|log| match log.topics().first() {
            Some(&IZecSwap::PaidOut::SIGNATURE_HASH) => {
                IZecSwap::PaidOut::decode_log_data(log.data())
                    .ok()
                    .map(|paid| (paid.relayer, paid.fee))
            }
            Some(&IZecSwap::Rescued::SIGNATURE_HASH) => {
                IZecSwap::Rescued::decode_log_data(log.data())
                    .ok()
                    .map(|rescued| (rescued.relayer, rescued.fee))
            }
            _ => None,
        })
        .filter(|(relayer, _)| *relayer == sender)
        .fold(U256::ZERO, |total, (_, fee)| total.saturating_add(fee));
    let relayed = input
        .and_then(|input| railgun::IRelayAdapt::relayCall::abi_decode(input).ok())
        .map_or(U256::ZERO, |relay| {
            relay
                ._actionData
                .calls
                .iter()
                .filter(|call| call.to == token)
                .filter_map(|call| IErc20::transferCall::abi_decode(&call.data).ok())
                .fold(U256::ZERO, |total, payment| {
                    total.saturating_add(payment.amount)
                })
        });
    transferred.max(escrow_fees.saturating_add(relayed))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnChainSwap {
    pub stage: Stage,
    pub maker: Address,
    pub user: Address,
    pub token: Address,
    pub amount: u128,
    pub t0: u64,
    pub t1: u64,
    pub maker_share: PublicShare,
    pub user_share: PublicShare,
    pub secret: [u8; 32],
    pub payout_note: Option<B256>,
    pub paid_out: bool,
}

impl OnChainSwap {
    pub fn revealed(&self) -> Result<Option<SecretShare>, Error> {
        match self.stage {
            Stage::Claimed | Stage::Refunded => Ok(Some(SecretShare::from_be_bytes(&self.secret)?)),
            Stage::Open | Stage::Ready => Ok(None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapState {
    pub stage: Stage,
    pub terms_hash: B256,
    pub secret: [u8; 32],
    pub paid_out: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub number: u64,
    pub timestamp: u64,
    pub hash: B256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    pub min: u64,
    pub late: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reveal {
    Claim,
    Refund,
}

impl Reveal {
    pub fn name(self) -> &'static str {
        match self {
            Reveal::Claim => "claim",
            Reveal::Refund => "refund",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    Shut,
    Until(u64),
    Lasting,
}

impl Window {
    pub fn leaves(self, now: u64, margin: u64) -> bool {
        match self {
            Window::Shut => false,
            Window::Until(end) => now.saturating_add(margin) < end,
            Window::Lasting => true,
        }
    }
}

impl Windows {
    pub fn window(
        &self,
        reveal: Reveal,
        reverse: bool,
        stage: Stage,
        (t0, t1): (u64, u64),
        now: u64,
    ) -> Window {
        if !matches!(stage, Stage::Open | Stage::Ready) {
            return Window::Shut;
        }
        let late = t1.saturating_add(self.late);
        let open = |yes: bool, window: Window| if yes { window } else { Window::Shut };
        match (reveal, reverse) {
            (Reveal::Claim, false) if now >= t1 => open(now >= late, Window::Lasting),
            (Reveal::Claim, false) => open(stage == Stage::Ready || now >= t0, Window::Until(t1)),
            (Reveal::Claim, true) => open(now < t1 && stage == Stage::Ready, Window::Until(t1)),
            (Reveal::Refund, false) if now >= t1 => open(now < late, Window::Until(late)),
            (Reveal::Refund, false) => open(stage == Stage::Open && now < t0, Window::Until(t0)),
            (Reveal::Refund, true) => open(stage == Stage::Open || now >= t1, Window::Lasting),
        }
    }
}

pub fn wall_now() -> u64 {
    #[cfg(feature = "sim")]
    if let Some(now) = simulated_wall() {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(feature = "sim")]
thread_local! {
    static SIMULATED_WALL: std::cell::Cell<Option<(i64, Instant)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(feature = "sim")]
pub fn simulate_wall(epoch: Option<i64>) -> Option<i64> {
    let before = SIMULATED_WALL.get().map(|(epoch, _)| epoch);
    let started = SIMULATED_WALL.get().map(|(_, started)| started);
    SIMULATED_WALL.set(epoch.map(|epoch| (epoch, started.unwrap_or_else(Instant::now))));
    before
}

#[cfg(feature = "sim")]
pub fn simulated_wall() -> Option<u64> {
    SIMULATED_WALL.get().map(|(epoch, started)| {
        let elapsed = i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX);
        u64::try_from(epoch.saturating_add(elapsed)).unwrap_or(0)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shielded {
    pub note: ShieldNote,
    pub token: Address,
    pub value: u128,
    pub fee: u128,
}

pub fn swap_id(maker: Address, user_share: &PublicShare) -> B256 {
    let mut preimage = [0; 96];
    preimage[12..32].copy_from_slice(maker.as_slice());
    preimage[32..].copy_from_slice(&user_share.to_affine_bytes());
    keccak256(preimage)
}

pub fn reverse_swap_id(user: Address, maker_share: &PublicShare) -> B256 {
    let mut preimage = [0; 128];
    preimage[12..32].copy_from_slice(user.as_slice());
    preimage[32..96].copy_from_slice(&maker_share.to_affine_bytes());
    preimage[127] = 1;
    keccak256(preimage)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReverseFunding {
    pub refund_note: B256,
    pub block_number: u64,
}

pub fn reverse_funding_calls(
    contract: Address,
    terms: &ReverseOpen,
    signature: &[u8; 65],
) -> [(Address, Vec<u8>); 2] {
    let approve = IErc20::approveCall {
        spender: contract,
        amount: U256::from(terms.amount),
    }
    .abi_encode();
    let open = IZecSwap::openReverseCall {
        terms: ReverseOpenWords {
            maker: terms.maker.into(),
            user: terms.user.into(),
            token: terms.token.into(),
            amount: terms.amount,
            makerKey: share_words(&terms.maker_share),
            userKey: share_words(&terms.user_share),
            t0: terms.t0,
            t1: terms.t1,
            refundNote: terms.refund_note.into(),
            deadline: terms.deadline,
        },
        signature: signature.to_vec().into(),
    }
    .abi_encode();
    [(terms.token.into(), approve), (contract, open)]
}

pub struct Settlement {
    provider: DynProvider,
    contract: IZecSwap::IZecSwapInstance<DynProvider>,
    account: Option<Address>,
    wallet: Option<EthereumWallet>,
    private: Option<Private>,
    reveal_margin: u64,
    sending: Line,
    meter: RpcMeter,
    railgun: OnceCell<Address>,
    multicall: OnceCell<bool>,
    windows: OnceCell<Windows>,
    reveal_headroom: u128,
    alerts: Option<Alerts>,
    landed: std::sync::Mutex<Option<(u64, Unlanded)>>,
}

struct Private {
    provider: DynProvider,
    blocks: u64,
    least_nonce: AtomicU64,
    watch: std::sync::Mutex<Watch>,
}

#[derive(Default)]
struct Watch {
    raised: Option<(B256, u64)>,
    dropped: Option<B256>,
}

impl Private {
    fn new(provider: DynProvider, blocks: u64) -> Self {
        Self {
            provider,
            blocks,
            least_nonce: AtomicU64::new(0),
            watch: Default::default(),
        }
    }

    fn window(&self) -> u64 {
        self.blocks + PROTECT_MARGIN_BLOCKS
    }
}

#[derive(Clone, Debug)]
enum Check {
    Reveal {
        id: B256,
        terms: Box<Terms>,
        reveal: Reveal,
        margin: u64,
    },
    Ready {
        id: B256,
        deadline: u64,
        t0: u64,
        margin: u64,
    },
}

impl Check {
    fn reveals(&self) -> bool {
        matches!(self, Check::Reveal { .. })
    }
}

struct Sent {
    account: Address,
    tx: TransactionRequest,
    nonce: u64,
    cap: u128,
    tip: u128,
    priced_at: u64,
    hashes: Vec<(B256, u128)>,
}

#[derive(Default)]
struct Line {
    turn: Mutex<()>,
    ended: Notify,
}

impl Line {
    async fn turn(&self) -> Turn<'_> {
        Turn {
            _turn: self.turn.lock().await,
            ended: &self.ended,
        }
    }
}

struct Turn<'a> {
    _turn: MutexGuard<'a, ()>,
    ended: &'a Notify,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.ended.notify_one();
    }
}

impl Settlement {
    pub fn connect(
        rpc_url: &str,
        contract: Address,
        signer: PrivateKeySigner,
    ) -> Result<Self, Error> {
        Self::connect_metered(rpc_url, contract, signer, &RpcMeter::default())
    }

    pub fn connect_metered(
        rpc_url: &str,
        contract: Address,
        signer: PrivateKeySigner,
        meter: &RpcMeter,
    ) -> Result<Self, Error> {
        let account = signer.address();
        let wallet = EthereumWallet::from(signer);
        Ok(Self::new(
            signing_provider(rpc_url, Some(wallet.clone()), meter)?,
            contract,
            Some((account, wallet)),
            meter,
        ))
    }

    pub fn read_only(rpc_url: &str, contract: Address) -> Result<Self, Error> {
        let meter = RpcMeter::default();
        Ok(Self::new(
            signing_provider(rpc_url, None, &meter)?,
            contract,
            None,
            &meter,
        ))
    }

    fn new(
        provider: DynProvider,
        contract: Address,
        account: Option<(Address, EthereumWallet)>,
        meter: &RpcMeter,
    ) -> Self {
        let (account, wallet) = account.unzip();
        Self {
            contract: IZecSwap::new(contract, provider.clone()),
            provider,
            account,
            wallet,
            private: None,
            reveal_margin: 0,
            sending: Line::default(),
            meter: meter.clone(),
            railgun: OnceCell::new(),
            multicall: OnceCell::new(),
            windows: OnceCell::new(),
            reveal_headroom: 0,
            alerts: None,
            landed: Default::default(),
        }
    }

    #[cfg(feature = "sim")]
    pub fn over(
        transport: alloy::transports::BoxTransport,
        contract: Address,
        signer: Option<PrivateKeySigner>,
    ) -> Self {
        let meter = RpcMeter::default();
        let account = signer.map(|signer| (signer.address(), EthereumWallet::from(signer)));
        let wallet = account.as_ref().map(|(_, wallet)| wallet.clone());
        let client = ClientBuilder::default()
            .layer(meter.layer())
            .transport(transport, false);
        Self::new(provider_over(client, wallet), contract, account, &meter)
    }

    #[cfg(feature = "sim")]
    pub fn broadcast_privately_over(
        &mut self,
        transport: alloy::transports::BoxTransport,
        blocks: u64,
    ) {
        let client = ClientBuilder::default()
            .layer(RpcMeter::default().layer())
            .transport(transport, false);
        self.private = Some(Private::new(provider_over(client, None), blocks));
    }

    #[cfg(feature = "sim")]
    pub fn forget_sends(&self) {
        if let Some(private) = &self.private {
            private.least_nonce.store(0, Ordering::Relaxed);
            *private.watch.lock().unwrap() = Watch::default();
        }
        *self.landed.lock().unwrap() = None;
    }

    pub fn broadcast_privately(&mut self, rpc_url: &str) -> Result<(), Error> {
        let blocks = protect_blocks(&private_url(rpc_url)?)?;
        let provider = signing_provider(rpc_url, None, &RpcMeter::default())?;
        self.private = Some(Private::new(provider, blocks));
        Ok(())
    }

    pub fn private_window(&self) -> Option<u64> {
        self.private.as_ref().map(Private::window)
    }

    pub fn reveal_with_margin(&mut self, seconds: u64) {
        self.reveal_margin = seconds;
    }

    pub fn alerts_to(&mut self, alerts: Alerts) {
        self.alerts = Some(alerts);
    }

    pub async fn check_private_rpc(&self) -> Result<(), Error> {
        let Some(private) = &self.private else {
            return Ok(());
        };
        let theirs = private
            .provider
            .get_chain_id()
            .await
            .map_err(|_| Error::Contract("the private RPC is unavailable".into()))?;
        let ours = self.chain_id().await?;
        if theirs != ours {
            return Err(Error::Config(format!(
                "the private RPC serves chain {theirs}, not {ours}"
            )));
        }
        Ok(())
    }

    fn broadcaster(&self) -> &DynProvider {
        self.private
            .as_ref()
            .map_or(&self.provider, |private| &private.provider)
    }

    pub fn contract(&self) -> Address {
        *self.contract.address()
    }

    pub fn account(&self) -> Option<Address> {
        self.account
    }

    pub fn meter(&self) -> &RpcMeter {
        &self.meter
    }

    pub async fn chain_id(&self) -> Result<u64, Error> {
        self.provider.get_chain_id().await.map_err(Error::contract)
    }

    pub async fn now(&self) -> Result<u64, Error> {
        Ok(self.head().await?.timestamp)
    }

    pub async fn head(&self) -> Result<Head, Error> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("no latest block".into()))?;
        Ok(Head {
            number: block.header.number,
            timestamp: block.header.timestamp,
            hash: block.header.hash,
        })
    }

    pub async fn confirmed_now(&self, confirmations: u64) -> Result<u64, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let height = latest.saturating_sub(confirmations - 1);
        let block = self
            .provider
            .get_block_by_number(height.into())
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("confirmed block unavailable".into()))?;
        Ok(block.header.timestamp)
    }

    pub async fn windows(&self) -> Result<Windows, Error> {
        self.windows
            .get_or_try_init(|| async {
                let missing = |e| {
                    Error::Contract(format!(
                        "{} has no MIN_WINDOW or LATE_WINDOW, as only the windows contract has: {e}",
                        self.contract()
                    ))
                };
                let (min, late) = tokio::try_join!(
                    async { self.contract.MIN_WINDOW().call().await.map_err(missing) },
                    async { self.contract.LATE_WINDOW().call().await.map_err(missing) },
                )?;
                Ok::<_, Error>(Windows {
                    min: u64::try_from(min).map_err(Error::contract)?,
                    late: u64::try_from(late).map_err(Error::contract)?,
                })
            })
            .await
            .copied()
    }

    pub async fn read(
        &self,
        id: B256,
        terms: &Terms,
    ) -> Result<(Option<OnChainSwap>, Head), Error> {
        let head = self.head().await?;
        Ok((
            self.swap_at(id, terms, BlockId::hash(head.hash)).await?,
            head,
        ))
    }

    pub async fn swap_at(
        &self,
        id: B256,
        terms: &Terms,
        at: BlockId,
    ) -> Result<Option<OnChainSwap>, Error> {
        let swap = self
            .contract
            .getSwap(id)
            .block(at)
            .call()
            .await
            .map_err(Error::contract)?;
        on_chain(id, decode_state(swap)?, terms)
    }

    pub async fn reveal_window(
        &self,
        id: B256,
        terms: &Terms,
        reveal: Reveal,
    ) -> Result<(Window, Head), Error> {
        let windows = self.windows().await?;
        let head = self.head().await?;
        let at = BlockId::hash(head.hash);
        let (swap, funding) = tokio::try_join!(self.swap_at(id, terms, at), async {
            let funding = self.contract.reverseFunding(id).block(at).call().await;
            funding.map_err(Error::contract)
        })?;
        let window = swap.map_or(Window::Shut, |swap| {
            let reverse = !funding.refundNote.is_zero();
            windows.window(
                reveal,
                reverse,
                swap.stage,
                (swap.t0, swap.t1),
                head.timestamp,
            )
        });
        Ok((window, head))
    }

    pub async fn swap(&self, id: B256, terms: &Terms) -> Result<Option<OnChainSwap>, Error> {
        on_chain(id, self.swap_state(id).await?, terms)
    }

    pub async fn swap_state(&self, id: B256) -> Result<Option<SwapState>, Error> {
        let swap = self
            .contract
            .getSwap(id)
            .call()
            .await
            .map_err(Error::contract)?;
        decode_state(swap)
    }

    pub async fn confirmed_swap(
        &self,
        id: B256,
        terms: &Terms,
        confirmations: u64,
    ) -> Result<Option<OnChainSwap>, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let Some(height) = latest.checked_sub(confirmations - 1) else {
            return Ok(None);
        };
        let swap = self
            .contract
            .getSwap(id)
            .block(height.into())
            .call()
            .await
            .map_err(Error::contract)?;
        on_chain(id, decode_state(swap)?, terms)
    }

    pub async fn swap_states(
        &self,
        ids: &[B256],
        at: BlockId,
    ) -> Result<Vec<Option<SwapState>>, Error> {
        if !self.multicall_deployed().await? {
            let mut states = Vec::with_capacity(ids.len());
            for id in ids {
                let swap = self
                    .contract
                    .getSwap(*id)
                    .block(at)
                    .call()
                    .await
                    .map_err(Error::contract)?;
                states.push(decode_state(swap)?);
            }
            return Ok(states);
        }
        let calls = ids.iter().map(|id| {
            (
                self.contract(),
                IZecSwap::getSwapCall { id: *id }.abi_encode(),
            )
        });
        self.aggregate(calls.collect(), at)
            .await?
            .into_iter()
            .map(|data| {
                let swap =
                    IZecSwap::getSwapCall::abi_decode_returns(&data).map_err(Error::contract)?;
                decode_state(swap)
            })
            .collect()
    }

    pub async fn eth_balances(&self, owners: &[Address]) -> Result<Vec<U256>, Error> {
        if !self.multicall_deployed().await? {
            let mut balances = Vec::with_capacity(owners.len());
            for owner in owners {
                balances.push(self.eth_balance(*owner).await?);
            }
            return Ok(balances);
        }
        let calls = owners.iter().map(|owner| {
            let call = IMulticall3::getEthBalanceCall { addr: *owner };
            (MULTICALL3_ADDRESS, call.abi_encode())
        });
        self.aggregate(calls.collect(), BlockId::latest())
            .await?
            .into_iter()
            .map(|data| {
                IMulticall3::getEthBalanceCall::abi_decode_returns(&data).map_err(Error::contract)
            })
            .collect()
    }

    async fn multicall_deployed(&self) -> Result<bool, Error> {
        self.multicall
            .get_or_try_init(|| async {
                let code = self
                    .provider
                    .get_code_at(MULTICALL3_ADDRESS)
                    .await
                    .map_err(Error::contract)?;
                Ok::<_, Error>(!code.is_empty())
            })
            .await
            .copied()
    }

    async fn aggregate(
        &self,
        calls: Vec<(Address, Vec<u8>)>,
        at: BlockId,
    ) -> Result<Vec<alloy::primitives::Bytes>, Error> {
        self.aggregate_by(calls, at, BATCH).await
    }

    async fn aggregate_by(
        &self,
        calls: Vec<(Address, Vec<u8>)>,
        at: BlockId,
        most: usize,
    ) -> Result<Vec<alloy::primitives::Bytes>, Error> {
        let mut answers = Vec::with_capacity(calls.len());
        for batch in calls.chunks(most) {
            let calls = batch
                .iter()
                .map(|(target, data)| IMulticall3::Call3 {
                    target: *target,
                    allowFailure: false,
                    callData: data.clone().into(),
                })
                .collect();
            let request = TransactionRequest::default()
                .with_to(MULTICALL3_ADDRESS)
                .with_input(IMulticall3::aggregate3Call { calls }.abi_encode());
            let returned = self
                .provider
                .call(request)
                .block(at)
                .await
                .map_err(Error::contract)?;
            let results = IMulticall3::aggregate3Call::abi_decode_returns(&returned)
                .map_err(Error::contract)?;
            if results.len() != batch.len() {
                return Err(Error::Contract("Multicall3 answered another count".into()));
            }
            answers.extend(results.into_iter().map(|result| result.returnData));
        }
        Ok(answers)
    }

    pub async fn railgun(&self) -> Result<Address, Error> {
        self.railgun
            .get_or_try_init(|| async {
                self.contract
                    .RAILGUN()
                    .call()
                    .await
                    .map_err(Error::contract)
            })
            .await
            .copied()
    }

    pub async fn railgun_accepts(&self, token: Address) -> Result<bool, Error> {
        let railgun = self.railgun().await?;
        if railgun.is_zero() {
            return Ok(false);
        }
        match IRailgun::new(railgun, &self.provider)
            .tokenBlocklist(token)
            .call()
            .await
        {
            Ok(blocked) => Ok(!blocked),
            Err(e) if e.as_revert_data().is_some() => Ok(false),
            Err(e) => Err(Error::contract(e)),
        }
    }

    pub async fn transaction_facts(
        &self,
        tx: B256,
        token: Address,
    ) -> Result<Option<TransactionFacts>, Error> {
        let Some(receipt) = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
        else {
            return Ok(None);
        };
        let (Some(block_number), Some(block_hash)) = (receipt.block_number, receipt.block_hash)
        else {
            return Ok(None);
        };
        let railgun = self.railgun().await?;
        let logs = receipt.inner.logs();
        let uses_railgun = !railgun.is_zero() && logs.iter().any(|log| log.address() == railgun);
        let input = match uses_railgun && receipt.to != Some(self.contract()) {
            true => self
                .provider
                .get_transaction_by_hash(tx)
                .await
                .map_err(Error::contract)?
                .map(|transaction| alloy::consensus::Transaction::input(&transaction).clone()),
            false => None,
        };
        Ok(Some(TransactionFacts {
            sender: receipt.from,
            succeeded: receipt.status(),
            block_number,
            block_hash,
            gas_used: receipt.gas_used,
            gas_price: receipt.effective_gas_price,
            uses_railgun,
            paid_to_sender: paid_to(
                receipt.from,
                token,
                self.contract(),
                logs,
                input.as_deref().map(|input| &input[..]),
            ),
        }))
    }

    pub async fn transaction_cost(&self, tx: B256) -> Result<Option<(u64, u128)>, Error> {
        Ok(self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
            .map(|receipt| {
                let gas = receipt.gas_used;
                (gas, u128::from(gas) * receipt.effective_gas_price)
            }))
    }

    pub async fn shielded(&self, tx: B256) -> Result<Vec<Shielded>, Error> {
        let railgun = self.railgun().await?;
        let receipt = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract(format!("no receipt for {tx}")))?;
        let mut notes = Vec::new();
        for log in receipt.inner.logs() {
            if log.address() != railgun {
                continue;
            }
            let Ok(event) = IRailgun::Shield::decode_log_data(log.data()) else {
                continue;
            };
            for ((preimage, ciphertext), fee) in event
                .commitments
                .iter()
                .zip(&event.shieldCiphertext)
                .zip(&event.fees)
            {
                notes.push(Shielded {
                    note: ShieldNote {
                        npk: preimage.npk.0,
                        ciphertext: ShieldCiphertext {
                            encrypted_bundle: ciphertext.encryptedBundle.map(|word| word.0),
                            shield_key: ciphertext.shieldKey.0,
                        },
                    },
                    token: preimage.token.tokenAddress,
                    value: preimage.value.to(),
                    fee: u128::try_from(*fee).map_err(Error::contract)?,
                });
            }
        }
        Ok(notes)
    }

    pub async fn balance_of(&self, owner: Address, token: Address) -> Result<u128, Error> {
        self.balance_of_at(owner, token, BlockId::latest()).await
    }

    pub async fn balance_of_at(
        &self,
        owner: Address,
        token: Address,
        at: BlockId,
    ) -> Result<u128, Error> {
        let balance = self
            .contract
            .balanceOf(owner, token)
            .block(at)
            .call()
            .await
            .map_err(Error::contract)?;
        u128::try_from(balance).map_err(Error::contract)
    }

    pub async fn withdraw(&self, token: Address, amount: u128, to: Address) -> Result<B256, Error> {
        let call = self.contract.withdraw(token, U256::from(amount), to);
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(operation = "open", chain = "evm"))]
    pub async fn open(&self, terms: &Terms) -> Result<B256, Error> {
        if self.account != Some(terms.maker.into()) {
            return Err(Error::Config(
                "the terms name another account as the maker".into(),
            ));
        }
        let call = self.contract.open(
            terms.token.into(),
            terms.amount,
            share_words(&terms.maker_share),
            share_words(&terms.user_share),
            terms.user.into(),
            terms.t0,
            terms.t1,
            terms.payout_note.into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    pub async fn reverse_funding(&self, id: B256) -> Result<Option<ReverseFunding>, Error> {
        let funding = self
            .contract
            .reverseFunding(id)
            .call()
            .await
            .map_err(Error::contract)?;
        Ok((!funding.refundNote.is_zero()).then_some(ReverseFunding {
            refund_note: funding.refundNote,
            block_number: funding.blockNumber,
        }))
    }

    pub async fn confirmed_reverse_funding(
        &self,
        id: B256,
        confirmations: u64,
    ) -> Result<Option<ReverseFunding>, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        let latest = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let Some(height) = latest.checked_sub(confirmations - 1) else {
            return Ok(None);
        };
        let funding = self
            .contract
            .reverseFunding(id)
            .block(height.into())
            .call()
            .await
            .map_err(Error::contract)?;
        Ok((!funding.refundNote.is_zero()).then_some(ReverseFunding {
            refund_note: funding.refundNote,
            block_number: funding.blockNumber,
        }))
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "ready_with_sig", chain = "evm"))]
    pub async fn ready_with_sig(
        &self,
        id: B256,
        terms: &Terms,
        deadline: u64,
        signature: &[u8; 65],
        margin: u64,
    ) -> Result<B256, Error> {
        let call =
            self.contract
                .readyWithSig(id, terms_words(terms), deadline, signature.to_vec().into());
        let check = Box::new(Check::Ready {
            id,
            deadline,
            t0: terms.t0,
            margin,
        });
        Ok(self
            .submit_held(call, Some(check), 0)
            .await?
            .transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "refund_payout", chain = "evm"))]
    pub async fn refund_payout(
        &self,
        id: B256,
        terms: &Terms,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.refundPayout(
            id,
            terms_words(terms),
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "ready", chain = "evm"))]
    pub async fn ready(&self, id: B256, terms: &Terms) -> Result<B256, Error> {
        let call = self.contract.ready(id, terms_words(terms));
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "payout", chain = "evm"))]
    pub async fn payout(
        &self,
        id: B256,
        terms: &Terms,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
    ) -> Result<B256, Error> {
        let call = self.contract.payout(
            id,
            terms_words(terms),
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "rescue", chain = "evm"))]
    pub async fn rescue(
        &self,
        id: B256,
        terms: &Terms,
        note: &ShieldNote,
        fee: u128,
        signature: &[u8; 65],
        authorization: zecswap_core::RescueAuthorization,
    ) -> Result<B256, Error> {
        let call = self.contract.rescue(
            id,
            terms_words(terms),
            note.npk.into(),
            ciphertext_words(&note.ciphertext),
            fee,
            authorization.nonce,
            authorization.deadline,
            signature.to_vec().into(),
        );
        Ok(self.submit(call).await?.transaction_hash)
    }

    pub async fn rescue_nonce(&self, id: B256) -> Result<u64, Error> {
        self.contract
            .rescueNonces(id)
            .call()
            .await
            .map_err(Error::contract)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "claim", chain = "evm"))]
    pub async fn claim(
        &self,
        id: B256,
        terms: &Terms,
        user_secret: &SecretShare,
    ) -> Result<B256, Error> {
        let secret = U256::from_be_bytes(user_secret.to_be_bytes());
        let call = self.contract.claim(id, terms_words(terms), secret);
        Ok(self
            .reveal(call, id, terms, Reveal::Claim)
            .await?
            .transaction_hash)
    }

    #[tracing::instrument(skip_all, fields(swap_id = %id, operation = "refund", chain = "evm"))]
    pub async fn refund(
        &self,
        id: B256,
        terms: &Terms,
        maker_secret: &SecretShare,
    ) -> Result<B256, Error> {
        let secret = U256::from_be_bytes(maker_secret.to_be_bytes());
        let call = self.contract.refund(id, terms_words(terms), secret);
        Ok(self
            .reveal(call, id, terms, Reveal::Refund)
            .await?
            .transaction_hash)
    }

    pub async fn add_inventory(&self, token: Address, amount: u128) -> Result<(), Error> {
        let erc20 = IErc20::new(token, &self.provider);
        self.submit(erc20.approve(self.contract(), U256::from(amount)))
            .await?;
        self.submit(self.contract.deposit(token, U256::from(amount)))
            .await?;
        Ok(())
    }

    pub async fn token_balance(&self, token: Address, owner: Address) -> Result<u128, Error> {
        let balance = IErc20::new(token, &self.provider)
            .balanceOf(owner)
            .call()
            .await
            .map_err(Error::contract)?;
        u128::try_from(balance).map_err(Error::contract)
    }

    pub async fn mint_test_token(
        &self,
        token: Address,
        to: Address,
        amount: u128,
    ) -> Result<(), Error> {
        self.submit(IErc20::new(token, &self.provider).mint(to, U256::from(amount)))
            .await?;
        Ok(())
    }

    pub async fn gas_price(&self) -> Result<u128, Error> {
        self.provider.get_gas_price().await.map_err(Error::contract)
    }

    pub async fn max_fee_per_gas(&self) -> Result<u128, Error> {
        Ok(fees(&self.provider).await?.max_fee_per_gas)
    }

    pub async fn reveal_need(&self) -> Result<u128, Error> {
        let cap = self.max_fee_per_gas().await?;
        Ok(u128::from(REVEAL_GAS).saturating_mul(cap.saturating_add(self.reveal_headroom)))
    }

    pub fn set_reveal_headroom(&mut self, wei: u128) {
        self.reveal_headroom = wei;
    }

    pub fn reveal_headroom(&self) -> u128 {
        self.reveal_headroom
    }

    pub async fn eth_balance(&self, owner: Address) -> Result<U256, Error> {
        self.provider
            .get_balance(owner)
            .await
            .map_err(Error::contract)
    }

    pub async fn sent(&self) {
        self.sending.ended.notified().await;
    }

    pub async fn send_eth(&self, to: Address, wei: U256) -> Result<B256, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let _sending = self.sending.turn().await;
        self.transfer(account, to, wei).await
    }

    pub async fn send_eth_above(&self, to: Address, keep: U256) -> Result<Option<U256>, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let _sending = self.sending.turn().await;
        let count = || self.provider.get_transaction_count(account);
        let (mined, sent) =
            tokio::try_join!(count().latest(), count().pending()).map_err(Error::contract)?;
        if sent != mined {
            return Ok(None);
        }
        let balance = self.eth_balance(account).await?;
        let Some(wei) = balance.checked_sub(keep).filter(|wei| !wei.is_zero()) else {
            return Ok(None);
        };
        self.transfer(account, to, wei).await?;
        Ok(Some(wei))
    }

    async fn transfer(&self, account: Address, to: Address, wei: U256) -> Result<B256, Error> {
        let tx = TransactionRequest::default()
            .with_from(account)
            .with_to(to)
            .with_value(wei);
        let pending = send_at_fees(&self.provider, tx, 0).await?;
        Ok(confirmed(pending).await?.transaction_hash)
    }

    async fn submit<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
    ) -> Result<TransactionReceipt, Error> {
        self.submit_held(call, None, 0).await
    }

    async fn reveal<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
        id: B256,
        terms: &Terms,
        reveal: Reveal,
    ) -> Result<TransactionReceipt, Error> {
        let check = Box::new(Check::Reveal {
            id,
            terms: Box::new(terms.clone()),
            reveal,
            margin: self.reveal_margin,
        });
        self.submit_held(call, Some(check), self.reveal_headroom)
            .await
    }

    async fn submit_held<P: Provider, D: CallDecoder>(
        &self,
        call: CallBuilder<P, D>,
        check: Option<Box<Check>>,
        headroom: u128,
    ) -> Result<TransactionReceipt, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        let call = call.from(account);
        let mut attempts = 1;
        let mut unclear = None;
        let ended = |unclear: Option<B256>, error: Error| match unclear {
            Some(transaction) if error.nothing_sent() => Error::Unconfirmed(transaction),
            _ => error,
        };
        loop {
            let simulated = async {
                let simulation = self.provider.estimate_gas(call.as_ref().clone()).await;
                simulation.map(|_| ()).map_err(unsimulated)
            };
            let judged = match check.as_deref() {
                Some(check) => match self.check(check).await {
                    Ok(()) if check.reveals() && self.private.is_some() => Ok(()),
                    Ok(()) => simulated.await,
                    refused => refused,
                },
                None => simulated.await,
            };
            let failed = match judged {
                Ok(()) => {
                    let _sending = self.sending.turn().await;
                    let attempt = self.attempt(call.as_ref().clone(), check.as_deref(), headroom);
                    match Box::pin(attempt).await {
                        Ok(landed) => return landed,
                        Err(late @ Error::Late(_)) => return Err(ended(unclear, late)),
                        Err(Error::Unconfirmed(transaction)) => {
                            unclear = Some(transaction);
                            Error::Unconfirmed(transaction)
                        }
                        Err(e) => unsent(e),
                    }
                }
                Err(late @ Error::Late(_)) => return Err(ended(unclear, late)),
                Err(e) => unsent(e),
            };
            if attempts == SEND_ATTEMPTS {
                warn!(attempt = attempts, outcome = "send_failed", error = %failed, "transaction submission failed");
                return Err(ended(unclear, failed));
            }
            warn!(attempt = attempts, max_attempts = SEND_ATTEMPTS, outcome = "retrying", error = %failed, "RPC rejected transaction submission");
            attempts += 1;
            tokio::time::sleep(SEND_RETRY_DELAY).await;
        }
    }

    async fn attempt(
        &self,
        tx: TransactionRequest,
        check: Option<&Check>,
        headroom: u128,
    ) -> Result<Result<TransactionReceipt, Error>, Error> {
        if let Some(check) = check {
            self.check(check).await?;
        }
        Ok(match &self.private {
            None => match self.send_publicly(tx, headroom).await {
                Ok(sent) => self.landed_publicly(sent, headroom).await,
                Err(e) => Err(unsent(e)),
            },
            Some(private) => {
                let gas = check.filter(|check| check.reveals()).map(|_| REVEAL_GAS);
                let (signed, until, above) =
                    self.send_privately(private, tx, headroom, gas).await?;
                self.landed_privately(private, &signed, until, above).await
            }
        })
    }

    async fn check(&self, check: &Check) -> Result<(), Error> {
        match check {
            Check::Reveal {
                id,
                terms,
                reveal,
                margin,
            } => {
                let (window, head) = self.reveal_window(*id, terms, *reveal).await?;
                if !window.leaves(head.timestamp.max(wall_now()), *margin) {
                    warn!(swap_id = %id, reveal = reveal.name(), outcome = "late", "the reveal's window is shut or ends too soon to send it; nothing was sent");
                    return Err(Error::Late(reveal.name()));
                }
            }
            Check::Ready {
                id,
                deadline,
                t0,
                margin,
            } => {
                let lands_by = self.now().await?.max(wall_now()).saturating_add(*margin);
                if lands_by >= *t0 || lands_by > *deadline {
                    warn!(swap_id = %id, outcome = "late", "the ready's deadline or t0 is too near to send it; nothing was sent");
                    return Err(Error::Late("ready"));
                }
            }
        }
        Ok(())
    }

    async fn send_publicly(&self, tx: TransactionRequest, headroom: u128) -> Result<Sent, Error> {
        let account = self
            .account
            .ok_or_else(|| Error::Config("a read-only connection cannot send".into()))?;
        let gas = self
            .provider
            .estimate_gas(tx.clone())
            .await
            .map_err(unsimulated)?;
        let tx = tx.with_chain_id(self.chain_id().await?).with_gas_limit(gas);
        let now = fees(&self.provider).await?;
        let (cap, tip) = (now.max_fee_per_gas + headroom, now.max_priority_fee_per_gas);
        let priced_at = self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?;
        let first = self.sign_next(priced(&tx, cap, tip), 0).await?;
        match self.provider.send_raw_transaction(&first.raw).await {
            Ok(_) => {}
            Err(RpcError::ErrorResp(e)) if refused(&e) => return Err(Error::Unsent(e.to_string())),
            Err(_) => {
                warn!(transaction_hash = %first.hash, outcome = "unknown", "the node's answer was lost or unclear; the transaction may still land");
            }
        }
        info!(transaction_hash = %first.hash, outcome = "submitted", "transaction submitted; awaiting receipt");
        Ok(Sent {
            account,
            tx,
            nonce: first.nonce,
            cap,
            tip,
            priced_at,
            hashes: vec![(first.hash, cap)],
        })
    }

    async fn landed_publicly(
        &self,
        mut sent: Sent,
        headroom: u128,
    ) -> Result<TransactionReceipt, Error> {
        let started = Instant::now();
        let mut taken = 0;
        let (mut queued, mut told) = (None, None);
        loop {
            tokio::time::sleep(self.provider.client().poll_interval()).await;
            let counted = self
                .provider
                .get_transaction_count(sent.account)
                .latest()
                .await
                .ok();
            let mined = counted.unwrap_or_default();
            for (hash, cap) in &sent.hashes {
                if let Ok(Some(receipt)) = self.provider.get_transaction_receipt(*hash).await {
                    let transaction_hash = receipt.transaction_hash;
                    let unlanded = Unlanded {
                        hash: transaction_hash,
                        block: receipt.block_number.unwrap_or_default(),
                        max_fee_per_gas: *cap,
                    };
                    *self.landed.lock().unwrap() = Some((sent.nonce, unlanded));
                    if receipt.status() {
                        info!(%transaction_hash, block_number = receipt.block_number, outcome = "mined", "transaction mined successfully");
                        return Ok(receipt);
                    }
                    warn!(%transaction_hash, block_number = receipt.block_number, outcome = "reverted", "transaction reverted on chain");
                    return Err(Error::Reverted(transaction_hash));
                }
            }
            let last = sent.hashes.last().expect("one was sent").0;
            if mined > sent.nonce {
                taken += 1;
                if taken == NONCE_TAKEN_READS {
                    warn!(transaction_hash = %last, outcome = "dropped", "another transaction took the nonce");
                    return Err(Error::Dropped(last));
                }
                continue;
            }
            taken = 0;
            if started.elapsed() >= LAND_TIMEOUT {
                warn!(transaction_hash = %last, outcome = "unknown", "receipt unavailable; submission may still mine");
                return Err(Error::Unconfirmed(last));
            }
            let Ok(head) = self.provider.get_block_number().await else {
                continue;
            };
            match counted {
                Some(behind) if behind < sent.nonce => {
                    let held = queued.filter(|(most, _)| behind <= *most);
                    let (_, since) = *queued.insert(held.unwrap_or((behind, head)));
                    if told != Some(behind) && head >= since + QUEUED_BLOCKS {
                        told = Some(behind);
                        let landed = self.landed.lock().unwrap().clone();
                        let raised = Alert::SendQueued {
                            account: sent.account,
                            waiting: last,
                            nonce: sent.nonce,
                            behind,
                            blocks: head - since,
                            max_fee_per_gas: sent.cap,
                            unlanded: landed
                                .filter(|(nonce, _)| *nonce == behind)
                                .map(|(_, unlanded)| unlanded),
                        };
                        alert::raise(self.alerts.as_ref(), raised);
                    }
                }
                Some(_) => queued = None,
                None => {}
            }
            if head < sent.priced_at + REPRICE_BLOCKS {
                continue;
            }
            let Ok(now) = fees(&self.provider).await else {
                continue;
            };
            sent.cap = (now.max_fee_per_gas + headroom).max(bumped(sent.cap));
            sent.tip = now.max_priority_fee_per_gas.max(bumped(sent.tip));
            let repriced = priced(&sent.tx, sent.cap, sent.tip);
            let Ok(again) = self.sign_at(repriced, sent.nonce).await else {
                return Err(Error::Unconfirmed(last));
            };
            match self.provider.send_raw_transaction(&again.raw).await {
                Ok(_) => {
                    info!(transaction_hash = %again.hash, replaces = %last, max_fee_per_gas = sent.cap, outcome = "repriced", "transaction unmined; sent again at the fees now")
                }
                Err(e) => {
                    warn!(transaction_hash = %again.hash, outcome = "refused", "the node refused the repriced transaction: {e}")
                }
            }
            sent.hashes.push((again.hash, sent.cap));
            sent.priced_at = head;
        }
    }

    async fn send_privately(
        &self,
        private: &Private,
        tx: TransactionRequest,
        headroom: u128,
        gas: Option<u64>,
    ) -> Result<(Signed, u64, bool), Error> {
        let unavailable = |what: &str| Error::Unsent(format!("{what} is unavailable"));
        let fees = fees(&self.provider).await?;
        let sent_at = self
            .provider
            .get_block_number()
            .await
            .map_err(|_| unavailable("the latest block"))?;
        let gas = match gas {
            Some(gas) => gas,
            None => {
                self.provider
                    .estimate_gas(tx.clone())
                    .await
                    .map_err(|e| match reverts(&e) {
                        true => Error::WouldRevert("the transaction fails in simulation".into()),
                        false => unavailable("the simulation"),
                    })?
            }
        };
        let tx = tx
            .with_chain_id(self.chain_id().await?)
            .with_gas_limit(gas)
            .with_max_fee_per_gas(fees.max_fee_per_gas + headroom)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        let least_nonce = private.least_nonce.load(Ordering::Relaxed);
        let counted = self.pending_nonce().await?;
        let signed = self.sign_at(tx, counted.max(least_nonce)).await?;
        let handed = Self::hand_over(private, &signed).await;
        self.tell_stuck(private, counted);
        handed?;
        Ok((signed, sent_at + private.window(), counted < least_nonce))
    }

    async fn hand_over(private: &Private, signed: &Signed) -> Result<(), Error> {
        let transaction_hash = signed.hash;
        match private.provider.send_raw_transaction(&signed.raw).await {
            Ok(_) => {
                info!(%transaction_hash, outcome = "submitted", "transaction sent privately; awaiting its block");
            }
            Err(RpcError::ErrorResp(e)) if refused(&e) => {
                return Err(Error::Unsent(
                    "the private RPC refused the transaction".into(),
                ));
            }
            Err(RpcError::ErrorResp(e)) if !known(&e) => {
                warn!(%transaction_hash, outcome = "unclear", "the private RPC answered the transaction with an error that does not say it was refused");
                return Err(Error::Unconfirmed(transaction_hash));
            }
            Err(_) => {
                warn!(%transaction_hash, outcome = "unknown", "the private RPC's answer was lost; the transaction may still land");
            }
        }
        Ok(())
    }

    fn tell_stuck(&self, private: &Private, chain: u64) {
        let signing = private.least_nonce.load(Ordering::Relaxed);
        let stuck = {
            let mut watch = private.watch.lock().unwrap();
            let dropped = watch.dropped.take().filter(|_| chain < signing);
            watch.raised.zip(dropped)
        };
        if let (Some(account), Some(((raised_by, raised_at), dropped))) = (self.account, stuck) {
            let raised = Alert::PrivateNonceStuck {
                account,
                signing,
                chain,
                window: private.window(),
                raised_by,
                raised_at,
                dropped,
            };
            alert::raise(self.alerts.as_ref(), raised);
        }
    }

    async fn landed_privately(
        &self,
        private: &Private,
        signed: &Signed,
        until: u64,
        above: bool,
    ) -> Result<TransactionReceipt, Error> {
        let transaction_hash = signed.hash;
        loop {
            let past = matches!(self.provider.get_block_number().await, Ok(head) if head >= until);
            if let Ok(Some(receipt)) = self
                .provider
                .get_transaction_receipt(transaction_hash)
                .await
            {
                let floor = signed.nonce + 1;
                let before = private.least_nonce.fetch_max(floor, Ordering::Relaxed);
                let mut watch = private.watch.lock().unwrap();
                if before < floor {
                    let block = receipt.block_number.unwrap_or_default();
                    watch.raised = Some((transaction_hash, block));
                }
                watch.dropped = None;
                drop(watch);
                if receipt.status() {
                    info!(%transaction_hash, block_number = receipt.block_number, outcome = "mined", "transaction mined successfully");
                    return Ok(receipt);
                }
                warn!(%transaction_hash, block_number = receipt.block_number, outcome = "reverted", "transaction reverted on chain");
                return Err(Error::Reverted(transaction_hash));
            }
            if past {
                warn!(%transaction_hash, outcome = "dropped", "the private RPC dropped the transaction unmined");
                if above {
                    private.watch.lock().unwrap().dropped = Some(transaction_hash);
                    if let Ok(counted) = self.pending_nonce().await {
                        self.tell_stuck(private, counted);
                    }
                }
                return Err(Error::Dropped(transaction_hash));
            }
            tokio::time::sleep(self.provider.client().poll_interval()).await;
        }
    }

    async fn pending_nonce(&self) -> Result<u64, Error> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        self.provider
            .get_transaction_count(account)
            .pending()
            .await
            .map_err(|_| Error::Contract("the account's nonce is unavailable".into()))
    }

    async fn sign_next(&self, tx: TransactionRequest, least_nonce: u64) -> Result<Signed, Error> {
        let nonce = self.pending_nonce().await?.max(least_nonce);
        self.sign_at(tx, nonce).await
    }

    async fn sign_at(&self, mut tx: TransactionRequest, nonce: u64) -> Result<Signed, Error> {
        let Some(wallet) = &self.wallet else {
            return Err(Error::Config("a read-only connection cannot send".into()));
        };
        tx.set_nonce(nonce);
        let envelope = tx
            .build(wallet)
            .await
            .map_err(|_| Error::Contract("cannot sign the transaction".into()))?;
        Ok(Signed {
            hash: *envelope.tx_hash(),
            nonce,
            raw: envelope.encoded_2718().into(),
        })
    }
}

pub fn check_keeps_secrets(rpc_url: &str) -> Result<(), Error> {
    let refuse = |why: &str| Err(Error::Config(format!("the private RPC {why}")));
    let url = private_url(rpc_url)?;
    protect_blocks(&url)?;
    if url.path() != "/" {
        return refuse(
            "must be Flashbots Protect's default endpoint, with no path: /fast shows TEE searchers whole transactions, failed ones too",
        );
    }
    let mut hints = Vec::new();
    for (name, value) in url.query_pairs() {
        match name.to_ascii_lowercase().as_str() {
            "hint" => hints.push(value),
            "blockrange" | "builder" | "refund" | "originid" | "auctiontimeout" => {}
            _ => {
                return refuse(
                    "URL sets a parameter other than hint, blockRange, builder, refund, originId and auctionTimeout",
                );
            }
        }
    }
    if hints.is_empty() || hints.iter().any(|hint| hint != "hash") {
        return refuse(
            "must share a transaction's hash alone, hint=hash: its default hints share logs too",
        );
    }
    Ok(())
}

fn private_url(rpc_url: &str) -> Result<reqwest::Url, Error> {
    reqwest::Url::parse(rpc_url)
        .map_err(|_| Error::Config("the private RPC's URL is unreadable".into()))
}

fn protect_blocks(url: &reqwest::Url) -> Result<u64, Error> {
    let mut ranges = url
        .query_pairs()
        .filter(|(name, _)| name.eq_ignore_ascii_case("blockRange"));
    match (ranges.next(), ranges.next()) {
        (None, _) => Ok(PROTECT_BLOCKS),
        (Some((_, blocks)), None) => blocks
            .parse()
            .ok()
            .filter(|blocks| *blocks > 0)
            .ok_or_else(|| Error::Config("the private RPC's blockRange is not positive".into())),
        (Some(_), Some(_)) => Err(Error::Config(
            "the private RPC's URL sets blockRange more than once".into(),
        )),
    }
}

pub async fn deploy(
    rpc_url: &str,
    signer: PrivateKeySigner,
    init_code: Vec<u8>,
) -> Result<Address, Error> {
    let provider = signing_provider(
        rpc_url,
        Some(EthereumWallet::from(signer)),
        &RpcMeter::default(),
    )?;
    let tx = TransactionRequest::default().with_deploy_code(init_code);
    confirmed(send_at_fees(&provider, tx, 0).await?)
        .await?
        .contract_address
        .ok_or_else(|| Error::Contract("deployment created no contract".into()))
}

async fn send_at_fees(
    provider: &DynProvider,
    tx: TransactionRequest,
    headroom: u128,
) -> Result<PendingTransactionBuilder<Ethereum>, Error> {
    let fees = fees(provider).await?;
    let tx = tx
        .with_max_fee_per_gas(fees.max_fee_per_gas + headroom)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    provider.send_transaction(tx).await.map_err(Error::contract)
}

async fn fees(provider: &DynProvider) -> Result<Eip1559Estimation, Error> {
    let history = provider
        .get_fee_history(1, BlockNumberOrTag::Latest, &[])
        .await
        .map_err(Error::contract)?;
    let base_fee = history
        .next_block_base_fee()
        .ok_or_else(|| Error::Contract("the node reports no base fee".into()))?;
    let tip = provider
        .get_max_priority_fee_per_gas()
        .await
        .map_err(Error::contract)?;
    Ok(eip1559(base_fee, tip))
}

const MIN_TIP_WEI: u128 = 20_000_000;

fn bumped(fee: u128) -> u128 {
    fee + fee / 8 + 1
}

fn priced(tx: &TransactionRequest, cap: u128, tip: u128) -> TransactionRequest {
    tx.clone()
        .with_max_fee_per_gas(cap)
        .with_max_priority_fee_per_gas(tip)
}

fn reverts(error: &TransportError) -> bool {
    error.as_error_resp().is_some_and(|refusal| {
        refusal.code == 3 || refusal.message.to_ascii_lowercase().contains("revert")
    })
}

fn known(refusal: &ErrorPayload) -> bool {
    let said = refusal.message.to_ascii_lowercase();
    [
        "already known",
        "alreadyknown",
        "already imported",
        "known transaction",
    ]
    .iter()
    .any(|held| said.contains(held))
}

fn refused(refusal: &ErrorPayload) -> bool {
    let said = refusal.message.to_ascii_lowercase();
    refusal.code == 429
        || [
            "nonce too low",
            "underpriced",
            "insufficient funds",
            "intrinsic gas too low",
            "exceeds block gas limit",
            "less than block base fee",
            "higher than max fee per gas",
            "exceeds the configured cap",
            "invalid sender",
            "invalid signature",
            "invalid chain id",
            "rate limit",
            "too many requests",
            "capacity",
        ]
        .iter()
        .any(|refusing| said.contains(refusing))
}

fn unsimulated(error: TransportError) -> Error {
    match reverts(&error) {
        true => Error::WouldRevert(error.to_string()),
        false => Error::Unsent(error.to_string()),
    }
}

fn unsent(error: Error) -> Error {
    match error {
        Error::Contract(why) => Error::Unsent(why),
        other => other,
    }
}

fn eip1559(base_fee: u128, suggested_tip: u128) -> Eip1559Estimation {
    let tip = suggested_tip.max(MIN_TIP_WEI);
    Eip1559Estimation {
        max_fee_per_gas: base_fee * EIP1559_BASE_FEE_MULTIPLIER + tip,
        max_priority_fee_per_gas: tip,
    }
}

fn signing_provider(
    rpc_url: &str,
    wallet: Option<EthereumWallet>,
    meter: &RpcMeter,
) -> Result<DynProvider, Error> {
    let url = rpc_url
        .parse()
        .map_err(|e| Error::Config(format!("RPC URL {rpc_url}: {e}")))?;
    let http = reqwest::Client::builder()
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(Error::contract)?;
    let client = ClientBuilder::default()
        .layer(meter.layer())
        .http_with_client(http, url);
    Ok(provider_over(client, wallet))
}

fn provider_over(
    client: alloy::rpc::client::RpcClient,
    wallet: Option<EthereumWallet>,
) -> DynProvider {
    let Some(wallet) = wallet else {
        return ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_client(client)
            .erased();
    };
    ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .fetch_chain_id()
        .wallet(wallet)
        .connect_client(client)
        .erased()
}

async fn confirmed(
    pending: PendingTransactionBuilder<Ethereum>,
) -> Result<TransactionReceipt, Error> {
    let transaction_hash = *pending.tx_hash();
    info!(%transaction_hash, outcome = "submitted", "transaction submitted; awaiting receipt");
    let receipt = pending
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .map_err(|error| {
            warn!(%transaction_hash, outcome = "unknown", error = %error, "receipt unavailable; submission may still mine");
            Error::Unconfirmed(transaction_hash)
        })?;
    if receipt.status() {
        info!(%transaction_hash, block_number = receipt.block_number, outcome = "mined", "transaction mined successfully");
        Ok(receipt)
    } else {
        warn!(%transaction_hash, block_number = receipt.block_number, outcome = "reverted", "transaction reverted on chain");
        Err(Error::Reverted(receipt.transaction_hash))
    }
}

fn ciphertext_words(ciphertext: &ShieldCiphertext) -> ShieldCiphertextWords {
    ShieldCiphertextWords {
        encryptedBundle: ciphertext.encrypted_bundle.map(B256::from),
        shieldKey: ciphertext.shield_key.into(),
    }
}

fn terms_words(terms: &Terms) -> TermsWords {
    TermsWords {
        maker: terms.maker.into(),
        token: terms.token.into(),
        amount: terms.amount,
        makerKey: share_words(&terms.maker_share),
        userKey: share_words(&terms.user_share),
        user: terms.user.into(),
        t0: terms.t0,
        t1: terms.t1,
        payoutNote: terms.payout_note.into(),
    }
}

fn share_words(share: &PublicShare) -> [U256; 2] {
    let bytes = share.to_affine_bytes();
    [
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..]),
    ]
}

fn share_from_words(x: U256, y: U256) -> Result<PublicShare, Error> {
    let mut bytes = [0; 64];
    bytes[..32].copy_from_slice(&x.to_be_bytes::<32>());
    bytes[32..].copy_from_slice(&y.to_be_bytes::<32>());
    Ok(PublicShare::from_affine_bytes(&bytes)?)
}

fn decode_state(swap: IZecSwap::Swap) -> Result<Option<SwapState>, Error> {
    let stage = match swap.stage {
        0 => return Ok(None),
        1 => Stage::Open,
        2 => Stage::Ready,
        3 => Stage::Claimed,
        4 => Stage::Refunded,
        other => return Err(Error::Contract(format!("unknown stage {other}"))),
    };
    Ok(Some(SwapState {
        stage,
        terms_hash: swap.termsHash,
        secret: swap.secret.to_be_bytes(),
        paid_out: swap.paidOut,
    }))
}

pub fn on_chain(
    id: B256,
    state: Option<SwapState>,
    terms: &Terms,
) -> Result<Option<OnChainSwap>, Error> {
    let Some(state) = state else {
        return Ok(None);
    };
    if state.terms_hash != terms.hash() {
        return Err(Error::WrongTerms(id));
    }
    Ok(Some(OnChainSwap {
        stage: state.stage,
        maker: terms.maker.into(),
        user: terms.user.into(),
        token: terms.token.into(),
        amount: terms.amount,
        t0: terms.t0,
        t1: terms.t1,
        maker_share: terms.maker_share,
        user_share: terms.user_share,
        secret: state.secret,
        payout_note: (terms.payout_note != [0; 32]).then(|| terms.payout_note.into()),
        paid_out: state.paid_out,
    }))
}
