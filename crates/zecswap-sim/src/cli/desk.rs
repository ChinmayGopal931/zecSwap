use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use alloy::primitives::B256;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use zecswap_api::Quote;
use zecswap_api::relayer::ShieldDeposit;
use zecswap_chain::evm::wall_now;
use zecswap_cli::store::{Fuse, Touch};

use crate::lightwallet::ledger::Ledger;
use crate::node::Node;
use crate::panics::{Killed, Unsent};

pub const STATE: &str = "swap.json";
pub const NEXT_INDEX: &str = "next-swap-index.json";
pub const PENDING: &str = "deposit.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Index,
    Quote,
    Fee,
    FeeOut,
    Accept,
    Joint,
    Deposit,
    DepositOut,
    Finish,
    SweepOut,
    Pending,
    Unpending,
}

impl Step {
    pub const ALL: [Self; 12] = [
        Self::Index,
        Self::Quote,
        Self::Fee,
        Self::FeeOut,
        Self::Accept,
        Self::Joint,
        Self::Deposit,
        Self::DepositOut,
        Self::Finish,
        Self::SweepOut,
        Self::Pending,
        Self::Unpending,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tear {
    pub step: Step,
    pub after: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aim {
    At(u64),
    Tear(Tear),
    Ready(u64),
    Refunded(u64),
    Unsent(Step),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Kill {
    pub aim: Aim,
    pub rest: u64,
}

#[derive(Clone, Debug)]
pub struct Met {
    pub tear: Tear,
    pub wall: u64,
}

#[derive(Clone, Debug)]
pub struct Kept {
    pub wall: u64,
    pub tip: u32,
    pub run: Run,
}

pub struct Desk {
    pub node: Node,
    pub ledger: Ledger,
    pub dir: PathBuf,
    pub start: tokio::time::Instant,
    pub armed: RefCell<Vec<(Tear, bool)>>,
    pub met: RefCell<Vec<Met>>,
    pub kept: RefCell<Vec<Kept>>,
    pub pending: RefCell<Vec<(u64, Pending)>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Quoted {
    pub index: u32,
    pub quote: Quote,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Opened {
    pub index: u32,
    pub quote: Quote,
    pub swap_id: B256,
    pub t0: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub enum Progress {
    Quoted {
        quoted: Quoted,
        fee: Option<[u8; 32]>,
    },
    Accepted(Opened),
}

#[derive(Clone, Debug, Deserialize)]
pub struct Run {
    pub progress: Progress,
    pub joint_account: Option<IgnoredAny>,
    pub deposit: Option<[u8; 32]>,
    pub finished: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pending {
    pub request: ShieldDeposit,
}

impl Run {
    pub fn quote(&self) -> &Quote {
        match &self.progress {
            Progress::Quoted { quoted, .. } => &quoted.quote,
            Progress::Accepted(opened) => &opened.quote,
        }
    }

    pub fn opened(&self) -> Option<&Opened> {
        match &self.progress {
            Progress::Accepted(opened) => Some(opened),
            Progress::Quoted { .. } => None,
        }
    }

    pub fn joint(&self) -> bool {
        self.joint_account.is_some()
    }
}

pub fn index_of(run: &Run) -> Option<u32> {
    Some(match &run.progress {
        Progress::Quoted { quoted, .. } => quoted.index,
        Progress::Accepted(opened) => opened.index,
    })
}

pub fn quote_id(run: &Run) -> Option<B256> {
    Some(run.quote().quote_id)
}

pub fn swap_id(run: &Run) -> Option<B256> {
    run.opened().map(|opened| opened.swap_id)
}

pub fn fee(run: &Run) -> Option<[u8; 32]> {
    match run.progress {
        Progress::Quoted { fee, .. } => fee,
        Progress::Accepted(_) => None,
    }
}

pub fn deposit(run: &Run) -> Option<[u8; 32]> {
    run.deposit
}

pub fn finished(run: &Run) -> bool {
    run.finished
}

fn step_of(run: &Run) -> Step {
    match (run.finished, run.opened().is_some()) {
        (true, _) => Step::Finish,
        (false, false) if fee(run).is_some() => Step::Fee,
        (false, false) => Step::Quote,
        (false, true) if run.deposit.is_some() => Step::Deposit,
        (false, true) if run.joint() => Step::Joint,
        (false, true) => Step::Accept,
    }
}

enum Kind {
    Run(Run),
    Pending(Pending),
    Nothing,
}

impl Desk {
    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let bytes = std::fs::read(self.dir.join(name)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn classify(&self, touch: &Touch<'_>) -> Option<(Tear, Kind)> {
        let tear = |step, after| Tear { step, after };
        match touch {
            Touch::Saving(name, bytes) => {
                let value: Option<Run> = serde_json::from_slice(bytes).ok();
                match *name {
                    STATE => Some((tear(step_of(&value?), false), Kind::Nothing)),
                    NEXT_INDEX => Some((tear(Step::Index, false), Kind::Nothing)),
                    PENDING => Some((tear(Step::Pending, false), Kind::Nothing)),
                    _ => None,
                }
            }
            Touch::Saved(name) => match *name {
                STATE => {
                    let value: Run = self.read(STATE)?;
                    Some((tear(step_of(&value), true), Kind::Run(value)))
                }
                NEXT_INDEX => Some((tear(Step::Index, true), Kind::Nothing)),
                PENDING => Some((
                    tear(Step::Pending, true),
                    Kind::Pending(self.read(PENDING)?),
                )),
                _ => None,
            },
            Touch::Removing(name) if *name == PENDING => {
                Some((tear(Step::Unpending, false), Kind::Nothing))
            }
            Touch::Removed(name) if *name == PENDING => {
                Some((tear(Step::Unpending, true), Kind::Nothing))
            }
            Touch::Removing(_) | Touch::Removed(_) => None,
            Touch::Sending(txid) | Touch::Sent(txid) => {
                let after = matches!(touch, Touch::Sent(_));
                let txid: [u8; 32] = *txid.as_ref();
                let run: Option<Run> = self.read(STATE);
                let step = match run {
                    Some(run) if fee(&run) == Some(txid) => Step::FeeOut,
                    Some(run) if deposit(&run) == Some(txid) => Step::DepositOut,
                    _ => Step::SweepOut,
                };
                Some((tear(step, after), Kind::Nothing))
            }
        }
    }

    fn touched(&self, touch: Touch<'_>) {
        let Some((tear, value)) = self.classify(&touch) else {
            return;
        };
        let (wall, tip) = (wall_now(), self.ledger.tip().0);
        match value {
            Kind::Pending(pending) => {
                let at = self.start.elapsed().as_millis() as u64;
                self.pending.borrow_mut().push((at, pending));
            }
            Kind::Run(run) => self.kept.borrow_mut().push(Kept { wall, tip, run }),
            Kind::Nothing => {}
        }
        let armed = self
            .armed
            .borrow()
            .iter()
            .position(|(held, _)| *held == tear);
        let side = match tear.after {
            true => "after",
            false => "before",
        };
        self.node.note(format!("cli {side} {:?}", tear.step));
        self.met.borrow_mut().push(Met { tear, wall });
        if let Some(at) = armed {
            let (_, unsent) = self.armed.borrow_mut().remove(at);
            if unsent {
                self.node.note(format!(
                    "lightwalletd refused the broadcast of {:?}",
                    tear.step
                ));
                std::panic::panic_any(Unsent);
            }
            self.node
                .note(format!("the cli was killed {side} {:?}", tear.step));
            std::panic::panic_any(Killed);
        }
    }

    pub fn fuse(self: &Rc<Self>) -> Fuse {
        let desk = self.clone();
        Rc::new(move |touch| desk.touched(touch))
    }
}
