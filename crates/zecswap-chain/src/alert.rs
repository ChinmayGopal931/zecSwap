use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use alloy::primitives::{Address, B256};

pub type Alerts = Arc<dyn Fn(&Alert) + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unlanded {
    pub hash: B256,
    pub block: u64,
    pub max_fee_per_gas: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alert {
    PrivateNonceStuck {
        account: Address,
        signing: u64,
        chain: u64,
        window: u64,
        raised_by: B256,
        raised_at: u64,
        dropped: B256,
    },
    SendQueued {
        account: Address,
        waiting: B256,
        nonce: u64,
        behind: u64,
        blocks: u64,
        max_fee_per_gas: u128,
        unlanded: Option<Unlanded>,
    },
    WalletRewindRefused {
        wallet: String,
        reorg_at: u32,
        rewind: u32,
        refusal: String,
    },
}

impl Alert {
    pub fn name(&self) -> &'static str {
        match self {
            Alert::PrivateNonceStuck { .. } => "private_nonce_stuck",
            Alert::SendQueued { .. } => "send_queued",
            Alert::WalletRewindRefused { .. } => "wallet_rewind_refused",
        }
    }

    pub fn key(&self) -> String {
        let name = self.name();
        match self {
            Alert::PrivateNonceStuck {
                account, signing, ..
            } => format!("{name}:{account}:{signing}"),
            Alert::SendQueued {
                account, behind, ..
            } => format!("{name}:{account}:{behind}"),
            Alert::WalletRewindRefused {
                wallet, reorg_at, ..
            } => format!("{name}:{wallet}:{reorg_at}"),
        }
    }
}

impl fmt::Display for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Alert::PrivateNonceStuck {
                account,
                signing,
                chain,
                window,
                raised_by,
                raised_at,
                dropped,
            } => write!(
                f,
                "Private sends from {account} are stuck. They are signed as transaction number \
                 {signing}, and the chain expects number {chain}: {raised_by} was seen mined in \
                 block {raised_at} and is no longer counted, and {dropped}, sent after it, was \
                 dropped unmined. Nothing this account sends privately can be mined until the \
                 service restarts, or a public send from the account takes number {chain} (and \
                 each one after it below {signing}). Wait {window} blocks after a restart before \
                 repeating a transfer by hand: one already handed to the private relay may land \
                 until then."
            ),
            Alert::SendQueued {
                account,
                waiting,
                nonce,
                behind,
                blocks,
                max_fee_per_gas,
                unlanded,
            } => {
                write!(
                    f,
                    "Sends from {account} are queued. {waiting} (transaction number {nonce}, fee \
                     limit {max_fee_per_gas} wei a gas) has waited {blocks} blocks behind this \
                     account's unmined transaction number {behind}"
                )?;
                if let Some(unlanded) = unlanded {
                    write!(
                        f,
                        ": {}, seen mined in block {} and no longer counted, with a fee limit of \
                         {} wei a gas",
                        unlanded.hash, unlanded.block, unlanded.max_fee_per_gas
                    )?;
                }
                write!(
                    f,
                    ". Nothing later from this account can be mined before it: send number \
                     {behind} again at a higher fee, replace it, or wait for fees to fall."
                )
            }
            Alert::WalletRewindRefused {
                wallet,
                reorg_at,
                rewind,
                refusal,
            } => write!(
                f,
                "The Zcash wallet {wallet} could not sync. It saw a reorg at height {reorg_at} and \
                 could not step back to height {rewind}: {refusal}. Every later sync may fail the \
                 same way. A wallet file rebuilt from its seed would not see what this one \
                 received before."
            ),
        }
    }
}

pub(crate) fn raise(alerts: Option<&Alerts>, alert: Alert) {
    tracing::error!(alert = alert.name(), key = %alert.key(), "{alert}");
    let Some(alerts) = alerts else { return };
    if catch_unwind(AssertUnwindSafe(|| alerts(&alert))).is_err() {
        tracing::error!(alert = alert.name(), key = %alert.key(), "the sink of this alert panicked");
    }
}
