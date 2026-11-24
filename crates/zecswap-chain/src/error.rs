use core::fmt::Display;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("lightwalletd: {0}")]
    Lightwalletd(#[from] tonic::Status),
    #[error("lightwalletd connection: {0}")]
    Connection(#[from] tonic::transport::Error),
    #[error("lightwalletd rejected the transaction ({code}): {message}")]
    Rejected { code: i32, message: String },
    #[error("wallet database: {0}")]
    Database(#[from] zcash_client_sqlite::error::SqliteClientError),
    #[error("wallet: {0}")]
    Wallet(String),
    #[error("a sweep would pay the network {fee} zatoshis, over the {ceiling} one may")]
    SweepFee { fee: u64, ceiling: u64 },
    #[error("settlement contract: {0}")]
    Contract(String),
    #[error("transaction {0} reverted")]
    Reverted(alloy::primitives::B256),
    #[error("transaction {0} sent, but its receipt is unavailable: it may still mine")]
    Unconfirmed(alloy::primitives::B256),
    #[error("the private RPC dropped transaction {0} unmined: it never landed")]
    Dropped(alloy::primitives::B256),
    #[error("the swap's {0} window is shut, or ends too soon to send it in; nothing was sent")]
    Late(&'static str),
    #[error("the transaction would revert, so nothing was sent: {0}")]
    WouldRevert(String),
    #[error("nothing was sent: {0}")]
    Unsent(String),
    #[error("swap {0} opened on other terms than these")]
    WrongTerms(alloy::primitives::B256),
    #[error("recording a transaction before its broadcast: {0}")]
    Journal(String),
    #[error(transparent)]
    Swap(#[from] zecswap_core::Error),
}

impl Error {
    pub fn nothing_sent(&self) -> bool {
        matches!(
            self,
            Error::Config(_)
                | Error::Late(_)
                | Error::WouldRevert(_)
                | Error::Unsent(_)
                | Error::WrongTerms(_)
        )
    }

    pub(crate) fn wallet(e: impl Display) -> Self {
        Error::Wallet(e.to_string())
    }

    pub(crate) fn contract(e: impl Display) -> Self {
        Error::Contract(e.to_string())
    }
}
