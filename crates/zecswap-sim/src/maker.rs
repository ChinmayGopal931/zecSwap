use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use zecswap_api::service::{ErrorCode, MakerInfo};
use zecswap_api::{Acceptance, Accepted, Network, OpenFee, Quote, QuoteRequest, Status, reverse};
use zecswap_chain::evm::judge::{self, Go};
use zecswap_chain::evm::{BlockId, Settlement, Stage, reverse_swap_id, swap_id};
use zecswap_core::{
    JointAccount, NetworkType, Payout, SecretShare, Terms, derive_maker_share, derive_user_keys,
};

use crate::net::{Answer, Answering, Asked, Served};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, Chain, LATE_WINDOW, MIN_WINDOW, UNIT, UNIT_ZAT, wall};
use crate::zcash::{PaymentId, Sees, Zcash};

pub const QUOTE_LIFE: u64 = 10 * 60;
pub const TO_T0: u64 = 45 * 60;
pub const T0_TO_T1: u64 = 40 * 60;
pub const CONFIRMATIONS: u64 = 2;
const READY_BY: u64 = 4 * 60;
const CANCEL_BY: u64 = 8 * 60;
const REVEAL_MARGIN: u64 = 5 * 60;
const TEND: Duration = Duration::from_secs(9);
pub const FUNDING_LIFE: u64 = 10 * 60;
pub const REVERSE_TO_T0: u64 = 45 * 60;
pub const REVERSE_T0_TO_T1: u64 = 40 * 60;
pub const MAX_READY_WAIT: u64 = 24 * 60 * 60;
const FUNDING_CONFIRMATIONS: u64 = 3;
const DEPOSIT_BY: u64 = 15 * 60;
const SHORT_ZAT: u64 = UNIT_ZAT / 100;
const AT_THE_WIRE: u64 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Honest,
    NeverReadies,
    Vanishes,
    CancelsEarly,
    KeepsFee,
    OpensLess,
    OpensAnotherNote,
    OpensFar,
    LiesAboutDeadlines,
}

impl Mood {
    pub fn cheats_on_terms(self) -> bool {
        matches!(
            self,
            Mood::OpensLess | Mood::OpensAnotherNote | Mood::LiesAboutDeadlines
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fee {
    None,
    Asked(u64),
    Transparent(u64),
}

#[derive(Clone)]
struct Quoted {
    quote: Quote,
    nonce: u64,
    payout: Address,
    payout_note: Option<B256>,
    accepted: Option<Accepted>,
    opening: bool,
}

#[derive(Clone)]
pub struct Opened {
    pub id: B256,
    pub nonce: u64,
    pub opened: Terms,
    pub joint: String,
    pub deposit_zat: u64,
    pub cancel_at: Option<u64>,
    pub swept: u64,
    pub done: bool,
}

#[derive(Default)]
struct Book {
    quotes: BTreeMap<B256, Quoted>,
    swaps: BTreeMap<B256, Opened>,
    next: u64,
}

pub struct Maker {
    seed: Seed,
    pub mood: Mood,
    fee: Fee,
    chain: Chain,
    zcash: Arc<dyn Sees>,
    settlement: Settlement,
    pub address: Address,
    root: [u8; 32],
    pub fee_address: String,
    book: Mutex<Book>,
}

impl Maker {
    pub(crate) fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    pub fn new(
        seed: Seed,
        mood: Mood,
        fee: Fee,
        chain: Chain,
        zcash: Arc<dyn Sees>,
        mut settlement: Settlement,
    ) -> Arc<Self> {
        settlement.reveal_with_margin(REVEAL_MARGIN);
        let address = settlement.account().expect("the maker sends as an account");
        let keys = derive_user_keys(
            &seed.bytes::<64>("maker-fee-wallet"),
            NetworkType::Test,
            0,
            0,
        )
        .expect("fee wallet keys");
        let other = derive_maker_share(&seed.bytes("maker-fee-share"), 0).expect("a share");
        let fee_account =
            JointAccount::derive(&other.public(), &keys.share.public(), &keys.viewing)
                .expect("a fee account");
        zcash.watch(&fee_account);
        let fee_address = fee_account.unified_address(NetworkType::Test);
        Arc::new(Self {
            seed,
            mood,
            fee,
            chain,
            zcash,
            settlement,
            address,
            root: seed.bytes("maker-root"),
            fee_address,
            book: Mutex::default(),
        })
    }

    pub fn share(&self, nonce: u64) -> SecretShare {
        derive_maker_share(&self.root, nonce).expect("a maker share")
    }

    pub fn swaps(&self) -> Vec<Opened> {
        self.book.lock().unwrap().swaps.values().cloned().collect()
    }

    fn quote(&self, asked: &Asked) -> Result<Answer, Answer> {
        let request: QuoteRequest = asked.json()?;
        if request.units == 0 || request.units > 4 {
            return Err(Answer::refuse(400, ErrorCode::InvalidRequest, "units"));
        }
        let mut book = self.book.lock().unwrap();
        let nonce = book.next;
        book.next += 1;
        let e = self.share(nonce);
        let quote_id = B256::from(self.seed.bytes::<32>(&format!("quote-{nonce}")));
        let rng = ChaCha20Rng::from_seed(self.seed.bytes(&format!("quote-proof-{nonce}")));
        let open_fee = match self.fee {
            Fee::None => None,
            Fee::Asked(zat) => Some(OpenFee {
                zat,
                address: self.fee_address.clone(),
            }),
            Fee::Transparent(zat) => Some(OpenFee {
                zat,
                address: "tmEZhbWHTpdKMw5it8YDspUXSMGQyFwovpU".into(),
            }),
        };
        let quote = Quote {
            quote_id,
            maker: self.address,
            maker_share: e.public(),
            maker_proof: self.chain.context(quote_id).prove_maker(&e, rng),
            chain_id: CHAIN_ID,
            contract: self.chain.contract,
            token: self.chain.token,
            amount: u128::from(request.units) * UNIT,
            deposit_zat: u64::from(request.units) * UNIT_ZAT,
            expires_at: wall() + QUOTE_LIFE,
            network_cost: None,
            open_fee,
        };
        book.quotes.insert(
            quote_id,
            Quoted {
                quote: quote.clone(),
                nonce,
                payout: request.payout,
                payout_note: request.payout_note,
                accepted: None,
                opening: false,
            },
        );
        Ok(Answer::ok(&quote))
    }

    async fn accept(&self, quote_id: B256, asked: &Asked) -> Result<Answer, Answer> {
        let acceptance: Acceptance = asked.json()?;
        let quoted = {
            let mut book = self.book.lock().unwrap();
            let Some(quoted) = book.quotes.get_mut(&quote_id) else {
                return Err(Answer::refuse(
                    404,
                    ErrorCode::UnknownQuote,
                    "no such quote",
                ));
            };
            if let Some(accepted) = &quoted.accepted {
                return Ok(Answer::ok(accepted));
            }
            if quoted.opening {
                return Err(Answer::refuse(503, ErrorCode::Unavailable, "opening").after(5));
            }
            quoted.clone()
        };
        let quote = &quoted.quote;
        if wall() >= quote.expires_at {
            return Err(Answer::refuse(
                410,
                ErrorCode::UnknownQuote,
                "the quote expired",
            ));
        }
        let payout = Payout {
            user: quoted.payout.into(),
            note: quoted.payout_note.map(|note| note.0),
        };
        self.chain
            .context(quote_id)
            .verify_user(
                &quote.maker_share,
                &acceptance.user_share,
                &payout,
                &acceptance.user_proof,
            )
            .map_err(|e| Answer::refuse(400, ErrorCode::Rejected, e.to_string()))?;
        if let Some(fee) = &quote.open_fee {
            let paid = self.zcash.received(&fee.address, 1) >= fee.zat;
            if !paid || self.mood == Mood::KeepsFee {
                return Err(Answer::refuse(
                    402,
                    ErrorCode::FeeRequired,
                    "the open fee is not paid",
                )
                .after(4));
            }
        }
        self.book
            .lock()
            .unwrap()
            .quotes
            .get_mut(&quote_id)
            .expect("the quote")
            .opening = true;
        let opened = self.open(&quoted, &acceptance).await;
        let mut book = self.book.lock().unwrap();
        let quoted = book.quotes.get_mut(&quote_id).expect("the quote");
        quoted.opening = false;
        let (accepted, swap) = opened?;
        quoted.accepted = Some(accepted.clone());
        book.swaps.insert(swap.id, swap);
        Ok(Answer::ok(&accepted))
    }

    async fn open(
        &self,
        quoted: &Quoted,
        acceptance: &Acceptance,
    ) -> Result<(Accepted, Opened), Answer> {
        let busy = |why: String| Answer::refuse(503, ErrorCode::Unavailable, why).after(5);
        let quote = &quoted.quote;
        let now = self
            .settlement
            .now()
            .await
            .map_err(|e| busy(e.to_string()))?;
        let (t0, t1) = (now + TO_T0, now + TO_T0 + T0_TO_T1);
        let honest = Terms {
            maker: self.address.into(),
            token: quote.token.into(),
            amount: quote.amount,
            maker_share: quote.maker_share,
            user_share: acceptance.user_share,
            user: quoted.payout.into(),
            t0,
            t1,
            payout_note: quoted.payout_note.map(|note| note.0).unwrap_or_default(),
        };
        let mut opened = honest.clone();
        match self.mood {
            Mood::OpensLess => opened.amount -= 1,
            Mood::OpensAnotherNote => opened.payout_note = self.seed.bytes("another-note"),
            Mood::OpensFar => {
                opened.t0 = now + zecswap_client::MAX_TIME_TO_T0 + 5 * 60;
                opened.t1 = opened.t0 + T0_TO_T1;
            }
            Mood::LiesAboutDeadlines => opened.t0 = t0 - 10 * 60,
            _ => {}
        }
        let told = match self.mood {
            Mood::OpensFar => &opened,
            _ => &honest,
        };
        let accepted = Accepted {
            swap_id: swap_id(self.address, &acceptance.user_share),
            t0: told.t0,
            t1: told.t1,
            late_window: Some(LATE_WINDOW),
            ticket: format!("ticket-{}", quoted.nonce),
        };
        self.settlement
            .open(&opened)
            .await
            .map_err(|e| busy(e.to_string()))?;
        let joint = JointAccount::derive(
            &quote.maker_share,
            &acceptance.user_share,
            &acceptance.viewing_keys,
        )
        .map_err(|e| Answer::refuse(400, ErrorCode::Rejected, e.to_string()))?;
        self.zcash.watch(&joint);
        let joint = joint.unified_address(NetworkType::Test);
        let cancel_at = (self.mood == Mood::CancelsEarly).then(|| {
            now + self
                .seed
                .between("cancel-at", quoted.nonce, 60, TO_T0 - REVEAL_MARGIN - 60)
        });
        Ok((
            accepted.clone(),
            Opened {
                id: accepted.swap_id,
                nonce: quoted.nonce,
                opened,
                joint,
                deposit_zat: quote.deposit_zat,
                cancel_at,
                swept: 0,
                done: false,
            },
        ))
    }

    pub async fn tend(self: Arc<Self>) {
        loop {
            tokio::time::sleep(TEND).await;
            for swap in self.swaps() {
                if !swap.done {
                    self.tend_one(swap).await;
                }
            }
        }
    }

    async fn tend_one(&self, mut swap: Opened) {
        let Ok(Some(chain)) = self.settlement.swap(swap.id, &swap.opened).await else {
            return;
        };
        let Ok(now) = self.settlement.now().await else {
            return;
        };
        let e = self.share(swap.nonce);
        let (t0, t1) = (swap.opened.t0, swap.opened.t1);
        let deposited = self.zcash.received(&swap.joint, CONFIRMATIONS) >= swap.deposit_zat;
        let gone = self.mood == Mood::Vanishes;
        match chain.stage {
            Stage::Claimed => {
                if matches!(chain.revealed(), Ok(Some(_))) {
                    swap.swept = self.zcash.sweep(&swap.joint);
                }
                swap.done = true;
            }
            Stage::Refunded => swap.done = true,
            Stage::Open if now < t0 => {
                let cancelling = swap.cancel_at.is_some_and(|at| now >= at)
                    || (!deposited && now + CANCEL_BY >= t0);
                let readying = deposited
                    && now + READY_BY < t0
                    && !matches!(self.mood, Mood::NeverReadies | Mood::CancelsEarly);
                if gone {
                } else if cancelling {
                    let _ = self.settlement.refund(swap.id, &swap.opened, &e).await;
                } else if readying {
                    let _ = self.settlement.ready(swap.id, &swap.opened).await;
                }
            }
            Stage::Open | Stage::Ready if now >= t1 && now < t1 + LATE_WINDOW && !gone => {
                let _ = self.settlement.refund(swap.id, &swap.opened, &e).await;
            }
            _ => {}
        }
        if let Some(kept) = self.book.lock().unwrap().swaps.get_mut(&swap.id) {
            kept.swept = kept.swept.max(swap.swept);
            kept.done = swap.done;
        }
    }

    fn info(&self) -> Answer {
        Answer::ok(&MakerInfo {
            api_version: 1,
            maker: self.address,
            chain_id: CHAIN_ID,
            contract: self.chain.contract,
            contract_kind: Some(zecswap_api::WINDOWS_CONTRACT.into()),
            min_window: Some(MIN_WINDOW),
            late_window: Some(LATE_WINDOW),
            token: self.chain.token,
            zcash_network: Network::Testnet,
            reverse_enabled: false,
            token_return_key: None,
        })
    }
}

pub struct MakerDoor(pub Arc<Maker>);

impl Served for MakerDoor {
    fn serve(&self, asked: Asked) -> Answering {
        let maker = self.0.clone();
        Box::pin(async move {
            let parts: Vec<&str> = asked.path.trim_matches('/').split('/').collect();
            let answer = match (asked.method.as_str(), parts.as_slice()) {
                ("GET", ["v1", "info"]) => Ok(maker.info()),
                ("POST", ["v1", "quote"]) => maker.quote(&asked),
                ("POST", ["v1", "quote", id, "accept"]) => match id.parse() {
                    Ok(id) => maker.accept(id, &asked).await,
                    Err(_) => Err(Answer::refuse(400, ErrorCode::InvalidRequest, "quote id")),
                },
                ("GET", ["v1", "swaps", id]) => match id.parse() {
                    Ok(swap_id) => Ok(Answer::ok(&Status {
                        swap_id,
                        token_return: None,
                    })),
                    Err(_) => Err(Answer::refuse(400, ErrorCode::InvalidRequest, "swap id")),
                },
                _ => Err(Answer::refuse(404, ErrorCode::NotFound, "no such route")),
            };
            answer.unwrap_or_else(|refusal| refusal)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReverseMood {
    Honest,
    NeverDeposits,
    DepositsLess,
    DepositsLate(u64),
    NeverClaims,
    ClaimsLate(u64),
    ClaimsAtTheWire,
    LocksLong,
    LiesAboutDeadlines(u64),
    LiesAboutNote,
    LiesAboutUser,
    LiesAboutAmounts(u64),
    LiesAboutDeployment(u64),
    BadProof,
    ReportsAnotherSwap,
}

impl ReverseMood {
    pub fn lies(self) -> bool {
        matches!(
            self,
            Self::LiesAboutDeadlines(_)
                | Self::LiesAboutNote
                | Self::LiesAboutUser
                | Self::LiesAboutAmounts(_)
                | Self::LiesAboutDeployment(_)
                | Self::BadProof
        )
    }

    pub fn keeps_its_margin(self) -> bool {
        self != Self::ClaimsAtTheWire
    }
}

#[derive(Clone)]
struct Offer {
    quote: reverse::Quote,
    nonce: u64,
    accepted: Option<Accepted>,
}

#[derive(Clone)]
pub struct Escrow {
    pub id: B256,
    pub nonce: u64,
    pub terms: Terms,
    pub refund_note: B256,
    pub joint: String,
    pub deposit_zat: u64,
    pub paid: Option<PaymentId>,
    pub done: bool,
}

#[derive(Default)]
struct Escrows {
    offers: BTreeMap<B256, Offer>,
    escrows: BTreeMap<B256, Escrow>,
    next: u64,
}

pub struct ReverseMaker {
    seed: Seed,
    pub mood: ReverseMood,
    chain: Chain,
    zcash: Zcash,
    settlement: Settlement,
    address: Address,
    root: [u8; 32],
    book: Mutex<Escrows>,
}

impl ReverseMaker {
    pub(crate) fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    pub fn new(
        seed: Seed,
        mood: ReverseMood,
        chain: Chain,
        zcash: Zcash,
        mut settlement: Settlement,
    ) -> Arc<Self> {
        settlement.reveal_with_margin(match mood.keeps_its_margin() {
            true => REVEAL_MARGIN,
            false => 0,
        });
        let address = settlement.account().expect("the maker sends as an account");
        Arc::new(Self {
            seed,
            mood,
            chain,
            zcash,
            settlement,
            address,
            root: seed.bytes("maker-root"),
            book: Mutex::default(),
        })
    }

    pub fn share(&self, nonce: u64) -> SecretShare {
        derive_maker_share(&self.root, nonce).expect("a maker share")
    }

    pub fn escrows(&self) -> Vec<Escrow> {
        self.book
            .lock()
            .unwrap()
            .escrows
            .values()
            .cloned()
            .collect()
    }

    fn quote(&self, asked: &Asked) -> Result<Answer, Answer> {
        let request: reverse::QuoteRequest = asked.json()?;
        if request.units == 0 || request.units > 4 {
            return Err(Answer::refuse(400, ErrorCode::InvalidRequest, "units"));
        }
        let mut book = self.book.lock().unwrap();
        let nonce = book.next;
        book.next += 1;
        let e = self.share(nonce);
        let quote_id = B256::from(self.seed.bytes::<32>(&format!("reverse-quote-{nonce}")));
        let prove = |share: &SecretShare, what: &str| {
            let rng = ChaCha20Rng::from_seed(self.seed.bytes(&format!("{what}-{nonce}")));
            self.chain.context(quote_id).prove_maker(share, rng)
        };
        let funding = wall() + FUNDING_LIFE;
        let ready = funding - FUNDING_LIFE + REVERSE_TO_T0;
        let mut quote = reverse::Quote {
            terms: Quote {
                quote_id,
                maker: self.address,
                maker_share: e.public(),
                maker_proof: prove(&e, "reverse-quote-proof"),
                chain_id: CHAIN_ID,
                contract: self.chain.contract,
                token: self.chain.token,
                amount: u128::from(request.units) * UNIT,
                deposit_zat: u64::from(request.units) * UNIT_ZAT,
                expires_at: funding,
                network_cost: None,
                open_fee: None,
            },
            user: request.user,
            refund_note: request.refund_note,
            funding_deadline: funding,
            ready_deadline: ready,
            refund_after: ready + REVERSE_T0_TO_T1,
        };
        let other = |what: &str| Address::from(self.seed.bytes::<20>(what));
        match self.mood {
            ReverseMood::LiesAboutDeadlines(0) => {
                quote.ready_deadline = quote.funding_deadline + MIN_WINDOW - 60;
            }
            ReverseMood::LiesAboutDeadlines(1) => {
                quote.refund_after = quote.ready_deadline + MIN_WINDOW - 60;
            }
            ReverseMood::LiesAboutDeadlines(2) => quote.funding_deadline = quote.ready_deadline,
            ReverseMood::LocksLong => {
                quote.refund_after = quote.funding_deadline + MAX_READY_WAIT - 60;
            }
            ReverseMood::LiesAboutDeadlines(_) => {
                quote.refund_after = quote.funding_deadline + MAX_READY_WAIT + 60 * 60;
            }
            ReverseMood::LiesAboutNote => {
                quote.refund_note = B256::from(self.seed.bytes::<32>("another-refund-note"));
            }
            ReverseMood::LiesAboutUser => quote.user = other("another-user"),
            ReverseMood::LiesAboutAmounts(0) => quote.terms.amount = 0,
            ReverseMood::LiesAboutAmounts(1) => quote.terms.deposit_zat = 0,
            ReverseMood::LiesAboutAmounts(_) => quote.terms.maker = Address::ZERO,
            ReverseMood::LiesAboutDeployment(0) => {
                quote.terms.contract = other("another-contract");
            }
            ReverseMood::LiesAboutDeployment(1) => quote.terms.token = other("another-token"),
            ReverseMood::LiesAboutDeployment(_) => quote.terms.chain_id += 1,
            ReverseMood::BadProof => {
                quote.terms.maker_proof = prove(&self.share(nonce + (1 << 32)), "a-bad-proof");
            }
            _ => {}
        }
        book.offers.insert(
            quote_id,
            Offer {
                quote: quote.clone(),
                nonce,
                accepted: None,
            },
        );
        Ok(Answer::ok(&quote))
    }

    fn accept(&self, quote_id: B256, asked: &Asked) -> Result<Answer, Answer> {
        let acceptance: Acceptance = asked.json()?;
        let mut book = self.book.lock().unwrap();
        let Some(offer) = book.offers.get(&quote_id).cloned() else {
            return Err(Answer::refuse(
                404,
                ErrorCode::UnknownQuote,
                "no such quote",
            ));
        };
        if let Some(accepted) = &offer.accepted {
            return Ok(Answer::ok(accepted));
        }
        let quote = &offer.quote;
        if wall() >= quote.funding_deadline {
            return Err(Answer::refuse(
                410,
                ErrorCode::UnknownQuote,
                "the quote expired",
            ));
        }
        let rejected =
            |e: zecswap_core::Error| Answer::refuse(400, ErrorCode::Rejected, e.to_string());
        let payout = Payout {
            user: quote.user.into(),
            note: Some(quote.refund_note.0),
        };
        if !self.mood.lies() {
            self.chain
                .context(quote_id)
                .verify_user(
                    &quote.terms.maker_share,
                    &acceptance.user_share,
                    &payout,
                    &acceptance.user_proof,
                )
                .map_err(rejected)?;
        }
        let joint = JointAccount::derive(
            &quote.terms.maker_share,
            &acceptance.user_share,
            &acceptance.viewing_keys,
        )
        .map_err(rejected)?
        .unified_address(NetworkType::Test);
        let id = reverse_swap_id(quote.user, &quote.terms.maker_share);
        let accepted = Accepted {
            swap_id: match self.mood {
                ReverseMood::ReportsAnotherSwap => {
                    B256::from(self.seed.bytes::<32>("another-swap"))
                }
                _ => id,
            },
            t0: quote.ready_deadline,
            t1: quote.refund_after,
            late_window: None,
            ticket: format!("reverse-ticket-{}", offer.nonce),
        };
        book.offers.get_mut(&quote_id).expect("the offer").accepted = Some(accepted.clone());
        book.escrows.insert(
            id,
            Escrow {
                id,
                nonce: offer.nonce,
                terms: quote.open(acceptance.user_share).terms(),
                refund_note: quote.refund_note,
                joint,
                deposit_zat: quote.terms.deposit_zat,
                paid: None,
                done: false,
            },
        );
        Ok(Answer::ok(&accepted))
    }

    pub async fn tend(self: Arc<Self>) {
        loop {
            tokio::time::sleep(TEND).await;
            for escrow in self.escrows() {
                if !escrow.done {
                    self.tend_one(escrow).await;
                }
            }
        }
    }

    async fn tend_one(&self, mut escrow: Escrow) {
        if let Some(paid) = escrow.paid {
            self.zcash.broadcast(paid);
        }
        let Ok(Some(chain)) = self.settlement.swap(escrow.id, &escrow.terms).await else {
            return;
        };
        let Ok(now) = self.settlement.now().await else {
            return;
        };
        match chain.stage {
            Stage::Open if escrow.paid.is_none() && self.pays(&escrow, now) => {
                let funding = self
                    .settlement
                    .confirmed_reverse_funding(escrow.id, FUNDING_CONFIRMATIONS)
                    .await;
                if matches!(funding, Ok(Some(funding)) if funding.refund_note == escrow.refund_note)
                {
                    let zat = match self.mood {
                        ReverseMood::DepositsLess => escrow.deposit_zat - SHORT_ZAT,
                        _ => escrow.deposit_zat,
                    };
                    let paid = self.zcash.build(&escrow.joint, zat);
                    self.zcash.broadcast(paid);
                    escrow.paid = Some(paid);
                    self.chain.node.note(format!("the maker paid {zat} zat in"));
                }
            }
            Stage::Ready => self.claim(&escrow, now).await,
            Stage::Claimed => escrow.done = true,
            Stage::Refunded => {
                self.zcash.sweep(&escrow.joint);
                escrow.done = true;
            }
            Stage::Open => {}
        }
        if let Some(kept) = self.book.lock().unwrap().escrows.get_mut(&escrow.id) {
            kept.paid = escrow.paid;
            kept.done = escrow.done;
        }
    }

    fn pays(&self, escrow: &Escrow, now: u64) -> bool {
        match self.mood {
            ReverseMood::NeverDeposits => false,
            ReverseMood::DepositsLate(lead) => now + lead >= escrow.terms.t0,
            _ => now + DEPOSIT_BY < escrow.terms.t0,
        }
    }

    async fn claim(&self, escrow: &Escrow, now: u64) {
        let e = self.share(escrow.nonce);
        let t1 = escrow.terms.t1;
        let paid = escrow
            .paid
            .is_some_and(|paid| self.zcash.confirmations(paid) > 0);
        let trying = match self.mood {
            ReverseMood::NeverClaims => false,
            ReverseMood::NeverDeposits | ReverseMood::DepositsLess => true,
            ReverseMood::ClaimsLate(lateness) => paid && now + lateness >= t1,
            ReverseMood::ClaimsAtTheWire => {
                if paid && now + AT_THE_WIRE >= t1 {
                    let _ = self.settlement.claim(escrow.id, &escrow.terms, &e).await;
                }
                false
            }
            _ => paid,
        };
        if !trying {
            return;
        }
        let Ok(windows) = self.settlement.windows().await else {
            return;
        };
        let Ok((Some(chain), head)) = self.settlement.read(escrow.id, &escrow.terms).await else {
            return;
        };
        let judged = judge::reverse_claim(
            windows,
            chain.stage,
            (chain.t0, chain.t1),
            &head,
            wall(),
            REVEAL_MARGIN,
        );
        let Ok(Go::OnceReadyAt(block)) = judged else {
            return;
        };
        let then = self
            .settlement
            .swap_at(escrow.id, &escrow.terms, BlockId::number(block))
            .await;
        if matches!(then, Ok(Some(then)) if then.stage == Stage::Ready) {
            let _ = self.settlement.claim(escrow.id, &escrow.terms, &e).await;
        }
    }
}

pub struct ReverseMakerDoor(pub Arc<ReverseMaker>);

impl Served for ReverseMakerDoor {
    fn serve(&self, asked: Asked) -> Answering {
        let maker = self.0.clone();
        Box::pin(async move {
            let parts: Vec<&str> = asked.path.trim_matches('/').split('/').collect();
            let answer = match (asked.method.as_str(), parts.as_slice()) {
                ("POST", ["v1", "reverse", "quote"]) => maker.quote(&asked),
                ("POST", ["v1", "reverse", "quote", id, "accept"]) => match id.parse() {
                    Ok(id) => maker.accept(id, &asked),
                    Err(_) => Err(Answer::refuse(400, ErrorCode::InvalidRequest, "quote id")),
                },
                _ => Err(Answer::refuse(404, ErrorCode::NotFound, "no such route")),
            };
            answer.unwrap_or_else(|refusal| refusal)
        })
    }
}
