use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use zecswap_api::service::{ErrorCode, MakerInfo};
use zecswap_api::{Acceptance, Accepted, Network, OpenFee, Quote, QuoteRequest, Status};
use zecswap_chain::evm::{Settlement, Stage, swap_id};
use zecswap_core::{
    JointAccount, NetworkType, Payout, SecretShare, SwapContext, Terms, derive_maker_share,
    derive_user_keys,
};

use crate::net::{Answer, Answering, Asked, Served};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, Chain, LATE_WINDOW, MIN_WINDOW, UNIT, UNIT_ZAT, wall};
use crate::zcash::Zcash;

pub const QUOTE_LIFE: u64 = 10 * 60;
pub const TO_T0: u64 = 45 * 60;
pub const T0_TO_T1: u64 = 40 * 60;
pub const CONFIRMATIONS: u64 = 2;
const READY_BY: u64 = 4 * 60;
const CANCEL_BY: u64 = 8 * 60;
const REVEAL_MARGIN: u64 = 5 * 60;
const TEND: Duration = Duration::from_secs(9);

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
    zcash: Zcash,
    settlement: Settlement,
    pub address: Address,
    root: [u8; 32],
    pub fee_address: String,
    book: Mutex<Book>,
}

impl Maker {
    pub fn new(
        seed: Seed,
        mood: Mood,
        fee: Fee,
        chain: Chain,
        zcash: Zcash,
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
        let fee_address =
            JointAccount::derive(&other.public(), &keys.share.public(), &keys.viewing)
                .expect("a fee account")
                .unified_address(NetworkType::Test);
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

    fn context(&self, quote_id: B256) -> SwapContext {
        SwapContext {
            chain_id: CHAIN_ID,
            contract: self.chain.contract.into(),
            quote_id: quote_id.0,
        }
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
            maker_proof: self.context(quote_id).prove_maker(&e, rng),
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
        self.context(quote_id)
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
        .map_err(|e| Answer::refuse(400, ErrorCode::Rejected, e.to_string()))?
        .unified_address(NetworkType::Test);
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
