use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use incrementalmerkletree::frontier::{CommitmentTree, Frontier};
use incrementalmerkletree::witness::IncrementalWitness;
use incrementalmerkletree::{Hashable, Level, Position};
use orchard::builder::{Builder, BundleType};
use orchard::bundle::{BatchValidator, BundleVersion};
use orchard::circuit::{OrchardCircuitVersion, VerifyingKey};
use orchard::keys::{FullViewingKey, PreparedIncomingViewingKey, Scope};
use orchard::note::{ExtractedNoteCommitment, Nullifier};
use orchard::note_encryption::{CompactAction, IronwoodDomain, OrchardDomain};
use orchard::tree::{MerkleHashOrchard, MerklePath};
use orchard::value::NoteValue;
use orchard::{Address, Anchor, Note, ValuePool};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use zcash_client_backend::proto::compact_formats::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx,
};
use zcash_client_backend::proto::service::{SubtreeRoot, TreeState};
use zcash_note_encryption::{EphemeralKeyBytes, try_compact_note_decryption};
use zcash_primitives::merkle_tree::write_commitment_tree;
use zcash_primitives::transaction::Transaction;
use zcash_primitives::transaction::components::orchard::bundle_version_for_branch;
use zcash_protocol::consensus::{BlockHeight, BranchId, Network, NetworkType, Parameters};
use zcash_protocol::value::BalanceError;

use crate::seed::Seed;

pub const SHARD: u64 = 1 << 16;
pub const MARGINAL_FEE: u64 = 5_000;
pub const JOINT_KEYS: usize = 2;
const PAST: u32 = 24;

pub type Tree = Frontier<MerkleHashOrchard, 32>;

#[derive(Clone, Copy, Debug)]
pub struct Sky {
    pub steady: u32,
    pub reorg: u64,
    pub reorg_after_tx: u64,
    pub reorg_depth: u64,
    pub dropped: u64,
    pub delayed: u64,
    pub lost: u64,
    pub quick: u64,
    pub traffic: u64,
    pub rival: u64,
}

impl Sky {
    pub const CALM: Self = Self {
        steady: 0,
        reorg: 0,
        reorg_after_tx: 0,
        reorg_depth: 1,
        dropped: 0,
        delayed: 0,
        lost: 0,
        quick: 0,
        traffic: 1000,
        rival: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Malformed,
    Expired,
    AnotherBranch,
    AnchorOfAnOrphan,
    AnchorOfNoBlock,
    Spent,
    Conflicts,
    Unpaid,
    Invalid,
    Unjudged,
    InTheChain,
    InThePool,
}

impl Refused {
    pub fn code(self) -> i32 {
        match self {
            Self::Malformed => -22,
            Self::InTheChain => -27,
            Self::InThePool => -26,
            _ => -25,
        }
    }

    pub fn stale(self) -> bool {
        matches!(
            self,
            Self::Expired | Self::AnotherBranch | Self::AnchorOfAnOrphan
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Took,
    Refused(Refused),
}

#[derive(Clone)]
struct Owned {
    key: usize,
    note: Note,
    nullifier: [u8; 32],
}

#[derive(Clone)]
struct Leaf {
    pool: ValuePool,
    action: CompactAction,
    ephemeral: [u8; 32],
    ciphertext: [u8; 52],
    owned: Option<Owned>,
}

impl Leaf {
    fn of<T>(pool: ValuePool, action: &orchard::Action<T>, keys: &[FullViewingKey]) -> Self {
        let sealed = action.encrypted_note();
        let mut ciphertext = [0; 52];
        ciphertext.copy_from_slice(&sealed.enc_ciphertext.0[..52]);
        let action = CompactAction::from(action);
        let owned = keys.iter().enumerate().find_map(|(key, held)| {
            let note = decrypt(pool, &action, held)?;
            Some(Owned {
                key,
                nullifier: note.nullifier(held).to_bytes(),
                note,
            })
        });
        Self {
            pool,
            action,
            ephemeral: sealed.epk_bytes,
            ciphertext,
            owned,
        }
    }
}

#[derive(Clone)]
enum What {
    Paid(usize),
    Sent(usize),
    Traffic,
    Rival,
}

#[derive(Clone)]
struct Entry {
    what: What,
    txid: [u8; 32],
    leaves: Vec<Leaf>,
}

#[derive(Clone)]
struct Trees {
    orchard: Tree,
    ironwood: Tree,
}

impl Trees {
    fn of(&self, pool: ValuePool) -> &Tree {
        match pool {
            ValuePool::Orchard => &self.orchard,
            ValuePool::Ironwood => &self.ironwood,
        }
    }

    fn of_mut(&mut self, pool: ValuePool) -> &mut Tree {
        match pool {
            ValuePool::Orchard => &mut self.orchard,
            ValuePool::Ironwood => &mut self.ironwood,
        }
    }
}

struct Block {
    height: u32,
    hash: [u8; 32],
    time: u32,
    entries: Vec<Entry>,
    trees: Trees,
    roots: [[u8; 32]; 2],
}

struct Payment {
    entry: Entry,
    mined: Option<u32>,
    gone: bool,
}

struct Sent {
    raw: Vec<u8>,
    tx: Transaction,
    entry: Entry,
    nullifiers: Vec<[u8; 32]>,
    mined: Option<u32>,
    orphaned: bool,
    rivalled: bool,
    first: Outcome,
}

#[derive(Clone, Debug)]
pub struct Held {
    pub value: u64,
    pub mined: u32,
    pub to: Address,
    pub spent: Option<u32>,
    pub nullifier: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct Fact {
    pub ordinal: usize,
    pub expiry: u32,
    pub branch: BranchId,
    pub fee: u64,
    pub actions: usize,
    pub nullifiers: Vec<[u8; 32]>,
    pub mined: Option<u32>,
    pub first: Outcome,
    pub rivalled: bool,
}

struct Inner {
    seed: Seed,
    network: Network,
    keys: Vec<FullViewingKey>,
    sky: Sky,
    clears: u64,
    blocks: Vec<Block>,
    forks: u64,
    reorged: Vec<(u32, u32)>,
    shards: [Vec<(MerkleHashOrchard, u32)>; 2],
    orphans: BTreeSet<[u8; 32]>,
    payments: Vec<Payment>,
    queue: Vec<usize>,
    rivals: Vec<Entry>,
    transactions: Vec<Sent>,
    pool: Vec<usize>,
    trace: Vec<String>,
}

#[derive(Clone)]
pub struct Ledger(Arc<Mutex<Inner>>);

fn slot(pool: ValuePool) -> usize {
    match pool {
        ValuePool::Orchard => 0,
        ValuePool::Ironwood => 1,
    }
}

fn node(seed: Seed, what: &str) -> MerkleHashOrchard {
    let mut bytes = seed.bytes::<32>(what);
    bytes[31] &= 0x3f;
    Option::from(MerkleHashOrchard::from_bytes(&bytes)).expect("a field element")
}

fn grown(seed: Seed, name: &str, shards: u64, within: u64, height: u32) -> (Tree, Shards) {
    let roots: Vec<MerkleHashOrchard> = (0..shards)
        .map(|index| node(seed, &format!("{name}-shard-{index}")))
        .collect();
    let position = shards * SHARD + within;
    let mut ommers = Vec::new();
    for level in 0..32u8 {
        if position >> level & 1 == 0 {
            continue;
        }
        if level < 16 {
            ommers.push(node(seed, &format!("{name}-ommer-{level}")));
            continue;
        }
        let width = 1u64 << (level - 16);
        let first = (shards >> (level - 15)) << (level - 15);
        let mut row: Vec<MerkleHashOrchard> =
            roots[first as usize..(first + width) as usize].to_vec();
        let mut at = 16u8;
        while row.len() > 1 {
            row = row
                .chunks(2)
                .map(|pair| MerkleHashOrchard::combine(Level::from(at), &pair[0], &pair[1]))
                .collect();
            at += 1;
        }
        ommers.push(row[0]);
    }
    let leaf = node(seed, &format!("{name}-leaf"));
    let tree = Tree::from_parts(Position::from(position), leaf, ommers).expect("a frontier");
    let known = roots
        .into_iter()
        .enumerate()
        .map(|(index, root)| (root, height - 2_000 * (shards - index as u64) as u32))
        .collect();
    (tree, known)
}

type Shards = Vec<(MerkleHashOrchard, u32)>;

fn root(tree: &Tree) -> [u8; 32] {
    tree.root().to_bytes()
}

pub fn verifying_key(version: OrchardCircuitVersion) -> Arc<VerifyingKey> {
    static KEYS: Mutex<Vec<(OrchardCircuitVersion, Arc<VerifyingKey>)>> = Mutex::new(Vec::new());
    let mut keys = KEYS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, key)) = keys.iter().find(|(held, _)| *held == version) {
        return key.clone();
    }
    let key = Arc::new(VerifyingKey::build(version));
    keys.push((version, key.clone()));
    key
}

fn bundles(tx: &Transaction) -> impl Iterator<Item = (ValuePool, &TxBundle)> {
    [
        (ValuePool::Orchard, tx.orchard_bundle()),
        (ValuePool::Ironwood, tx.ironwood_bundle()),
    ]
    .into_iter()
    .filter_map(|(pool, bundle)| Some((pool, bundle?)))
}

type TxBundle = orchard::Bundle<orchard::bundle::Authorized, zcash_protocol::value::ZatBalance>;

fn decrypt(pool: ValuePool, action: &CompactAction, key: &FullViewingKey) -> Option<Note> {
    [Scope::External, Scope::Internal]
        .into_iter()
        .find_map(|scope| {
            let ivk = PreparedIncomingViewingKey::new(&key.to_ivk(scope));
            match pool {
                ValuePool::Orchard => try_compact_note_decryption(
                    &OrchardDomain::for_compact_action(action),
                    &ivk,
                    action,
                ),
                ValuePool::Ironwood => try_compact_note_decryption(
                    &IronwoodDomain::for_compact_action(action),
                    &ivk,
                    action,
                ),
            }
        })
        .map(|(note, _)| note)
}

impl Inner {
    fn tip(&self) -> &Block {
        self.blocks.last().expect("a base block")
    }

    fn base(&self) -> u32 {
        self.blocks[0].height + PAST
    }

    fn at(&self, height: u32) -> Option<&Block> {
        self.blocks
            .get(height.checked_sub(self.blocks[0].height)? as usize)
    }

    fn note(&mut self, what: String) {
        let tip = self.tip().height - self.base();
        self.trace.push(format!("#{tip} {what}"));
    }

    fn hash(&self, height: u32) -> [u8; 32] {
        self.seed
            .bytes(&format!("zcash-block-{height}-{}", self.forks))
    }

    fn spent(&self, nullifier: &[u8; 32]) -> Option<u32> {
        self.blocks.iter().find_map(|block| {
            block
                .entries
                .iter()
                .flat_map(|entry| &entry.leaves)
                .any(|leaf| leaf.action.nullifier().to_bytes() == *nullifier)
                .then_some(block.height)
        })
    }

    fn judge(&self, ordinal: usize, height: u32, verify: bool) -> Result<(), Refused> {
        let sent = &self.transactions[ordinal];
        let tx = &sent.tx;
        let branch = BranchId::for_height(&self.network, BlockHeight::from(height));
        let expiry = u32::from(tx.expiry_height());
        if expiry != 0 && expiry < height {
            return Err(Refused::Expired);
        }
        if tx.consensus_branch_id() != branch || !tx.version().valid_in_branch(branch) {
            return Err(Refused::AnotherBranch);
        }
        if tx.transparent_bundle().is_some()
            || tx.sapling_bundle().is_some()
            || tx.sprout_bundle().is_some()
        {
            return Err(Refused::Unjudged);
        }
        for (pool, bundle) in bundles(tx) {
            if bundle_version_for_branch(branch, pool) != Some(bundle.bundle_version()) {
                return Err(Refused::AnotherBranch);
            }
            let anchor = bundle.anchor().to_bytes();
            let known = self
                .blocks
                .iter()
                .any(|block| block.roots[slot(pool)] == anchor);
            if !known && self.orphans.contains(&anchor) {
                return Err(Refused::AnchorOfAnOrphan);
            }
            if !known {
                return Err(Refused::AnchorOfNoBlock);
            }
        }
        if sent.nullifiers.is_empty() {
            return Err(Refused::Malformed);
        }
        let mut seen = BTreeSet::new();
        for nullifier in &sent.nullifiers {
            if !seen.insert(*nullifier) || self.spent(nullifier).is_some() {
                return Err(Refused::Spent);
            }
        }
        let rivals = self.pool.iter().filter(|other| **other != ordinal);
        for rival in rivals {
            let theirs = &self.transactions[*rival].nullifiers;
            if theirs.iter().any(|nullifier| seen.contains(nullifier)) {
                return Err(Refused::Conflicts);
            }
        }
        let fee = tx
            .fee_paid(|_| Ok::<_, BalanceError>(None))
            .ok()
            .flatten()
            .map(u64::from);
        let actions = sent.nullifiers.len().max(2) as u64;
        if fee.is_none_or(|fee| fee < MARGINAL_FEE * actions) {
            return Err(Refused::Unpaid);
        }
        if !verify {
            return Ok(());
        }
        let sighash: [u8; 32] = *tx.txid().as_ref();
        for (_, bundle) in bundles(tx) {
            let key = verifying_key(bundle.bundle_version().circuit_version());
            let mut validator = BatchValidator::new(&key);
            if validator.add_bundle(bundle, sighash).is_err() {
                return Err(Refused::Invalid);
            }
            let rng = ChaCha20Rng::from_seed(self.seed.bytes(&format!("validate-{ordinal}")));
            if !validator.validate(rng) {
                return Err(Refused::Invalid);
            }
        }
        Ok(())
    }

    fn traffic(&self, height: u32) -> Entry {
        let name = |what: &str| format!("zcash-traffic-{what}-{height}-{}", self.forks);
        let mut spent = self.seed.bytes::<32>(&name("nullifier"));
        spent[31] &= 0x3f;
        let spent = Nullifier::from_bytes(&spent).into_option();
        let made = node(self.seed, &name("note")).to_bytes();
        let made = ExtractedNoteCommitment::from_bytes(&made).into_option();
        let ephemeral = self.seed.bytes(&name("key"));
        let ciphertext = self.seed.bytes(&name("sealed"));
        Entry {
            what: What::Traffic,
            txid: self.seed.bytes(&name("txid")),
            leaves: vec![Leaf {
                pool: ValuePool::Ironwood,
                action: CompactAction::from_parts(
                    spent.expect("a nullifier"),
                    made.expect("a commitment"),
                    EphemeralKeyBytes(ephemeral),
                    ciphertext,
                ),
                ephemeral,
                ciphertext,
                owned: None,
            }],
        }
    }

    fn shared(&self, ordinal: usize) -> bool {
        let spent = &self.transactions[ordinal].nullifiers;
        let leaves = self.blocks.iter().flat_map(|block| &block.entries);
        let owned = leaves
            .flat_map(|entry| &entry.leaves)
            .filter_map(|leaf| leaf.owned.as_ref());
        owned
            .filter(|owned| spent.contains(&owned.nullifier))
            .any(|owned| owned.key >= JOINT_KEYS)
    }

    fn rival(&self, ordinal: usize) -> Entry {
        let name = |what: &str, n: usize| format!("zcash-rival-{what}-{ordinal}-{n}");
        let leaves = self.transactions[ordinal].nullifiers.iter().enumerate();
        let leaves = leaves.map(|(n, spent)| {
            let spent = Nullifier::from_bytes(spent).into_option();
            let made = node(self.seed, &name("note", n)).to_bytes();
            let made = ExtractedNoteCommitment::from_bytes(&made).into_option();
            let ephemeral = self.seed.bytes(&name("key", n));
            let ciphertext = self.seed.bytes(&name("sealed", n));
            Leaf {
                pool: ValuePool::Ironwood,
                action: CompactAction::from_parts(
                    spent.expect("a nullifier"),
                    made.expect("a commitment"),
                    EphemeralKeyBytes(ephemeral),
                    ciphertext,
                ),
                ephemeral,
                ciphertext,
                owned: None,
            }
        });
        Entry {
            what: What::Rival,
            txid: self.seed.bytes(&name("txid", 0)),
            leaves: leaves.collect(),
        }
    }

    fn mine(&mut self, time: u64) {
        let height = self.tip().height + 1;
        let calm = time >= self.clears;
        let mut trees = self.tip().trees.clone();
        let mut entries = Vec::new();
        let mut names = Vec::new();
        let roll = u64::from(height) * 64 + self.forks;
        let young = height - self.base() <= self.sky.steady + 1;
        if young || self.seed.chance("zcash-traffic", roll, self.sky.traffic) {
            entries.push(self.traffic(height));
        }
        entries.append(&mut self.rivals);
        for id in std::mem::take(&mut self.queue) {
            let roll = (id as u64 * 4096 + u64::from(height)) * 64 + self.forks % 64;
            if !calm
                && self
                    .seed
                    .chance("zcash-payment-delayed", roll, self.sky.delayed)
            {
                self.queue.push(id);
                continue;
            }
            self.payments[id].mined = Some(height);
            entries.push(self.payments[id].entry.clone());
            names.push(format!("payment {id}"));
        }
        for ordinal in std::mem::take(&mut self.pool) {
            let roll = (ordinal as u64 * 4096 + u64::from(height)) * 64 + self.forks % 64;
            let taken: BTreeSet<[u8; 32]> = entries
                .iter()
                .flat_map(|entry: &Entry| &entry.leaves)
                .map(|leaf| leaf.action.nullifier().to_bytes())
                .collect();
            let clash = self.transactions[ordinal]
                .nullifiers
                .iter()
                .any(|nullifier| taken.contains(nullifier));
            let verdict = match clash {
                true => Err(Refused::Spent),
                false => self.judge(ordinal, height, false),
            };
            if let Err(why) = verdict {
                self.note(format!("transaction {ordinal} left the pool: {why:?}"));
                continue;
            }
            if !calm && self.seed.chance("zcash-tx-lost", roll, self.sky.lost) {
                self.note(format!("transaction {ordinal} was lost from the pool"));
                continue;
            }
            if !calm && self.seed.chance("zcash-tx-delayed", roll, self.sky.delayed) {
                self.pool.push(ordinal);
                continue;
            }
            self.transactions[ordinal].mined = Some(height);
            entries.push(self.transactions[ordinal].entry.clone());
            names.push(format!("transaction {ordinal}"));
        }
        for leaf in entries.iter().flat_map(|entry| &entry.leaves) {
            let tree = trees.of_mut(leaf.pool);
            tree.append(MerkleHashOrchard::from_cmx(&leaf.action.cmx()));
            let size = tree.tree_size();
            if size.is_multiple_of(SHARD) {
                let full = tree.value().expect("a leaf").root(Some(Level::from(16)));
                self.shards[slot(leaf.pool)].push((full, height));
            }
        }
        let mut roots = self.tip().roots;
        for pool in [ValuePool::Orchard, ValuePool::Ironwood] {
            let mut leaves = entries.iter().flat_map(|entry| &entry.leaves);
            if leaves.any(|leaf| leaf.pool == pool) {
                roots[slot(pool)] = root(trees.of(pool));
            }
        }
        let hash = self.hash(height);
        self.blocks.push(Block {
            height,
            hash,
            time: time as u32,
            entries,
            trees,
            roots,
        });
        self.trace.push(format!(
            "block {} {} with {}",
            height - self.base(),
            hex::encode(&hash[..4]),
            match names.is_empty() {
                true => "nothing".to_owned(),
                false => names.join(", "),
            }
        ));
    }

    fn reorg(&mut self, depth: u64, time: u64) {
        let room = u64::from(self.tip().height - self.base()).saturating_sub(1);
        let depth = depth.min(room) as usize;
        if depth == 0 {
            return self.mine(time);
        }
        let from = self.tip().height - self.base();
        let last = self.tip().height;
        self.reorged.push((last + 1 - depth as u32, last));
        let removed = self.blocks.split_off(self.blocks.len() - depth);
        self.forks += 1;
        let tip = self.tip().height;
        for shards in &mut self.shards {
            shards.retain(|(_, height)| *height <= tip);
        }
        self.trace.push(format!("reorg of {depth} from {from}"));
        for block in removed {
            self.orphans.extend(block.roots);
            for entry in block.entries {
                match entry.what {
                    What::Paid(id) => {
                        self.payments[id].mined = None;
                        let roll = id as u64 * 4096 + self.forks;
                        if self
                            .seed
                            .chance("zcash-payment-dropped", roll, self.sky.dropped)
                        {
                            self.payments[id].gone = true;
                            self.trace
                                .push(format!("payment {id} is gone with its block"));
                        } else {
                            self.queue.push(id);
                        }
                    }
                    What::Sent(ordinal) => {
                        self.transactions[ordinal].mined = None;
                        self.transactions[ordinal].orphaned = true;
                        self.pool.push(ordinal);
                        let roll = ordinal as u64 * 4096 + self.forks;
                        let shared = self.shared(ordinal);
                        if shared && self.seed.chance("zcash-rival", roll, self.sky.rival) {
                            self.transactions[ordinal].rivalled = true;
                            self.rivals.push(self.rival(ordinal));
                            self.trace.push(format!(
                                "another holder of the key spends what transaction {ordinal} spent"
                            ));
                        }
                    }
                    What::Traffic => {}
                    What::Rival => self.rivals.push(Entry {
                        what: What::Rival,
                        txid: entry.txid,
                        leaves: entry.leaves,
                    }),
                }
            }
        }
        for _ in 0..=depth {
            self.mine(time);
        }
    }
}

impl Ledger {
    pub fn new(
        seed: Seed,
        network: Network,
        keys: Vec<FullViewingKey>,
        base: u32,
        time: u64,
        sky: Sky,
        clears: u64,
    ) -> Self {
        let within = |name: &str| match seed.below(&format!("{name}-fill"), 0, 3) {
            0 => seed.between(&format!("{name}-near"), 0, SHARD - 12, SHARD - 2),
            _ => seed.between(&format!("{name}-far"), 0, 1, SHARD / 2),
        };
        let shards = |name: &str| seed.between(&format!("{name}-shards"), 0, 1, 6);
        let (orchard, orchard_shards) =
            grown(seed, "orchard", shards("orchard"), within("orchard"), base);
        let (ironwood, ironwood_shards) = grown(
            seed,
            "ironwood",
            shards("ironwood"),
            within("ironwood"),
            base,
        );
        let trees = Trees { orchard, ironwood };
        let roots = [root(&trees.orchard), root(&trees.ironwood)];
        let mut inner = Inner {
            seed,
            network,
            keys,
            sky,
            clears,
            blocks: Vec::new(),
            forks: 0,
            reorged: Vec::new(),
            shards: [orchard_shards, ironwood_shards],
            orphans: BTreeSet::new(),
            payments: Vec::new(),
            queue: Vec::new(),
            rivals: Vec::new(),
            transactions: Vec::new(),
            pool: Vec::new(),
            trace: Vec::new(),
        };
        for back in (0..2 + PAST).rev() {
            let height = base - back;
            let hash = inner.hash(height);
            inner.blocks.push(Block {
                height,
                hash,
                time: time as u32,
                entries: Vec::new(),
                trees: trees.clone(),
                roots,
            });
        }
        Self(Arc::new(Mutex::new(inner)))
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn kind(&self) -> NetworkType {
        self.inner().network.network_type()
    }

    pub fn tip(&self) -> (u32, [u8; 32]) {
        let inner = self.inner();
        (inner.tip().height, inner.tip().hash)
    }

    pub fn base(&self) -> u32 {
        self.inner().base()
    }

    pub fn note(&self, what: String) {
        self.inner().note(what);
    }

    pub fn reorged(&self, at: u32) -> bool {
        let reorged = &self.inner().reorged;
        reorged
            .iter()
            .any(|(first, last)| *first < at && at <= last + 1)
    }

    pub fn trace(&self) -> Vec<String> {
        self.inner().trace.clone()
    }

    pub fn quick(&self, time: u64) -> bool {
        let inner = self.inner();
        let height = u64::from(inner.tip().height);
        time < inner.clears
            && inner
                .seed
                .chance("zcash-quick-block", height, inner.sky.quick)
    }

    pub fn tick(&self, time: u64) {
        let mut inner = self.inner();
        let height = u64::from(inner.tip().height);
        let roll = height * 64 + inner.forks;
        let odds = match inner.tip().entries.is_empty() {
            true => inner.sky.reorg,
            false => inner.sky.reorg_after_tx,
        };
        let young = inner.tip().height - inner.base() <= inner.sky.steady;
        if !young && time < inner.clears && inner.seed.chance("zcash-reorg", roll, odds) {
            let depth = inner
                .seed
                .between("zcash-reorg-depth", roll, 1, inner.sky.reorg_depth);
            inner.reorg(depth, time);
        } else {
            inner.mine(time);
        }
    }

    pub fn pay(&self, to: Address, value: u64, pool: ValuePool) -> usize {
        let mut inner = self.inner();
        let id = inner.payments.len();
        let version = match pool {
            ValuePool::Orchard => BundleVersion::orchard_v3(),
            ValuePool::Ironwood => BundleVersion::ironwood_v3(),
        };
        let mut builder = Builder::new(
            BundleType::DEFAULT,
            version,
            version.default_flags(),
            Anchor::empty_tree(),
        )
        .expect("a builder");
        builder
            .add_output(None, to, NoteValue::from_raw(value), [0; 512])
            .expect("an output");
        let mut rng = ChaCha20Rng::from_seed(inner.seed.bytes(&format!("zcash-payment-{id}")));
        let (bundle, meta): (orchard::Bundle<_, i64>, _) = builder
            .build(&mut rng)
            .expect("a bundle")
            .expect("its actions");
        let action = &bundle.actions()[meta.output_action_index(0).expect("the output")];
        let entry = Entry {
            what: What::Paid(id),
            txid: inner.seed.bytes(&format!("zcash-payment-txid-{id}")),
            leaves: vec![Leaf::of(pool, action, &inner.keys)],
        };
        inner.payments.push(Payment {
            entry,
            mined: None,
            gone: false,
        });
        inner.queue.push(id);
        inner.note(format!("payment {id} of {value} is on its way"));
        id
    }

    pub fn payment(&self, id: usize) -> ([u8; 32], Option<u32>, bool) {
        let inner = self.inner();
        let payment = &inner.payments[id];
        (payment.entry.txid, payment.mined, payment.gone)
    }

    pub fn accept(&self, raw: &[u8], repeats_quietly: bool) -> Outcome {
        let mut inner = self.inner();
        let next = inner.tip().height + 1;
        let branch = BranchId::for_height(&inner.network, BlockHeight::from(next));
        let Ok(tx) = Transaction::read(raw, branch) else {
            let outcome = Outcome::Refused(Refused::Malformed);
            inner.note("a broadcast that is no transaction was refused".into());
            return outcome;
        };
        let txid: [u8; 32] = *tx.txid().as_ref();
        let known = inner
            .transactions
            .iter()
            .position(|sent| sent.entry.txid == txid);
        let ordinal = known.unwrap_or(inner.transactions.len());
        if known.is_none() {
            let keys = &inner.keys;
            let leaves: Vec<Leaf> = bundles(&tx)
                .flat_map(|(pool, bundle)| {
                    let actions = bundle.actions().iter();
                    actions.map(move |action| Leaf::of(pool, action, keys))
                })
                .collect();
            let nullifiers = leaves
                .iter()
                .map(|leaf| leaf.action.nullifier().to_bytes())
                .collect();
            inner.transactions.push(Sent {
                raw: raw.to_vec(),
                tx,
                entry: Entry {
                    what: What::Sent(ordinal),
                    txid,
                    leaves,
                },
                nullifiers,
                mined: None,
                orphaned: false,
                rivalled: false,
                first: Outcome::Took,
            });
        }
        let held = match (
            inner.transactions[ordinal].mined,
            inner.pool.contains(&ordinal),
        ) {
            (Some(_), _) => Some(Refused::InTheChain),
            (None, true) => Some(Refused::InThePool),
            (None, false) => None,
        };
        let outcome = match held {
            Some(Refused::InThePool) if repeats_quietly => Outcome::Took,
            Some(held) => Outcome::Refused(held),
            None => match inner.judge(ordinal, next, true) {
                Ok(()) => {
                    inner.pool.push(ordinal);
                    Outcome::Took
                }
                Err(why) => Outcome::Refused(why),
            },
        };
        if known.is_none() {
            inner.transactions[ordinal].first = outcome;
        }
        inner.note(match outcome {
            Outcome::Took => format!("transaction {ordinal} was taken"),
            Outcome::Refused(why) => format!("transaction {ordinal} was refused: {why:?}"),
        });
        outcome
    }

    pub fn holds(&self, txid: &[u8; 32]) -> Option<(Vec<u8>, Option<u32>)> {
        let inner = self.inner();
        let ordinal = inner
            .transactions
            .iter()
            .position(|sent| sent.entry.txid == *txid)?;
        let sent = &inner.transactions[ordinal];
        (sent.mined.is_some() || inner.pool.contains(&ordinal))
            .then(|| (sent.raw.clone(), sent.mined))
    }

    pub fn orphan(&self, txid: &[u8; 32]) -> Option<Vec<u8>> {
        let inner = self.inner();
        let sent = inner
            .transactions
            .iter()
            .find(|sent| sent.entry.txid == *txid)?;
        sent.orphaned.then(|| sent.raw.clone())
    }

    pub fn ordinal(&self, txid: &[u8; 32]) -> Option<usize> {
        let inner = self.inner();
        inner
            .transactions
            .iter()
            .position(|sent| sent.entry.txid == *txid)
    }

    pub fn mined(&self, txid: &[u8; 32]) -> Option<u32> {
        let inner = self.inner();
        let sent = inner
            .transactions
            .iter()
            .find(|sent| sent.entry.txid == *txid)
            .map(|sent| sent.mined);
        let paid = inner
            .payments
            .iter()
            .find(|payment| payment.entry.txid == *txid)
            .map(|payment| payment.mined);
        sent.or(paid).flatten()
    }

    pub fn facts(&self) -> Vec<Fact> {
        let inner = self.inner();
        inner
            .transactions
            .iter()
            .enumerate()
            .map(|(ordinal, sent)| Fact {
                ordinal,
                expiry: sent.tx.expiry_height().into(),
                branch: sent.tx.consensus_branch_id(),
                fee: sent
                    .tx
                    .fee_paid(|_| Ok::<_, BalanceError>(None))
                    .ok()
                    .flatten()
                    .map_or(0, u64::from),
                actions: sent.nullifiers.len(),
                nullifiers: sent.nullifiers.clone(),
                mined: sent.mined,
                first: sent.first,
                rivalled: sent.rivalled,
            })
            .collect()
    }

    pub fn with_transaction<T>(&self, ordinal: usize, read: impl FnOnce(&Transaction) -> T) -> T {
        read(&self.inner().transactions[ordinal].tx)
    }

    pub fn paid_by(&self, ordinal: usize, key: usize) -> Vec<(Address, u64)> {
        let inner = self.inner();
        let leaves = &inner.transactions[ordinal].entry.leaves;
        let owned = leaves.iter().filter_map(|leaf| leaf.owned.as_ref());
        owned
            .filter(|owned| owned.key == key)
            .map(|owned| (owned.note.recipient(), owned.note.value().inner()))
            .collect()
    }

    pub fn limbo(&self, key: usize) -> bool {
        let inner = self.inner();
        let waiting = inner.queue.iter().map(|id| &inner.payments[*id]);
        waiting
            .flat_map(|payment| &payment.entry.leaves)
            .filter_map(|leaf| leaf.owned.as_ref())
            .any(|owned| owned.key == key)
    }

    pub fn holdings(&self, key: usize, upto: u32) -> Vec<Held> {
        let inner = self.inner();
        let mut held = Vec::new();
        for block in inner.blocks.iter().filter(|block| block.height <= upto) {
            for leaf in block.entries.iter().flat_map(|entry| &entry.leaves) {
                let Some(owned) = leaf.owned.as_ref().filter(|owned| owned.key == key) else {
                    continue;
                };
                held.push(Held {
                    value: owned.note.value().inner(),
                    mined: block.height,
                    to: owned.note.recipient(),
                    spent: inner.spent(&owned.nullifier).filter(|at| *at <= upto),
                    nullifier: owned.nullifier,
                });
            }
        }
        held
    }

    pub fn everything(&self, key: usize) -> Vec<Held> {
        let tip = self.tip().0;
        self.holdings(key, tip)
    }

    pub fn witness(
        &self,
        pool: ValuePool,
        nullifier: &[u8; 32],
    ) -> Option<(Note, Anchor, MerklePath)> {
        let inner = self.inner();
        let mut found = None;
        let mut witness: Option<IncrementalWitness<MerkleHashOrchard, 32>> = None;
        for pair in inner.blocks.windows(2) {
            let mut tree = CommitmentTree::from_frontier(pair[0].trees.of(pool));
            let leaves = pair[1].entries.iter().flat_map(|entry| &entry.leaves);
            for leaf in leaves.filter(|leaf| leaf.pool == pool) {
                let cmx = MerkleHashOrchard::from_cmx(&leaf.action.cmx());
                if let Some(witness) = &mut witness {
                    witness.append(cmx).ok()?;
                    continue;
                }
                tree.append(cmx).ok()?;
                let ours = leaf.owned.as_ref();
                if let Some(owned) = ours.filter(|owned| owned.nullifier == *nullifier) {
                    found = Some(owned.note);
                    witness = IncrementalWitness::from_tree(tree.clone());
                }
            }
        }
        let witness = witness?;
        let path = witness.path()?;
        Some((found?, witness.root().into(), path.into()))
    }

    pub fn latest(&self, lag: u32, floor: u32) -> (u32, [u8; 32]) {
        let inner = self.inner();
        let height = (inner.tip().height - lag).max(inner.base()).max(floor);
        let height = height.min(inner.tip().height);
        let block = inner.at(height).expect("a block");
        (block.height, block.hash)
    }

    pub fn compact(&self, height: u32) -> Option<CompactBlock> {
        let inner = self.inner();
        let block = inner.at(height)?;
        let previous = inner.at(height.checked_sub(1)?);
        let action = |leaf: &Leaf| CompactOrchardAction {
            nullifier: leaf.action.nullifier().to_bytes().to_vec(),
            cmx: leaf.action.cmx().to_bytes().to_vec(),
            ephemeral_key: leaf.ephemeral.to_vec(),
            ciphertext: leaf.ciphertext.to_vec(),
        };
        let of = |entry: &Entry, pool: ValuePool| -> Vec<CompactOrchardAction> {
            let leaves = entry.leaves.iter().filter(|leaf| leaf.pool == pool);
            leaves.map(action).collect()
        };
        let vtx = block
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| CompactTx {
                index: index as u64,
                txid: entry.txid.to_vec(),
                actions: of(entry, ValuePool::Orchard),
                ironwood_actions: of(entry, ValuePool::Ironwood),
                ..Default::default()
            })
            .collect();
        Some(CompactBlock {
            height: u64::from(height),
            hash: block.hash.to_vec(),
            prev_hash: previous.map_or(vec![0; 32], |previous| previous.hash.to_vec()),
            time: block.time,
            vtx,
            chain_metadata: Some(ChainMetadata {
                sapling_commitment_tree_size: 0,
                orchard_commitment_tree_size: block.trees.orchard.tree_size() as u32,
                ironwood_commitment_tree_size: block.trees.ironwood.tree_size() as u32,
            }),
            ..Default::default()
        })
    }

    pub fn tree_state(&self, height: u32) -> Option<TreeState> {
        let inner = self.inner();
        let block = inner.at(height)?;
        let encoded = |tree: &Tree| {
            let mut bytes = Vec::new();
            write_commitment_tree(&CommitmentTree::from_frontier(tree), &mut bytes)
                .expect("a tree in bytes");
            hex::encode(bytes)
        };
        let mut hash = block.hash;
        hash.reverse();
        Some(TreeState {
            network: match inner.network.network_type() {
                NetworkType::Main => "main".into(),
                _ => "test".into(),
            },
            height: u64::from(height),
            hash: hex::encode(hash),
            time: block.time,
            sapling_tree: String::new(),
            orchard_tree: encoded(&block.trees.orchard),
            ironwood_tree: encoded(&block.trees.ironwood),
        })
    }

    pub fn shard_roots(&self, pool: ValuePool, start: u32) -> Vec<SubtreeRoot> {
        let inner = self.inner();
        let hash = |height: u32| {
            inner
                .at(height)
                .map_or(vec![0; 32], |block| block.hash.to_vec())
        };
        inner.shards[slot(pool)]
            .iter()
            .skip(start as usize)
            .map(|(root, height)| SubtreeRoot {
                root_hash: root.to_bytes().to_vec(),
                completing_block_hash: hash(*height),
                completing_block_height: u64::from(*height),
            })
            .collect()
    }
}
