use alloy::eips::BlockId;
use alloy::network::TransactionBuilder;
use alloy::primitives::Bytes;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::RpcError;
use zecswap_core::{RescueAuthorization, TokenDomain};
use zecswap_railgun::ShieldNote;

use super::railgun::{Later, SendError, Signed, require};
use super::{
    Address, B256, IDeposits, IEip3009, IErc20, ISanctionsOracle, Settlement, U256,
    ciphertext_words, fees,
};
use crate::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub token: Address,
    pub railgun: Address,
    pub sanctions: Option<Address>,
    pub token_domain: TokenDomain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositPolicy {
    pub contract: Address,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u128,
}

pub struct Shield<'a> {
    pub from: Address,
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    pub note: &'a ShieldNote,
    pub fee: u128,
    pub signature: &'a [u8; 65],
}

pub struct Rescue<'a> {
    pub from: Address,
    pub note: &'a ShieldNote,
    pub fee: u128,
    pub authorization: RescueAuthorization,
    pub signature: &'a [u8; 65],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Used {
    pub balance: u128,
    pub deposited: bool,
}

impl Used {
    pub fn used(&self) -> bool {
        self.balance > 0 || self.deposited
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Shield {
        from: Address,
        value: u128,
        fee: u128,
        valid_before: u64,
    },
    Rescue {
        from: Address,
        fee: u128,
        deadline: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Landed {
    pub from: Address,
    pub note: B256,
    pub fee: u128,
    pub relayer: Address,
}

impl Shield<'_> {
    pub fn calldata(&self) -> Bytes {
        let signature = self.signature;
        IDeposits::shieldCall {
            from: self.from,
            value: U256::from(self.value),
            validAfter: U256::from(self.valid_after),
            validBefore: U256::from(self.valid_before),
            npk: self.note.npk.into(),
            ciphertext: ciphertext_words(&self.note.ciphertext),
            fee: self.fee,
            v: signature[64],
            r: B256::from_slice(&signature[..32]),
            s: B256::from_slice(&signature[32..64]),
        }
        .abi_encode()
        .into()
    }
}

impl Rescue<'_> {
    pub fn calldata(&self) -> Bytes {
        IDeposits::rescueCall {
            from: self.from,
            npk: self.note.npk.into(),
            ciphertext: ciphertext_words(&self.note.ciphertext),
            fee: self.fee,
            nonce: self.authorization.nonce,
            deadline: self.authorization.deadline,
            signature: self.signature.to_vec().into(),
        }
        .abi_encode()
        .into()
    }
}

impl Call {
    pub fn decode(data: &[u8]) -> Option<Self> {
        if let Ok(call) = IDeposits::shieldCall::abi_decode(data) {
            return Some(Call::Shield {
                from: call.from,
                value: call.value.try_into().ok()?,
                fee: call.fee,
                valid_before: call.validBefore.try_into().unwrap_or(u64::MAX),
            });
        }
        let call = IDeposits::rescueCall::abi_decode(data).ok()?;
        Some(Call::Rescue {
            from: call.from,
            fee: call.fee,
            deadline: call.deadline,
        })
    }

    pub fn expires_at(&self) -> u64 {
        match *self {
            Call::Shield { valid_before, .. } => valid_before,
            Call::Rescue { deadline, .. } => deadline.saturating_add(1),
        }
    }
}

impl Settlement {
    pub async fn deposits(&self, contract: Address) -> Result<Deployment, Error> {
        let deposits = IDeposits::new(contract, &self.provider);
        let token = deposits.TOKEN().call().await.map_err(Error::contract)?;
        let railgun = deposits.RAILGUN().call().await.map_err(Error::contract)?;
        let sanctions = deposits.SANCTIONS().call().await.map_err(Error::contract)?;
        let erc20 = IEip3009::new(token, &self.provider);
        let token_domain = TokenDomain {
            name: erc20.name().call().await.map_err(Error::contract)?,
            version: erc20.version().call().await.map_err(Error::contract)?,
            chain_id: self.chain_id().await?,
            token: token.into(),
        };
        let separator = erc20
            .DOMAIN_SEPARATOR()
            .call()
            .await
            .map_err(Error::contract)?;
        if separator != B256::from(token_domain.separator()) {
            return Err(Error::Contract(format!(
                "{token}'s DOMAIN_SEPARATOR is not that of its name {:?} and version {:?}",
                token_domain.name, token_domain.version
            )));
        }
        Ok(Deployment {
            token,
            railgun,
            sanctions: (!sanctions.is_zero()).then_some(sanctions),
            token_domain,
        })
    }

    pub async fn deposit_addresses(
        &self,
        contract: Address,
        token: Address,
        owners: &[Address],
    ) -> Result<Vec<Used>, Error> {
        let balance = |data: &[u8]| {
            IErc20::balanceOfCall::abi_decode_returns(data)
                .map_err(Error::contract)
                .and_then(|balance| u128::try_from(balance).map_err(Error::contract))
        };
        let deposited = |data: &[u8]| {
            IDeposits::depositedCall::abi_decode_returns(data).map_err(Error::contract)
        };
        if !self.multicall_deployed().await? {
            let mut used = Vec::with_capacity(owners.len());
            for owner in owners {
                used.push(Used {
                    balance: self.token_balance(token, *owner).await?,
                    deposited: IDeposits::new(contract, &self.provider)
                        .deposited(*owner)
                        .call()
                        .await
                        .map_err(Error::contract)?,
                });
            }
            return Ok(used);
        }
        let calls = owners.iter().flat_map(|owner| {
            [
                (token, IErc20::balanceOfCall { owner: *owner }.abi_encode()),
                (
                    contract,
                    IDeposits::depositedCall { from: *owner }.abi_encode(),
                ),
            ]
        });
        let answers = self.aggregate(calls.collect(), BlockId::latest()).await?;
        answers
            .chunks(2)
            .map(|pair| {
                Ok(Used {
                    balance: balance(&pair[0])?,
                    deposited: deposited(&pair[1])?,
                })
            })
            .collect()
    }

    pub async fn deposits_landed(&self, contract: Address, tx: B256) -> Result<Vec<Landed>, Error> {
        let Some(receipt) = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(Error::contract)?
        else {
            return Ok(Vec::new());
        };
        Ok(receipt
            .inner
            .logs()
            .iter()
            .filter(|log| log.address() == contract)
            .filter_map(|log| match log.topics().first() {
                Some(&IDeposits::DepositShielded::SIGNATURE_HASH) => {
                    let event = IDeposits::DepositShielded::decode_log(&log.inner).ok()?;
                    Some((event.from, event.note, event.fee, event.relayer))
                }
                Some(&IDeposits::Rescued::SIGNATURE_HASH) => {
                    let event = IDeposits::Rescued::decode_log(&log.inner).ok()?;
                    Some((event.from, event.note, event.fee, event.relayer))
                }
                _ => None,
            })
            .map(|(from, note, fee, relayer)| Landed {
                from,
                note,
                fee: fee.saturating_to(),
                relayer,
            })
            .collect())
    }

    pub async fn has_code(&self, address: Address) -> Result<bool, Error> {
        let code = self
            .provider
            .get_code_at(address)
            .await
            .map_err(Error::contract)?;
        Ok(!code.is_empty())
    }

    pub async fn authorization_used(
        &self,
        token: Address,
        authorizer: Address,
        nonce: B256,
    ) -> Result<bool, Error> {
        IEip3009::new(token, &self.provider)
            .authorizationState(authorizer, nonce)
            .call()
            .await
            .map_err(Error::contract)
    }

    pub async fn sanctioned(&self, oracle: Address, address: Address) -> Result<bool, Error> {
        ISanctionsOracle::new(oracle, &self.provider)
            .isSanctioned(address)
            .call()
            .await
            .map_err(Error::contract)
    }

    pub async fn deposit_vault(&self, contract: Address, from: Address) -> Result<Address, Error> {
        IDeposits::new(contract, &self.provider)
            .vaultOf(from)
            .call()
            .await
            .map_err(Error::contract)
    }

    pub async fn deposit_rescue_nonce(
        &self,
        contract: Address,
        from: Address,
    ) -> Result<u64, Error> {
        IDeposits::new(contract, &self.provider)
            .rescueNonces(from)
            .call()
            .await
            .map_err(Error::contract)
    }

    pub async fn send_deposit(
        &self,
        policy: &DepositPolicy,
        data: Bytes,
        least_nonce: u64,
        covers: impl FnOnce(u64, u128) -> bool,
        record: impl FnOnce(&Signed) -> Result<(), Error>,
    ) -> Result<B256, SendError> {
        let Some(account) = self.account else {
            return Err(Error::Config("a read-only connection cannot send".into()).into());
        };
        let gas_price = self
            .provider
            .get_gas_price()
            .await
            .map_err(|_| Error::Contract("the gas price is unavailable".into()))?;
        if gas_price > policy.max_gas_price_wei {
            return Err(SendError::Later(Later::GasPrice));
        }
        let fees = fees(&self.provider).await?;
        let balance = self
            .provider
            .get_balance(account)
            .await
            .map_err(|_| Error::Contract("the relayer's balance is unavailable".into()))?;
        let afford = |gas: u64| balance >= U256::from(gas) * U256::from(fees.max_fee_per_gas);
        let mut tx = TransactionRequest::default()
            .with_from(account)
            .with_to(policy.contract)
            .with_value(U256::ZERO)
            .with_input(data)
            .with_chain_id(self.chain_id().await?)
            .with_gas_limit(policy.max_gas_limit)
            .with_max_fee_per_gas(fees.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
        let estimate = match self
            .provider
            .estimate_gas(tx.clone())
            .block(BlockId::pending())
            .await
        {
            Ok(gas) => gas,
            Err(RpcError::ErrorResp(_)) if !afford(policy.max_gas_limit) => {
                return Err(SendError::Later(Later::Eth));
            }
            Err(RpcError::ErrorResp(_)) => {
                return Err(SendError::Rejected("the transaction fails in simulation"));
            }
            Err(_) => return Err(Error::Contract("the simulation is unavailable".into()).into()),
        };
        require(
            estimate <= policy.max_gas_limit,
            "the transaction needs more gas than the relayer's cap",
        )?;
        require(
            covers(estimate, gas_price),
            "the deposit's fee does not cover its gas",
        )?;
        let gas_limit = estimate
            .saturating_add(estimate / 5)
            .min(policy.max_gas_limit);
        if !afford(gas_limit) {
            return Err(SendError::Later(Later::Eth));
        }
        let _sending = self.sending.turn().await;
        tx.set_gas_limit(gas_limit);
        let signed = self.sign_next(tx, least_nonce).await?;
        record(&signed)?;
        match self.broadcaster().send_raw_transaction(&signed.raw).await {
            Ok(_) => {
                tracing::info!(transaction_hash = %signed.hash, "sent a deposit");
                Ok(signed.hash)
            }
            Err(RpcError::ErrorResp(_)) if self.private.is_some() => {
                Err(SendError::Refused(signed.hash))
            }
            Err(_) => Err(SendError::Unknown(signed.hash)),
        }
    }

    pub async fn transfer_token(
        &self,
        token: Address,
        to: Address,
        amount: u128,
    ) -> Result<B256, Error> {
        let erc20 = super::IErc20::new(token, &self.provider);
        Ok(self
            .submit(erc20.transfer(to, U256::from(amount)))
            .await?
            .transaction_hash)
    }
}
