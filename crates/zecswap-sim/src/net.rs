use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::service::{ErrorCode, ErrorResponse};
use zecswap_client::{Exchange, Wire};

use crate::seed::Seed;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug)]
pub struct Line {
    pub busy: u64,
    pub hang: u64,
    pub unheard: u64,
    pub unanswered: u64,
}

impl Line {
    pub const CLEAR: Self = Self {
        busy: 0,
        hang: 0,
        unheard: 0,
        unanswered: 0,
    };
}

pub struct Asked {
    pub method: http::Method,
    pub path: String,
    pub body: Vec<u8>,
}

impl Asked {
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, Answer> {
        serde_json::from_slice(&self.body)
            .map_err(|e| Answer::refuse(400, ErrorCode::InvalidRequest, e.to_string()))
    }
}

pub struct Answer {
    pub status: u16,
    pub retry_after: Option<u64>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn ok(body: &impl Serialize) -> Self {
        Self {
            status: 200,
            retry_after: None,
            body: serde_json::to_vec(body).expect("a JSON body"),
        }
    }

    pub fn refuse(status: u16, code: ErrorCode, why: impl Into<String>) -> Self {
        Self {
            status,
            retry_after: None,
            body: serde_json::to_vec(&ErrorResponse {
                code,
                error: why.into(),
            })
            .expect("a JSON body"),
        }
    }

    pub fn after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }
}

pub type Answering = Pin<Box<dyn Future<Output = Answer> + Send>>;

pub trait Served: Send + Sync {
    fn serve(&self, asked: Asked) -> Answering;
}

#[derive(Clone)]
pub struct Net {
    seed: Seed,
    name: &'static str,
    line: Line,
    clears: tokio::time::Instant,
    calls: Arc<AtomicU64>,
    service: Arc<dyn Served>,
    log: Arc<dyn Fn(String) + Send + Sync>,
}

impl Net {
    pub fn to(
        seed: Seed,
        name: &'static str,
        line: Line,
        clears: tokio::time::Instant,
        service: Arc<dyn Served>,
        log: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Arc<dyn Wire> {
        Arc::new(Self {
            seed,
            name,
            line,
            clears,
            calls: Arc::default(),
            service,
            log,
        })
    }

    fn respond(answer: Answer) -> reqwest::Response {
        let mut response = http::Response::builder().status(answer.status);
        if let Some(seconds) = answer.retry_after {
            response = response.header(http::header::RETRY_AFTER, seconds.to_string());
        }
        response = response.header(http::header::CONTENT_TYPE, "application/json");
        reqwest::Response::from(response.body(answer.body).expect("a response"))
    }

    fn cut() -> reqwest::Error {
        Self::respond(Answer {
            status: 504,
            retry_after: None,
            body: Vec::new(),
        })
        .error_for_status()
        .expect_err("a gateway timeout is an error")
    }
}

impl Wire for Net {
    fn exchange(&self, request: reqwest::Request) -> Exchange {
        let this = self.clone();
        Box::pin(async move {
            let call = this.calls.fetch_add(1, Ordering::SeqCst);
            let stream = |what: &str| format!("net-{}-{what}", this.name);
            let path = request.url().path().to_owned();
            let line = match tokio::time::Instant::now() >= this.clears {
                true => Line::CLEAR,
                false => this.line,
            };
            let roll = |what: &str, odds: u64| this.seed.chance(&stream(what), call, odds);
            tokio::time::sleep(Duration::from_millis(this.seed.between(
                &stream("lag"),
                call,
                30,
                900,
            )))
            .await;
            if roll("hang", line.hang) {
                (this.log)(format!("{} hung on {path}", this.name));
                tokio::time::sleep(CLIENT_TIMEOUT).await;
                return Err(Self::cut());
            }
            if roll("unheard", line.unheard) {
                (this.log)(format!("{} never heard {path}", this.name));
                return Err(Self::cut());
            }
            if roll("busy", line.busy) {
                (this.log)(format!("{} was busy for {path}", this.name));
                let wait = this.seed.between(&stream("busy-wait"), call, 1, 40);
                let shed = Answer::refuse(503, ErrorCode::Unavailable, "busy");
                return Ok(Self::respond(match wait > 35 {
                    true => shed,
                    false => shed.after(wait),
                }));
            }
            let asked = Asked {
                method: request.method().clone(),
                path: path.clone(),
                body: request
                    .body()
                    .and_then(|body| body.as_bytes())
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default(),
            };
            let answer = this.service.serve(asked).await;
            if roll("unanswered", line.unanswered) {
                (this.log)(format!("{} lost its answer to {path}", this.name));
                return Err(Self::cut());
            }
            Ok(Self::respond(answer))
        })
    }
}
