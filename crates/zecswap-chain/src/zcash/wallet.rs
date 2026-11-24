use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use orchard::keys::SpendAuthorizingKey;
use pczt::Pczt;
use pczt::roles::signer::Signer;
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use secrecy::SecretVec;
use zcash_address::ZcashAddress;
use zcash_client_backend::data_api::locking::LockOwner;
use zcash_client_backend::data_api::wallet::input_selection::{
    GreedyInputSelector, LockedInputPolicy, NonEmptyBTreeSet, SpendPolicy,
};
use zcash_client_backend::data_api::wallet::{
    ConfirmationsPolicy, LockRequest, create_pczt_from_proposal,
    extract_and_store_transaction_from_pczt, propose_send_max_transfer, propose_transfer,
    unlock_proposal_inputs,
};
use zcash_client_backend::data_api::{
    Account, AccountBirthday, AccountPurpose, AccountSource, MaxSpendMode, WalletRead, WalletWrite,
};
use zcash_client_backend::fees::standard::SingleOutputChangeStrategy;
use zcash_client_backend::fees::{DustOutputPolicy, StandardFeeRule};
use zcash_client_backend::proposal::Proposal;
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_backend::zip321::{Payment, TransactionRequest};
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_client_sqlite::{AccountUuid, ReceivedNoteId, WalletDb};
use zcash_keys::keys::{UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_primitives::transaction::builder::{BundlePadding, default_tx_expiry_delta};
use zcash_protocol::consensus::{BlockHeight, BranchId, Network, Parameters, UPGRADES_IN_ORDER};
use zcash_protocol::value::{BalanceError, Zatoshis};
use zcash_protocol::{ShieldedPool, TxId};
use zecswap_core::{JointAccount, SpendKey, SweepIntent, sign_pczt};
use zip32::DiversifierIndex;

use super::cache::MemoryBlockCache;
use super::lightwalletd::{self, Lightwalletd};
use super::prover::{Circuit, Prover};
use crate::Error;

mod sync;

const JOINT_KEY_SOURCE: &str = "zecswap";
const INVENTORY: &str = "reverse swap inventory";
const SYNC_BATCH_SIZE: u32 = 1_000;
const ORCHARD_POOLS: [ShieldedPool; 2] = [ShieldedPool::Orchard, ShieldedPool::Ironwood];
pub const SWEEP_FEE_CEILING: u64 = 320_000;

type Db = WalletDb<rusqlite::Connection, Network, SystemClock, UnwrapErr<SysRng>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Funds {
    pub total: u64,
    pub spendable: u64,
}

pub struct ProvedPayment {
    pczt: Pczt,
    circuit: Arc<Circuit>,
    proposal: Proposal<StandardFeeRule, ReceivedNoteId>,
    holder: LockOwner,
    held_until: BlockHeight,
}

pub struct Wallet {
    db: Db,
    views: rusqlite::Connection,
    network: Network,
    blocks: MemoryBlockCache,
    confirmations: ConfirmationsPolicy,
    server_checked: bool,
}

impl Wallet {
    pub fn open(path: impl AsRef<Path>, network: Network) -> Result<Self, Error> {
        let path = path.as_ref();
        let mut db = WalletDb::for_path(path, network, SystemClock, UnwrapErr(SysRng))
            .map_err(Error::wallet)?;
        init_wallet_db(&mut db, None)
            .map_err(|e| Error::Wallet(format!("migrating wallet: {e}")))?;
        let views = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(Error::wallet)?;
        Ok(Self {
            db,
            views,
            network,
            blocks: MemoryBlockCache::default(),
            confirmations: ConfirmationsPolicy::default(),
            server_checked: false,
        })
    }

    pub fn with_confirmations(mut self, confirmations: NonZeroU32) -> Self {
        self.confirmations = ConfirmationsPolicy::new_symmetrical(confirmations, false);
        self
    }

    pub fn network(&self) -> Network {
        self.network
    }

    async fn check_server(&mut self, client: &mut Lightwalletd) -> Result<(), Error> {
        if !self.server_checked {
            lightwalletd::check_network(client, self.network.network_type()).await?;
            self.server_checked = true;
        }
        Ok(())
    }

    pub async fn sync(&mut self, client: &mut Lightwalletd) -> Result<(), Error> {
        if self.db.get_account_ids()?.is_empty() {
            return Ok(());
        }
        self.check_server(client).await?;
        let tip = lightwalletd::chain_tip(client).await?;
        if self.born_above(tip)? {
            return Ok(());
        }
        sync::run(
            client,
            &self.network,
            &self.blocks,
            &mut self.db,
            SYNC_BATCH_SIZE,
        )
        .await
    }

    pub async fn import_joint(
        &mut self,
        client: &mut Lightwalletd,
        joint: &JointAccount,
        name: &str,
    ) -> Result<AccountUuid, Error> {
        let ufvk = self.joint_ufvk(joint)?;
        if let Some(account) = self.find_joint(joint, &ufvk)? {
            return Ok(account);
        }
        self.check_server(client).await?;
        let birthday = Self::birthday_at_tip(client).await?;
        let purpose = AccountPurpose::Spending { derivation: None };
        let account =
            self.db
                .import_account_ufvk(name, &ufvk, &birthday, purpose, Some(JOINT_KEY_SOURCE))?;
        Ok(account.id())
    }

    pub async fn import_joint_view(
        &mut self,
        client: &mut Lightwalletd,
        joint: &JointAccount,
        name: &str,
        from_height: Option<u32>,
    ) -> Result<AccountUuid, Error> {
        let ufvk = self.joint_ufvk(joint)?;
        if let Some(account) = self.find_joint(joint, &ufvk)? {
            return Ok(account);
        }
        self.check_server(client).await?;
        let birthday = if let Some(height) = from_height {
            let state = lightwalletd::tree_state(client, BlockHeight::from(height)).await?;
            AccountBirthday::from_treestate(state, None).map_err(Error::wallet)?
        } else {
            Self::birthday_at_tip(client).await?
        };
        Ok(self
            .db
            .import_account_ufvk(
                name,
                &ufvk,
                &birthday,
                AccountPurpose::ViewOnly,
                Some(JOINT_KEY_SOURCE),
            )?
            .id())
    }

    pub async fn create_account(
        &mut self,
        client: &mut Lightwalletd,
        seed: &[u8],
        name: &str,
    ) -> Result<(AccountUuid, UnifiedSpendingKey), Error> {
        self.check_server(client).await?;
        let birthday = Self::birthday_at_tip(client).await?;
        Ok(self
            .db
            .create_account(name, &SecretVec::new(seed.to_vec()), &birthday, None)?)
    }

    pub fn funds(&self, account: AccountUuid) -> Result<Funds, Error> {
        Ok(self
            .funds_by_account()?
            .remove(&account)
            .unwrap_or_default())
    }

    pub fn funds_by_account(&self) -> Result<HashMap<AccountUuid, Funds>, Error> {
        let summary = self.db.get_wallet_summary(self.confirmations)?;
        Ok(summary
            .iter()
            .flat_map(|summary| summary.account_balances())
            .map(|(account, balance)| {
                let pools = [balance.orchard_balance(), balance.ironwood_balance()];
                let funds = Funds {
                    total: pools.iter().map(|pool| u64::from(pool.total())).sum(),
                    spendable: pools
                        .iter()
                        .map(|pool| u64::from(pool.spendable_value()))
                        .sum(),
                };
                (*account, funds)
            })
            .collect())
    }

    pub fn address(&mut self, account: AccountUuid) -> Result<String, Error> {
        let request = UnifiedAddressRequest::ORCHARD;
        let address = match self
            .db
            .get_last_generated_address_matching(account, request)?
        {
            Some(address) => address,
            None => {
                self.db
                    .get_next_available_address(account, request)?
                    .ok_or_else(|| {
                        Error::Wallet("account cannot generate an Orchard address".into())
                    })?
                    .0
            }
        };
        Ok(address.encode(&self.network))
    }

    pub fn fresh_address(&mut self, account: AccountUuid) -> Result<String, Error> {
        let (address, _) = self
            .db
            .get_next_available_address(account, UnifiedAddressRequest::ORCHARD)?
            .ok_or_else(|| Error::Wallet("account cannot generate an Orchard address".into()))?;
        Ok(address.encode(&self.network))
    }

    pub fn accounts(&self) -> Result<Vec<AccountUuid>, Error> {
        Ok(self.db.get_account_ids()?)
    }

    pub fn derived_account(&self) -> Result<Option<AccountUuid>, Error> {
        for id in self.db.get_account_ids()? {
            let account = self.db.get_account(id)?;
            if matches!(
                account.map(|a| a.source().clone()),
                Some(AccountSource::Derived { .. })
            ) {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub async fn seed_account(
        &mut self,
        client: &mut Lightwalletd,
        seed: &[u8],
        name: &str,
    ) -> Result<(AccountUuid, UnifiedSpendingKey), Error> {
        let key = self.seed_key(seed)?;
        match self.account_of(&key)? {
            Some(account) => Ok((account, key)),
            None => self.create_account(client, seed, name).await,
        }
    }

    pub async fn inventory_account(
        &mut self,
        client: &mut Lightwalletd,
        seed: &[u8],
    ) -> Result<(AccountUuid, UnifiedSpendingKey), Error> {
        let key = self.seed_key(seed)?;
        match self.account_of(&key)? {
            Some(account) => Ok((account, key)),
            None if self.has_account_named(INVENTORY)? => Err(Error::Wallet(
                "inventory seed does not match the wallet".into(),
            )),
            None => self.create_account(client, seed, INVENTORY).await,
        }
    }

    fn seed_key(&self, seed: &[u8]) -> Result<UnifiedSpendingKey, Error> {
        UnifiedSpendingKey::from_seed(&self.network, seed, zip32::AccountId::ZERO)
            .map_err(Error::wallet)
    }

    fn account_of(&self, key: &UnifiedSpendingKey) -> Result<Option<AccountUuid>, Error> {
        Ok(self
            .db
            .get_account_for_ufvk(&key.to_unified_full_viewing_key())?
            .map(|account| account.id()))
    }

    fn has_account_named(&self, name: &str) -> Result<bool, Error> {
        for id in self.db.get_account_ids()? {
            let account = self.db.get_account(id)?;
            if account.is_some_and(|account| account.name() == Some(name)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn received_at(&self, account: AccountUuid, indices: &[u64]) -> Result<Vec<u64>, Error> {
        if indices.is_empty() {
            return Ok(Vec::new());
        }
        let mut received = self
            .views
            .prepare_cached(
                "SELECT diversifier_index_be, SUM(value) FROM v_tx_outputs
                 WHERE to_account_uuid = ?1 AND tx_mined_height IS NOT NULL
                   AND output_pool IN (3, 4) AND diversifier_index_be IS NOT NULL
                 GROUP BY diversifier_index_be",
            )
            .map_err(Error::wallet)?;
        let by_index: HashMap<Vec<u8>, u64> = received
            .query_map([account.expose_uuid().as_bytes().as_slice()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .and_then(Iterator::collect)
            .map_err(Error::wallet)?;
        Ok(indices
            .iter()
            .map(|&index| {
                let mut big_endian = *DiversifierIndex::from(index).as_bytes();
                big_endian.reverse();
                by_index.get(big_endian.as_slice()).copied().unwrap_or(0)
            })
            .collect())
    }

    pub fn tracks_joint(&self, account: AccountUuid, joint: &JointAccount) -> Result<bool, Error> {
        Ok(self
            .db
            .get_account(account)?
            .is_some_and(|account| is_joint(&account, joint)))
    }

    fn joint_ufvk(&self, joint: &JointAccount) -> Result<UnifiedFullViewingKey, Error> {
        let encoded = joint.ufvk(self.network.network_type());
        UnifiedFullViewingKey::decode(&self.network, &encoded).map_err(Error::wallet)
    }

    fn find_joint(
        &self,
        joint: &JointAccount,
        ufvk: &UnifiedFullViewingKey,
    ) -> Result<Option<AccountUuid>, Error> {
        Ok(self
            .db
            .get_account_for_ufvk(ufvk)?
            .filter(|account| is_joint(account, joint))
            .map(|account| account.id()))
    }

    pub fn sweep_fee(&mut self, account: AccountUuid, to: &ZcashAddress) -> Result<u64, Error> {
        let proposal = self.propose_sweep(account, to)?;
        Ok(proposal.steps().head.balance().fee_required().into())
    }

    pub fn sweep(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        key: &SpendKey,
        to: &ZcashAddress,
    ) -> Result<TxId, Error> {
        let proposal = self.propose_sweep(account, to)?;
        let fee = u64::from(proposal.steps().head.balance().fee_required());
        if fee > SWEEP_FEE_CEILING {
            return Err(Error::SweepFee {
                fee,
                ceiling: SWEEP_FEE_CEILING,
            });
        }
        let minimum = proposal
            .steps()
            .head
            .transaction_request()
            .total()
            .map_err(Error::wallet)?
            .ok_or_else(|| Error::Wallet("sweep amount is missing".into()))?;
        let intent = SweepIntent::from_address(
            &to.to_string(),
            self.network.network_type(),
            minimum.into(),
            SWEEP_FEE_CEILING,
        )?;
        let sign = |pczt| Ok(sign_pczt(pczt, std::slice::from_ref(key), &intent)?);
        self.store_proposal(prover, account, &proposal, OvkPolicy::Discard, sign)
    }

    fn propose_sweep(
        &mut self,
        account: AccountUuid,
        to: &ZcashAddress,
    ) -> Result<Proposal<StandardFeeRule, ReceivedNoteId>, Error> {
        let proposal = propose_send_max_transfer::<_, _, _, Infallible>(
            &mut self.db,
            &self.network,
            account,
            &ORCHARD_POOLS,
            &StandardFeeRule::Zip317,
            to.clone(),
            None,
            MaxSpendMode::Everything,
            self.confirmations,
            &LockedInputPolicy::Exclude,
            None,
        )
        .map_err(Error::wallet)?;
        if proposal.steps().len() != 1 {
            return Err(Error::Wallet("a sweep must fit one transaction".into()));
        }
        Ok(proposal)
    }

    pub fn pay(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        usk: &UnifiedSpendingKey,
        payments: &[(ZcashAddress, u64)],
    ) -> Result<TxId, Error> {
        let proposal = self.propose_payment(account, payments, &SpendPolicy::default(), None)?;
        let ask = SpendAuthorizingKey::from(usk.orchard());
        self.store_proposal(prover, account, &proposal, OvkPolicy::Sender, |pczt| {
            sign_own_spends(pczt, &ask)
        })
    }

    pub fn prove_payment(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        holder: [u8; 32],
        payments: &[(ZcashAddress, u64)],
        hold_blocks: u32,
    ) -> Result<ProvedPayment, Error> {
        let holder = LockOwner::new(holder);
        let own = NonEmptyBTreeSet::from_set(BTreeSet::from([holder]))
            .ok_or_else(|| Error::Wallet("a hold needs a holder".into()))?;
        let policy =
            SpendPolicy::default().with_locked_input_policy(LockedInputPolicy::PreferLocked(own));
        let hold = LockRequest::new(holder, hold_blocks);
        let proposal = self.propose_payment(account, payments, &policy, Some(hold))?;
        let held_until = BlockHeight::from(proposal.min_target_height()) + hold_blocks;
        match self.prove(prover, account, &proposal, OvkPolicy::Sender) {
            Ok((pczt, circuit)) => Ok(ProvedPayment {
                pczt,
                circuit,
                proposal,
                holder,
                held_until,
            }),
            Err(e) => {
                self.give_back(&proposal, holder);
                Err(e)
            }
        }
    }

    pub fn holds(&self, payment: &ProvedPayment) -> Result<bool, Error> {
        let Some(tip) = self.db.chain_height()? else {
            return Ok(false);
        };
        let next = tip + 1;
        let expiry = *payment.pczt.global().expiry_height();
        Ok(payment.held_until >= next && (expiry == 0 || BlockHeight::from(expiry) >= next))
    }

    pub fn send_proved(
        &mut self,
        payment: ProvedPayment,
        usk: &UnifiedSpendingKey,
    ) -> Result<TxId, Error> {
        if !self.holds(&payment)? {
            self.give_back(&payment.proposal, payment.holder);
            return Err(Error::Wallet(
                "a proved payment was held too long to send".into(),
            ));
        }
        let ProvedPayment {
            pczt,
            circuit,
            proposal,
            holder,
            ..
        } = payment;
        let ask = SpendAuthorizingKey::from(usk.orchard());
        let stored = self.store_proved(pczt, &circuit, |pczt| sign_own_spends(pczt, &ask));
        if stored.is_err() {
            self.give_back(&proposal, holder);
        }
        stored
    }

    pub fn release(&mut self, payment: ProvedPayment) -> Result<(), Error> {
        Ok(unlock_proposal_inputs(
            &mut self.db,
            &payment.proposal,
            payment.holder,
        )?)
    }

    fn give_back(
        &mut self,
        proposal: &Proposal<StandardFeeRule, ReceivedNoteId>,
        holder: LockOwner,
    ) {
        if let Err(e) = unlock_proposal_inputs(&mut self.db, proposal, holder) {
            tracing::warn!(?holder, "releasing a failed payment's notes: {e}");
        }
    }

    #[tracing::instrument(skip_all, fields(transaction_hash = %txid, chain = "zcash", operation = "broadcast"), err(level = "warn"))]
    pub async fn broadcast(&mut self, client: &mut Lightwalletd, txid: TxId) -> Result<(), Error> {
        self.check_server(client).await?;
        let tx = self
            .db
            .get_transaction(txid)?
            .ok_or_else(|| Error::Wallet(format!("transaction {txid} is not stored")))?;
        let mut raw = Vec::new();
        tx.write(&mut raw).map_err(Error::wallet)?;
        tracing::info!(
            outcome = "submitting",
            "broadcasting stored Zcash transaction"
        );
        lightwalletd::broadcast(client, txid, raw).await?;
        tracing::info!(
            outcome = "accepted",
            "Zcash submission verified; awaiting mining"
        );
        Ok(())
    }

    pub fn fee(&self, txid: TxId) -> Result<Option<u64>, Error> {
        Ok(self
            .db
            .get_transaction(txid)?
            .and_then(|tx| tx.fee_paid(|_| Ok::<_, BalanceError>(None)).ok().flatten())
            .map(u64::from))
    }

    pub fn is_mined(&self, txid: TxId) -> Result<bool, Error> {
        Ok(self.db.get_tx_height(txid)?.is_some())
    }

    pub fn is_confirmed(&self, txid: TxId) -> Result<bool, Error> {
        let Some(mined) = self.db.get_tx_height(txid)? else {
            return Ok(false);
        };
        Ok(self.db.block_fully_scanned()?.is_some_and(|block| {
            u32::from(block.block_height())
                .checked_sub(u32::from(mined))
                .is_some_and(|depth| depth >= self.confirmations.untrusted().get() - 1)
        }))
    }

    pub fn is_expired(&self, txid: TxId) -> Result<bool, Error> {
        if self.is_mined(txid)? {
            return Ok(false);
        }
        let tx = self
            .db
            .get_transaction(txid)?
            .ok_or_else(|| Error::Wallet(format!("transaction {txid} is not stored")))?;
        Ok(u32::from(tx.expiry_height()) != 0
            && self
                .db
                .block_fully_scanned()?
                .is_some_and(|block| block.block_height() > tx.expiry_height()))
    }

    pub fn forget(&mut self, account: AccountUuid) -> Result<(), Error> {
        Ok(self.db.delete_account(account)?)
    }

    async fn birthday_at_tip(client: &mut Lightwalletd) -> Result<AccountBirthday, Error> {
        let tip = lightwalletd::chain_tip(client).await?;
        let treestate = lightwalletd::tree_state(client, tip).await?;
        AccountBirthday::from_treestate(treestate, None)
            .map_err(|e| Error::Wallet(format!("account birthday: {e:?}")))
    }

    fn born_above(&self, tip: BlockHeight) -> Result<bool, Error> {
        Ok(self
            .db
            .get_wallet_birthday()?
            .is_some_and(|birthday| birthday > tip))
    }

    fn propose_payment(
        &mut self,
        account: AccountUuid,
        payments: &[(ZcashAddress, u64)],
        policy: &SpendPolicy,
        hold: Option<LockRequest>,
    ) -> Result<Proposal<StandardFeeRule, ReceivedNoteId>, Error> {
        let payments = payments
            .iter()
            .map(|(to, amount)| {
                let amount = Zatoshis::from_u64(*amount)
                    .map_err(|_| Error::Wallet(format!("{amount} zatoshis is out of range")))?;
                Ok(Payment::without_memo(to.clone(), amount))
            })
            .collect::<Result<_, Error>>()?;
        let request = TransactionRequest::new(payments).map_err(Error::wallet)?;
        let change_strategy = SingleOutputChangeStrategy::new(
            StandardFeeRule::Zip317,
            None,
            ShieldedPool::Ironwood,
            DustOutputPolicy::default(),
        );
        propose_transfer::<_, _, _, _, Infallible>(
            &mut self.db,
            &self.network,
            account,
            &GreedyInputSelector::new(),
            &change_strategy,
            request,
            self.confirmations,
            policy,
            hold,
            None,
        )
        .map_err(Error::wallet)
    }

    fn store_proposal(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        proposal: &Proposal<StandardFeeRule, ReceivedNoteId>,
        ovk_policy: OvkPolicy,
        sign: impl FnOnce(Pczt) -> Result<Pczt, Error>,
    ) -> Result<TxId, Error> {
        let (pczt, circuit) = self.prove(prover, account, proposal, ovk_policy)?;
        self.store_proved(pczt, &circuit, sign)
    }

    fn prove(
        &mut self,
        prover: &Prover,
        account: AccountUuid,
        proposal: &Proposal<StandardFeeRule, ReceivedNoteId>,
        ovk_policy: OvkPolicy,
    ) -> Result<(Pczt, Arc<Circuit>), Error> {
        let target = BlockHeight::from(proposal.min_target_height());
        let pczt = create_pczt_from_proposal::<_, _, Infallible, _, Infallible, _>(
            &mut self.db,
            &self.network,
            &SystemClock,
            &mut UnwrapErr(SysRng),
            account,
            ovk_policy,
            proposal,
            expiry_before_upgrade(&self.network, target),
            BundlePadding::DEFAULT,
        )
        .map_err(Error::wallet)?;
        prover.prove(pczt)
    }

    fn store_proved(
        &mut self,
        pczt: Pczt,
        circuit: &Circuit,
        sign: impl FnOnce(Pczt) -> Result<Pczt, Error>,
    ) -> Result<TxId, Error> {
        extract_and_store_transaction_from_pczt::<_, ReceivedNoteId>(
            &mut self.db,
            &SystemClock,
            &mut UnwrapErr(SysRng),
            sign(pczt)?,
            None,
            Some(&circuit.verifying_key),
        )
        .map_err(Error::wallet)
    }
}

fn expiry_before_upgrade(network: &impl Parameters, target: BlockHeight) -> Option<BlockHeight> {
    let upgrade = UPGRADES_IN_ORDER
        .iter()
        .filter_map(|&upgrade| network.activation_height(upgrade))
        .find(|&activation| activation > target)?;
    let default = target + default_tx_expiry_delta(BranchId::for_height(network, target));
    Some(default.min(upgrade - 1))
}

pub fn address_at(
    key: &UnifiedFullViewingKey,
    network: Network,
    index: u64,
) -> Result<String, Error> {
    Ok(key
        .address(
            DiversifierIndex::from(index),
            UnifiedAddressRequest::ORCHARD,
        )
        .map_err(Error::wallet)?
        .encode(&network))
}

fn is_joint(account: &impl Account, joint: &JointAccount) -> bool {
    account
        .ufvk()
        .and_then(|ufvk| ufvk.orchard())
        .map(|fvk| fvk.to_bytes())
        == Some(joint.fvk().to_bytes())
}

fn sign_own_spends(pczt: Pczt, ask: &SpendAuthorizingKey) -> Result<Pczt, Error> {
    let orchard = unsigned_spends(pczt.orchard());
    let ironwood = unsigned_spends(pczt.ironwood());
    let mut signer = Signer::new(pczt).map_err(|e| Error::Wallet(format!("{e:?}")))?;
    for index in orchard {
        signer
            .sign_orchard(UnwrapErr(SysRng), index, ask)
            .map_err(|e| Error::Wallet(format!("{e:?}")))?;
    }
    for index in ironwood {
        signer
            .sign_ironwood(UnwrapErr(SysRng), index, ask)
            .map_err(|e| Error::Wallet(format!("{e:?}")))?;
    }
    Ok(signer.finish())
}

fn unsigned_spends(bundle: &pczt::orchard::Bundle) -> Vec<usize> {
    bundle
        .actions()
        .iter()
        .enumerate()
        .filter(|(_, action)| action.spend().spend_auth_sig().is_none())
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests;
