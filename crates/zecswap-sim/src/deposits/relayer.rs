use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, Bytes, keccak256};
use tokio::sync::OnceCell;
use tokio::time::Instant;
use zecswap_api::relayer::{
    Conflict, DepositTerms, Rescue as PayoutRescue, RescueDeposit, ShieldDeposit,
    Terms as RelayerTerms, deposit_fee,
};
use zecswap_api::service::ErrorCode;
use zecswap_chain::evm::Settlement;
use zecswap_chain::evm::deposits::{DepositPolicy, Rescue, Shield};
use zecswap_chain::evm::railgun::{Later, SendError, Signed, Status};
use zecswap_client::FeeCap;
use zecswap_core::{
    DepositsDomain, ReceiveAuthorization, RescueAuthorization, Terms, TokenDomain, signer,
};
use zecswap_railgun::ShieldNote;

use crate::net::{Answer, Answering, Asked, Served};
use crate::relayer::{FEE, Funder, Relayer as SwapRelayer, sent};
use crate::world::{CHAIN_ID, Chain, EPOCH, LATE_WINDOW, MIN_WINDOW};

pub const LANDING: u64 = 15 * 60;
const HOLD: u64 = 5 * 60;
const MAX_GAS: u64 = 3_000_000;
const MAX_GAS_PRICE: u128 = 1_000_000_000_000;
const FORCED_GAS: u64 = 1_500_000;
const LOST_ANSWERS: usize = 2;
const PAYOUT_LANDING: u64 = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Honest,
    Overcharges(u64),
    AnotherDeployment(u64),
    TakesNone,
    LowMinimum,
    StaleTerms(u64),
    Sits(u64),
    NeverSends,
    LosesAnswers,
    Tampers(u64),
    NamesAnother(u64),
}

impl Service {
    pub fn lies(self) -> bool {
        matches!(
            self,
            Self::Overcharges(_) | Self::AnotherDeployment(_) | Self::TakesNone
        )
    }

    pub fn delivers(self) -> bool {
        matches!(
            self,
            Self::Honest
                | Self::LowMinimum
                | Self::StaleTerms(_)
                | Self::Sits(_)
                | Self::LosesAnswers
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    pub fee: u128,
    pub fee_bps: u32,
    pub min_value: u128,
    pub max_validity: u64,
}

#[derive(Clone, Debug)]
pub struct Heard {
    pub at: u64,
    pub time: u64,
    pub from: Address,
    pub key: B256,
    pub until: u64,
    pub rescue: bool,
}

struct Flight {
    key: B256,
    hashes: Vec<B256>,
}

pub struct Relayer {
    service: Service,
    offer: Offer,
    reprice: Option<(u64, u128)>,
    cap: FeeCap,
    chain: Chain,
    deposits: Address,
    settlement: Settlement,
    forcer: Funder,
    address: Address,
    start: Instant,
    token: OnceCell<TokenDomain>,
    book: Mutex<BTreeMap<Address, Vec<Flight>>>,
    held: Mutex<BTreeMap<Address, u64>>,
    heard: Mutex<Vec<Heard>>,
    named: Mutex<Vec<B256>>,
}

pub struct Desk {
    pub service: Service,
    pub offer: Offer,
    pub reprice: Option<(u64, u128)>,
    pub cap: FeeCap,
    pub deposits: Address,
    pub start: Instant,
}

fn refuse(why: impl Into<String>) -> Answer {
    Answer::refuse(400, ErrorCode::Rejected, why)
}

fn busy(why: &str, after: Option<u64>) -> Answer {
    let shed = Answer::refuse(503, ErrorCode::Unavailable, why);
    match after {
        Some(seconds) => shed.after(seconds),
        None => shed,
    }
}

fn internal(error: impl std::fmt::Display) -> Answer {
    Answer::refuse(500, ErrorCode::Internal, error.to_string())
}

fn require(holds: bool, why: impl Into<String>) -> Result<(), Answer> {
    match holds {
        true => Ok(()),
        false => Err(refuse(why)),
    }
}

impl Relayer {
    pub fn new(desk: Desk, chain: Chain, settlement: Settlement, forcer: Funder) -> Arc<Self> {
        let address = settlement
            .account()
            .expect("the relayer sends as an account");
        Arc::new(Self {
            service: desk.service,
            offer: desk.offer,
            reprice: desk.reprice,
            cap: desk.cap,
            chain,
            deposits: desk.deposits,
            settlement,
            forcer,
            address,
            start: desk.start,
            token: OnceCell::new(),
            book: Mutex::default(),
            held: Mutex::default(),
            heard: Mutex::default(),
            named: Mutex::default(),
        })
    }

    pub fn heard(&self) -> Vec<Heard> {
        self.heard.lock().unwrap().clone()
    }

    pub fn named(&self) -> Vec<B256> {
        self.named.lock().unwrap().clone()
    }

    pub fn asks(&self) -> Offer {
        self.asked(0)
    }

    fn now(&self) -> u64 {
        EPOCH + self.start.elapsed().as_secs()
    }

    fn asked(&self, lag: u64) -> Offer {
        let fee = match self.reprice {
            Some((at, fee)) if self.now() >= EPOCH + at + lag => fee,
            _ => self.offer.fee,
        };
        match self.service {
            Service::Overcharges(0) => Offer {
                fee: self.cap.fee + 50_000,
                fee_bps: self.cap.fee_bps,
                ..self.offer
            },
            Service::Overcharges(_) => Offer {
                fee: self.cap.fee,
                fee_bps: self.cap.fee_bps + 10,
                ..self.offer
            },
            Service::LowMinimum => Offer {
                fee,
                min_value: 1,
                ..self.offer
            },
            _ => Offer { fee, ..self.offer },
        }
    }

    fn terms(&self) -> Answer {
        let shown = self.asked(match self.service {
            Service::StaleTerms(lag) => lag,
            _ => 0,
        });
        let named = |kind: u64, honest: Address| match self.service {
            Service::AnotherDeployment(lie) if lie == kind => Address::repeat_byte(0xad),
            _ => honest,
        };
        Answer::ok(&RelayerTerms {
            relayer: self.address,
            chain_id: CHAIN_ID + u64::from(self.service == Service::AnotherDeployment(2)),
            contract: self.chain.contract,
            contract_kind: Some(zecswap_api::WINDOWS_CONTRACT.into()),
            min_window: Some(MIN_WINDOW),
            late_window: Some(LATE_WINDOW),
            fee: FEE,
            fee_expires_at: None,
            reverse_funding: None,
            railgun_sends: None,
            deposits: (self.service != Service::TakesNone).then(|| DepositTerms {
                contract: named(0, self.deposits),
                relayer: self.address,
                token: named(1, self.chain.token),
                fee: shown.fee,
                fee_bps: shown.fee_bps,
                fee_expires_at: None,
                min_value: shown.min_value,
                max_validity: shown.max_validity,
                max_gas_price_wei: MAX_GAS_PRICE,
            }),
        })
    }

    fn hear(&self, from: Address, key: B256, until: u64, rescue: bool) {
        self.heard.lock().unwrap().push(Heard {
            at: self.start.elapsed().as_millis() as u64,
            time: self.now(),
            from,
            key,
            until,
            rescue,
        });
    }

    fn answered(&self, key: B256, answer: Result<Answer, Answer>) -> Result<Answer, Answer> {
        let heard = self.heard.lock().unwrap();
        let posts = heard.iter().filter(|heard| heard.key == key).count();
        if self.service == Service::LosesAnswers && posts <= LOST_ANSWERS {
            return Err(Answer::refuse(
                502,
                ErrorCode::Internal,
                "the relayer's answer was lost",
            ));
        }
        answer
    }

    fn conflict(&self, transactions: Vec<B256>, why: &str) -> Answer {
        self.named.lock().unwrap().extend(&transactions);
        let conflict = Conflict {
            code: ErrorCode::Conflict,
            error: why.into(),
            transactions,
        };
        Answer {
            status: 409,
            retry_after: None,
            body: serde_json::to_vec(&conflict).expect("a JSON body"),
        }
    }

    fn domain(&self) -> DepositsDomain {
        DepositsDomain {
            chain_id: CHAIN_ID,
            contract: self.deposits.into(),
        }
    }

    async fn token(&self) -> Result<&TokenDomain, Answer> {
        self.token
            .get_or_try_init(|| async {
                let deployment = self.settlement.deposits(self.deposits).await;
                deployment.map(|deployment| deployment.token_domain)
            })
            .await
            .map_err(internal)
    }

    async fn status(&self, hash: B256) -> Result<Status, Answer> {
        self.settlement
            .transaction_status(hash)
            .await
            .map_err(internal)
    }

    fn flights(&self) -> Vec<(Address, B256, B256)> {
        let book = self.book.lock().unwrap();
        book.iter()
            .flat_map(|(owner, flights)| {
                flights
                    .iter()
                    .flat_map(|flight| flight.hashes.iter().map(|hash| (*owner, flight.key, *hash)))
            })
            .collect()
    }

    fn fly(&self, from: Address, key: B256, hash: B256) {
        let mut book = self.book.lock().unwrap();
        let flights = book.entry(from).or_default();
        match flights.iter_mut().find(|flight| flight.key == key) {
            Some(flight) => flight.hashes.push(hash),
            None => flights.push(Flight {
                key,
                hashes: vec![hash],
            }),
        }
    }

    async fn known(&self, from: Address, key: B256) -> Result<Option<Answer>, Answer> {
        let (mut landed, mut pending, mut rival) = (None, None, None);
        let mut reverted = Vec::new();
        for (owner, flight, hash) in self.flights() {
            if owner != from {
                continue;
            }
            match (self.status(hash).await?, flight == key) {
                (Status::Mined { succeeded: true }, true) => landed = Some(hash),
                (Status::Pending, true) => pending = Some(hash),
                (Status::Mined { succeeded: false }, true) => reverted.push(hash),
                (Status::Pending, false) => rival = Some(hash),
                _ => {}
            }
        }
        if let Some(hash) = landed.or(pending) {
            return Ok(Some(sent(vec![hash])));
        }
        let now = self.now();
        if !reverted.is_empty() {
            if let Some(flights) = self.book.lock().unwrap().get_mut(&from) {
                for flight in flights {
                    flight.hashes.retain(|hash| !reverted.contains(hash));
                }
            }
            self.held.lock().unwrap().insert(from, now + HOLD);
        }
        let until = self.held.lock().unwrap().get(&from).copied();
        if let Some(left) = until
            .and_then(|until| until.checked_sub(now))
            .filter(|left| *left > 0)
        {
            return Ok(Some(busy(
                "a deposit of this address reverted a little while ago",
                Some(left),
            )));
        }
        Ok(rival
            .map(|hash| self.conflict(vec![hash], "another deposit of this address is on its way")))
    }

    async fn decoy(&self, from: Address, key: B256, kind: u64) -> Result<Option<Answer>, Answer> {
        for (owner, flight, hash) in self.flights() {
            let fits = match kind {
                0 => owner == from && flight != key,
                _ => owner != from,
            };
            if fits && self.status(hash).await? == (Status::Mined { succeeded: true }) {
                return Ok(Some(match kind {
                    0 => sent(vec![hash]),
                    _ => self.conflict(vec![hash], "the authorization is used"),
                }));
            }
        }
        Ok(None)
    }

    async fn crooked(
        &self,
        from: Address,
        key: B256,
        forged: Option<Bytes>,
    ) -> Result<Option<Answer>, Answer> {
        match self.service {
            Service::Sits(seconds) if self.start.elapsed().as_secs() < seconds => {
                let after = [Some(20), Some(120), None][(seconds % 3) as usize];
                Ok(Some(busy("the relayer is busy", after)))
            }
            Service::NeverSends => Ok(Some(sent(vec![keccak256(key)]))),
            Service::NamesAnother(kind) => self.decoy(from, key, kind).await,
            _ => match forged {
                Some(data) => {
                    let forced = self.forcer.forced(self.deposits, data, FORCED_GAS).await;
                    Ok(Some(match forced {
                        Some(hash) => sent(vec![hash]),
                        None => internal("the transaction was not sent"),
                    }))
                }
                None => Ok(None),
            },
        }
    }

    async fn send(&self, from: Address, key: B256, data: Bytes) -> Answer {
        let policy = DepositPolicy {
            contract: self.deposits,
            max_gas_limit: MAX_GAS,
            max_gas_price_wei: MAX_GAS_PRICE,
        };
        let record = |signed: &Signed| {
            self.fly(from, key, signed.hash);
            Ok(())
        };
        let sending = self
            .settlement
            .send_deposit(&policy, data, 0, |_, _| true, record);
        match sending.await {
            Ok(hash) | Err(SendError::Unknown(hash)) => sent(vec![hash]),
            Err(SendError::Rejected(why)) => refuse(why),
            Err(SendError::Later(later)) => {
                let after = match later {
                    Later::GasPrice => 300,
                    Later::Eth => 120,
                };
                busy(later.reason(), Some(after))
            }
            Err(other) => internal(other),
        }
    }

    async fn shield(&self, asked: &Asked) -> Result<Answer, Answer> {
        let request: ShieldDeposit = asked.json()?;
        let key = keccak256(request.signature);
        self.hear(request.from, key, request.valid_before, false);
        self.answered(key, self.shielding(&request, key).await)
    }

    async fn shielding(&self, request: &ShieldDeposit, key: B256) -> Result<Answer, Answer> {
        let from = request.from;
        let (note, signature) = (ShieldNote::from(&request.note), request.signature.0);
        let shield = |value: u128, note: &ShieldNote, fee: u128| {
            let shield = Shield {
                from,
                value,
                valid_after: request.valid_after,
                valid_before: request.valid_before,
                note,
                fee,
                signature: &signature,
            };
            shield.calldata()
        };
        let forged = match self.service {
            Service::Tampers(0) => Some(shield(request.value, &note, request.fee + 1)),
            Service::Tampers(1) => {
                let mut another = note;
                another.npk[31] ^= 1;
                Some(shield(request.value, &another, request.fee))
            }
            Service::Tampers(_) => Some(shield(request.value - 1, &note, request.fee)),
            _ => None,
        };
        if let Some(answer) = self.crooked(from, key, forged).await? {
            return Ok(answer);
        }
        if let Some(answer) = self.known(from, key).await? {
            return Ok(answer);
        }
        let (asks, now) = (self.asks(), self.now());
        require(
            request.chain_id == CHAIN_ID && request.contract == self.deposits,
            "wrong chain or Deposits contract",
        )?;
        require(
            request.value >= asks.min_value,
            format!("a deposit moves at least {}", asks.min_value),
        )?;
        require(
            request.fee < request.value,
            "the fee leaves nothing to shield",
        )?;
        require(
            request.valid_after == 0,
            "the authorization's validAfter is not two minutes past: sign 0",
        )?;
        require(
            request.valid_before >= now + LANDING,
            "the authorization expires too soon to land: sign it again",
        )?;
        require(
            request.valid_before <= now + asks.max_validity,
            "the authorization is valid for longer than the relayer takes",
        )?;
        let nonce = self
            .domain()
            .deposit_nonce(&note.words(), request.fee, &self.address.into());
        let digest = self
            .token()
            .await?
            .receive_with_authorization(&ReceiveAuthorization {
                from: from.into(),
                to: self.deposits.into(),
                value: request.value,
                valid_after: 0,
                valid_before: request.valid_before,
                nonce,
            });
        require(
            signer(&digest, &signature) == Some(from.into()),
            "not signed by the deposit address for this note, fee and relayer",
        )?;
        let owed = deposit_fee(asks.fee, asks.fee_bps, request.value);
        require(
            request.fee >= owed,
            format!(
                "a fee of {} is less than the relayer's {owed}: fetch the terms again and sign again",
                request.fee
            ),
        )?;
        let (chain, token) = (&self.settlement, self.chain.token);
        require(
            !chain.has_code(from).await.map_err(internal)?,
            "the deposit address has code: deposits are from plain accounts",
        )?;
        let used = chain.authorization_used(token, from, nonce.into()).await;
        if used.map_err(internal)? {
            return Ok(self.conflict(Vec::new(), "the authorization is used"));
        }
        require(
            chain.token_balance(token, from).await.map_err(internal)? >= request.value,
            "the deposit address holds less than it authorizes",
        )?;
        let data = shield(request.value, &note, request.fee);
        Ok(self.send(from, key, data).await)
    }

    async fn rescue(&self, asked: &Asked) -> Result<Answer, Answer> {
        let request: RescueDeposit = asked.json()?;
        let key = keccak256(request.signature);
        self.hear(request.from, key, request.deadline, true);
        self.answered(key, self.rescuing(&request, key).await)
    }

    async fn rescuing(&self, request: &RescueDeposit, key: B256) -> Result<Answer, Answer> {
        let from = request.from;
        let (note, signature) = (ShieldNote::from(&request.note), request.signature.0);
        let authorization = RescueAuthorization {
            nonce: request.nonce,
            deadline: request.deadline,
        };
        let rescue = |fee: u128| {
            let rescue = Rescue {
                from,
                note: &note,
                fee,
                authorization,
                signature: &signature,
            };
            rescue.calldata()
        };
        let forged = matches!(self.service, Service::Tampers(_)).then(|| rescue(request.fee + 1));
        if let Some(answer) = self.crooked(from, key, forged).await? {
            return Ok(answer);
        }
        if let Some(answer) = self.known(from, key).await? {
            return Ok(answer);
        }
        let (asks, now) = (self.asks(), self.now());
        require(
            request.chain_id == CHAIN_ID && request.contract == self.deposits,
            "wrong chain or Deposits contract",
        )?;
        require(
            request.fee >= asks.fee,
            format!(
                "a fee of {} is less than the relayer's {}: fetch the terms again and sign again",
                request.fee, asks.fee
            ),
        )?;
        require(
            request.deadline >= now + LANDING,
            "the rescue expires too soon to land: sign it again",
        )?;
        require(
            request.deadline <= now + asks.max_validity,
            "the rescue is valid for longer than the relayer takes",
        )?;
        let digest = self.domain().rescue(
            &from.into(),
            &note.commitment(),
            &self.address.into(),
            request.fee,
            authorization,
        );
        require(
            signer(&digest, &signature) == Some(from.into()),
            "not signed by the deposit address for this note, fee and relayer",
        )?;
        let (chain, deposits) = (&self.settlement, self.deposits);
        let next = chain.deposit_rescue_nonce(deposits, from).await;
        require(
            next.map_err(internal)? == request.nonce,
            "the rescue's nonce is not its vault's next",
        )?;
        let vault = chain
            .deposit_vault(deposits, from)
            .await
            .map_err(internal)?;
        let holds = chain.token_balance(self.chain.token, vault).await;
        require(
            holds.map_err(internal)? > request.fee,
            "the address's vault holds nothing to rescue beyond the fee",
        )?;
        Ok(self.send(from, key, rescue(request.fee)).await)
    }

    async fn rescue_payout(&self, asked: &Asked) -> Result<Answer, Answer> {
        let request: PayoutRescue = asked.json()?;
        let key = keccak256(request.signature);
        self.hear(Address::ZERO, key, request.deadline, true);
        self.answered(key, self.rescuing_payout(&request, key).await)
    }

    async fn rescuing_payout(&self, request: &PayoutRescue, key: B256) -> Result<Answer, Answer> {
        if let Some(answer) = self.crooked(Address::ZERO, key, None).await? {
            return Ok(answer);
        }
        let (id, terms) = (request.swap_id, Terms::from(&request.terms));
        let chain = &self.settlement;
        require(request.fee >= FEE, "a fee under the relayer's")?;
        require(
            request.deadline >= self.now() + PAYOUT_LANDING,
            "the rescue expires too soon to land: sign it again",
        )?;
        let swap = chain.swap(id, &terms).await.map_err(internal)?;
        require(
            swap.is_some_and(|swap| swap.paid_out),
            "the swap has paid nothing out",
        )?;
        require(
            chain.rescue_nonce(id).await.map_err(internal)? == request.nonce,
            "the rescue's nonce is not its vault's next",
        )?;
        let authorization = RescueAuthorization {
            nonce: request.nonce,
            deadline: request.deadline,
        };
        let note = ShieldNote::from(&request.note);
        let (fee, signature) = (request.fee, &request.signature.0);
        let rescued = chain.rescue(id, &terms, &note, fee, signature, authorization);
        Ok(match rescued.await {
            Ok(hash) => sent(vec![hash]),
            Err(e) => SwapRelayer::failed(e),
        })
    }
}

pub struct Door(pub Arc<Relayer>);

impl Served for Door {
    fn serve(&self, asked: Asked) -> Answering {
        let relayer = self.0.clone();
        Box::pin(async move {
            let answer = match (asked.method.as_str(), asked.path.as_str()) {
                ("GET", "/v1/terms") => Ok(relayer.terms()),
                ("POST", "/v1/deposits/shield") => relayer.shield(&asked).await,
                ("POST", "/v1/deposits/rescue") => relayer.rescue(&asked).await,
                ("POST", "/v1/rescue" | "/v1/reverse/rescue") => {
                    relayer.rescue_payout(&asked).await
                }
                _ => Err(Answer::refuse(404, ErrorCode::NotFound, "no such route")),
            };
            answer.unwrap_or_else(|refusal| refusal)
        })
    }
}
