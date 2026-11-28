use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::Result;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::relayer::{
    Claim, Conflict, Payout, RailgunTransact, RescueDeposit, Sent, ShieldDeposit, Terms,
};
use zecswap_api::service::{ErrorCode, ErrorResponse};
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest, TICKET_HEADER};
use zecswap_chain::evm::wall_now;

const TIMEOUT: Duration = Duration::from_secs(180);
const BUSY_PATIENCE: Duration = Duration::from_secs(90);
const BUSY_WAIT: Duration = Duration::from_secs(30);
const FEE_WAIT: Duration = Duration::from_secs(5);
const FEE_WAIT_MIN: Duration = Duration::from_secs(1);
const FEE_PACE: Duration = Duration::from_secs(12);

#[derive(Debug)]
pub struct ServiceError {
    pub service: &'static str,
    pub status: StatusCode,
    pub body: String,
}

impl ServiceError {
    pub fn refused(&self) -> bool {
        self.status.is_client_error()
    }

    pub fn code(&self) -> Option<ErrorCode> {
        serde_json::from_str::<ErrorResponse>(&self.body)
            .ok()
            .map(|error| error.code)
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} answered {}: {}",
            self.service, self.status, self.body
        )
    }
}

impl std::error::Error for ServiceError {}

#[derive(Debug)]
pub struct FeeNotHonoured;

impl std::fmt::Display for FeeNotHonoured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the quote expired before the maker saw its open fee: a fee paid for it is lost",
        )
    }
}

impl std::error::Error for FeeNotHonoured {}

pub(crate) fn refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServiceError>()
        .is_some_and(ServiceError::refused)
}

#[cfg(feature = "sim")]
pub type Exchange =
    std::pin::Pin<Box<dyn Future<Output = reqwest::Result<reqwest::Response>> + Send>>;

#[cfg(feature = "sim")]
pub trait Wire: Send + Sync {
    fn exchange(&self, request: reqwest::Request) -> Exchange;
}

#[derive(Clone)]
pub struct MakerApi(Endpoint);

impl MakerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("maker", url).map(Self)
    }

    #[cfg(feature = "sim")]
    pub fn over(url: impl Into<String>, wire: Arc<dyn Wire>) -> Self {
        Self(Endpoint::over("maker", url, wire))
    }

    pub async fn info(&self) -> Result<zecswap_api::service::MakerInfo> {
        self.0.get("/v1/info").await
    }

    pub async fn reverse_quote(
        &self,
        request: &zecswap_api::reverse::QuoteRequest,
    ) -> Result<zecswap_api::reverse::Quote> {
        self.0.post("/v1/reverse/quote", request).await
    }

    pub async fn accept_reverse(
        &self,
        quote_id: B256,
        acceptance: &Acceptance,
    ) -> Result<Accepted> {
        let path = format!("/v1/reverse/quote/{quote_id}/accept");
        let response = self.0.accept(&path, acceptance).await?;
        self.0.read(response).await
    }

    pub async fn reverse_status(&self, swap_id: B256) -> Result<zecswap_api::reverse::Status> {
        self.0.get(&format!("/v1/reverse/swaps/{swap_id}")).await
    }

    pub(crate) async fn quote(
        &self,
        units: u32,
        payout: Address,
        payout_note: Option<B256>,
    ) -> Result<Quote> {
        let request = QuoteRequest {
            units,
            payout,
            payout_note,
        };
        self.0.post("/v1/quote", &request).await
    }

    pub(crate) async fn accept(
        &self,
        quote_id: B256,
        expires_at: u64,
        paid: bool,
        acceptance: &Acceptance,
    ) -> Result<Accepted> {
        let path = format!("/v1/quote/{quote_id}/accept");
        let mut pace = FEE_WAIT_MIN;
        loop {
            let response = self.0.accept(&path, acceptance).await?;
            let busy = paid && response.status() == StatusCode::SERVICE_UNAVAILABLE;
            if !busy && response.status() != StatusCode::PAYMENT_REQUIRED {
                return self.0.read(response).await;
            }
            let asked = retry_after(&response);
            let refusal = self.0.failure(response).await?;
            if !busy && refusal.code() != Some(ErrorCode::FeeRequired) {
                return Err(refusal.into());
            }
            let left = Duration::from_secs(expires_at.saturating_sub(wall_now()));
            let wait = asked.unwrap_or(FEE_WAIT).clamp(pace, BUSY_WAIT);
            pace = FEE_PACE;
            tokio::time::sleep(wait.min(left)).await;
            if wall_now() >= expires_at {
                return Err(if busy {
                    refusal.into()
                } else {
                    FeeNotHonoured.into()
                });
            }
        }
    }
}

#[derive(Clone)]
pub struct RelayerApi(Endpoint);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broadcast {
    Sent(Vec<B256>),
    Refused(String),
    Spent(Vec<B256>),
    Retry(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deposited {
    Sent(Vec<B256>),
    Refused(String),
    Conflict(Vec<B256>),
    Retry {
        reason: String,
        after: Option<Duration>,
    },
}

impl RelayerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("relayer", url).map(Self)
    }

    #[cfg(feature = "sim")]
    pub fn over(url: impl Into<String>, wire: Arc<dyn Wire>) -> Self {
        Self(Endpoint::over("relayer", url, wire))
    }

    pub async fn shield_deposit(&self, request: &ShieldDeposit) -> Deposited {
        self.deposit("/v1/deposits/shield", request).await
    }

    pub async fn rescue_deposit(&self, request: &RescueDeposit) -> Deposited {
        self.deposit("/v1/deposits/rescue", request).await
    }

    async fn deposit(&self, path: &str, request: &impl Serialize) -> Deposited {
        let url = format!("{}{path}", self.0.url);
        let response = match self.0.send(|| self.0.http.post(&url).json(request)).await {
            Ok(response) => response,
            Err(e) => {
                return Deposited::Retry {
                    reason: e.to_string(),
                    after: None,
                };
            }
        };
        let (status, after) = (response.status(), retry_after(&response));
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return match serde_json::from_str::<Sent>(&body) {
                Ok(sent) => Deposited::Sent(sent.transactions),
                Err(e) => Deposited::Retry {
                    reason: format!("an unreadable answer: {e}"),
                    after: None,
                },
            };
        }
        if status == StatusCode::CONFLICT
            && let Ok(conflict) = serde_json::from_str::<Conflict>(&body)
        {
            return Deposited::Conflict(conflict.transactions);
        }
        let error = serde_json::from_str::<ErrorResponse>(&body).map_or(body, |e| e.error);
        if status.is_client_error() {
            return Deposited::Refused(error);
        }
        Deposited::Retry {
            reason: format!("relayer answered {status}: {error}"),
            after,
        }
    }

    pub fn with_timeout(self, timeout: Duration) -> Result<Self> {
        Ok(Self(Endpoint {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            ..self.0
        }))
    }

    pub async fn fund_reverse(
        &self,
        request: &zecswap_api::reverse::Funding,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/fund", request, ticket)
            .await
    }

    pub async fn ready_reverse(
        &self,
        request: &zecswap_api::reverse::Authorization,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/ready", request, ticket)
            .await
    }

    pub async fn refund_reverse(
        &self,
        request: &zecswap_api::reverse::Refund,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/refund", request, ticket)
            .await
    }

    pub async fn reverse_refund_payout(
        &self,
        request: &Payout,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/refund-payout", request, ticket)
            .await
    }

    pub async fn terms(&self) -> Result<Terms> {
        self.0.get("/v1/terms").await
    }

    pub async fn railgun_transact(&self, request: &RailgunTransact) -> Broadcast {
        let url = format!("{}/v1/railgun/transact", self.0.url);
        let response = match self.0.exchange(self.0.http.post(url).json(request)).await {
            Ok(response) => response,
            Err(e) => return Broadcast::Retry(e.to_string()),
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return match serde_json::from_str::<Sent>(&body) {
                Ok(sent) => Broadcast::Sent(sent.transactions),
                Err(e) => Broadcast::Retry(format!("an unreadable answer: {e}")),
            };
        }
        let error = serde_json::from_str::<ErrorResponse>(&body).ok();
        match (status, error) {
            (StatusCode::CONFLICT, Some(e)) if e.code == ErrorCode::AlreadySpent => {
                Broadcast::Spent(
                    serde_json::from_str::<Conflict>(&body)
                        .map(|spent| spent.transactions)
                        .unwrap_or_default(),
                )
            }
            (status, error) if status.is_client_error() && status != StatusCode::CONFLICT => {
                Broadcast::Refused(error.map_or(body, |e| e.error))
            }
            (status, error) => Broadcast::Retry(format!(
                "relayer answered {status}: {}",
                error.map_or(body, |e| e.error)
            )),
        }
    }

    pub(crate) async fn claim(&self, request: &Claim, ticket: Option<&str>) -> Result<Sent> {
        self.0.post_for_swap("/v1/claim", request, ticket).await
    }

    pub(crate) async fn payout(&self, request: &Payout, ticket: Option<&str>) -> Result<Sent> {
        self.0.post_for_swap("/v1/payout", request, ticket).await
    }
}

#[derive(Clone)]
struct Endpoint {
    name: &'static str,
    http: reqwest::Client,
    url: String,
    #[cfg(feature = "sim")]
    wire: Option<Arc<dyn Wire>>,
}

impl Endpoint {
    fn new(name: &'static str, url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            name,
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url: url.into(),
            #[cfg(feature = "sim")]
            wire: None,
        })
    }

    #[cfg(feature = "sim")]
    fn over(name: &'static str, url: impl Into<String>, wire: Arc<dyn Wire>) -> Self {
        static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        Self {
            name,
            http: HTTP.get_or_init(reqwest::Client::new).clone(),
            url: url.into(),
            wire: Some(wire),
        }
    }

    async fn exchange(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        #[cfg(feature = "sim")]
        if let Some(wire) = &self.wire {
            return wire.exchange(request.build()?).await;
        }
        request.send().await
    }

    async fn accept(&self, path: &str, acceptance: &Acceptance) -> Result<reqwest::Response> {
        let url = format!("{}{path}", self.url);
        Ok(self.send(|| self.http.post(&url).json(acceptance)).await?)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.url);
        self.read(self.send(|| self.http.get(&url)).await?).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.post_for_swap(path, body, None).await
    }

    async fn post_for_swap<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        ticket: Option<&str>,
    ) -> Result<T> {
        let url = format!("{}{path}", self.url);
        let request = || {
            let request = self.http.post(&url).json(body);
            match ticket {
                Some(ticket) => request.header(TICKET_HEADER, ticket),
                None => request,
            }
        };
        self.read(self.send(request).await?).await
    }

    async fn send(
        &self,
        request: impl Fn() -> reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let mut waited = Duration::ZERO;
        loop {
            let response = self.exchange(request()).await?;
            let wait = (response.status() == StatusCode::SERVICE_UNAVAILABLE)
                .then(|| retry_after(&response))
                .flatten()
                .filter(|wait| *wait <= BUSY_WAIT && waited + *wait <= BUSY_PATIENCE);
            let Some(wait) = wait else {
                return Ok(response);
            };
            waited += wait;
            tokio::time::sleep(wait).await;
        }
    }

    async fn read<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
        if response.status().is_success() {
            return Ok(response.json().await?);
        }
        Err(self.failure(response).await?.into())
    }

    async fn failure(&self, response: reqwest::Response) -> Result<ServiceError> {
        Ok(ServiceError {
            service: self.name,
            status: response.status(),
            body: response.text().await?,
        })
    }
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let seconds = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    seconds.trim().parse().ok().map(Duration::from_secs)
}
