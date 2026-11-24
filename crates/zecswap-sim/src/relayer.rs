use std::sync::Arc;

use alloy::primitives::{Address, B256};
use zecswap_api::relayer::{Claim, Payout, Sent, Terms as RelayerTerms};
use zecswap_api::service::ErrorCode;
use zecswap_chain::Error;
use zecswap_chain::evm::{Settlement, Stage};
use zecswap_core::{SecretShare, Terms};
use zecswap_railgun::ShieldNote;

use crate::net::{Answer, Answering, Asked, Served};
use crate::world::{CHAIN_ID, Chain, LATE_WINDOW, MIN_WINDOW};

pub const FEE: u128 = 150_000;

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

    async fn pay(&self, payout: &Payout, terms: &Terms) -> Result<Option<B256>, Answer> {
        if payout.fee != FEE {
            return Err(Answer::refuse(400, ErrorCode::Rejected, "another fee"));
        }
        let (stage, paid_out) = self.stage(payout.swap_id, terms).await?;
        if paid_out {
            return Ok(None);
        }
        if stage != Stage::Claimed {
            return Err(Answer::refuse(409, ErrorCode::Conflict, "not claimed"));
        }
        self.settlement
            .payout(
                payout.swap_id,
                terms,
                &ShieldNote::from(&payout.note),
                payout.fee,
                &payout.signature.0,
            )
            .await
            .map(Some)
            .map_err(Self::failed)
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
        if let Ok(Some(paid)) = self.pay(&claim.payout, &terms).await {
            transactions.push(paid);
        }
        Ok(Answer::ok(&Sent { transactions }))
    }

    async fn payout(&self, asked: &Asked) -> Result<Answer, Answer> {
        let payout: Payout = asked.json()?;
        let terms = Terms::from(&payout.terms);
        let transactions = self.pay(&payout, &terms).await?.into_iter().collect();
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
                ("POST", "/v1/payout") => relayer.payout(&asked).await,
                _ => Err(Answer::refuse(404, ErrorCode::NotFound, "no such route")),
            };
            answer.unwrap_or_else(|refusal| refusal)
        })
    }
}
