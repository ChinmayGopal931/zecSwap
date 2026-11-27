use std::collections::BTreeMap;
use std::sync::Mutex;

use zecswap_core::{JointAccount, NetworkType};

use crate::lightwallet::ledger::Ledger;
use crate::zcash::Sees;

pub struct Seen {
    ledger: Ledger,
    keys: Mutex<BTreeMap<String, usize>>,
}

impl Seen {
    pub fn new(ledger: Ledger) -> Self {
        Self {
            ledger,
            keys: Mutex::default(),
        }
    }

    pub fn key(&self, address: &str) -> Option<usize> {
        self.keys.lock().unwrap().get(address).copied()
    }

    pub fn unspent(&self, key: usize, confirmations: u32) -> u64 {
        let tip = self.ledger.tip().0;
        let held = self.ledger.holdings(key, tip);
        let ripe = held
            .iter()
            .filter(|note| note.spent.is_none() && tip + 1 - note.mined >= confirmations);
        ripe.map(|note| note.value).sum()
    }
}

impl Sees for Seen {
    fn watch(&self, account: &JointAccount) {
        let address = account.unified_address(NetworkType::Test);
        let mut keys = self.keys.lock().unwrap();
        keys.entry(address)
            .or_insert_with(|| self.ledger.watch(account.fvk().clone()));
    }

    fn received(&self, address: &str, confirmations: u64) -> u64 {
        let confirmations = u32::try_from(confirmations).unwrap_or(u32::MAX);
        self.key(address)
            .map_or(0, |key| self.unspent(key, confirmations))
    }

    fn sweep(&self, address: &str) -> u64 {
        self.key(address).map_or(0, |key| self.ledger.take(key))
    }
}
