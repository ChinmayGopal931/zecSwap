use std::fmt::Display;

use zcash_client_backend::data_api::chain::{
    BlockCache, ChainState, CommitmentTreeRoot, error::Error as ChainError, scan_cached_blocks,
};
use zcash_client_backend::data_api::scanning::{ScanPriority, ScanRange};
use zcash_client_backend::data_api::{WalletCommitmentTrees, WalletRead, WalletWrite};
use zcash_client_backend::proto::service::{
    BlockId, BlockRange, GetSubtreeRootsArg, ShieldedProtocol,
};
use zcash_client_sqlite::error::SqliteClientError;
use zcash_primitives::merkle_tree::HashSer;
use zcash_protocol::consensus::{BlockHeight, Network};

use super::Db;
use crate::Error;
use crate::alert::{self, Alert, Alerts};
use crate::zcash::cache::MemoryBlockCache;
use crate::zcash::lightwalletd::{self, Lightwalletd};

const SUBTREES: u64 = 1 << 16;
const MOST_STEPS_BACK: u32 = 3;

pub(super) struct Watch<'a> {
    pub alerts: Option<&'a Alerts>,
    pub wallet: &'a str,
    pub steps_back: u32,
}

pub(super) fn born_above(db: &Db, tip: BlockHeight) -> Result<bool, Error> {
    Ok(db
        .get_wallet_birthday()?
        .is_some_and(|birthday| birthday > tip))
}

pub(super) async fn run(
    client: &mut Lightwalletd,
    network: &Network,
    cache: &MemoryBlockCache,
    db: &mut Db,
    batch_size: u32,
    mut watch: Watch<'_>,
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
    while running(client, network, cache, db, batch_size, &mut watch).await? {}
    Ok(())
}

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

async fn running(
    client: &mut Lightwalletd,
    network: &Network,
    cache: &MemoryBlockCache,
    db: &mut Db,
    batch_size: u32,
    watch: &mut Watch<'_>,
) -> Result<bool, Error> {
    let tip = lightwalletd::chain_tip(client).await?;
    if born_above(db, tip)? {
        return Ok(false);
    }
    db.update_chain_tip(tip)?;
    let mut ranges = db.suggest_scan_ranges()?;
    while let Some(range) = ranges
        .first()
        .filter(|range| range.priority() == ScanPriority::Verify)
        .cloned()
    {
        if !scan(client, network, cache, db, &range, watch).await? {
            break;
        }
        ranges = db.suggest_scan_ranges()?;
    }
    for range in db
        .suggest_scan_ranges()?
        .into_iter()
        .flat_map(|range| batches(range, batch_size))
    {
        if scan(client, network, cache, db, &range, watch).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

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

async fn chain_state(client: &mut Lightwalletd, height: BlockHeight) -> Result<ChainState, Error> {
    lightwalletd::tree_state(client, height)
        .await?
        .to_chain_state()
        .map_err(|e| Error::Wallet(format!("lightwalletd's tree state: {e}")))
}

async fn scan(
    client: &mut Lightwalletd,
    network: &Network,
    cache: &MemoryBlockCache,
    db: &mut Db,
    range: &ScanRange,
    watch: &mut Watch<'_>,
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
    let state = chain_state(client, from - 1).await?;
    let changed = match scan_cached_blocks(network, cache, db, from, &state, range.len()) {
        Err(ChainError::Scan(e)) if e.is_continuity_error() => {
            let (at, rewind) = (e.at_height(), e.at_height().saturating_sub(10));
            tracing::info!(%at, %rewind, "Zcash chain reorganised; rewinding");
            let kept = match db.truncate_to_height(rewind) {
                Err(SqliteClientError::RequestedRewindInvalid {
                    safe_rewind_height: Some(safe),
                    ..
                }) if safe + 1 < at => {
                    if watch.steps_back == MOST_STEPS_BACK {
                        return Err(Error::Wallet(format!(
                            "lightwalletd's chain still changed after {MOST_STEPS_BACK} steps back to height {safe}"
                        )));
                    }
                    watch.steps_back += 1;
                    let state = chain_state(client, safe).await?;
                    if state.block_height() != safe {
                        return Err(Error::Wallet(format!(
                            "lightwalletd's tree state is of height {}, not {safe}",
                            state.block_height()
                        )));
                    }
                    db.truncate_to_chain_state(state).map(|()| safe)
                }
                outcome => outcome,
            };
            let kept = kept.inspect_err(|refusal| {
                let lasts = matches!(
                    refusal,
                    SqliteClientError::RequestedRewindInvalid { .. }
                        | SqliteClientError::TruncateCommitmentTree { .. }
                );
                if lasts {
                    let refused = Alert::WalletRewindRefused {
                        wallet: watch.wallet.to_owned(),
                        reorg_at: at.into(),
                        rewind: rewind.into(),
                        refusal: refusal.to_string(),
                    };
                    alert::raise(watch.alerts, refused);
                }
            })?;
            let Ok(()) = cache.truncate(kept).await;
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
