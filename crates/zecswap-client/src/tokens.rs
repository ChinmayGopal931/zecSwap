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
