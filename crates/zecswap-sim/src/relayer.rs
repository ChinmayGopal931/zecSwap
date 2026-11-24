use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::eips::{BlockNumberOrTag, Encodable2718};
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use tokio::task::spawn_local;
use zecswap_api::relayer::{Claim, Payout, Sent, Terms as RelayerTerms};
use zecswap_api::reverse::{Authorization, Funding, Refund};
use zecswap_api::service::ErrorCode;
use zecswap_chain::Error;
use zecswap_chain::evm::railgun::IRelayAdapt;
use zecswap_chain::evm::{Settlement, Stage};
use zecswap_core::{SecretShare, Terms};
use zecswap_railgun::ShieldNote;

use crate::net::{Answer, Answering, Asked, Served};
use crate::node::Node;
use crate::world::{CHAIN_ID, Chain, EPOCH, LATE_WINDOW, MIN_WINDOW, wall};

pub const FEE: u128 = 150_000;
const READY_MARGIN: u64 = 30;
const BUSY_WITH_REFUNDS: u64 = 10 * 60;
const HOLD_DEPTH: u64 = 3;
const LANDING_POLL: Duration = Duration::from_secs(4);
const LANDING_POLLS: u32 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Honest,
    SkipsPayouts,
}

pub struct Relayer {
    service: Service,
    chain: Chain,
    settlement: Settlement,
    pub address: Address,
}

impl Relayer {
    pub fn new(service: Service, chain: Chain, mut settlement: Settlement) -> Arc<Self> {
        settlement.reveal_with_margin(zecswap_client::CLAIM_MARGIN);
        let address = settlement
            .account()
            .expect("the relayer sends as an account");
        Arc::new(Self {
            service,
            chain,
            settlement,
            address,
        })
    }

    fn terms(&self) -> Answer {
        Answer::ok(&RelayerTerms {
            relayer: self.address,
            chain_id: CHAIN_ID,
            contract: self.chain.contract,
            contract_kind: Some(zecswap_api::WINDOWS_CONTRACT.into()),
            min_window: Some(MIN_WINDOW),
            late_window: Some(LATE_WINDOW),
            fee: FEE,
            fee_expires_at: None,
            reverse_funding: None,
            railgun_sends: None,
            deposits: None,
        })
    }

    fn failed(error: Error) -> Answer {
        let why = error.to_string();
        match error {
            Error::WouldRevert(_) | Error::Late(_) => Answer::refuse(422, ErrorCode::Rejected, why),
            other if other.nothing_sent() => {
                Answer::refuse(503, ErrorCode::Unavailable, why).after(5)
            }
            _ => Answer::refuse(502, ErrorCode::Internal, why),
        }
    }

    async fn stage(&self, id: B256, terms: &Terms) -> Result<(Stage, bool), Answer> {
        match self.settlement.swap(id, terms).await {
            Ok(Some(chain)) => Ok((chain.stage, chain.paid_out)),
            Ok(None) => Err(Answer::refuse(404, ErrorCode::UnknownSwap, "no such swap")),
            Err(e) => Err(Answer::refuse(503, ErrorCode::Unavailable, e.to_string()).after(5)),
        }
    }

    async fn pay(
        &self,
        payout: &Payout,
        terms: &Terms,
        refund: bool,
    ) -> Result<Option<B256>, Answer> {
        if payout.fee != FEE {
            return Err(Answer::refuse(400, ErrorCode::Rejected, "another fee"));
        }
        let (stage, paid_out) = self.stage(payout.swap_id, terms).await?;
        if paid_out {
            return Ok(None);
        }
        let (id, note) = (payout.swap_id, ShieldNote::from(&payout.note));
        let (fee, signature) = (payout.fee, &payout.signature.0);
        let sent = match (refund, stage) {
            (false, Stage::Claimed) => {
                self.settlement
                    .payout(id, terms, &note, fee, signature)
                    .await
            }
            (true, Stage::Refunded) => {
                self.settlement
                    .refund_payout(id, terms, &note, fee, signature)
                    .await
            }
            (false, _) => return Err(Answer::refuse(409, ErrorCode::Conflict, "not claimed")),
            (true, _) => return Err(Answer::refuse(409, ErrorCode::Conflict, "not refunded")),
        };
        sent.map(Some).map_err(Self::failed)
    }

    async fn claim(&self, asked: &Asked) -> Result<Answer, Answer> {
        let claim: Claim = asked.json()?;
        let terms = Terms::from(&claim.terms);
        let secret = SecretShare::from_be_bytes(&claim.secret.0)
            .map_err(|e| Answer::refuse(400, ErrorCode::InvalidRequest, e.to_string()))?;
        let (stage, _) = self.stage(claim.swap_id, &terms).await?;
        let mut transactions = Vec::new();
        if matches!(stage, Stage::Open | Stage::Ready) {
            let sent = self.settlement.claim(claim.swap_id, &terms, &secret).await;
            transactions.push(sent.map_err(Self::failed)?);
        }
        if self.service == Service::SkipsPayouts {
            return Ok(Answer::ok(&Sent { transactions }));
        }
        if let Ok(Some(paid)) = self.pay(&claim.payout, &terms, false).await {
            transactions.push(paid);
        }
        Ok(Answer::ok(&Sent { transactions }))
    }

    async fn payout(&self, asked: &Asked, refund: bool) -> Result<Answer, Answer> {
        let payout: Payout = asked.json()?;
        let terms = Terms::from(&payout.terms);
        let transactions = self
            .pay(&payout, &terms, refund)
            .await?
            .into_iter()
            .collect();
        Ok(Answer::ok(&Sent { transactions }))
    }
}

pub struct RelayerDoor(pub Arc<Relayer>);

impl Served for RelayerDoor {
    fn serve(&self, asked: Asked) -> Answering {
        let relayer = self.0.clone();
        Box::pin(async move {
            let answer = match (asked.method.as_str(), asked.path.as_str()) {
                ("GET", "/v1/terms") => Ok(relayer.terms()),
                ("POST", "/v1/claim") => relayer.claim(&asked).await,
                ("POST", "/v1/payout") => relayer.payout(&asked, false).await,
                _ => Err(Answer::refuse(404, ErrorCode::NotFound, "no such route")),
            };
            answer.unwrap_or_else(|refusal| refusal)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReverseService {
    Honest,
    SkipsPayouts,
    SkipsOpen,
    Careless,
    BusyWithRefunds,
    LosesReadyAnswers,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Errand {
    Funding(Vec<Address>),
    Ready(u64),
    Refund,
}

#[derive(Clone, Debug)]
pub struct Heard {
    pub id: B256,
    pub at: u64,
    pub time: u64,
    pub errand: Errand,
}

#[derive(Default)]
struct Sending {
    funding: Option<B256>,
    approved: bool,
    readies: Vec<u64>,
    refunded: bool,
}

struct Funder {
    provider: DynProvider,
    wallet: EthereumWallet,
    account: Address,
}

impl Funder {
    fn new(node: &Node, signer: PrivateKeySigner) -> Self {
        let client = RpcClient::new(node.rpc("funder"), false);
        Self {
            provider: ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_client(client)
                .erased(),
            account: signer.address(),
            wallet: EthereumWallet::from(signer),
        }
    }

    async fn send(&self, to: Address, data: Bytes) -> Option<B256> {
        let provider = &self.provider;
        let tx = TransactionRequest::default()
            .with_from(self.account)
            .with_to(to)
            .with_input(data);
        let gas = provider.estimate_gas(tx.clone()).await.ok()?;
        let nonce = provider
            .get_transaction_count(self.account)
            .pending()
            .await
            .ok()?;
        let history = provider
            .get_fee_history(1, BlockNumberOrTag::Latest, &[])
            .await
            .ok()?;
        let tip = provider.get_max_priority_fee_per_gas().await.ok()?;
        let envelope = tx
            .with_gas_limit(gas)
            .with_nonce(nonce)
            .with_chain_id(CHAIN_ID)
            .with_max_fee_per_gas(2 * history.next_block_base_fee()? + tip)
            .with_max_priority_fee_per_gas(tip)
            .build(&self.wallet)
            .await
            .ok()?;
        let _ = provider
            .send_raw_transaction(&envelope.encoded_2718())
            .await;
        Some(*envelope.tx_hash())
    }

    async fn landed(&self, hash: B256) -> Option<bool> {
        let receipt = self.provider.get_transaction_receipt(hash).await.ok()??;
        Some(receipt.status())
    }

    async fn pending(&self, hash: B256) -> bool {
        self.landed(hash).await.is_none()
            && matches!(
                self.provider.get_transaction_by_hash(hash).await,
                Ok(Some(_))
            )
    }

    async fn landing(&self, to: Address, data: Bytes) -> Option<B256> {
        let hash = self.send(to, data).await?;
        for _ in 0..LANDING_POLLS {
            tokio::time::sleep(LANDING_POLL).await;
            if let Some(landed) = self.landed(hash).await {
                return landed.then_some(hash);
            }
        }
        None
    }
}

pub struct ReverseRelayer {
    relayer: Arc<Relayer>,
    service: ReverseService,
    funder: Funder,
    sending: Mutex<BTreeMap<B256, Sending>>,
    heard: Mutex<Vec<Heard>>,
}

fn sent(transactions: Vec<B256>) -> Answer {
    Answer::ok(&Sent { transactions })
}

fn unavailable(why: impl std::fmt::Display) -> Answer {
    Answer::refuse(503, ErrorCode::Unavailable, why.to_string()).after(5)
}

impl ReverseRelayer {
    pub fn new(
        service: ReverseService,
        relayer: Arc<Relayer>,
        funder: PrivateKeySigner,
    ) -> Arc<Self> {
        let funder = Funder::new(&relayer.chain.node, funder);
        Arc::new(Self {
            relayer,
            service,
            funder,
            sending: Mutex::default(),
            heard: Mutex::default(),
        })
    }

    pub fn heard(&self) -> Vec<Heard> {
        self.heard.lock().unwrap().clone()
    }

    fn hear(&self, id: B256, errand: Errand) {
        let (at, time) = self.relayer.chain.node.head();
        self.heard.lock().unwrap().push(Heard {
            id,
            at,
            time,
            errand,
        });
    }

    fn sending<T>(&self, id: B256, change: impl FnOnce(&mut Sending) -> T) -> T {
        change(self.sending.lock().unwrap().entry(id).or_default())
    }

    async fn fund(&self, asked: &Asked) -> Result<Answer, Answer> {
        let funding: Funding = asked.json()?;
        let relay = IRelayAdapt::relayCall::abi_decode(&funding.data)
            .map_err(|e| Answer::refuse(400, ErrorCode::InvalidRequest, e.to_string()))?;
        let calls: Vec<(Address, Bytes)> = relay
            ._actionData
            .calls
            .into_iter()
            .map(|call| (call.to, call.data))
            .collect();
        let id = funding.swap_id;
        self.hear(
            id,
            Errand::Funding(calls.iter().map(|(to, _)| *to).collect()),
        );
        let [(token, approve), (contract, open)] = calls.as_slice() else {
            return Err(Answer::refuse(400, ErrorCode::InvalidRequest, "two calls"));
        };
        match self.relayer.settlement.swap_state(id).await {
            Ok(Some(_)) => return Ok(sent(Vec::new())),
            Ok(None) => {}
            Err(e) => return Err(unavailable(e)),
        }
        let (approved, pending) = self.sending(id, |sending| (sending.approved, sending.funding));
        if let Some(hash) = pending
            && self.funder.pending(hash).await
        {
            return Ok(sent(vec![hash]));
        }
        if !approved {
            let Some(hash) = self.funder.landing(*token, approve.clone()).await else {
                return Err(unavailable("the approval did not land"));
            };
            self.sending(id, |sending| sending.approved = true);
            if self.service == ReverseService::SkipsOpen {
                return Ok(sent(vec![hash]));
            }
        }
        let Some(hash) = self.funder.send(*contract, open.clone()).await else {
            return Err(Answer::refuse(
                422,
                ErrorCode::Rejected,
                "the funding fails in simulation",
            ));
        };
        self.sending(id, |sending| sending.funding = Some(hash));
        Ok(sent(vec![hash]))
    }

    async fn ready(self: Arc<Self>, asked: &Asked) -> Result<Answer, Answer> {
        let ready: Authorization = asked.json()?;
        let (id, terms) = (ready.swap_id, Terms::from(&ready.terms));
        self.hear(id, Errand::Ready(ready.deadline));
        match self.relayer.stage(id, &terms).await?.0 {
            Stage::Ready | Stage::Claimed => return Ok(sent(Vec::new())),
            Stage::Refunded => return Err(Answer::refuse(409, ErrorCode::Conflict, "refunded")),
            Stage::Open => {}
        }
        let careless = self.service == ReverseService::Careless;
        if !careless {
            let now = self.relayer.settlement.now().await.map_err(unavailable)?;
            if self.sending(id, |sending| sending.refunded) || now >= terms.t0 {
                return Err(Answer::refuse(
                    400,
                    ErrorCode::Rejected,
                    "a refund was sent, or t0 passed",
                ));
            }
        }
        self.sending(id, |sending| sending.readies.push(ready.deadline));
        let margin = if careless { 0 } else { READY_MARGIN };
        let (deadline, signature) = (ready.deadline, ready.signature.0);
        let relayer = self.clone();
        let readied = async move {
            relayer
                .relayer
                .settlement
                .ready_with_sig(id, &terms, deadline, &signature, margin)
                .await
        };
        if self.service == ReverseService::LosesReadyAnswers {
            spawn_local(async move {
                let _ = readied.await;
            });
            return Err(Answer::refuse(
                502,
                ErrorCode::Internal,
                "the ready went out and its answer was lost",
            ));
        }
        readied
            .await
            .map(|hash| sent(vec![hash]))
            .map_err(Relayer::failed)
    }

    async fn refund(&self, asked: &Asked) -> Result<Answer, Answer> {
        let refund: Refund = asked.json()?;
        let (id, terms) = (refund.swap_id, Terms::from(&refund.terms));
        self.hear(id, Errand::Refund);
        let secret = SecretShare::from_be_bytes(&refund.secret.0)
            .map_err(|e| Answer::refuse(400, ErrorCode::InvalidRequest, e.to_string()))?;
        if self.service == ReverseService::BusyWithRefunds && wall() < EPOCH + BUSY_WITH_REFUNDS {
            return Err(Answer::refuse(503, ErrorCode::Unavailable, "busy").after(20));
        }
        let mut transactions = Vec::new();
        match self.relayer.stage(id, &terms).await?.0 {
            Stage::Claimed => return Err(Answer::refuse(409, ErrorCode::Conflict, "claimed")),
            Stage::Refunded => {}
            Stage::Open | Stage::Ready => {
                if self.service != ReverseService::Careless
                    && self.ready_may_land(id, &terms).await?
                {
                    return Err(Answer::refuse(
                        503,
                        ErrorCode::Unavailable,
                        "a ready this relayer sent may still land",
                    )
                    .after(10));
                }
                self.sending(id, |sending| sending.refunded = true);
                let refunded = self.relayer.settlement.refund(id, &terms, &secret).await;
                transactions.push(refunded.map_err(Relayer::failed)?);
            }
        }
        if self.service != ReverseService::SkipsPayouts
            && let Ok(Some(paid)) = self.relayer.pay(&refund.payout, &terms, true).await
        {
            transactions.push(paid);
        }
        Ok(sent(transactions))
    }

    async fn ready_may_land(&self, id: B256, terms: &Terms) -> Result<bool, Answer> {
        let Some(last) = self.sending(id, |sending| sending.readies.iter().max().copied()) else {
            return Ok(false);
        };
        let settled = self
            .relayer
            .settlement
            .confirmed_now(HOLD_DEPTH)
            .await
            .map_err(unavailable)?;
        Ok(settled <= last && settled < terms.t0)
    }
}

pub struct ReverseRelayerDoor(pub Arc<ReverseRelayer>);

impl Served for ReverseRelayerDoor {
    fn serve(&self, asked: Asked) -> Answering {
        let relayer = self.0.clone();
        Box::pin(async move {
            let answer = match (asked.method.as_str(), asked.path.as_str()) {
                ("GET", "/v1/terms") => Ok(relayer.relayer.terms()),
                ("POST", "/v1/reverse/fund") => relayer.fund(&asked).await,
                ("POST", "/v1/reverse/ready") => relayer.clone().ready(&asked).await,
                ("POST", "/v1/reverse/refund") => relayer.refund(&asked).await,
                ("POST", "/v1/reverse/refund-payout") => relayer.relayer.payout(&asked, true).await,
                _ => Err(Answer::refuse(404, ErrorCode::NotFound, "no such route")),
            };
            answer.unwrap_or_else(|refusal| refusal)
        })
    }
}
