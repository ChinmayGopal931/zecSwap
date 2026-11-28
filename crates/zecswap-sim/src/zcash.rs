use std::sync::{Arc, Mutex};

use zecswap_core::JointAccount;

pub trait Sees: Send + Sync {
    fn watch(&self, account: &JointAccount);

    fn received(&self, address: &str, confirmations: u64) -> u64;

    fn sweep(&self, address: &str) -> u64;
}

impl Sees for Zcash {
    fn watch(&self, _: &JointAccount) {}

    fn received(&self, address: &str, confirmations: u64) -> u64 {
        Zcash::received(self, address, confirmations)
    }

    fn sweep(&self, address: &str) -> u64 {
        Zcash::sweep(self, address)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaymentId(pub usize);

#[derive(Clone, Debug)]
struct Payment {
    to: String,
    zat: u64,
    pooled: bool,
    mined_at: Option<u64>,
    spent: bool,
    void: bool,
}

#[derive(Default)]
struct Ledger {
    height: u64,
    payments: Vec<Payment>,
}

#[derive(Clone, Default)]
pub struct Zcash(Arc<Mutex<Ledger>>);

impl Zcash {
    pub fn height(&self) -> u64 {
        self.0.lock().unwrap().height
    }

    pub fn build(&self, to: &str, zat: u64) -> PaymentId {
        let mut ledger = self.0.lock().unwrap();
        ledger.payments.push(Payment {
            to: to.to_owned(),
            zat,
            pooled: false,
            mined_at: None,
            spent: false,
            void: false,
        });
        PaymentId(ledger.payments.len() - 1)
    }

    pub fn broadcast(&self, id: PaymentId) {
        let mut ledger = self.0.lock().unwrap();
        let payment = &mut ledger.payments[id.0];
        if payment.mined_at.is_none() && !payment.void {
            payment.pooled = true;
        }
    }

    pub fn mine(&self, lost: impl Fn(PaymentId) -> bool) {
        let mut ledger = self.0.lock().unwrap();
        ledger.height += 1;
        let height = ledger.height;
        for (index, payment) in ledger.payments.iter_mut().enumerate() {
            if !payment.pooled {
                continue;
            }
            payment.pooled = false;
            if !lost(PaymentId(index)) {
                payment.mined_at = Some(height);
            }
        }
    }

    pub fn confirmations(&self, id: PaymentId) -> u64 {
        let ledger = self.0.lock().unwrap();
        ledger.payments[id.0]
            .mined_at
            .map_or(0, |at| ledger.height - at + 1)
    }

    pub fn received(&self, address: &str, confirmations: u64) -> u64 {
        let ledger = self.0.lock().unwrap();
        ledger
            .payments
            .iter()
            .filter(|payment| payment.to == address && !payment.spent)
            .filter(|payment| {
                payment
                    .mined_at
                    .is_some_and(|at| ledger.height - at + 1 >= confirmations)
            })
            .map(|payment| payment.zat)
            .sum()
    }

    pub fn void(&self, id: PaymentId) {
        let mut ledger = self.0.lock().unwrap();
        let payment = &mut ledger.payments[id.0];
        if payment.mined_at.is_none() {
            payment.pooled = false;
            payment.void = true;
        }
    }

    pub fn payments_to(&self, address: &str) -> Vec<(PaymentId, u64, bool)> {
        let ledger = self.0.lock().unwrap();
        ledger
            .payments
            .iter()
            .enumerate()
            .filter(|(_, payment)| payment.to == address)
            .map(|(index, payment)| (PaymentId(index), payment.zat, payment.mined_at.is_some()))
            .collect()
    }

    pub fn sweep(&self, address: &str) -> u64 {
        let mut ledger = self.0.lock().unwrap();
        let mut swept = 0;
        for payment in &mut ledger.payments {
            if payment.to == address && payment.mined_at.is_some() && !payment.spent {
                payment.spent = true;
                swept += payment.zat;
            }
        }
        swept
    }
}
