use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{TransportError, TransportFut};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tower_layer::Layer;
use tower_service::Service;
use tracing::info;

#[derive(Clone, Default)]
pub struct RpcMeter(Arc<Mutex<RpcUsage>>);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RpcUsage {
    pub requests: BTreeMap<String, u64>,
    pub failed: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcReport {
    pub endpoint: String,
    pub requests: u64,
    pub compute_units: u64,
    pub failed: u64,
    pub methods: BTreeMap<String, u64>,
}

impl RpcMeter {
    pub fn usage(&self) -> RpcUsage {
        self.0.lock().unwrap().clone()
    }

    pub fn report(&self, endpoint: String) -> RpcReport {
        let usage = self.usage();
        RpcReport {
            endpoint,
            requests: usage.total(),
            compute_units: usage.compute_units(),
            failed: usage.failed,
            methods: usage.requests,
        }
    }

    pub(crate) fn layer(&self) -> MeterLayer {
        MeterLayer(self.clone())
    }
}

impl RpcUsage {
    pub fn total(&self) -> u64 {
        self.requests.values().sum()
    }

    pub fn compute_units(&self) -> u64 {
        self.requests
            .iter()
            .map(|(method, count)| compute_units(method) * count)
            .sum()
    }

    pub fn since(&self, earlier: &RpcUsage) -> RpcUsage {
        RpcUsage {
            requests: self
                .requests
                .iter()
                .map(|(method, count)| {
                    let before = earlier.requests.get(method).copied().unwrap_or(0);
                    (method.clone(), count.saturating_sub(before))
                })
                .filter(|(_, count)| *count > 0)
                .collect(),
            failed: self.failed.saturating_sub(earlier.failed),
        }
    }

    pub fn methods(&self) -> String {
        let mut methods: Vec<_> = self.requests.iter().collect();
        methods.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        methods
            .iter()
            .map(|(method, count)| format!("{method}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub fn compute_units(method: &str) -> u64 {
    match method {
        "eth_chainId" => 0,
        "eth_blockNumber" | "eth_feeHistory" | "eth_maxPriorityFeePerGas" => 10,
        "eth_call" => 26,
        "eth_sendRawTransaction" => 40,
        "eth_getLogs" => 60,
        _ => 20,
    }
}

pub async fn log_usage(meter: RpcMeter, period: Duration) {
    let mut last = meter.usage();
    loop {
        tokio::time::sleep(period).await;
        let usage = meter.usage();
        let sent = usage.since(&last);
        info!(
            requests = sent.total(),
            compute_units = sent.compute_units(),
            failed = sent.failed,
            methods = %sent.methods(),
            period_seconds = period.as_secs(),
            "RPC requests in the last period"
        );
        last = usage;
    }
}

pub fn endpoint(url: &str) -> String {
    let Ok(url) = reqwest::Url::parse(url) else {
        return "an unreadable URL".into();
    };
    let host = url.host_str().unwrap_or("no host");
    let key = url
        .path_segments()
        .into_iter()
        .flatten()
        .rfind(|segment| !segment.is_empty())
        .or(url.query().filter(|query| !query.is_empty()));
    match key {
        Some(key) => {
            let digest = Sha256::digest(key.as_bytes());
            format!("{host}, key {}", hex_prefix(&digest))
        }
        None => format!("{host}, no key"),
    }
}

fn hex_prefix(digest: &[u8]) -> String {
    digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone)]
pub(crate) struct MeterLayer(RpcMeter);

impl<S> Layer<S> for MeterLayer {
    type Service = Metered<S>;

    fn layer(&self, inner: S) -> Metered<S> {
        Metered {
            inner,
            meter: self.0.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Metered<S> {
    inner: S,
    meter: RpcMeter,
}

impl<S> Service<RequestPacket> for Metered<S>
where
    S: Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        >,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        {
            let mut usage = self.meter.0.lock().unwrap();
            for method in request.method_names() {
                *usage.requests.entry(method.to_owned()).or_default() += 1;
            }
        }
        let meter = self.meter.clone();
        let response = self.inner.call(request);
        Box::pin(async move {
            let response = response.await;
            if response.is_err() {
                meter.0.lock().unwrap().failed += 1;
            }
            response
        })
    }
}
