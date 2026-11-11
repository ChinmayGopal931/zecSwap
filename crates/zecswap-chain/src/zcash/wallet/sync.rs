//! `zcash_client_backend::sync::run` (0.25.0), except that it adds only the subtree roots the
//! wallet lacks, and refreshes no transparent UTXOs.
//!
//! The library puts every pool's whole root list on each run, and merging that list into the
//! stored tree hashes it again at every level: seconds of CPU per sync for mainnet's ~1,900 roots,
//! of which a few change a day. The accounts here spend and report only Orchard-protocol funds
//! (`transparent-inputs` is on only because `pczt` needs it), so UTXOs would be stored unread.

use std::fmt::Display;

use zcash_client_backend::data_api::chain::{
    BlockCache, CommitmentTreeRoot, error::Error as ChainError, scan_cached_blocks,
};
use zcash_client_backend::data_api::scanning::{ScanPriority, ScanRange};
use zcash_client_backend::data_api::{WalletCommitmentTrees, WalletRead, WalletWrite};
use zcash_client_backend::proto::service::{
    BlockId, BlockRange, GetSubtreeRootsArg, ShieldedProtocol,
};
use zcash_primitives::merkle_tree::HashSer;
use zcash_protocol::consensus::{BlockHeight, Network};

use super::Db;
use crate::Error;
use crate::zcash::cache::MemoryBlockCache;
use crate::zcash::lightwalletd::{self, Lightwalletd};

/// The subtrees of a note commitment tree: its depth, 32, less the shard height, 16.
const SUBTREES: u64 = 1 << 16;

pub(super) async fn run(
    client: &mut Lightwalletd,
    network: &Network,
    cache: &MemoryBlockCache,
    db: &mut Db,
    batch_size: u32,
) -> Result<(), Error> {
    update_roots(
        client,
        db,
        ShieldedProtocol::Sapling,
        Db::get_sapling_subtree_root,
        Db::put_sapling_subtree_roots,
    )
    .await?;
    update_roots(
        client,
        db,
        ShieldedProtocol::Orchard,
        Db::get_orchard_subtree_root,
        Db::put_orchard_subtree_roots,
    )
    .await?;
    update_roots(
        client,
        db,
        ShieldedProtocol::Ironwood,
        Db::get_ironwood_subtree_root,
        Db::put_ironwood_subtree_roots,
    )
    .await?;
    while running(client, network, cache, db, batch_size).await? {}
    Ok(())
}

/// Adds a pool's subtree roots from the last one the wallet holds. That one is fetched again, so
/// a root a reorganisation replaced conflicts as it would in the library's full put.
async fn update_roots<H, E>(
    client: &mut Lightwalletd,
    db: &mut Db,
    protocol: ShieldedProtocol,
    get: impl Fn(&mut Db, u64) -> Result<Option<H>, E>,
    put: impl Fn(&mut Db, u64, &[CommitmentTreeRoot<H>]) -> Result<(), E>,
) -> Result<(), Error>
where
    H: HashSer + PartialEq,
    E: Display,
{
    let start = held_roots(db, &get)?.saturating_sub(1);
    let roots = download_roots(client, protocol, start).await?;
    add_roots(db, start, &roots, &get, &put)
}

/// Puts `roots`, fetched from index `start`, unless they only repeat the root held there.
fn add_roots<H: PartialEq, E: Display>(
    db: &mut Db,
    start: u64,
    roots: &[CommitmentTreeRoot<H>],
    get: &impl Fn(&mut Db, u64) -> Result<Option<H>, E>,
    put: &impl Fn(&mut Db, u64, &[CommitmentTreeRoot<H>]) -> Result<(), E>,
) -> Result<(), Error> {
    let held = get(db, start).map_err(Error::wallet)?;
    if adds_to(held.as_ref(), roots) {
        put(db, start, roots).map_err(Error::wallet)?;
    }
    Ok(())
}

/// How many roots the wallet holds. Roots fill a tree from its first subtree, so those held are
/// the first ones.
fn held_roots<H, E: Display>(
    db: &mut Db,
    get: &impl Fn(&mut Db, u64) -> Result<Option<H>, E>,
) -> Result<u64, Error> {
    let (mut held, mut missing) = (0, SUBTREES);
    while held < missing {
        let index = held + (missing - held) / 2;
        if get(db, index).map_err(Error::wallet)?.is_some() {
            held = index + 1;
        } else {
            missing = index;
        }
    }
    Ok(held)
}

/// Whether `roots`, fetched from the last root held, carry anything the wallet lacks.
fn adds_to<H: PartialEq>(held: Option<&H>, roots: &[CommitmentTreeRoot<H>]) -> bool {
    match roots {
        [] => false,
        [last] => held != Some(last.root_hash()),
        _ => true,
    }
}

async fn download_roots<H: HashSer>(
    client: &mut Lightwalletd,
    protocol: ShieldedProtocol,
    start: u64,
) -> Result<Vec<CommitmentTreeRoot<H>>, Error> {
    let mut request = GetSubtreeRootsArg {
        start_index: u32::try_from(start).map_err(Error::wallet)?,
        ..Default::default()
    };
    request.set_shielded_protocol(protocol);
    let mut stream = client.get_subtree_roots(request).await?.into_inner();
    let mut roots = Vec::new();
    while let Some(root) = stream.message().await? {
        let end = u32::try_from(root.completing_block_height).map_err(|_| {
            Error::Wallet(format!(
                "implausible subtree end {}",
                root.completing_block_height
            ))
        })?;
        let hash = H::read(&root.root_hash[..]).map_err(Error::wallet)?;
        roots.push(CommitmentTreeRoot::from_parts(
            BlockHeight::from_u32(end),
            hash,
        ));
    }
    Ok(roots)
}

/// One pass of the library's loop: true when scanning changed the ranges it was working through.
async fn running(
    client: &mut Lightwalletd,
    network: &Network,
    cache: &MemoryBlockCache,
    db: &mut Db,
    batch_size: u32,
) -> Result<bool, Error> {
    let tip = lightwalletd::chain_tip(client).await?;
    db.update_chain_tip(tip)?;
    // A range to verify always comes first, and is scanned until the wallet's view of its tip
    // connects to the server's chain.
    let mut ranges = db.suggest_scan_ranges()?;
    while let Some(range) = ranges
        .first()
        .filter(|range| range.priority() == ScanPriority::Verify)
        .cloned()
    {
        if !scan(client, network, cache, db, &range).await? {
            break;
        }
        ranges = db.suggest_scan_ranges()?;
    }
    for range in db
        .suggest_scan_ranges()?
        .into_iter()
        .flat_map(|range| batches(range, batch_size))
    {
        if scan(client, network, cache, db, &range).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Splits `range` into runs of at most `size` blocks.
fn batches(range: ScanRange, size: u32) -> impl Iterator<Item = ScanRange> {
    (0..).scan(range, move |rest, _| {
        if rest.is_empty() {
            None
        } else if let Some((batch, next)) = rest.split_at(rest.block_range().start + size) {
            *rest = next;
            Some(batch)
        } else {
            let batch = rest.clone();
            let end = rest.block_range().end;
            *rest = ScanRange::from_parts(end..end, rest.priority());
            Some(batch)
        }
    })
}

/// Downloads and scans `range`: true when a reorganisation, or a range of higher priority that
/// scanning added, invalidates the ranges being worked through.
async fn scan(
    client: &mut Lightwalletd,
    network: &Network,
    cache: &MemoryBlockCache,
    db: &mut Db,
    range: &ScanRange,
) -> Result<bool, Error> {
    let block = |height: BlockHeight| {
        Some(BlockId {
            height: height.into(),
            hash: vec![],
        })
    };
    let request = BlockRange {
        start: block(range.block_range().start),
        end: block(range.block_range().end - 1),
        pool_types: vec![],
    };
    let mut stream = client.get_block_range(request).await?.into_inner();
    let mut blocks = Vec::new();
    while let Some(compact) = stream.message().await? {
        blocks.push(compact);
    }
    let Ok(()) = cache.insert(blocks).await;
    let from = range.block_range().start;
    let state = lightwalletd::tree_state(client, from - 1)
        .await?
        .to_chain_state()
        .map_err(|e| Error::Wallet(format!("lightwalletd's tree state: {e}")))?;
    let changed = match scan_cached_blocks(network, cache, db, from, &state, range.len()) {
        Err(ChainError::Scan(e)) if e.is_continuity_error() => {
            // The library's heuristic: rewind ten blocks below the break.
            let rewind = e.at_height().saturating_sub(10);
            tracing::info!(at = %e.at_height(), %rewind, "Zcash chain reorganised; rewinding");
            db.truncate_to_height(rewind)?;
            let Ok(()) = cache.truncate(rewind).await;
            true
        }
        Ok(_) => db
            .suggest_scan_ranges()?
            .first()
            .is_some_and(|first| first.priority() > range.priority()),
        Err(e) => return Err(Error::wallet(e)),
    };
    let Ok(()) = cache.delete(range.clone()).await;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use incrementalmerkletree::{Hashable, Level};
    use orchard::tree::MerkleHashOrchard;

    use super::*;
    use crate::zcash::Wallet;

    fn roots(count: u8) -> Vec<CommitmentTreeRoot<MerkleHashOrchard>> {
        (0..count)
            .map(|i| {
                CommitmentTreeRoot::from_parts(
                    BlockHeight::from_u32(3_000_000 + u32::from(i)),
                    MerkleHashOrchard::empty_root(Level::from(i)),
                )
            })
            .collect()
    }

    fn held(wallet: &mut Wallet) -> u64 {
        held_roots(&mut wallet.db, &Db::get_orchard_subtree_root).unwrap()
    }

    fn add(wallet: &mut Wallet, start: u64, roots: &[CommitmentTreeRoot<MerkleHashOrchard>]) {
        add_roots(
            &mut wallet.db,
            start,
            roots,
            &Db::get_orchard_subtree_root,
            &Db::put_orchard_subtree_roots,
        )
        .unwrap();
    }

    #[test]
    fn later_syncs_add_only_the_roots_past_the_last_one_held() {
        let dir = tempfile::tempdir().unwrap();
        let mut wallet =
            Wallet::open(dir.path().join("wallet.sqlite"), Network::MainNetwork).unwrap();
        let chain = roots(5);
        assert_eq!(held(&mut wallet), 0);
        // A new wallet fetches from the first root, as the library does on every sync.
        add(&mut wallet, 0, &chain[..3]);
        assert_eq!(held(&mut wallet), 3);
        // Later, from the last root held: it comes back with any completed since.
        add(&mut wallet, 2, &chain[2..]);
        assert_eq!(held(&mut wallet), 5);
        for (index, root) in chain.iter().enumerate() {
            assert_eq!(
                wallet.db.get_orchard_subtree_root(index as u64).unwrap(),
                Some(*root.root_hash())
            );
        }
        add(&mut wallet, 4, &chain[4..]);
        assert_eq!(held(&mut wallet), 5);
    }

    #[test]
    fn a_replaced_last_root_still_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut wallet =
            Wallet::open(dir.path().join("wallet.sqlite"), Network::MainNetwork).unwrap();
        let chain = roots(3);
        add(&mut wallet, 0, &chain);
        let replaced = CommitmentTreeRoot::from_parts(
            BlockHeight::from_u32(3_000_002),
            MerkleHashOrchard::empty_root(Level::from(9)),
        );
        let result = add_roots(
            &mut wallet.db,
            2,
            &[replaced],
            &Db::get_orchard_subtree_root,
            &Db::put_orchard_subtree_roots,
        );
        assert!(result.is_err(), "a reorganised root was skipped");
        assert_eq!(
            wallet.db.get_orchard_subtree_root(2).unwrap(),
            Some(*chain[2].root_hash())
        );
    }
}
