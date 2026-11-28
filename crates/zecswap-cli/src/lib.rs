pub mod deposit;
pub mod store;
pub mod swap;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use zcash_address::ZcashAddress;
use zcash_keys::keys::UnifiedSpendingKey;
use zecswap_api::Network;
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Prover, TxId, Wallet};

use crate::store::Store;
#[cfg(feature = "sim")]
use crate::store::Touch;

const POLL_INTERVAL: Duration = Duration::from_secs(20);

pub struct Session {
    pub network: Network,
    pub store: Store,
    pub wallet: Wallet,
    pub client: Lightwalletd,
    pub prover: Arc<Prover>,
}

impl Session {
    pub async fn sync(&mut self) -> Result<()> {
        self.wallet.sync(&mut self.client).await.context("syncing")
    }

    pub async fn sync_until(
        &mut self,
        mut done: impl FnMut(&mut Self) -> Result<bool>,
    ) -> Result<()> {
        loop {
            self.sync().await?;
            if done(self)? {
                return Ok(());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    pub fn account(&self) -> Result<AccountUuid> {
        self.wallet
            .derived_account()?
            .context("no wallet yet; run `init`")
    }

    pub fn pay(&mut self, to: &ZcashAddress, zatoshis: u64) -> Result<TxId> {
        self.pay_all(&[(to.clone(), zatoshis)])
    }

    pub fn pay_all(&mut self, payments: &[(ZcashAddress, u64)]) -> Result<TxId> {
        let network = self.wallet.network();
        let usk =
            UnifiedSpendingKey::from_seed(&network, &self.store.seed()?, zip32::AccountId::ZERO)
                .map_err(|e| anyhow::anyhow!("deriving the spending key: {e:?}"))?;
        let account = self.account()?;
        Ok(self.wallet.pay(&self.prover, account, &usk, payments)?)
    }

    pub async fn broadcast(&mut self, txid: TxId) -> Result<()> {
        #[cfg(feature = "sim")]
        self.store.touch(Touch::Sending(txid));
        self.wallet.broadcast(&mut self.client, txid).await?;
        #[cfg(feature = "sim")]
        self.store.touch(Touch::Sent(txid));
        Ok(())
    }
}
