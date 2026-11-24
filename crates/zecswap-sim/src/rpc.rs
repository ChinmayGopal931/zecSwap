use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::primitives::Bytes;
use alloy::rpc::json_rpc::{
    ErrorPayload, RequestPacket, Response, ResponsePacket, ResponsePayload, SerializedRequest,
};
use alloy::transports::{
    BoxTransport, IntoBoxTransport, TransportError, TransportErrorKind, TransportFut,
};
use serde_json::{Value, json};
use tower_service::Service;

use crate::node::{Node, Refusal};

const NODE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Rpc {
    node: Node,
    name: String,
    calls: Arc<AtomicU64>,
    private: Option<u64>,
}

impl Node {
    pub fn rpc(&self, name: &str) -> BoxTransport {
        Rpc {
            node: self.clone(),
            name: name.to_owned(),
            calls: Arc::default(),
            private: None,
        }
        .into_box_transport()
    }

    pub fn private_rpc(&self, name: &str, blocks: u64) -> BoxTransport {
        Rpc {
            node: self.clone(),
            name: format!("{name}-private"),
            calls: Arc::default(),
            private: Some(blocks),
        }
        .into_box_transport()
    }
}

impl Rpc {
    fn raw(value: &impl serde::Serialize) -> Box<serde_json::value::RawValue> {
        serde_json::value::to_raw_value(value).expect("a JSON value")
    }

    async fn serve(&self, request: SerializedRequest) -> Result<Response, TransportError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (seed, weather) = self.node.weather();
        let stream = |what: &str| format!("rpc-{}-{what}", self.name);
        if seed.chance(&stream("stall"), call, weather.rpc_stall) {
            tokio::time::sleep(NODE_TIMEOUT).await;
            self.node
                .note(format!("{} stalled on {}", self.name, request.method()));
            return Err(TransportErrorKind::custom_str("the request timed out"));
        }
        tokio::time::sleep(Duration::from_millis(seed.between(
            &stream("lag"),
            call,
            20,
            400,
        )))
        .await;
        let failed = seed.chance(&stream("error"), call, weather.rpc_error);
        let unheard = failed && seed.chance(&stream("unheard"), call, 500);
        if unheard {
            self.node
                .note(format!("{} lost its {}", self.name, request.method()));
            return Err(TransportErrorKind::custom_str("the connection was reset"));
        }
        let stale = seed.chance(&stream("stale"), call, weather.stale_read);
        let params = match request.params() {
            Some(raw) => serde_json::from_str(raw.get()).unwrap_or(Value::Null),
            None => Value::Null,
        };
        let answer = match (self.private, request.method()) {
            (Some(blocks), "eth_sendRawTransaction") => {
                match serde_json::from_value::<Bytes>(params[0].clone()) {
                    Ok(raw) => self
                        .node
                        .take_privately(&raw, blocks)
                        .map(|hash| json!(hash)),
                    Err(e) => Err(Refusal {
                        code: -32602,
                        message: e.to_string(),
                        data: None,
                    }),
                }
            }
            (Some(_), method) if method != "eth_chainId" => Err(Refusal {
                code: -32601,
                message: format!("the private endpoint does not serve {method}"),
                data: None,
            }),
            (_, method) => self.node.answer(method, params, stale),
        };
        if failed {
            self.node.note(format!(
                "{} lost the answer to {}",
                self.name,
                request.method()
            ));
            return Err(TransportErrorKind::custom_str("the connection was reset"));
        }
        let payload = match answer {
            Ok(value) => ResponsePayload::Success(Self::raw(&value)),
            Err(refusal) => ResponsePayload::Failure(ErrorPayload {
                code: refusal.code,
                message: refusal.message.into(),
                data: refusal.data.map(|data| Self::raw(&data)),
            }),
        };
        Ok(Response {
            id: request.id().clone(),
            payload,
        })
    }
}

impl Service<RequestPacket> for Rpc {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), TransportError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: RequestPacket) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            match request {
                RequestPacket::Single(request) => {
                    Ok(ResponsePacket::Single(this.serve(request).await?))
                }
                RequestPacket::Batch(requests) => {
                    let mut answers = Vec::with_capacity(requests.len());
                    for request in requests {
                        answers.push(this.serve(request).await?);
                    }
                    Ok(ResponsePacket::Batch(answers))
                }
            }
        })
    }
}
