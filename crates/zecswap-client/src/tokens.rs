use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::B256;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use zecswap_api::tokens::{
    Attestation, AttestationChallenge, TokenKey as Published, TokenRequests, TokenResponses,
};
use zecswap_tokens::{Challenge, Pending, Token, TokenKey, day, read_www_authenticate};

const CLOCK_SKEW: u64 = 10 * 60;

pub trait Attest: Send + Sync {
    fn attest(&self, challenge: &str, blinded: &[Vec<u8>]) -> Result<Attestation>;
}

pub struct Unattested(pub Vec<u8>);

impl Attest for Unattested {
    fn attest(&self, challenge: &str, _: &[Vec<u8>]) -> Result<Attestation> {
        Ok(Attestation {
            challenge: challenge.into(),
            chain: vec![URL_SAFE_NO_PAD.encode(&self.0)],
            signature: String::new(),
        })
    }
}

pub struct Tokens {
    issuer: String,
    attest: Box<dyn Attest>,
    batch: usize,
    return_key: TokenKey,
    http: reqwest::Client,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    held: HashMap<(Challenge, [u8; 32]), Vec<Token>>,
    returns: HashMap<B256, Vec<(Challenge, Pending)>>,
    exhausted: Option<u64>,
}

pub(crate) struct Payment {
    pub(crate) token: Token,
    pub(crate) request: String,
    challenge: Challenge,
    pending: Pending,
}

impl Tokens {
    pub fn new(
        issuer: impl Into<String>,
        attest: impl Attest + 'static,
        batch: usize,
        return_key: &str,
    ) -> Result<Self> {
        ensure!(batch > 0, "a token batch of none");
        Ok(Self {
            issuer: issuer.into(),
            attest: Box::new(attest),
            batch,
            return_key: TokenKey::from_base64(return_key).context("the return key")?,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            state: Mutex::default(),
        })
    }

    pub(crate) async fn pay(&self, asked: &str) -> Result<Payment> {
        let (challenge, key) = read_www_authenticate(asked)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(
            (day(now.saturating_sub(CLOCK_SKEW))..=day(now + CLOCK_SKEW))
                .contains(&challenge.day()),
            "the maker asks for tokens for another day"
        );
        let today = challenge.day();
        {
            let mut state = self.state();
            state.held.retain(|(held, _), _| held.day() >= today);
            state.returns.retain(|_, requests| {
                requests.retain(|(challenge, _)| challenge.day() >= today);
                !requests.is_empty()
            });
        }
        let exhausted = self.state().exhausted == Some(today);
        let token = match self.take(&challenge, key.id()) {
            Some(token) => token,
            None if exhausted => self.returned(&challenge)?,
            None => match self.fetch(&challenge, &key).await? {
                Some(token) => token,
                None => {
                    self.state().exhausted = Some(today);
                    self.returned(&challenge)?
                }
            },
        };
        let (pending, blinded) = Pending::new(&self.return_key, &challenge)?;
        Ok(Payment {
            token,
            request: URL_SAFE_NO_PAD.encode(blinded),
            challenge,
            pending,
        })
    }

    pub(crate) fn settle(&self, swap_id: B256, payment: Payment, status: Option<StatusCode>) {
        let Payment {
            token,
            challenge,
            pending,
            ..
        } = payment;
        if status == Some(StatusCode::UNAUTHORIZED) {
            return;
        }
        let mut state = self.state();
        if !status.is_some_and(|status| status.is_success()) {
            let held = (challenge.clone(), token.key_id());
            state.held.entry(held).or_default().push(token);
        }
        state
            .returns
            .entry(swap_id)
            .or_default()
            .push((challenge, pending));
    }

    pub(crate) fn collect(&self, swap_id: B256, token_return: Option<&str>) -> Result<bool> {
        let Some(token_return) = token_return else {
            return Ok(false);
        };
        let signature = URL_SAFE_NO_PAD.decode(token_return)?;
        let mut state = self.state();
        let requests = state
            .returns
            .remove(&swap_id)
            .context("no request for this swap's token is pending")?;
        for (challenge, pending) in requests {
            if let Ok(token) = pending.finalize(&self.return_key, &signature) {
                let held = (challenge, self.return_key.id());
                state.held.entry(held).or_default().push(token);
                return Ok(true);
            }
        }
        bail!("the maker's signature finalizes none of the swap's requests")
    }

    fn take(&self, challenge: &Challenge, key: [u8; 32]) -> Option<Token> {
        self.state()
            .held
            .get_mut(&(challenge.clone(), key))
            .and_then(Vec::pop)
    }

    fn returned(&self, challenge: &Challenge) -> Result<Token> {
        self.take(challenge, self.return_key.id())
            .ok_or_else(|| anyhow!("this device's tokens for today are spent"))
    }

    async fn fetch(&self, challenge: &Challenge, key: &TokenKey) -> Result<Option<Token>> {
        let published: Published = self
            .http
            .get(format!("{}/v1/token-key", self.issuer))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            published.token_key == key.to_base64() && published.issuer == challenge.issuer(),
            "the service asks for tokens its issuer does not sign"
        );
        let (pending, blinded): (Vec<Pending>, Vec<Vec<u8>>) = (0..self.batch)
            .map(|_| Pending::new(key, challenge))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip();
        let attesting: AttestationChallenge = self
            .http
            .get(format!("{}/v1/challenge", self.issuer))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let request = TokenRequests {
            attestation: self.attest.attest(&attesting.challenge, &blinded)?,
            blinded: blinded.iter().map(|b| URL_SAFE_NO_PAD.encode(b)).collect(),
        };
        let response = self
            .http
            .post(format!("{}/v1/tokens", self.issuer))
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Ok(None);
        }
        ensure!(
            status.is_success(),
            "the issuer answered {status}: {}",
            response.text().await?
        );
        let issued: TokenResponses = response.json().await?;
        ensure!(
            (1..=pending.len()).contains(&issued.blind_signatures.len()),
            "the issuer signed another number of tokens"
        );
        let mut tokens = pending
            .into_iter()
            .zip(issued.blind_signatures)
            .map(|(pending, signature)| pending.finalize(key, &URL_SAFE_NO_PAD.decode(signature)?))
            .collect::<Result<Vec<_>>>()
            .context("the issuer's signatures")?;
        let token = tokens.pop().expect("a batch of at least one");
        self.state()
            .held
            .entry((challenge.clone(), key.id()))
            .or_default()
            .extend(tokens);
        Ok(Some(token))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use axum::extract::{Path, Request, State};
    use axum::http::header::AUTHORIZATION;
    use axum::middleware::{Next, from_fn_with_state};
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Extension, Json, Router};
    use rand::{rand_core::UnwrapErr, rngs::SysRng};
    use zecswap_api::{Acceptance, Accepted, Status};
    use zecswap_core::{SecretShare, ShareProof, ViewingKeys};
    use zecswap_tokens::server::{Config, Gate, Spend, require};
    use zecswap_tokens::{IssuerKey, today, www_authenticate};

    use super::*;
    use crate::MakerApi;

    const REFUSED: B256 = B256::repeat_byte(0xff);

    struct Maker {
        gate: Arc<Gate>,
        sent: Mutex<Vec<Token>>,
        requests: Mutex<Vec<(B256, String)>>,
        paid: Mutex<Vec<B256>>,
    }

    struct Issuer {
        key: IssuerKey,
        per_day: usize,
        issued: Mutex<HashMap<Vec<String>, usize>>,
    }

    pub(crate) async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    fn issuer(key: &IssuerKey, per_day: usize) -> Router {
        async fn token_key(State(issuer): State<Arc<Issuer>>) -> Json<Published> {
            Json(Published {
                issuer: "issuer.test".into(),
                token_key: issuer.key.token_key().to_base64(),
                tokens_per_day: issuer.per_day as u32,
            })
        }
        async fn challenge() -> Json<AttestationChallenge> {
            Json(AttestationChallenge {
                challenge: URL_SAFE_NO_PAD.encode([7; 32]),
            })
        }
        async fn tokens(
            State(issuer): State<Arc<Issuer>>,
            Json(request): Json<TokenRequests>,
        ) -> Result<Json<TokenResponses>, StatusCode> {
            let mut issued = issuer.issued.lock().unwrap();
            let count = issued.entry(request.attestation.chain).or_default();
            let left = issuer.per_day - *count;
            if left == 0 {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            let blind_signatures: Vec<String> = request.blinded[..left.min(request.blinded.len())]
                .iter()
                .map(|blinded| {
                    let blinded = URL_SAFE_NO_PAD.decode(blinded).unwrap();
                    URL_SAFE_NO_PAD.encode(issuer.key.sign(&blinded).unwrap())
                })
                .collect();
            *count += blind_signatures.len();
            Ok(Json(TokenResponses { blind_signatures }))
        }
        let issuer = Issuer {
            key: IssuerKey::from_pem(&key.to_pem().unwrap()).unwrap(),
            per_day,
            issued: Mutex::default(),
        };
        Router::new()
            .route("/v1/token-key", get(token_key))
            .route("/v1/challenge", get(challenge))
            .route("/v1/tokens", post(tokens))
            .with_state(Arc::new(issuer))
    }

    async fn maker(dir: &std::path::Path, key: &IssuerKey) -> (Arc<Maker>, String) {
        let return_key = dir.join("return.pem");
        std::fs::write(
            &return_key,
            IssuerKey::generate().unwrap().to_pem().unwrap(),
        )
        .unwrap();
        let gate = Gate::open(&Config {
            issuer: "issuer.test".into(),
            origin: "maker".into(),
            keys: vec![key.token_key().to_base64()],
            return_key,
            spent: dir.join(format!("{}.sqlite", key.token_key().to_base64().len())),
        })
        .unwrap();
        let maker = Arc::new(Maker {
            gate: Arc::new(gate),
            sent: Mutex::default(),
            requests: Mutex::default(),
            paid: Mutex::default(),
        });
        async fn accept(
            State(maker): State<Arc<Maker>>,
            Path(quote): Path<B256>,
            Extension(spend): Extension<Spend>,
            Json(acceptance): Json<Acceptance>,
        ) -> Result<Json<Accepted>, StatusCode> {
            if quote == REFUSED {
                return Err(StatusCode::BAD_REQUEST);
            }
            spend.keep().unwrap();
            let request = acceptance.token_request.unwrap();
            maker.requests.lock().unwrap().push((quote, request));
            Ok(Json(Accepted {
                swap_id: quote,
                t0: 0,
                t1: 0,
                late_window: None,
                ticket: String::new(),
            }))
        }
        async fn status(
            State(maker): State<Arc<Maker>>,
            Path(swap_id): Path<B256>,
        ) -> Json<Status> {
            let paid = maker.paid.lock().unwrap().contains(&swap_id);
            let requests = maker.requests.lock().unwrap();
            let request = requests.iter().find(|(id, _)| *id == swap_id);
            let token_return = request.filter(|_| paid).map(|(_, request)| {
                let blinded = maker.gate.read_return_request(request).unwrap();
                maker.gate.sign_return(&blinded).unwrap()
            });
            Json(Status {
                swap_id,
                token_return,
            })
        }
        async fn record(State(maker): State<Arc<Maker>>, request: Request, next: Next) -> Response {
            if let Some(header) = request.headers().get(AUTHORIZATION) {
                let token = Token::from_authorization(header.to_str().unwrap()).unwrap();
                maker.sent.lock().unwrap().push(token);
            }
            next.run(request).await
        }
        let app = Router::new()
            .route("/v1/quote/{quote}/accept", post(accept))
            .route_layer(from_fn_with_state(maker.gate.clone(), require))
            .route("/v1/swaps/{swap_id}", get(status))
            .layer(from_fn_with_state(maker.clone(), record))
            .with_state(maker.clone());
        let url = serve(app).await;
        (maker, url)
    }

    fn acceptance() -> Acceptance {
        Acceptance {
            user_share: SecretShare::random(UnwrapErr(SysRng)).public(),
            user_proof: ShareProof::from_bytes([0; 64]),
            viewing_keys: ViewingKeys::random(UnwrapErr(SysRng)),
            token_request: None,
        }
    }

    #[tokio::test]
    async fn accepts_spend_the_issuers_tokens_then_those_handed_back() {
        let dir = tempfile::tempdir().unwrap();
        let key = IssuerKey::generate().unwrap();
        let issuer = serve(issuer(&key, 2)).await;
        let (fake, url) = maker(dir.path(), &key).await;
        let return_key = fake.gate.return_key().clone();
        let phone = Unattested(b"phone".to_vec());
        let tokens = Tokens::new(&issuer, phone, 2, &return_key.to_base64()).unwrap();
        let tokens = Arc::new(tokens);
        let api = MakerApi::new(url.clone())
            .unwrap()
            .with_tokens(tokens.clone());
        let accept = |id: u8| {
            let api = api.clone();
            let id = B256::repeat_byte(id);
            async move { api.accept(id, 0, false, id, &acceptance()).await }
        };
        let sent = || fake.sent.lock().unwrap().clone();

        accept(1).await.unwrap();
        let refused = api.accept(REFUSED, 0, false, REFUSED, &acceptance()).await;
        assert!(refused.unwrap_err().to_string().contains("400"));
        assert!(
            !api.collect_token(B256::repeat_byte(1)).await.unwrap(),
            "not paid yet"
        );
        fake.paid.lock().unwrap().push(B256::repeat_byte(1));
        assert!(api.collect_token(B256::repeat_byte(1)).await.unwrap());
        accept(2).await.unwrap();
        let [first, refused, again] = <[Token; 3]>::try_from(sent()).unwrap();
        assert_eq!(
            again, refused,
            "the refused accept's token paid for the next"
        );
        assert!(
            [first, again]
                .iter()
                .all(|t| t.key_id() == key.token_key().id())
        );

        accept(3).await.unwrap();
        assert_eq!(
            sent()[3].key_id(),
            return_key.id(),
            "the issuer had none left"
        );

        let challenge = fake.gate.challenge(today());
        let (pending, blinded) = Pending::new(&return_key, &challenge.on(today() - 1)).unwrap();
        let stale = B256::repeat_byte(4);
        let request = (stale, URL_SAFE_NO_PAD.encode(blinded));
        fake.requests.lock().unwrap().push(request);
        fake.paid.lock().unwrap().push(stale);
        let returns = (challenge.on(today() - 1), pending);
        tokens.state().returns.insert(stale, vec![returns]);
        assert!(api.collect_token(stale).await.unwrap());
        let spent = accept(5).await.unwrap_err().to_string();
        assert!(spent.contains("spent"), "{spent}");
        assert_eq!(sent().len(), 4, "yesterday's token was sent");

        let tomorrow = www_authenticate(&challenge.on(today() + 2), key.token_key());
        let refused = tokens.pay(&tomorrow).await.err().unwrap().to_string();
        assert!(refused.contains("another day"), "{refused}");
        let marking = IssuerKey::generate().unwrap();
        let (elsewhere, url) = maker(dir.path(), &marking).await;
        let tablet = Unattested(b"tablet".to_vec());
        let fresh = Tokens::new(&issuer, tablet, 2, &return_key.to_base64());
        let api = MakerApi::new(url)
            .unwrap()
            .with_tokens(Arc::new(fresh.unwrap()));
        let refused = api
            .accept(B256::ZERO, 0, false, B256::ZERO, &acceptance())
            .await;
        let refused = refused.unwrap_err().to_string();
        assert!(refused.contains("does not sign"), "{refused}");
        assert!(elsewhere.sent.lock().unwrap().is_empty());
    }
}
