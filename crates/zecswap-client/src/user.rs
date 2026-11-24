use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use serde::{Deserialize, Serialize};
use zecswap_api::relayer::{self, Claim, Sent};
use zecswap_api::{Acceptance, Network, OpenFee, Quote};
use zecswap_chain::evm::judge::{self, Go, READY_DEPTH, Refusal};
use zecswap_chain::evm::{
    BlockId, OnChainSwap, Reveal, Settlement, Stage, Window, swap_id, wall_now,
};
use zecswap_core::{
    Domain, JointAccount, NetworkType, Payout, SpendKey, SwapContext, Terms, UserSwapKeys,
    derive_user_keys, orchard_only,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};
use zeroize::Zeroizing;

use crate::api::{MakerApi, RelayerApi, refused};

pub const MIN_TIME_TO_T0: u64 = 25 * 60;
pub const MAX_TIME_TO_T0: u64 = 2 * 60 * 60;
pub const CLAIM_MARGIN: u64 = 5 * 60;
const DEPTH_WAIT: Duration = Duration::from_secs(10 * 60);
const CATCH_UP: Duration = Duration::from_secs(60);
const RELAY_RETRY: Duration = Duration::from_secs(15);
const RELAY_PATIENCE: Duration = Duration::from_secs(30 * 60);
pub(crate) const RAILGUN_WALLET: u32 = 0;
pub const OPEN_FEE_MARGIN: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Quoted {
    pub index: u32,
    pub quote: Quote,
}

impl Quoted {
    pub fn fee_due(&self, now: u64) -> Result<Option<&OpenFee>> {
        let Some(fee) = &self.quote.open_fee else {
            return Ok(None);
        };
        let left = self.quote.expires_at.saturating_sub(now);
        ensure!(
            left >= OPEN_FEE_MARGIN,
            "the quote expires in {left}s, too soon for its open fee to be mined first"
        );
        Ok(Some(fee))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSwap {
    pub index: u32,
    pub quote: Quote,
    pub swap_id: B256,
    pub t0: u64,
    pub t1: u64,
    #[serde(default)]
    pub ticket: Option<String>,
}

pub enum Route {
    Account,
    Railgun { relayer: RelayerApi, max_fee: u128 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paid {
    pub amount: u128,
    pub tx: Option<B256>,
}

pub struct User {
    seed: Zeroizing<Vec<u8>>,
    network: Network,
    settlement: Settlement,
    maker: MakerApi,
    token: Address,
    route: Route,
    railgun: RailgunKeys,
    min_time_to_t0: u64,
    max_open_fee: u64,
}

impl User {
    pub fn new(
        seed: &[u8],
        railgun_seed: &[u8],
        network: Network,
        mut settlement: Settlement,
        maker: MakerApi,
        token: Address,
        route: Route,
    ) -> Self {
        settlement.reveal_with_margin(CLAIM_MARGIN);
        Self {
            seed: Zeroizing::new(seed.to_vec()),
            railgun: RailgunKeys::from_seed(railgun_seed, RAILGUN_WALLET),
            network,
            settlement,
            maker,
            token,
            route,
            min_time_to_t0: MIN_TIME_TO_T0,
            max_open_fee: 0,
        }
    }

    pub fn with_max_open_fee(mut self, zatoshis: u64) -> Self {
        self.max_open_fee = zatoshis;
        self
    }

    pub fn with_min_time_to_t0(mut self, seconds: u64) -> Self {
        self.min_time_to_t0 = seconds;
        self
    }

    pub fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    pub fn railgun(&self) -> &RailgunKeys {
        &self.railgun
    }

    pub async fn quote(&self, index: u32, units: u32) -> Result<Quoted> {
        let keys = self.keys(index)?;
        let payout = self.payout(&keys)?;
        let quote = self
            .maker
            .quote(units, payout.user.into(), payout.note.map(B256::from))
            .await?;
        let chain_id = self.settlement.chain_id().await?;
        let railgun = self.settlement.railgun().await?;
        self.network.check(chain_id, railgun, self.token)?;
        ensure!(quote.chain_id == chain_id, "the quote is for another chain");
        ensure!(
            quote.contract == self.settlement.contract() && quote.token == self.token,
            "the quote is for another contract or token"
        );
        self.context(chain_id, quote.quote_id)
            .verify_maker(&quote.maker_share, &quote.maker_proof)
            .context("the maker's share proof")?;
        if let Some(fee) = &quote.open_fee {
            check_open_fee(fee, self.network.zcash_type(), self.max_open_fee)?;
        }
        Ok(Quoted { index, quote })
    }

    pub async fn accept(&self, quoted: &Quoted) -> Result<UserSwap> {
        let Quoted { index, quote } = quoted;
        let keys = self.keys(*index)?;
        let payout = self.payout(&keys)?;
        let acceptance = Acceptance {
            user_share: keys.share.public(),
            user_proof: self.context(quote.chain_id, quote.quote_id).prove_user(
                &quote.maker_share,
                &keys.share,
                &payout,
                UnwrapErr(SysRng),
            ),
            viewing_keys: keys.viewing,
            token_request: None,
        };
        let swap_id = swap_id(quote.maker, &acceptance.user_share);
        let accepted = self
            .maker
            .accept(
                quote.quote_id,
                quote.expires_at,
                quote.open_fee.is_some(),
                swap_id,
                &acceptance,
            )
            .await?;
        ensure!(
            accepted.swap_id == swap_id,
            "the maker reported another swap"
        );
        Ok(UserSwap {
            index: *index,
            quote: quote.clone(),
            swap_id,
            t0: accepted.t0,
            t1: accepted.t1,
            ticket: Some(accepted.ticket),
        })
    }

    pub async fn open(&self, index: u32, units: u32) -> Result<UserSwap> {
        let quoted = self.quote(index, units).await?;
        ensure!(
            quoted.quote.open_fee.is_none(),
            "the maker asks an open fee: pay it between `quote` and `accept`"
        );
        self.accept(&quoted).await
    }

    pub async fn collect_token(&self, swap: &UserSwap) -> Result<bool> {
        self.maker.collect_token(swap.swap_id).await
    }

    pub async fn verify(&self, swap: &UserSwap) -> Result<JointAccount> {
        self.settlement
            .windows()
            .await
            .context("the swap's contract is not the windows contract")?;
        let chain = self.caught_up(swap, |_| true).await?;
        let keys = self.keys(swap.index)?;
        ensure!(chain.stage == Stage::Open, "the swap is {:?}", chain.stage);
        let now = self.settlement.now().await?;
        check_deadlines(now, self.min_time_to_t0, chain.t0, chain.t1)?;
        Ok(JointAccount::derive(
            &chain.maker_share,
            &chain.user_share,
            &keys.viewing,
        )?)
    }

    pub async fn state(&self, swap: &UserSwap) -> Result<OnChainSwap> {
        self.settlement
            .swap(swap.swap_id, &self.terms(swap)?)
            .await?
            .context("the swap is not on-chain")
    }

    pub fn terms(&self, swap: &UserSwap) -> Result<Terms> {
        let keys = self.keys(swap.index)?;
        let payout = self.payout(&keys)?;
        Ok(Terms {
            maker: swap.quote.maker.into(),
            token: self.token.into(),
            amount: swap.quote.amount,
            maker_share: swap.quote.maker_share,
            user_share: keys.share.public(),
            user: payout.user,
            t0: swap.t0,
            t1: swap.t1,
            payout_note: payout.note.unwrap_or_default(),
        })
    }

    pub async fn claim(&self, swap: &UserSwap) -> Result<Paid> {
        match &self.route {
            Route::Account => self.claim_to_account(swap).await,
            Route::Railgun { relayer, max_fee } => {
                self.claim_into_railgun(swap, relayer, *max_fee).await
            }
        }
    }

    pub async fn refund_key(&self, swap: &UserSwap) -> Result<SpendKey> {
        let chain = self.state(swap).await?;
        ensure!(
            chain.stage == Stage::Refunded,
            "the swap is {:?}",
            chain.stage
        );
        let e = chain
            .revealed()?
            .context("a refunded swap stores the maker share")?;
        let keys = self.keys(swap.index)?;
        let joint = JointAccount::derive(&chain.maker_share, &chain.user_share, &keys.viewing)?;
        Ok(joint.spend_key(&e, &keys.share)?)
    }

    async fn claim_to_account(&self, swap: &UserSwap) -> Result<Paid> {
        let chain = self.state(swap).await?;
        let amount = match chain.stage {
            Stage::Refunded => bail!("the maker refunded the swap"),
            Stage::Claimed => self.settlement.balance_of(chain.user, chain.token).await?,
            Stage::Open | Stage::Ready => {
                self.claim_window(swap).await?;
                let keys = self.keys(swap.index)?;
                self.settlement
                    .claim(swap.swap_id, &self.terms(swap)?, &keys.share)
                    .await?;
                chain.amount
            }
        };
        if amount == 0 {
            return Ok(Paid { amount, tx: None });
        }
        let tx = self
            .settlement
            .withdraw(chain.token, amount, chain.user)
            .await?;
        Ok(Paid {
            amount,
            tx: Some(tx),
        })
    }

    async fn claim_into_railgun(
        &self,
        swap: &UserSwap,
        relayer: &RelayerApi,
        max_fee: u128,
    ) -> Result<Paid> {
        let chain = self.state(swap).await?;
        let terms = relayer.terms().await?;
        let windows = self.settlement.windows().await?;
        ensure!(
            terms.chain_id == swap.quote.chain_id
                && terms.contract == swap.quote.contract
                && terms.fits_windows((windows.min, windows.late)),
            "the relayer serves another deployment"
        );
        ensure!(
            terms.fee <= max_fee && terms.fee < chain.amount,
            "the relayer asks a fee of {}",
            terms.fee
        );
        let keys = self.keys(swap.index)?;
        let note = self.railgun.note(&keys.note_entropy)?;
        let digest = self
            .domain(swap)
            .payout(&swap.swap_id.0, &terms.relayer.into(), terms.fee);
        let swap_terms = zecswap_api::Terms::from(&self.terms(swap)?);
        let payout = relayer::Payout {
            swap_id: swap.swap_id,
            terms: swap_terms.clone(),
            note: (&note).into(),
            fee: terms.fee,
            signature: keys.auth.sign(&digest).into(),
        };

        let ticket = swap.ticket.as_deref();
        let mut paid = None;
        match chain.stage {
            Stage::Refunded => bail!("the maker refunded the swap"),
            Stage::Claimed if chain.paid_out => {
                return Ok(Paid {
                    amount: 0,
                    tx: None,
                });
            }
            Stage::Claimed => {}
            Stage::Open | Stage::Ready => {
                ensure!(
                    self.settlement.railgun_accepts(chain.token).await?,
                    "Railgun is not taking the payout now; the share stays secret"
                );
                let window = self.claim_window(swap).await?;
                let claim = Claim {
                    swap_id: swap.swap_id,
                    terms: swap_terms,
                    secret: keys.share.to_be_bytes().into(),
                    payout: payout.clone(),
                };
                let claimed = |chain: &OnChainSwap| chain.stage == Stage::Claimed;
                let until = match window {
                    Window::Until(end) => end.saturating_sub(CLAIM_MARGIN),
                    _ => u64::MAX,
                };
                let sent = self
                    .relayed(swap, || relayer.claim(&claim, ticket), claimed, until)
                    .await?;
                self.caught_up(swap, claimed).await?;
                paid = sent
                    .filter(|sent| sent.transactions.len() == 2)
                    .and_then(|sent| last(&sent));
            }
        }
        if paid.is_none() {
            let paid_out = |chain: &OnChainSwap| chain.paid_out;
            let sent = self
                .relayed(swap, || relayer.payout(&payout, ticket), paid_out, u64::MAX)
                .await?;
            paid = sent.and_then(|sent| last(&sent));
        }
        self.caught_up(swap, |chain| chain.paid_out).await?;
        Ok(Paid {
            amount: chain.amount - terms.fee,
            tx: paid,
        })
    }

    async fn relayed<F: Future<Output = Result<Sent>>>(
        &self,
        swap: &UserSwap,
        post: impl Fn() -> F,
        landed: impl Fn(&OnChainSwap) -> bool,
        until: u64,
    ) -> Result<Option<Sent>> {
        let terms = self.terms(swap)?;
        let give_up = Instant::now() + RELAY_PATIENCE;
        loop {
            let failure = match post().await {
                Ok(sent) => return Ok(Some(sent)),
                Err(e) if refused(&e) => return Err(e),
                Err(e) => e,
            };
            let watched = Instant::now() + RELAY_RETRY;
            loop {
                if let Some(chain) = self.settlement.swap(swap.swap_id, &terms).await?
                    && landed(&chain)
                {
                    return Ok(None);
                }
                if Instant::now() >= watched {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            if self.settlement.now().await? >= until || Instant::now() >= give_up {
                return Err(failure.context("the relayer's request never showed on the chain"));
            }
        }
    }

    pub fn payout_note(&self, index: u32) -> Result<ShieldNote> {
        Ok(self.railgun.note(&self.keys(index)?.note_entropy)?)
    }

    fn payout(&self, keys: &UserSwapKeys) -> Result<Payout> {
        Ok(match self.route {
            Route::Account => Payout {
                user: self
                    .settlement
                    .account()
                    .context("paying an account needs a connection that sends as it")?
                    .into(),
                note: None,
            },
            Route::Railgun { .. } => Payout {
                user: keys.auth.address(),
                note: Some(self.railgun.note(&keys.note_entropy)?.commitment()),
            },
        })
    }

    async fn claim_window(&self, swap: &UserSwap) -> Result<Window> {
        let terms = self.terms(swap)?;
        let windows = self.settlement.windows().await?;
        let give_up = Instant::now() + DEPTH_WAIT;
        loop {
            let (chain, head) = self.settlement.read(swap.swap_id, &terms).await?;
            let chain = chain.context("the swap is not on-chain")?;
            let deadlines = (chain.t0, chain.t1);
            let window =
                windows.window(Reveal::Claim, false, chain.stage, deadlines, head.timestamp);
            match judge::forward_claim(
                windows,
                chain.stage,
                deadlines,
                &head,
                wall_now(),
                CLAIM_MARGIN,
            ) {
                Ok(Go::Now) => return Ok(window),
                Ok(Go::OnceReadyAt(block)) => {
                    let then = self
                        .settlement
                        .swap_at(swap.swap_id, &terms, BlockId::number(block))
                        .await?;
                    if then.is_some_and(|then| then.stage == Stage::Ready) {
                        return Ok(window);
                    }
                }
                Err(Refusal::MakersUntil(late)) => bail!(
                    "from t1 the swap is its maker's to refund, which gives the deposit back; it can be claimed again from {late}"
                ),
                Err(Refusal::TooSoon) => {
                    bail!("the claim's window ends too soon to claim in; the share stays secret")
                }
                Err(_) => bail!("the swap can be claimed once ready, or from t0"),
            }
            ensure!(
                Instant::now() < give_up,
                "the swap's ready never got {READY_DEPTH} blocks deep"
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn caught_up(
        &self,
        swap: &UserSwap,
        seen: impl Fn(&OnChainSwap) -> bool,
    ) -> Result<OnChainSwap> {
        let terms = self.terms(swap)?;
        let deadline = Instant::now() + CATCH_UP;
        loop {
            if let Some(chain) = self.settlement.swap(swap.swap_id, &terms).await?
                && seen(&chain)
            {
                return Ok(chain);
            }
            ensure!(
                Instant::now() < deadline,
                "our RPC node never showed the swap as expected"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    fn keys(&self, index: u32) -> Result<UserSwapKeys> {
        Ok(derive_user_keys(
            &self.seed,
            self.network.zcash_type(),
            0,
            index,
        )?)
    }

    fn domain(&self, swap: &UserSwap) -> Domain {
        Domain {
            chain_id: swap.quote.chain_id,
            contract: swap.quote.contract.into(),
        }
    }

    fn context(&self, chain_id: u64, quote_id: B256) -> SwapContext {
        SwapContext {
            chain_id,
            contract: self.settlement.contract().into(),
            quote_id: quote_id.0,
        }
    }
}

fn last(sent: &Sent) -> Option<B256> {
    sent.transactions.last().copied()
}

fn check_open_fee(fee: &OpenFee, network: NetworkType, max: u64) -> Result<()> {
    ensure!(fee.zat > 0, "the maker asks an open fee of nothing");
    ensure!(
        fee.zat <= max,
        "the maker asks an open fee of {} zat, over the {max} allowed",
        fee.zat
    );
    orchard_only(&fee.address, network)
        .map_err(|wrong| anyhow!("the open fee's address {wrong}"))?;
    Ok(())
}

fn check_deadlines(now: u64, min: u64, t0: u64, t1: u64) -> Result<()> {
    ensure!(
        (now + min..=now + MAX_TIME_TO_T0).contains(&t0),
        "t0 is {}s away, outside {min}..={MAX_TIME_TO_T0}s",
        t0.saturating_sub(now)
    );
    ensure!(
        t1 >= t0 + 2 * CLAIM_MARGIN,
        "t1 follows t0 by {}s, too soon to claim in should the maker never ready the swap",
        t1.saturating_sub(t0)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use alloy_primitives::address;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::http::header::RETRY_AFTER;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use zcash_address::unified::{self, Container, Encoding, Receiver, Revision, Uitem};
    use zecswap_api::Accepted;
    use zecswap_api::service::{ErrorCode, ErrorResponse};
    use zecswap_core::derive_maker_share;

    use super::*;
    use crate::api::{FeeNotHonoured, ServiceError};
    use crate::tokens::tests::serve;

    const SEED: [u8; 64] = [7; 64];
    const SEPOLIA: u64 = 11_155_111;
    const RAILGUN: Address = address!("eCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea");
    const MAKER: Address = address!("09eD1F966745Be18C711C346242c0974DAd7c3e5");
    const CONTRACT: Address = address!("1111111111111111111111111111111111111111");
    const TOKEN: Address = address!("3333333333333333333333333333333333333333");
    const QUOTE: B256 = B256::repeat_byte(5);
    const FEE: u64 = 40_000;

    struct Maker {
        fee: Option<OpenFee>,
        life: u64,
        busy: u32,
        unseen: u32,
        code: ErrorCode,
        accepts: AtomicU32,
    }

    impl Maker {
        fn asking(fee: Option<OpenFee>) -> Self {
            Self {
                fee,
                life: 600,
                busy: 0,
                unseen: 0,
                code: ErrorCode::FeeRequired,
                accepts: AtomicU32::new(0),
            }
        }

        fn accepts(&self) -> u32 {
            self.accepts.load(Ordering::SeqCst)
        }
    }

    async fn quote(State(maker): State<Arc<Maker>>) -> Json<Quote> {
        let context = SwapContext {
            chain_id: SEPOLIA,
            contract: CONTRACT.into(),
            quote_id: QUOTE.0,
        };
        let e = derive_maker_share(&[9; 32], 0).unwrap();
        Json(Quote {
            quote_id: QUOTE,
            maker: MAKER,
            maker_share: e.public(),
            maker_proof: context.prove_maker(&e, UnwrapErr(SysRng)),
            chain_id: SEPOLIA,
            contract: CONTRACT,
            token: TOKEN,
            amount: 1_000_000,
            deposit_zat: 100_000,
            expires_at: wall_now() + maker.life,
            network_cost: None,
            open_fee: maker.fee.clone(),
        })
    }

    async fn accept(
        State(maker): State<Arc<Maker>>,
        Json(acceptance): Json<Acceptance>,
    ) -> Response {
        let seen = maker.accepts.fetch_add(1, Ordering::SeqCst);
        if seen < maker.busy {
            let shed = ErrorResponse {
                code: ErrorCode::Unavailable,
                error: "busy".into(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(shed)).into_response();
        }
        if seen < maker.unseen {
            let refusal = ErrorResponse {
                code: maker.code,
                error: "the open fee is not paid".into(),
            };
            return (
                StatusCode::PAYMENT_REQUIRED,
                [(RETRY_AFTER, "1")],
                Json(refusal),
            )
                .into_response();
        }
        Json(Accepted {
            swap_id: swap_id(MAKER, &acceptance.user_share),
            t0: 1_790_003_600,
            t1: 1_790_007_200,
            late_window: Some(3_600),
            ticket: "ticket".into(),
        })
        .into_response()
    }

    async fn node(Json(request): Json<Value>) -> Json<Value> {
        let result = match request["method"].as_str() {
            Some("eth_chainId") => format!("{SEPOLIA:#x}"),
            Some("eth_call") => B256::left_padding_from(RAILGUN.as_slice()).to_string(),
            method => panic!("the user asked its node for {method:?}"),
        };
        Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
    }

    async fn user_of(maker: Maker, max_open_fee: Option<u64>) -> (Arc<Maker>, User) {
        let maker = Arc::new(maker);
        let app = Router::new()
            .route("/v1/quote", post(quote))
            .route("/v1/quote/{quote}/accept", post(accept))
            .route("/node", post(node))
            .with_state(maker.clone());
        let url = serve(app).await;
        let user = User::new(
            &SEED,
            &SEED,
            Network::Testnet,
            Settlement::read_only(&format!("{url}/node"), CONTRACT).unwrap(),
            MakerApi::new(url).unwrap(),
            TOKEN,
            Route::Railgun {
                relayer: RelayerApi::new("http://127.0.0.1:1").unwrap(),
                max_fee: 0,
            },
        );
        let user = match max_open_fee {
            Some(max) => user.with_max_open_fee(max),
            None => user,
        };
        (maker, user)
    }

    fn fee_address(network: NetworkType) -> String {
        let keys = derive_user_keys(&[1; 64], network, 0, 0).unwrap();
        let maker = derive_maker_share(&[9; 32], 1).unwrap().public();
        JointAccount::derive(&maker, &keys.share.public(), &keys.viewing)
            .unwrap()
            .unified_address(network)
    }

    fn fee(zat: u64) -> Option<OpenFee> {
        Some(OpenFee {
            zat,
            address: fee_address(NetworkType::Test),
        })
    }

    #[tokio::test]
    async fn a_fee_over_the_cap_or_payable_outside_orchard_is_refused_before_anything_is_paid() {
        let (network, _, orchard) =
            unified::Address::decode(&fee_address(NetworkType::Test)).unwrap();
        let receivers = orchard.items().into_iter().map(Uitem::Data);
        let beside_transparent = receivers
            .chain([Uitem::Data(Receiver::P2pkh([7; 20]))])
            .collect();
        let beside_transparent = unified::Address::try_from_items(Revision::R0, beside_transparent)
            .unwrap()
            .encode(&network);
        let at = |address: String| Some(OpenFee { zat: FEE, address });
        for (asked, max, wrong) in [
            (fee(FEE + 1), Some(FEE), "over the 40000 allowed"),
            (fee(FEE), None, "over the 0 allowed"),
            (fee(0), Some(FEE), "of nothing"),
            (
                at("tmEZhbWHTpdKMw5it8YDspUXSMGQyFwovpU".into()),
                Some(FEE),
                "must be a unified address",
            ),
            (at(beside_transparent), Some(FEE), "and no other"),
            (
                at(fee_address(NetworkType::Main)),
                Some(FEE),
                "is on another network",
            ),
        ] {
            let (maker, user) = user_of(Maker::asking(asked), max).await;
            let refused = user.quote(0, 1).await.unwrap_err().to_string();
            assert!(refused.contains(wrong), "{refused}");
            assert!(user.open(0, 1).await.is_err());
            assert_eq!(maker.accepts(), 0);
        }

        let (maker, user) = user_of(Maker::asking(fee(FEE)), Some(FEE)).await;
        let quoted = user.quote(0, 1).await.unwrap();
        assert_eq!(quoted.fee_due(wall_now()).unwrap(), fee(FEE).as_ref());
        let unpaid = user.open(0, 1).await.unwrap_err().to_string();
        assert!(unpaid.contains("pay it between"), "{unpaid}");
        assert_eq!(maker.accepts(), 0);
    }

    #[tokio::test]
    async fn an_accept_is_posted_again_until_the_maker_has_seen_the_fee() {
        let waiting = Maker {
            unseen: 2,
            ..Maker::asking(fee(FEE))
        };
        let (maker, user) = user_of(waiting, Some(FEE)).await;
        let quoted = user.quote(3, 1).await.unwrap();
        let started = Instant::now();
        let swap = user.accept(&quoted).await.unwrap();
        assert_eq!(maker.accepts(), 3);
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "it waited as asked"
        );
        let share = derive_user_keys(&SEED, NetworkType::Test, 0, 3)
            .unwrap()
            .share;
        assert_eq!(swap.swap_id, swap_id(MAKER, &share.public()));
        assert_eq!(
            (swap.index, swap.t0, swap.t1),
            (3, 1_790_003_600, 1_790_007_200)
        );

        let refusing = Maker {
            unseen: u32::MAX,
            code: ErrorCode::Rejected,
            ..Maker::asking(fee(FEE))
        };
        let (maker, user) = user_of(refusing, Some(FEE)).await;
        let refused = user.accept(&user.quote(0, 1).await.unwrap()).await;
        let refused = refused.unwrap_err();
        assert!(refused.downcast_ref::<ServiceError>().unwrap().refused());
        assert_eq!(maker.accepts(), 1);
    }

    #[tokio::test]
    async fn a_busy_answer_is_waited_out_once_the_fee_is_paid_and_reported_where_none_was() {
        let shedding = Maker {
            busy: 1,
            ..Maker::asking(fee(FEE))
        };
        let (maker, user) = user_of(shedding, Some(FEE)).await;
        user.accept(&user.quote(0, 1).await.unwrap()).await.unwrap();
        assert_eq!(maker.accepts(), 2);

        let shedding = Maker {
            busy: 1,
            ..Maker::asking(None)
        };
        let (maker, user) = user_of(shedding, None).await;
        let busy = user.open(0, 1).await.unwrap_err();
        let busy = busy.downcast_ref::<ServiceError>().unwrap();
        assert_eq!(busy.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(maker.accepts(), 1);
    }

    #[tokio::test]
    async fn an_accept_gives_up_when_the_quote_expires_with_its_fee_unseen() {
        let blind = Maker {
            life: 3,
            unseen: u32::MAX,
            ..Maker::asking(fee(FEE))
        };
        let (maker, user) = user_of(blind, Some(FEE)).await;
        let quoted = user.quote(0, 1).await.unwrap();
        let late = quoted.fee_due(wall_now()).unwrap_err().to_string();
        assert!(late.contains("too soon"), "{late}");

        let started = Instant::now();
        let lost = user.accept(&quoted).await.unwrap_err();
        assert!(lost.is::<FeeNotHonoured>(), "{lost:#}");
        assert!(started.elapsed() < Duration::from_secs(6));
        let asked = maker.accepts();
        assert!(asked >= 2, "it asked again while the quote lived");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(maker.accepts(), asked, "and never after");
    }

    #[tokio::test]
    async fn a_quote_that_asks_no_fee_opens_in_one_accept() {
        let (maker, user) = user_of(Maker::asking(None), None).await;
        let swap = user.open(0, 1).await.unwrap();
        assert_eq!(swap.quote.quote_id, QUOTE);
        assert_eq!(swap.ticket.as_deref(), Some("ticket"));
        assert_eq!(maker.accepts(), 1);
    }

    #[test]
    fn a_swap_leaves_its_user_time_to_claim_from_t0_without_a_ready() {
        let (now, min) = (1_000_000, 1_800);
        let t0 = now + 2_700;
        assert!(check_deadlines(now, min, t0, t0 + 3_600).is_ok());
        assert!(check_deadlines(now, min, t0, t0 + 2 * CLAIM_MARGIN).is_ok());
        let short = check_deadlines(now, min, t0, t0 + 2 * CLAIM_MARGIN - 1).unwrap_err();
        assert!(
            short.to_string().contains("too soon to claim in"),
            "{short}"
        );
        assert!(check_deadlines(now, min, now + min - 1, now + 2 * min).is_err());
        assert!(
            check_deadlines(now, min, now + MAX_TIME_TO_T0 + 1, now + 2 * MAX_TIME_TO_T0).is_err()
        );
    }
}
