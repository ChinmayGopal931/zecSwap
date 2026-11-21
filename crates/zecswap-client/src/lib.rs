//! The user side of a ZecSwap, step by step as a wallet runs it. The maker's API only
//! proposes; nothing is deposited until the contract shows what was promised.

mod api;
mod deposits;
mod tokens;
mod user;

pub use api::{Broadcast, Deposited, FeeNotHonoured, MakerApi, RelayerApi, ServiceError};
pub use deposits::{Deposit, DepositDeployment, Depositor, FeeCap};
pub use tokens::{Attest, Tokens, Unattested};
pub use user::{
    CLAIM_MARGIN, MAX_TIME_TO_T0, MIN_TIME_TO_T0, OPEN_FEE_MARGIN, Paid, Quoted, Route, User,
    UserSwap,
};
pub mod reverse;
