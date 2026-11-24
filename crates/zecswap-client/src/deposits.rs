use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, bail, ensure};
use tokio::sync::OnceCell;
use zecswap_api::Network;
use zecswap_api::relayer::{DepositTerms, Note, RescueDeposit, ShieldDeposit, deposit_fee};
use zecswap_chain::evm::deposits::{Deployment, Used};
use zecswap_chain::evm::railgun::Status;
use zecswap_chain::evm::{Settlement, Shielded};
use zecswap_core::{
    DepositKeys, DepositsDomain, ReceiveAuthorization, RescueAuthorization, derive_deposit_keys,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};
use zeroize::Zeroizing;

use crate::api::{Deposited, RelayerApi};
use crate::user::RAILGUN_WALLET;

const VALIDITY: u64 = 60 * 60;
const LANDING: u64 = 15 * 60;
const CLOCK_SLACK: u64 = 60;
const PATIENCE: Duration = Duration::from_secs(30 * 60);
const BACKOFF: Duration = Duration::from_secs(15);
const LONGEST_WAIT: Duration = Duration::from_secs(5 * 60);
const REPOST: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(3);
pub const GAP: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositDeployment {
    pub chain_id: u64,
    pub contract: Address,
    pub token: Address,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeeCap {
    pub fee: u128,
    pub fee_bps: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deposit {
    pub tx: B256,
    pub shielded: Shielded,
}

pub struct Depositor {
    seed: Zeroizing<Vec<u8>>,
    network: Network,
    account: u32,
    railgun: RailgunKeys,
    chain: Settlement,
    relayer: RelayerApi,
    deployment: DepositDeployment,
    cap: FeeCap,
    contract: OnceCell<Deployment>,
}

impl Depositor {
    pub fn new(
        seed: &[u8],
        railgun_seed: &[u8],
        network: Network,
        chain: Settlement,
        relayer: RelayerApi,
        deployment: DepositDeployment,
        cap: FeeCap,
    ) -> Self {
        Self {
            seed: Zeroizing::new(seed.to_vec()),
            network,
            account: 0,
            railgun: RailgunKeys::from_seed(railgun_seed, RAILGUN_WALLET),
            chain,
            relayer,
            deployment,
            cap,
            contract: OnceCell::new(),
        }
    }

    pub fn with_account(mut self, account: u32) -> Self {
        self.account = account;
        self
    }

    pub fn keys(&self, index: u32) -> Result<DepositKeys> {
        Ok(derive_deposit_keys(
            &self.seed,
            self.network.zcash_type(),
            self.account,
            index,
        )?)
    }

    pub fn address(&self, index: u32) -> Result<Address> {
        Ok(self.keys(index)?.key.address().into())
    }

    pub fn railgun(&self) -> &RailgunKeys {
        &self.railgun
    }

    pub async fn terms(&self) -> Result<DepositTerms> {
        let terms = self.relayer.terms().await?;
        ensure!(
            terms.chain_id == self.deployment.chain_id,
            "the relayer serves another chain"
        );
        let deposits = terms.deposits.context("the relayer takes no deposits")?;
        ensure!(
            (deposits.contract, deposits.token)
                == (self.deployment.contract, self.deployment.token),
            "the relayer takes deposits for another deployment"
        );
        Ok(deposits)
    }

    pub async fn scan(&self, first: u32, count: u32) -> Result<Vec<Used>> {
        let addresses = (first..first + count)
            .map(|index| self.address(index))
            .collect::<Result<Vec<_>>>()?;
        let DepositDeployment {
            contract, token, ..
        } = self.deployment;
        Ok(self
            .chain
            .deposit_addresses(contract, token, &addresses)
            .await?)
    }

    pub async fn recover(&self) -> Result<Vec<(u32, Used)>> {
        let (mut used, mut first) = (Vec::new(), 0);
        loop {
            let window = self.scan(first, GAP).await?;
            let found = window.iter().any(Used::used);
            used.extend(
                window
                    .into_iter()
                    .zip(first..)
                    .filter(|(state, _)| state.used())
                    .map(|(state, index)| (index, state)),
            );
            if !found {
                return Ok(used);
            }
            first += GAP;
        }
    }

    pub async fn showing(&self) -> Result<u32> {
        let used = self.recover().await?;
        let shielded = |index| {
            used.iter()
                .any(|(used, state)| *used == index && state.deposited)
        };
        Ok((0..).find(|index| !shielded(*index)).unwrap_or(u32::MAX))
    }

    pub async fn sign_shield(&self, index: u32) -> Result<Option<ShieldDeposit>> {
        let terms = self.terms().await?;
        let token = &self.contract().await?.token_domain;
        let keys = self.keys(index)?;
        let from = keys.key.address();
        let value = self
            .chain
            .token_balance(self.deployment.token, from.into())
            .await?;
        if value < terms.min_value {
            return Ok(None);
        }
        let fee = deposit_fee(terms.fee, terms.fee_bps, value);
        let cap = deposit_fee(self.cap.fee, self.cap.fee_bps, value);
        ensure!(
            fee <= cap && fee < value,
            "the relayer asks a fee of {fee} of {value}, over the {cap} this wallet pays"
        );
        let valid_before = now() + validity(&terms);
        let note = self.railgun.note(&keys.shield_entropy(valid_before))?;
        let nonce = self
            .domain()
            .deposit_nonce(&note.words(), fee, &terms.relayer.into());
        let digest = token.receive_with_authorization(&ReceiveAuthorization {
            from,
            to: self.deployment.contract.into(),
            value,
            valid_after: 0,
            valid_before,
            nonce,
        });
        Ok(Some(ShieldDeposit {
            chain_id: self.deployment.chain_id,
            contract: self.deployment.contract,
            from: from.into(),
            value,
            valid_after: 0,
            valid_before,
            note: (&note).into(),
            fee,
            signature: keys.key.sign(&digest).into(),
        }))
    }

    pub async fn shield(
        &self,
        index: u32,
        keep: impl FnMut(&ShieldDeposit) -> Result<()>,
    ) -> Result<Option<Deposit>> {
        let Some(request) = self.sign_shield(index).await? else {
            return Ok(None);
        };
        self.post_shield(index, request, keep).await.map(Some)
    }

    pub async fn post_shield(
        &self,
        index: u32,
        mut request: ShieldDeposit,
        mut keep: impl FnMut(&ShieldDeposit) -> Result<()>,
    ) -> Result<Deposit> {
        keep(&request)?;
        let give_up = Instant::now() + PATIENCE;
        let (mut wait, mut signed_again) = (BACKOFF, false);
        loop {
            if request.valid_before < now() + LANDING + CLOCK_SLACK {
                request = self.sign_again(index).await?;
                keep(&request)?;
            }
            match self.relayer.shield_deposit(&request).await {
                Deposited::Sent(sent) if !sent.is_empty() => {
                    if let Some(deposit) = self
                        .landed(&sent, request.from, Some(&request.note))
                        .await?
                    {
                        return Ok(deposit);
                    }
                    ensure!(Instant::now() < give_up, "the deposit never landed");
                }
                Deposited::Conflict(sent) if !sent.is_empty() => {
                    if let Some(deposit) = self.landed(&sent, request.from, None).await? {
                        return Ok(deposit);
                    }
                    ensure!(Instant::now() < give_up, "the deposit never landed");
                }
                Deposited::Conflict(_) => bail!(
                    "the authorization is used in a transaction the relayer doesn't name: read the address's deposits from the chain"
                ),
                Deposited::Refused(reason) if !signed_again => {
                    signed_again = true;
                    request = self
                        .sign_again(index)
                        .await
                        .with_context(|| format!("the relayer refused the deposit: {reason}"))?;
                    keep(&request)?;
                }
                Deposited::Refused(reason) => bail!("the relayer refused the deposit: {reason}"),
                Deposited::Sent(_) => bail!("the relayer sent nothing"),
                Deposited::Retry { reason, after } => {
                    ensure!(
                        Instant::now() < give_up,
                        "the relayer never sent the deposit: {reason}"
                    );
                    signed_again = false;
                    tokio::time::sleep(after.unwrap_or(wait).min(LONGEST_WAIT)).await;
                    wait = (wait * 2).min(LONGEST_WAIT);
                }
            }
        }
    }

    pub async fn rescue(
        &self,
        index: u32,
        mut keep: impl FnMut(&RescueDeposit) -> Result<()>,
    ) -> Result<Option<Deposit>> {
        let Some(request) = self.sign_rescue(index).await? else {
            return Ok(None);
        };
        keep(&request)?;
        let give_up = Instant::now() + PATIENCE;
        let mut wait = BACKOFF;
        loop {
            match self.relayer.rescue_deposit(&request).await {
                Deposited::Sent(sent) if !sent.is_empty() => {
                    if let Some(deposit) = self
                        .landed(&sent, request.from, Some(&request.note))
                        .await?
                    {
                        return Ok(Some(deposit));
                    }
                    ensure!(Instant::now() < give_up, "the rescue never landed");
                }
                Deposited::Conflict(sent) if !sent.is_empty() => {
                    if let Some(deposit) = self.landed(&sent, request.from, None).await? {
                        return Ok(Some(deposit));
                    }
                    ensure!(Instant::now() < give_up, "the rescue never landed");
                }
                Deposited::Retry { reason, after } => {
                    ensure!(
                        Instant::now() < give_up,
                        "the relayer never sent the rescue: {reason}"
                    );
                    tokio::time::sleep(after.unwrap_or(wait).min(LONGEST_WAIT)).await;
                    wait = (wait * 2).min(LONGEST_WAIT);
                }
                other => bail!("the relayer did not rescue: {other:?}"),
            }
        }
    }

    pub async fn sign_rescue(&self, index: u32) -> Result<Option<RescueDeposit>> {
        let terms = self.terms().await?;
        let keys = self.keys(index)?;
        let from = keys.key.address();
        let contract = self.deployment.contract;
        let vault = self.chain.deposit_vault(contract, from.into()).await?;
        let held = self
            .chain
            .token_balance(self.deployment.token, vault)
            .await?;
        let fee = terms.fee;
        ensure!(
            fee <= self.cap.fee,
            "the relayer asks a fee of {fee}, over the {} this wallet pays",
            self.cap.fee
        );
        if held <= fee {
            return Ok(None);
        }
        let nonce = self
            .chain
            .deposit_rescue_nonce(contract, from.into())
            .await?;
        let note = self.railgun.note(&keys.rescue_entropy(nonce))?;
        let authorization = RescueAuthorization {
            nonce,
            deadline: now() + validity(&terms),
        };
        let digest = self.domain().rescue(
            &from,
            &note.commitment(),
            &terms.relayer.into(),
            fee,
            authorization,
        );
        Ok(Some(RescueDeposit {
            chain_id: self.deployment.chain_id,
            contract,
            from: from.into(),
            note: (&note).into(),
            fee,
            nonce,
            deadline: authorization.deadline,
            signature: keys.key.sign(&digest).into(),
        }))
    }

    async fn sign_again(&self, index: u32) -> Result<ShieldDeposit> {
        self.sign_shield(index)
            .await?
            .context("the address no longer holds enough to deposit")
    }

    async fn landed(
        &self,
        sent: &[B256],
        from: Address,
        note: Option<&Note>,
    ) -> Result<Option<Deposit>> {
        let tx = *sent.last().context("no transaction")?;
        let deadline = Instant::now() + REPOST;
        loop {
            match self.chain.transaction_status(tx).await? {
                Status::Mined { succeeded: true } => break,
                Status::Mined { succeeded: false } => bail!(
                    "the deposit {tx} reverted: the relayer holds the address back for an hour"
                ),
                Status::Pending | Status::Unknown => {}
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(POLL).await;
        }
        let expected = note.map(|note| B256::from(ShieldNote::from(note).commitment()));
        let ours = |commitment: B256| expected.is_none_or(|expected| commitment == expected);
        let recorded = self
            .chain
            .deposits_landed(self.deployment.contract, tx)
            .await?;
        ensure!(
            recorded
                .iter()
                .any(|landed| landed.from == from && ours(landed.note)),
            "{tx} is no deposit of this address to its note"
        );
        let shielded = self
            .chain
            .shielded(tx)
            .await?
            .into_iter()
            .find(|shielded| {
                self.railgun.open(&shielded.note).is_some()
                    && ours(shielded.note.commitment().into())
            })
            .with_context(|| format!("{tx} shielded no note of this wallet's"))?;
        Ok(Some(Deposit { tx, shielded }))
    }

    async fn contract(&self) -> Result<&Deployment> {
        self.contract
            .get_or_try_init(|| async {
                let deployment = self.chain.deposits(self.deployment.contract).await?;
                ensure!(
                    deployment.token == self.deployment.token
                        && deployment.token_domain.chain_id == self.deployment.chain_id,
                    "the Deposits contract takes another token or is on another chain"
                );
                Ok(deployment)
            })
            .await
    }

    fn domain(&self) -> DepositsDomain {
        DepositsDomain {
            chain_id: self.deployment.chain_id,
            contract: self.deployment.contract.into(),
        }
    }
}

fn validity(terms: &DepositTerms) -> u64 {
    VALIDITY.min(terms.max_validity.saturating_sub(CLOCK_SLACK))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
