use std::sync::{Arc, Mutex};

use orchard::ValuePool;
use orchard::circuit::{OrchardCircuitVersion, ProvingKey};
use orchard::keys::{FullViewingKey, PreparedIncomingViewingKey, Scope};
use orchard::note_encryption::IronwoodDomain;
use pczt::Pczt;
use pczt::roles::combiner::Combiner;
use pczt::roles::creator::Creator;
use pczt::roles::io_finalizer::IoFinalizer;
use pczt::roles::prover::Prover;
use pczt::roles::redactor::Redactor;
use pczt::roles::spend_finalizer::SpendFinalizer;
use pczt::roles::tx_extractor::TransactionExtractor;
use pczt::roles::verifier::Verifier;
use rand_chacha::ChaCha20Rng;
use zcash_note_encryption::try_note_decryption;
use zcash_primitives::transaction::builder::{BuildConfig, Builder, BundlePadding};
use zcash_primitives::transaction::fees::FeeRule;
use zcash_primitives::transaction::fees::transparent::InputSize;
use zcash_protocol::consensus::{BlockHeight, Network, Parameters};
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;

use super::ledger::{Ledger, MARGINAL_FEE, verifying_key};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trick {
    None,
    Elsewhere,
    Skims,
    Partial,
    Overpays,
    Garbles,
    Hides,
    Presigned,
    Forges,
    AnothersKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Whole,
    Signers,
    Compact,
}

struct Flat(Zatoshis);

impl FeeRule for Flat {
    type Error = std::convert::Infallible;

    fn fee_required<P: Parameters>(
        &self,
        _: &P,
        _: BlockHeight,
        _: impl IntoIterator<Item = InputSize>,
        _: impl IntoIterator<Item = usize>,
        _: usize,
        _: usize,
        _: usize,
        _: usize,
    ) -> Result<Zatoshis, Self::Error> {
        Ok(self.0)
    }
}

pub struct Sweep {
    pub pczt: Pczt,
    pub fee: u64,
}

pub fn fee(spends: usize, outputs: usize) -> u64 {
    MARGINAL_FEE * spends.max(outputs).max(2) as u64
}

pub fn sweep(
    ledger: &Ledger,
    network: Network,
    key: usize,
    owner: &FullViewingKey,
    outputs: &[(orchard::Address, u64)],
    most: usize,
    rng: &mut ChaCha20Rng,
) -> Option<Sweep> {
    let tip = ledger.tip().0;
    let notes = ledger.holdings(key, tip);
    let unspent = notes.iter().filter(|note| note.spent.is_none());
    let unspent: Vec<_> = unspent.take(most).collect();
    let spent: u64 = unspent.iter().map(|note| note.value).sum();
    let paid: u64 = outputs.iter().map(|(_, value)| value).sum();
    let witnessed = unspent
        .iter()
        .map(|note| ledger.witness(ValuePool::Ironwood, &note.nullifier));
    let witnessed: Vec<_> = witnessed.collect::<Option<_>>()?;
    let anchor = witnessed.first()?.1;
    let mut builder = Builder::new(
        network,
        BlockHeight::from(tip + 1),
        BuildConfig::Standard {
            sapling_anchor: None,
            orchard_anchor: None,
            ironwood_anchor: Some(anchor),
            orchard_padding: BundlePadding::DEFAULT,
            ironwood_padding: BundlePadding::DEFAULT,
        },
    );
    for (note, _, path) in witnessed {
        builder
            .add_ironwood_spend::<Flat>(owner.clone(), note, path)
            .ok()?;
    }
    for (to, value) in outputs {
        let value = Zatoshis::from_u64(*value).ok()?;
        builder
            .add_ironwood_output::<Flat>(None, *to, value, MemoBytes::empty())
            .ok()?;
    }
    let fee = spent.checked_sub(paid)?;
    let rule = Flat(Zatoshis::from_u64(fee).ok()?);
    let parts = builder.build_for_pczt(&mut *rng, &rule).ok()?.pczt_parts;
    let pczt = IoFinalizer::new(Creator::build_from_parts(parts)?)
        .finalize_io(&mut *rng)
        .ok()?;
    Some(Sweep { pczt, fee })
}

pub fn shown(pczt: Pczt, view: View) -> Pczt {
    let redactor = Redactor::new(pczt);
    match view {
        View::Whole => redactor.finish(),
        View::Signers => redactor
            .redact_ironwood_with(|mut bundle| {
                bundle.redact_actions(|mut action| {
                    action.clear_spend_witness();
                    action.clear_spend_dummy_sk();
                });
            })
            .finish(),
        View::Compact => redactor
            .redact_ironwood_with(|mut bundle| {
                bundle.clear_zkproof();
                bundle.clear_bsk();
                bundle.redact_actions(|mut action| {
                    action.clear_spend_witness();
                    action.clear_spend_dummy_sk();
                });
                bundle.compact_resolvable_fields();
                bundle.clear_anchor();
            })
            .finish(),
    }
}

pub fn unvalued(pczt: Pczt) -> Pczt {
    Redactor::new(pczt)
        .redact_ironwood_with(|mut bundle| {
            bundle.redact_actions(|mut action| action.clear_output_value());
        })
        .finish()
}

pub fn garbled(pczt: &Pczt, to: &FullViewingKey) -> Option<Vec<u8>> {
    let ivk = PreparedIncomingViewingKey::new(&to.to_ivk(Scope::External));
    let mut sealed = None;
    Verifier::new(pczt.clone())
        .with_ironwood::<(), _>(|bundle| {
            sealed = bundle.actions().iter().find_map(|action| {
                let domain = IronwoodDomain::for_pczt_action(action);
                try_note_decryption(&domain, &ivk, action)?;
                Some(action.output().encrypted_note().enc_ciphertext.0)
            });
            Ok(())
        })
        .ok()?;
    let sealed = sealed?;
    let mut bytes = pczt.clone().serialize().ok()?;
    let at = bytes
        .windows(sealed.len())
        .position(|window| window == sealed)?;
    bytes[at + sealed.len() - 1] ^= 1;
    Some(bytes)
}

pub fn forged(pczt: &Pczt, from: orchard::Address, to: orchard::Address) -> Option<Vec<u8>> {
    let (from, to) = (from.to_raw_address_bytes(), to.to_raw_address_bytes());
    let mut bytes = pczt.clone().serialize().ok()?;
    let at = bytes
        .windows(from.len())
        .position(|window| window == from)?;
    bytes[at..at + to.len()].copy_from_slice(&to);
    Some(bytes)
}

pub struct Effects {
    pub nullifiers: Vec<[u8; 32]>,
    pub received: u64,
}

pub fn effects(bytes: &[u8], to: &FullViewingKey, at: orchard::Address) -> Option<Effects> {
    let ivk = PreparedIncomingViewingKey::new(&to.to_ivk(Scope::External));
    let mut effects = Effects {
        nullifiers: Vec::new(),
        received: 0,
    };
    Verifier::new(Pczt::parse(bytes).ok()?)
        .with_ironwood::<(), _>(|bundle| {
            for action in bundle.actions() {
                effects
                    .nullifiers
                    .push(action.spend().nullifier().to_bytes());
                let domain = IronwoodDomain::for_pczt_action(action);
                let opened = try_note_decryption(&domain, &ivk, action);
                let ours = opened.filter(|(_, recipient, _)| *recipient == at);
                effects.received += ours.map_or(0, |(note, _, _)| note.value().inner());
            }
            Ok(())
        })
        .ok()?;
    Some(effects)
}

fn proving_key() -> Arc<ProvingKey> {
    static KEY: Mutex<Option<Arc<ProvingKey>>> = Mutex::new(None);
    let mut key = KEY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    key.get_or_insert_with(|| Arc::new(ProvingKey::build(OrchardCircuitVersion::PostNu6_3)))
        .clone()
}

pub fn finished(whole: Pczt, signed: &[u8], rng: &mut ChaCha20Rng) -> Option<(Vec<u8>, [u8; 32])> {
    let proved = Prover::new(whole)
        .create_ironwood_proof(&mut *rng, &proving_key())
        .ok()?
        .finish();
    let signed = Pczt::parse(signed).ok()?;
    let both = Combiner::new(vec![proved, signed]).combine().ok()?;
    let ready = SpendFinalizer::new(both).finalize_spends().ok()?;
    let key = verifying_key(OrchardCircuitVersion::PostNu6_3);
    let tx = TransactionExtractor::new(ready)
        .with_orchard(&key)
        .extract(&mut *rng)
        .ok()?;
    let mut raw = Vec::new();
    tx.write(&mut raw).ok()?;
    Some((raw, *tx.txid().as_ref()))
}
