use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context as _, ensure};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zecswap_api::server::error;
use zecswap_api::service::ErrorCode;

use crate::{Challenge, DAY, IssuerKey, Token, TokenKey, day, today, www_authenticate};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub issuer: String,
    pub origin: String,
    pub keys: Vec<String>,
    pub return_key: PathBuf,
    pub spent: PathBuf,
}

pub struct Gate {
    challenge: Challenge,
    keys: Vec<TokenKey>,
    returns: IssuerKey,
    state: Mutex<Spent>,
}

struct Spent {
    store: Connection,
    held: HashSet<[u8; 32]>,
    today: Today,
}

enum Refusal {
    Token(Refused, String),
    Store(rusqlite::Error),
}

#[derive(Clone, Copy)]
enum Refused {
    Missing,
    Unreadable,
    Malformed,
    UnknownKey,
    Invalid,
    InUse,
    Spent,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Today {
    pub day: u64,
    pub spent_today: u64,
    pub since: u64,
    pub spent: Kept,
    pub refused: Refusals,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Kept {
    pub issuer: u64,
    pub returned: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Refusals {
    pub missing: u64,
    pub unreadable: u64,
    pub malformed: u64,
    pub unknown_key: u64,
    pub invalid: u64,
    pub in_use: u64,
    pub spent: u64,
}

impl Today {
    fn starting(now: u64) -> Self {
        Self {
            day: day(now),
            since: now,
            ..Self::default()
        }
    }

    fn roll(&mut self, now: u64) {
        if day(now) != self.day {
            *self = Self::starting(day(now) * DAY);
        }
    }

    fn kept(&mut self, returned: bool, now: u64) {
        self.roll(now);
        let kept = &mut self.spent;
        *if returned {
            &mut kept.returned
        } else {
            &mut kept.issuer
        } += 1;
    }

    fn refused(&mut self, why: Refused, now: u64) {
        self.roll(now);
        let refused = &mut self.refused;
        *match why {
            Refused::Missing => &mut refused.missing,
            Refused::Unreadable => &mut refused.unreadable,
            Refused::Malformed => &mut refused.malformed,
            Refused::UnknownKey => &mut refused.unknown_key,
            Refused::Invalid => &mut refused.invalid,
            Refused::InUse => &mut refused.in_use,
            Refused::Spent => &mut refused.spent,
        } += 1;
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

impl Gate {
    pub fn open(config: &Config) -> anyhow::Result<Self> {
        let challenge = Challenge::new(&config.issuer, &config.origin, today())?;
        let keys = config
            .keys
            .iter()
            .map(|key| TokenKey::from_base64(key))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("a token key")?;
        ensure!(!keys.is_empty(), "no token keys");
        let pem = std::fs::read_to_string(&config.return_key)
            .with_context(|| format!("reading {}", config.return_key.display()))?;
        let returns = IssuerKey::from_pem(&pem).context("the return key")?;
        let store = Connection::open(&config.spent)
            .with_context(|| format!("opening {}", config.spent.display()))?;
        store.execute_batch(
            "CREATE TABLE IF NOT EXISTS spent_tokens (nonce BLOB PRIMARY KEY, day INTEGER NOT NULL)",
        )?;
        Ok(Self {
            challenge,
            keys,
            returns,
            state: Mutex::new(Spent {
                store,
                held: HashSet::new(),
                today: Today::starting(now()),
            }),
        })
    }

    pub fn today(&self) -> anyhow::Result<Today> {
        let now = now();
        let mut state = self.state();
        state.today.roll(now);
        let spent_today = state.store.query_row(
            "SELECT count(*) FROM spent_tokens WHERE day = ?1",
            [day(now)],
            |row| row.get(0),
        )?;
        Ok(Today {
            spent_today,
            ..state.today.clone()
        })
    }

    pub fn challenge(&self, day: u64) -> Challenge {
        self.challenge.on(day)
    }

    pub fn return_key(&self) -> &TokenKey {
        self.returns.token_key()
    }

    pub fn read_return_request(&self, request: &str) -> anyhow::Result<Vec<u8>> {
        let blinded = URL_SAFE_NO_PAD
            .decode(request)
            .context("a token request that is not base64url")?;
        self.return_key().check_request(&blinded)?;
        Ok(blinded)
    }

    pub fn sign_return(&self, blinded: &[u8]) -> anyhow::Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(self.returns.sign(blinded)?))
    }

    fn hold(self: &Arc<Self>, headers: &HeaderMap) -> Result<Spend, Refusal> {
        let refuse = |kind: Refused, why: &str| Refusal::Token(kind, why.into());
        let header = headers
            .get(header::AUTHORIZATION)
            .ok_or_else(|| refuse(Refused::Missing, "this request takes a token"))?
            .to_str()
            .map_err(|_| refuse(Refused::Unreadable, "an unreadable authorization"))?;
        let token = Token::from_authorization(header)
            .map_err(|e| refuse(Refused::Malformed, &e.to_string()))?;
        let (key, returned) = match self.keys.iter().find(|key| key.id() == token.key_id()) {
            Some(key) => (key, false),
            None if self.return_key().id() == token.key_id() => (self.return_key(), true),
            None => {
                return Err(refuse(
                    Refused::UnknownKey,
                    "a token under a key not taken here",
                ));
            }
        };
        let day = today();
        token
            .verify(key, &self.challenge(day))
            .map_err(|e| refuse(Refused::Invalid, &e.to_string()))?;
        let nonce = token.nonce();
        let mut state = self.state();
        if state.held.contains(&nonce) {
            return Err(refuse(
                Refused::InUse,
                "a token another request is spending",
            ));
        }
        let spent = state
            .store
            .query_row(
                "SELECT 1 FROM spent_tokens WHERE nonce = ?1",
                [nonce],
                |_| Ok(()),
            )
            .optional()
            .map_err(Refusal::Store)?;
        if spent.is_some() {
            return Err(refuse(Refused::Spent, "a token already spent"));
        }
        state.held.insert(nonce);
        Ok(Spend(Arc::new(Held {
            gate: self.clone(),
            nonce,
            day,
            returned,
        })))
    }

    fn ask(&self) -> HeaderValue {
        HeaderValue::from_str(&www_authenticate(&self.challenge(today()), &self.keys[0]))
            .expect("base64url is a header value")
    }

    fn state(&self) -> MutexGuard<'_, Spent> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone)]
pub struct Spend(Arc<Held>);

struct Held {
    gate: Arc<Gate>,
    nonce: [u8; 32],
    day: u64,
    returned: bool,
}

impl Spend {
    pub fn keep(&self) -> anyhow::Result<()> {
        let Held {
            gate,
            nonce,
            day,
            returned,
        } = &*self.0;
        let mut state = gate.state();
        let kept = state.store.execute(
            "INSERT OR IGNORE INTO spent_tokens (nonce, day) VALUES (?1, ?2)",
            params![nonce, day],
        )?;
        state
            .store
            .execute("DELETE FROM spent_tokens WHERE day < ?1", [day])?;
        if kept == 1 {
            state.today.kept(*returned, now());
        }
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.gate.state().held.remove(&self.nonce);
    }
}

pub async fn require(State(gate): State<Arc<Gate>>, mut request: Request, next: Next) -> Response {
    match gate.hold(request.headers()) {
        Ok(spend) => {
            request.extensions_mut().insert(spend);
            next.run(request).await
        }
        Err(Refusal::Token(kind, why)) => {
            gate.state().today.refused(kind, now());
            let mut response = error(StatusCode::UNAUTHORIZED, ErrorCode::TokenRequired, why);
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, gate.ask());
            response
        }
        Err(Refusal::Store(e)) => {
            tracing::error!(error = %e, "spent tokens are unavailable");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "spent tokens are unavailable",
            )
        }
    }
}
