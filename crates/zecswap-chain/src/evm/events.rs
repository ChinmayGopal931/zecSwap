use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;

use super::{B256, IZecSwap, Settlement};
use crate::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapEventKind {
    Opened,
    Ready,
    Claimed,
    PaidOut,
    Rescued,
    Refunded,
}

impl SwapEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::PaidOut => "paid_out",
            Self::Rescued => "rescued",
            Self::Refunded => "refunded",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SwapEvent {
    pub id: B256,
    pub kind: SwapEventKind,
    pub transaction_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub log_index: u64,
}

fn signatures() -> Vec<B256> {
    vec![
        IZecSwap::Opened::SIGNATURE_HASH,
        IZecSwap::MarkedReady::SIGNATURE_HASH,
        IZecSwap::Claimed::SIGNATURE_HASH,
        IZecSwap::PaidOut::SIGNATURE_HASH,
        IZecSwap::Rescued::SIGNATURE_HASH,
        IZecSwap::Refunded::SIGNATURE_HASH,
    ]
}

fn decode(log: &Log) -> Option<SwapEvent> {
    if log.removed {
        return None;
    }
    let signature = *log.topics().first()?;
    macro_rules! event {
        ($($name:ident => $kind:ident),+ $(,)?) => {
            $(if signature == IZecSwap::$name::SIGNATURE_HASH {
                let decoded = IZecSwap::$name::decode_log_data(log.data()).ok()?;
                return Some(SwapEvent {
                    id: decoded.id,
                    kind: SwapEventKind::$kind,
                    transaction_hash: log.transaction_hash?,
                    block_number: log.block_number?,
                    block_hash: log.block_hash?,
                    log_index: log.log_index?,
                });
            })+
        };
    }
    event! {
        Opened => Opened, MarkedReady => Ready, Claimed => Claimed,
        PaidOut => PaidOut, Rescued => Rescued, Refunded => Refunded,
    }
    None
}

impl Settlement {
    pub async fn confirmed_height(&self, confirmations: u64) -> Result<u64, Error> {
        if confirmations == 0 {
            return Err(Error::Contract("confirmations must be positive".into()));
        }
        Ok(self
            .provider
            .get_block_number()
            .await
            .map_err(Error::contract)?
            .saturating_sub(confirmations - 1))
    }

    pub async fn block_timestamp(&self, height: u64) -> Result<u64, Error> {
        let block = self
            .provider
            .get_block_by_number(height.into())
            .await
            .map_err(Error::contract)?
            .ok_or_else(|| Error::Contract("block unavailable".into()))?;
        Ok(block.header.timestamp)
    }

    pub async fn block_time(&self, hash: B256) -> Result<Option<u64>, Error> {
        Ok(self
            .provider
            .get_block_by_hash(hash)
            .await
            .map_err(Error::contract)?
            .map(|block| block.header.timestamp))
    }

    pub async fn swap_events(&self, from: u64, to: u64) -> Result<Vec<SwapEvent>, Error> {
        if to < from || to - from >= 10 {
            return Err(Error::Contract(
                "event range must contain 1 to 10 blocks".into(),
            ));
        }
        let logs = self
            .provider
            .get_logs(
                &Filter::new()
                    .address(self.contract())
                    .event_signature(signatures())
                    .from_block(from)
                    .to_block(to),
            )
            .await
            .map_err(Error::contract)?;
        let mut events = Vec::new();
        for log in logs
            .iter()
            .filter(|log| log.address() == self.contract() && !log.removed)
        {
            let event =
                decode(log).ok_or_else(|| Error::Contract("incomplete settlement event".into()))?;
            if !(from..=to).contains(&event.block_number) {
                return Err(Error::Contract(
                    "settlement event outside requested range".into(),
                ));
            }
            events.push(event);
        }
        events.sort_by_key(|event| (event.block_number, event.log_index));
        Ok(events)
    }
}
