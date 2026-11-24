use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

use incrementalmerkletree::frontier::Frontier;
use orchard::builder::BundleType;
use orchard::bundle::BundleVersion;
use orchard::keys::Scope;
use orchard::note_encryption::COMPACT_NOTE_SIZE;
use orchard::tree::MerkleHashOrchard;
use orchard::value::NoteValue;
use orchard::{Anchor, Bundle};
use tempfile::TempDir;
use zcash_client_backend::data_api::WalletCommitmentTrees;
use zcash_client_backend::data_api::chain::{
    BlockCache, ChainState, CommitmentTreeRoot, scan_cached_blocks,
};
use zcash_client_backend::proto::compact_formats::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx,
};
use zcash_keys::address::Address;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade};
use zcash_protocol::memo::MemoBytes;
use zecswap_core::{SecretShare, ViewingKeys};

use super::*;

const TIP: u32 = 4_395_130;
const DEPOSIT: u64 = 1_000_000;
const FEE: u64 = 40_000;

static PROVER: LazyLock<Prover> = LazyLock::new(Prover::default);

fn nu7() -> u32 {
    Network::MainNetwork
        .activation_height(NetworkUpgrade::Nu7)
        .unwrap()
        .into()
}

fn random_joint() -> JointAccount {
    JointAccount::derive(
        &SecretShare::random(UnwrapErr(SysRng)).public(),
        &SecretShare::random(UnwrapErr(SysRng)).public(),
        &ViewingKeys::random(UnwrapErr(SysRng)),
    )
    .unwrap()
}

fn import_at(wallet: &mut Wallet, joint: &JointAccount, birthday: u32) -> AccountUuid {
    let ufvk =
        UnifiedFullViewingKey::decode(&wallet.network, &joint.ufvk(wallet.network.network_type()))
            .unwrap();
    let birthday = AccountBirthday::from_parts(
        ChainState::empty(BlockHeight::from_u32(birthday - 1), BlockHash([0; 32])),
        None,
    );
    wallet
        .db
        .import_account_ufvk(
            "test joint",
            &ufvk,
            &birthday,
            AccountPurpose::Spending { derivation: None },
            Some(JOINT_KEY_SOURCE),
        )
        .unwrap()
        .id()
}

fn previous_swap(scanned: bool) -> (TempDir, Wallet) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallet.sqlite");
    let mut wallet = Wallet::open(&path, Network::TestNetwork).unwrap();
    let old = import_at(&mut wallet, &random_joint(), 4_395_100);
    if scanned {
        let conn = rusqlite::Connection::open(&path).unwrap();
        for height in 4_395_100..=4_395_105 {
            conn.execute(
                "INSERT INTO blocks (height, hash, time, sapling_tree,
                 sapling_commitment_tree_size, orchard_commitment_tree_size,
                 ironwood_commitment_tree_size) VALUES (?1, zeroblob(32), 0, x'000000', 0, 0, 0)",
                [height],
            )
            .unwrap();
        }
        conn.execute_batch(
            "DELETE FROM scan_queue;
             INSERT INTO scan_queue VALUES (280000, 4395100, 0), (4395100, 4395106, 10);",
        )
        .unwrap();
    }
    wallet.forget(old).unwrap();
    assert!(wallet.db.get_account_ids().unwrap().is_empty());
    wallet
        .db
        .put_orchard_subtree_roots(
            0,
            &[CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(4_094_022),
                MerkleHashOrchard::from_bytes(&[0; 32]).unwrap(),
            )],
        )
        .unwrap();
    (dir, wallet)
}

#[test]
fn future_birthday_panics_only_with_retained_blocks_at_unchanged_tip() {
    for (scanned, birthday, tip, panics) in [
        (true, TIP + 1, TIP, true),
        (false, TIP + 1, TIP, false),
        (true, TIP, TIP, false),
        (true, TIP + 1, TIP + 1, false),
    ] {
        let (dir, mut wallet) = previous_swap(scanned);
        import_at(&mut wallet, &random_joint(), birthday);
        assert!(!panics || wallet.born_above(BlockHeight::from_u32(tip)).unwrap());
        if panics {
            let conn = rusqlite::Connection::open(dir.path().join("wallet.sqlite")).unwrap();
            let ranges: Vec<(u32, u32, u32)> = conn
                .prepare("SELECT * FROM scan_queue ORDER BY block_range_start")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(
                ranges,
                [
                    (280000, 4395100, 0),
                    (4395100, 4395106, 10),
                    (4395106, 4395131, 0)
                ]
            );
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            wallet
                .db
                .update_chain_tip(BlockHeight::from_u32(tip))
                .unwrap();
        }));
        assert_eq!(
            result.is_err(),
            panics,
            "scanned={scanned}, birthday={birthday}, tip={tip}"
        );
        if let Err(panic) = result {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            assert_eq!(
                message,
                Some("Split point is within the range of to_insert")
            );
        }
    }
}

#[test]
fn an_imported_joint_account_is_recognised_by_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), Network::TestNetwork).unwrap();
    let joint = random_joint();
    let account = import_at(&mut wallet, &joint, 4_395_100);
    assert!(wallet.tracks_joint(account, &joint).unwrap());
    assert!(!wallet.tracks_joint(account, &random_joint()).unwrap());
}

#[test]
fn a_joint_account_is_found_by_its_key_among_many() {
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), Network::TestNetwork).unwrap();
    let joints: Vec<_> = (0..4).map(|_| random_joint()).collect();
    let accounts: Vec<_> = joints
        .iter()
        .map(|joint| import_at(&mut wallet, joint, 4_395_100))
        .collect();
    for (joint, account) in joints.iter().zip(accounts) {
        let ufvk = wallet.joint_ufvk(joint).unwrap();
        assert_eq!(wallet.find_joint(joint, &ufvk).unwrap(), Some(account));
    }
    let stranger = random_joint();
    let ufvk = wallet.joint_ufvk(&stranger).unwrap();
    assert_eq!(wallet.find_joint(&stranger, &ufvk).unwrap(), None);
}

#[test]
fn one_summary_reads_every_accounts_funds() {
    let (_dir, mut wallet) = previous_swap(true);
    let accounts = [
        import_at(&mut wallet, &random_joint(), 4_395_106),
        import_at(&mut wallet, &random_joint(), 4_395_106),
    ];
    wallet
        .db
        .update_chain_tip(BlockHeight::from_u32(4_395_106))
        .unwrap();
    let all = wallet.funds_by_account().unwrap();
    assert_eq!(all.len(), accounts.len());
    for account in accounts {
        assert_eq!(all[&account], wallet.funds(account).unwrap());
    }
    wallet.forget(accounts[0]).unwrap();
    assert_eq!(wallet.funds(accounts[0]).unwrap(), Funds::default());
}

#[test]
fn a_birthday_above_the_tip_imports_and_syncs_from_the_next_block() {
    let (_dir, mut wallet) = previous_swap(true);
    import_at(&mut wallet, &random_joint(), 4_395_106);
    assert!(wallet.born_above(BlockHeight::from_u32(4_395_105)).unwrap());
    assert!(!wallet.born_above(BlockHeight::from_u32(4_395_106)).unwrap());
    wallet
        .db
        .update_chain_tip(BlockHeight::from_u32(4_395_106))
        .unwrap();
}

#[test]
fn sweep_finality_uses_scanned_confirmations_and_rewinds_after_a_reorg() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallet.sqlite");
    let mut wallet = Wallet::open(&path, Network::TestNetwork)
        .unwrap()
        .with_confirmations(NonZeroU32::new(3).unwrap());
    let account = import_at(&mut wallet, &random_joint(), TIP);
    let conn = rusqlite::Connection::open(&path).unwrap();
    for height in TIP..=TIP + 20 {
        conn.execute(
            "INSERT INTO blocks (height, hash, time, sapling_tree,
             sapling_commitment_tree_size, orchard_commitment_tree_size,
             ironwood_commitment_tree_size) VALUES (?1, zeroblob(32), 0, x'000000', 0, 0, 0)",
            [height],
        )
        .unwrap();
    }
    let txid = TxId::from_bytes([1; 32]);
    conn.execute(
        "INSERT INTO transactions (txid, mined_height, min_observed_height) VALUES (?1, ?2, ?2)",
        rusqlite::params![txid.as_ref(), TIP],
    )
    .unwrap();
    let scanned_to = |height: u32| {
        conn.execute("DELETE FROM scan_queue", []).unwrap();
        conn.execute(
            "INSERT INTO scan_queue VALUES (?1, ?2, 10)",
            [TIP, height + 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_queue VALUES (?1, ?2, 20)",
            [height + 1, TIP + 21],
        )
        .unwrap();
    };
    scanned_to(TIP);
    assert!(wallet.is_mined(txid).unwrap());
    assert!(!wallet.is_confirmed(txid).unwrap());
    scanned_to(TIP + 1);
    assert!(!wallet.is_confirmed(txid).unwrap());
    scanned_to(TIP + 2);
    assert!(wallet.is_confirmed(txid).unwrap());
    drop(wallet);
    let wallet = Wallet::open(&path, Network::TestNetwork)
        .unwrap()
        .with_confirmations(NonZeroU32::new(3).unwrap());
    assert!(wallet.is_confirmed(txid).unwrap());
    assert!(wallet.db.get_account(account).unwrap().is_some());
    scanned_to(TIP + 1);
    assert!(!wallet.is_confirmed(txid).unwrap());
    conn.execute("UPDATE transactions SET mined_height = NULL", [])
        .unwrap();
    assert!(!wallet.is_mined(txid).unwrap());
    assert!(!wallet.is_confirmed(txid).unwrap());
    assert!(!wallet.is_confirmed(TxId::from_bytes([2; 32])).unwrap());
}

fn blocks_paying(
    tip: u32,
    count: u8,
    outputs: Vec<(u8, orchard::Address, u64)>,
) -> (Vec<CompactBlock>, impl Fn(u8) -> ChainState) {
    let version = BundleVersion::ironwood_v3();
    let mut tree = Frontier::empty();
    let mut size = 0;
    let (blocks, trees): (Vec<_>, Vec<_>) = (0..count)
        .map(|i| {
            let paid = outputs.iter().enumerate().filter(|(_, o)| o.0 == i);
            let vtx = paid
                .map(|(n, &(_, to, zatoshis))| {
                    let mut builder = orchard::builder::Builder::new(
                        BundleType::DEFAULT,
                        version,
                        version.default_flags(),
                        Anchor::empty_tree(),
                    )
                    .unwrap();
                    builder
                        .add_output(
                            None,
                            to,
                            NoteValue::from_raw(zatoshis),
                            MemoBytes::empty().into_bytes(),
                        )
                        .unwrap();
                    let (bundle, meta): (Bundle<_, i64>, _) =
                        builder.build(&mut UnwrapErr(SysRng)).unwrap().unwrap();
                    let action = &bundle.actions()[meta.output_action_index(0).unwrap()];
                    let note = action.encrypted_note();
                    tree.append(MerkleHashOrchard::from_cmx(action.cmx()));
                    size += 1;
                    CompactTx {
                        index: n as u64,
                        txid: vec![9 + n as u8; 32],
                        ironwood_actions: vec![CompactOrchardAction {
                            nullifier: action.nullifier().to_bytes().to_vec(),
                            cmx: action.cmx().to_bytes().to_vec(),
                            ephemeral_key: note.epk_bytes.to_vec(),
                            ciphertext: note.enc_ciphertext.0[..COMPACT_NOTE_SIZE].to_vec(),
                        }],
                        ..Default::default()
                    }
                })
                .collect();
            let block = CompactBlock {
                height: u64::from(tip + u32::from(i)),
                hash: vec![i + 1; 32],
                prev_hash: vec![i; 32],
                vtx,
                chain_metadata: Some(ChainMetadata {
                    ironwood_commitment_tree_size: size,
                    ..Default::default()
                }),
                ..Default::default()
            };
            (block, tree.clone())
        })
        .unzip();
    let state = move |i: u8| {
        ChainState::new(
            BlockHeight::from_u32(tip + u32::from(i)),
            BlockHash([i + 1; 32]),
            Frontier::empty(),
            Frontier::empty(),
            trees[usize::from(i)].clone(),
        )
    };
    (blocks, state)
}

fn derive_at(
    wallet: &mut Wallet,
    seed: &[u8],
    name: &str,
    birthday: u32,
) -> (AccountUuid, UnifiedSpendingKey) {
    let birthday = AccountBirthday::from_parts(
        ChainState::empty(BlockHeight::from_u32(birthday - 1), BlockHash([0; 32])),
        None,
    );
    wallet
        .db
        .create_account(name, &SecretVec::new(seed.to_vec()), &birthday, None)
        .unwrap()
}

fn orchard_receiver(wallet: &Wallet, address: &str) -> orchard::Address {
    match Address::decode(&wallet.network, address) {
        Some(Address::Unified(address)) => *address.orchard().unwrap(),
        _ => panic!("{address} is not a unified address"),
    }
}

struct Funded {
    _dir: TempDir,
    wallet: Wallet,
    account: AccountUuid,
    tip: u32,
    state: Box<dyn Fn(u8) -> ChainState>,
}

impl Funded {
    async fn new(
        network: Network,
        tip: u32,
        count: u8,
        add: impl FnOnce(&mut Wallet) -> (AccountUuid, orchard::Address),
    ) -> Self {
        let funded = Self::paying(network, tip, count, |wallet| {
            let (account, to) = add(wallet);
            (account, vec![(0, to, DEPOSIT)])
        })
        .await;
        assert_eq!(funded.spendable(), DEPOSIT);
        funded
    }

    async fn paying(
        network: Network,
        tip: u32,
        count: u8,
        add: impl FnOnce(&mut Wallet) -> (AccountUuid, Vec<(u8, orchard::Address, u64)>),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), network)
            .unwrap()
            .with_confirmations(NonZeroU32::new(1).unwrap());
        let (account, outputs) = add(&mut wallet);
        let (blocks, state) = blocks_paying(tip, count, outputs);
        wallet.blocks.insert(blocks).await.unwrap();
        let mut funded = Self {
            _dir: dir,
            wallet,
            account,
            tip,
            state: Box::new(state),
        };
        funded.scan(tip + 1, 0, 2);
        funded
    }

    async fn joint(network: Network, tip: u32, count: u8) -> (Self, SpendKey, JointAccount) {
        let (e, z) = (
            SecretShare::random(UnwrapErr(SysRng)),
            SecretShare::random(UnwrapErr(SysRng)),
        );
        let joint = JointAccount::derive(
            &e.public(),
            &z.public(),
            &ViewingKeys::random(UnwrapErr(SysRng)),
        )
        .unwrap();
        let funded = Self::new(network, tip, count, |wallet| {
            (import_at(wallet, &joint, tip), joint.deposit_address())
        })
        .await;
        (funded, joint.spend_key(&e, &z).unwrap(), joint)
    }

    async fn derived(network: Network, tip: u32, count: u8) -> (Self, UnifiedSpendingKey) {
        let mut key = None;
        let funded = Self::new(network, tip, count, |wallet| {
            let (account, usk) = derive_at(wallet, &[7; 32], INVENTORY, tip);
            let fvk = usk.to_unified_full_viewing_key();
            let to = fvk.orchard().unwrap().address_at(0u32, Scope::External);
            key = Some(usk);
            (account, to)
        })
        .await;
        (funded, key.unwrap())
    }

    fn scan(&mut self, chain_tip: u32, from: u8, count: usize) {
        self.wallet
            .db
            .update_chain_tip(BlockHeight::from_u32(chain_tip))
            .unwrap();
        let prior = match from.checked_sub(1) {
            Some(before) => (self.state)(before),
            None => ChainState::empty(BlockHeight::from_u32(self.tip - 1), BlockHash([0; 32])),
        };
        scan_cached_blocks(
            &self.wallet.network,
            &self.wallet.blocks,
            &mut self.wallet.db,
            BlockHeight::from_u32(self.tip + u32::from(from)),
            &prior,
            count,
        )
        .unwrap();
    }

    fn spendable(&self) -> u64 {
        self.wallet.funds(self.account).unwrap().spendable
    }

    fn paid(&self, txid: TxId, home: &JointAccount) -> u64 {
        let tx = self.wallet.db.get_transaction(txid).unwrap().unwrap();
        [tx.orchard_bundle(), tx.ironwood_bundle()]
            .iter()
            .flatten()
            .flat_map(|bundle| {
                bundle.decrypt_outputs_with_keys(&[home.fvk().to_ivk(Scope::External)])
            })
            .map(|(_, _, note, _, _)| note.value().inner())
            .sum()
    }
}

#[tokio::test]
async fn the_other_side_cannot_read_where_a_sweep_went() {
    let (mut funded, key, joint) = Funded::joint(Network::TestNetwork, TIP, 2).await;
    let home = random_joint();
    let to = home
        .unified_address(funded.wallet.network.network_type())
        .parse()
        .unwrap();
    let txid = funded
        .wallet
        .sweep(&PROVER, funded.account, &key, &to)
        .unwrap();

    assert_eq!(
        funded.paid(txid, &home),
        DEPOSIT - funded.wallet.fee(txid).unwrap().unwrap()
    );
    let tx = funded.wallet.db.get_transaction(txid).unwrap().unwrap();
    let bundles = [tx.orchard_bundle(), tx.ironwood_bundle()];
    let joint_ovks = [Scope::External, Scope::Internal].map(|scope| joint.fvk().to_ovk(scope));
    for bundle in bundles.iter().flatten() {
        assert!(
            bundle.recover_outputs_with_ovks(&joint_ovks).is_empty(),
            "the joint viewing key reads where the sweep went"
        );
    }
}

#[tokio::test]
async fn a_sweep_unmined_at_nu7_expires_with_it_and_is_built_again_under_nu7() {
    let nu7 = nu7();
    let (mut funded, key, _) = Funded::joint(Network::MainNetwork, nu7 - 3, 4).await;
    let to = random_joint()
        .unified_address(funded.wallet.network.network_type())
        .parse()
        .unwrap();
    let sweep = |funded: &mut Funded| {
        funded
            .wallet
            .sweep(&PROVER, funded.account, &key, &to)
            .unwrap()
    };
    let before = sweep(&mut funded);
    let tx = funded.wallet.db.get_transaction(before).unwrap().unwrap();
    assert_eq!(tx.consensus_branch_id(), BranchId::Nu6_3);
    assert_eq!(tx.expiry_height(), BlockHeight::from_u32(nu7 - 1));
    assert_eq!(funded.spendable(), 0);

    funded.scan(nu7, 2, 2);
    assert!(!funded.wallet.is_mined(before).unwrap());
    assert!(funded.wallet.is_expired(before).unwrap());
    assert_eq!(funded.spendable(), DEPOSIT);

    let after = sweep(&mut funded);
    let tx = funded.wallet.db.get_transaction(after).unwrap().unwrap();
    assert_eq!(tx.consensus_branch_id(), BranchId::Nu7);
    assert_eq!(tx.expiry_height(), BlockHeight::from_u32(nu7 + 1 + 120));
}

#[tokio::test]
async fn a_payment_proved_ahead_holds_its_note_until_sent_or_released() {
    let (mut funded, usk) = Funded::derived(Network::TestNetwork, TIP, 5).await;
    let account = funded.account;
    let home = random_joint();
    let to: ZcashAddress = home
        .unified_address(funded.wallet.network.network_type())
        .parse()
        .unwrap();
    let payment = [(to, DEPOSIT / 2)];
    let prove = |funded: &mut Funded, holder: u8| {
        funded
            .wallet
            .prove_payment(&PROVER, account, [holder; 32], &payment, 2)
    };

    let held = prove(&mut funded, 1).unwrap();
    assert_eq!(funded.spendable(), 0);
    assert!(funded.wallet.pay(&PROVER, account, &usk, &payment).is_err());
    assert!(prove(&mut funded, 2).is_err());
    drop(held);
    let held = prove(&mut funded, 1).unwrap();
    funded.wallet.release(held).unwrap();
    assert_eq!(funded.spendable(), DEPOSIT);

    let held = prove(&mut funded, 1).unwrap();
    funded.scan(TIP + 3, 2, 2);
    assert!(funded.wallet.holds(&held).unwrap());
    funded.scan(TIP + 4, 4, 1);
    assert!(!funded.wallet.holds(&held).unwrap());
    assert!(funded.wallet.send_proved(held, &usk).is_err());
    assert_eq!(funded.spendable(), DEPOSIT);

    let held = prove(&mut funded, 1).unwrap();
    let another =
        UnifiedSpendingKey::from_seed(&funded.wallet.network, &[8; 32], zip32::AccountId::ZERO)
            .unwrap();
    assert!(funded.wallet.send_proved(held, &another).is_err());
    assert_eq!(funded.spendable(), DEPOSIT);

    let held = prove(&mut funded, 1).unwrap();
    let txid = funded.wallet.send_proved(held, &usk).unwrap();
    assert_eq!(funded.spendable(), 0);
    assert_eq!(funded.paid(txid, &home), DEPOSIT / 2);
}

#[test]
fn an_address_derived_from_the_key_alone_is_the_wallets_own_for_that_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), Network::TestNetwork).unwrap();
    let (account, key) = derive_at(&mut wallet, &[8; 32], "fees", TIP);
    let key = key.to_unified_full_viewing_key();
    let stranger = derive_at(&mut wallet, &[9; 32], "another", TIP)
        .1
        .to_unified_full_viewing_key();
    let request = UnifiedAddressRequest::ORCHARD;
    let mut own: Vec<_> = (0..3)
        .map(|_| {
            let next = wallet.db.get_next_available_address(account, request);
            let (address, index) = next.unwrap().unwrap();
            (u64::try_from(index).unwrap(), address)
        })
        .collect();
    for index in [4_207, u64::from(u32::MAX) + 3] {
        let named = wallet
            .db
            .get_address_for_index(account, index.into(), request);
        own.push((index, named.unwrap().unwrap()));
    }
    for (index, address) in own {
        let address = address.encode(&wallet.network);
        assert_eq!(address_at(&key, wallet.network, index).unwrap(), address);
        assert_ne!(
            address_at(&stranger, wallet.network, index).unwrap(),
            address
        );
    }
}

#[tokio::test]
async fn a_payment_to_a_derived_address_is_reported_under_its_index_while_mined() {
    const INDEX: u64 = 4_207;
    let mut fees = None;
    let mut funded = Funded::paying(Network::TestNetwork, TIP, 2, |wallet| {
        let (account, key) = derive_at(wallet, &[8; 32], "fees", TIP);
        let fee_address =
            address_at(&key.to_unified_full_viewing_key(), wallet.network, INDEX).unwrap();
        fees = Some((account, key));
        let (inventory, key) = derive_at(wallet, &[7; 32], INVENTORY, TIP);
        let fvk = key.to_unified_full_viewing_key();
        let deposit = fvk.orchard().unwrap().address_at(0u32, Scope::External);
        let outputs = vec![
            (0, deposit, DEPOSIT),
            (1, orchard_receiver(wallet, &fee_address), FEE),
        ];
        (inventory, outputs)
    })
    .await;
    let ((fees, fee_key), inventory) = (fees.unwrap(), funded.account);
    let received = |funded: &Funded, account, indices: &[u64]| {
        funded.wallet.received_at(account, indices).unwrap()
    };
    assert_eq!(
        received(&funded, fees, &[INDEX - 1, INDEX, INDEX + 1, INDEX]),
        [0, FEE, 0, FEE]
    );
    assert_eq!(received(&funded, inventory, &[INDEX, 0]), [0, DEPOSIT]);
    assert_eq!(received(&funded, fees, &[]), [0; 0]);

    funded
        .wallet
        .db
        .truncate_to_height(BlockHeight::from_u32(TIP))
        .unwrap();
    let unmined: u32 = funded
        .wallet
        .views
        .query_row(
            "SELECT COUNT(*) FROM v_tx_outputs WHERE tx_mined_height IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unmined, 1);
    assert_eq!(received(&funded, fees, &[INDEX]), [0]);
    assert_eq!(received(&funded, inventory, &[0]), [DEPOSIT]);

    funded.scan(TIP + 1, 1, 1);
    assert_eq!(received(&funded, fees, &[INDEX]), [FEE]);

    let home = random_joint()
        .unified_address(funded.wallet.network.network_type())
        .parse()
        .unwrap();
    funded
        .wallet
        .pay(&PROVER, fees, &fee_key, &[(home, FEE / 4)])
        .unwrap();
    assert_eq!(funded.wallet.funds(fees).unwrap().spendable, 0);
    assert_eq!(received(&funded, fees, &[INDEX]), [FEE]);
}

#[tokio::test]
async fn the_inventory_account_is_found_by_its_seed_among_other_seeds_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let mut wallet = Wallet::open(dir.path().join("wallet.sqlite"), Network::TestNetwork).unwrap();
    let (fees, _) = derive_at(&mut wallet, &[8; 32], "fees", TIP);
    let (inventory, key) = derive_at(&mut wallet, &[7; 32], INVENTORY, TIP);
    let mut client = lightwalletd::connect_lazy("http://127.0.0.1:9").unwrap();

    let (found, found_key) = wallet
        .inventory_account(&mut client, &[7; 32])
        .await
        .unwrap();
    assert_eq!(found, inventory);
    let encoded = |key: &UnifiedSpendingKey| {
        key.to_unified_full_viewing_key()
            .encode(&Network::TestNetwork)
    };
    assert_eq!(encoded(&found_key), encoded(&key));
    let (found, _) = wallet
        .seed_account(&mut client, &[8; 32], "fees")
        .await
        .unwrap();
    assert_eq!(found, fees);

    let refused = wallet.inventory_account(&mut client, &[9; 32]).await;
    assert!(
        matches!(&refused, Err(Error::Wallet(e)) if e == "inventory seed does not match the wallet"),
        "{:?}",
        refused.map(|(account, _)| account)
    );
    assert_eq!(wallet.accounts().unwrap().len(), 2);
}

#[test]
fn an_expiry_stops_before_an_upgrade_only_when_it_would_outlive_it() {
    let nu7 = BlockHeight::from_u32(nu7());
    let mainnet = Network::MainNetwork;
    assert_eq!(expiry_before_upgrade(&mainnet, nu7 - 100), Some(nu7 - 60));
    assert_eq!(expiry_before_upgrade(&mainnet, nu7 - 1), Some(nu7 - 1));
    assert_eq!(expiry_before_upgrade(&mainnet, nu7), None);
}

#[tokio::test]
async fn a_joint_account_paid_in_too_many_notes_is_refused_its_sweep_before_any_proof() {
    let (e, z) = (
        SecretShare::random(UnwrapErr(SysRng)),
        SecretShare::random(UnwrapErr(SysRng)),
    );
    let viewing = ViewingKeys::random(UnwrapErr(SysRng));
    let joint = JointAccount::derive(&e.public(), &z.public(), &viewing).unwrap();
    let key = joint.spend_key(&e, &z).unwrap();
    let joint = &joint;
    let paid_in = |notes: usize| {
        Funded::paying(Network::TestNetwork, TIP, 2, move |wallet| {
            let account = import_at(wallet, joint, TIP);
            (account, vec![(0, joint.deposit_address(), 10_000); notes])
        })
    };
    let home = random_joint();
    let to = home
        .unified_address(Network::TestNetwork.network_type())
        .parse()
        .unwrap();

    let mut most = paid_in(64).await;
    let fee = most.wallet.sweep_fee(most.account, &to).unwrap();
    assert_eq!(fee, SWEEP_FEE_CEILING);

    let mut over = paid_in(65).await;
    let fee = over.wallet.sweep_fee(over.account, &to).unwrap();
    assert_eq!(fee, SWEEP_FEE_CEILING + 5_000);
    let refused = over.wallet.sweep(&PROVER, over.account, &key, &to);
    assert!(
        matches!(
            refused,
            Err(crate::Error::SweepFee {
                fee: 325_000,
                ceiling: SWEEP_FEE_CEILING
            })
        ),
        "{refused:?}"
    );
    assert_eq!(over.spendable(), 650_000);
}
