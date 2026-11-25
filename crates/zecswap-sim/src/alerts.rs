use std::collections::{BTreeMap, BTreeSet};
use std::panic::resume_unwind;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, keccak256};
use alloy::signers::local::PrivateKeySigner;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::spawn_local;
use tokio::time::{Instant, sleep, sleep_until};
use zecswap_chain::alert::{Alert, Alerts};

use crate::node::{Node, PROTECT};
use crate::relayer::Funder;
use crate::seed::Seed;

const QUEUED: u64 = 6;
const REPRICED: u64 = 9;
const UNCLEARED: u64 = 80;
const TRIES: u32 = 6;
const RETRY: Duration = Duration::from_secs(15);
const AFTER: Duration = Duration::from_secs(40 * 60);

struct Call {
    who: String,
    alert: Alert,
    signer: PrivateKeySigner,
}

pub(crate) struct Alarms {
    node: Node,
    seed: Seed,
    told: Mutex<BTreeSet<String>>,
    heard: Mutex<Vec<Alert>>,
    wrong: Mutex<Vec<String>>,
    acted: Mutex<BTreeMap<String, u64>>,
    awaited: Mutex<Option<Instant>>,
    watched: Mutex<Vec<Address>>,
    calls: UnboundedSender<Call>,
}

pub(crate) struct Operator {
    alarms: Arc<Alarms>,
    calls: UnboundedReceiver<Call>,
    restarts: BTreeMap<String, Rc<dyn Fn()>>,
}

fn hold(fine: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    match fine {
        true => Ok(()),
        false => Err(why()),
    }
}

fn roll(key: &str) -> u64 {
    let word = keccak256(key.as_bytes());
    u64::from_be_bytes(word[..8].try_into().expect("eight bytes"))
}

impl Alarms {
    pub(crate) fn new(node: &Node, seed: Seed) -> (Arc<Self>, Operator) {
        let (calls, heard) = unbounded_channel();
        let alarms = Arc::new(Self {
            node: node.clone(),
            seed,
            told: Mutex::default(),
            heard: Mutex::default(),
            wrong: Mutex::default(),
            acted: Mutex::default(),
            awaited: Mutex::default(),
            watched: Mutex::default(),
            calls,
        });
        let operator = Operator {
            alarms: alarms.clone(),
            calls: heard,
            restarts: BTreeMap::new(),
        };
        (alarms, operator)
    }

    pub(crate) fn sink(self: &Arc<Self>, who: &str, signer: &PrivateKeySigner) -> Alerts {
        let (alarms, who, signer) = (self.clone(), who.to_owned(), signer.clone());
        self.watched.lock().unwrap().push(signer.address());
        Arc::new(move |alert: &Alert| {
            alarms.hear(&who, &signer, alert);
            if alarms.seed.chance("sink-fails", roll(&alert.key()), 250) {
                resume_unwind(Box::new("the alerts' sink failed"));
            }
        })
    }

    fn hear(&self, who: &str, signer: &PrivateKeySigner, alert: &Alert) {
        let first = self.told.lock().unwrap().insert(alert.key());
        if first {
            self.node.note(format!("alert from {who}: {alert}"));
        }
        if let Err(why) = self.judge(who, alert) {
            let what = format!("a false alert from {who}, {}: {why}", alert.name());
            self.node.note(what.clone());
            self.wrong.lock().unwrap().push(what);
        }
        self.heard.lock().unwrap().push(alert.clone());
        if first {
            let call = Call {
                who: who.to_owned(),
                alert: alert.clone(),
                signer: signer.clone(),
            };
            let _ = self.calls.send(call);
        }
    }

    fn judge(&self, who: &str, alert: &Alert) -> Result<(), String> {
        let node = &self.node;
        let head = node.head().0;
        let counted =
            |account: Address, count: u64| (0..2).any(|back| node.nonce_at(account, back) == count);
        match alert {
            Alert::PrivateNonceStuck {
                account,
                signing,
                chain,
                raised_by,
                dropped,
                ..
            } => {
                hold(counted(*account, *chain) && chain < signing, || {
                    format!(
                        "the chain counts {} from the account, the alert says {chain} under {signing}",
                        node.nonce_at(*account, 0)
                    )
                })?;
                let raised = node.known(*raised_by);
                let raised = raised.ok_or("the node never saw the transaction it calls mined")?;
                let lost = node.known(*dropped);
                let unheard = node.missed(&format!("{who}-private"), "eth_sendRawTransaction", 0);
                hold(lost.is_some() || unheard, || {
                    "the node never saw the transaction it calls dropped".into()
                })?;
                hold(
                    raised.from == *account
                        && raised.private
                        && raised.nonce + 1 == *signing
                        && raised.mined.is_none_or(|at| at == head),
                    || format!("the transaction it calls uncounted is {raised:?}"),
                )?;
                let gone = lost.as_ref().is_none_or(|lost| {
                    lost.from == *account
                        && lost.private
                        && lost.nonce == *signing
                        && lost.mined.is_none()
                        && !lost.pooled
                });
                hold(gone, || {
                    format!("the transaction it calls dropped is {lost:?}")
                })?;
                let sent_since = node.holds_privately(*account, *signing);
                let expired = lost.map_or(0, |lost| lost.sent + PROTECT);
                let unread = node.missed(who, "eth_getTransactionCount", expired);
                hold(!sent_since || unread, || {
                    "another went out before it told of the one dropped".into()
                })
            }
            Alert::SendQueued {
                account,
                waiting,
                nonce,
                behind,
                blocks,
                unlanded,
                ..
            } => {
                hold(counted(*account, *behind) && behind < nonce, || {
                    format!(
                        "the chain counts {} from the account, the alert says {behind} under {nonce}",
                        node.nonce_at(*account, 0)
                    )
                })?;
                let sent = node.known(*waiting);
                let sent = sent.ok_or("the node never saw the transaction it calls waiting")?;
                hold(
                    sent.from == *account && !sent.private && sent.nonce == *nonce,
                    || format!("the transaction it calls waiting is {sent:?}"),
                )?;
                let out = node.out_since(*account, *nonce);
                hold(out.is_some_and(|out| out + QUEUED <= head), || {
                    format!("number {nonce} went out at block {out:?} and it is block {head}")
                })?;
                let out = out.unwrap_or_default();
                let since = node.rose(*account, *behind, out).unwrap_or(out);
                hold(*blocks >= QUEUED && since + blocks <= head + 1, || {
                    format!(
                        "number {nonce} waits behind {behind} since block {since} and it is block {head}, the alert says {blocks} blocks"
                    )
                })?;
                let Some(unlanded) = unlanded else {
                    return Ok(());
                };
                let gone = node.known(unlanded.hash);
                let gone = gone.ok_or("the node never saw the transaction it calls unmined")?;
                hold(
                    gone.from == *account
                        && gone.nonce == *behind
                        && gone.mined.is_none_or(|at| at == head),
                    || format!("the transaction it calls unmined is {gone:?}"),
                )
            }
            Alert::WalletRewindRefused { .. } => Err("no sender raises a wallet's alert".into()),
        }
    }

    async fn act(self: Arc<Self>, call: Call, restart: Option<Rc<dyn Fn()>>) {
        let (key, who) = (call.alert.key(), call.who);
        let roll = roll(&key);
        let delay = match self.seed.chance("operator-away", roll, 500) {
            true => self.seed.between("operator-delay", roll, 3600, 7200),
            false => self.seed.between("operator-delay", roll, 60, 600),
        };
        let due = Instant::now() + Duration::from_secs(delay);
        let settled = due + AFTER;
        {
            let mut awaited = self.awaited.lock().unwrap();
            *awaited = Some(awaited.map_or(settled, |last| last.max(settled)));
        }
        sleep_until(due).await;
        match (&call.alert, restart) {
            (Alert::PrivateNonceStuck { .. }, Some(restart)) => {
                restart();
                self.node.note(format!("the operator restarted {who}"));
            }
            (Alert::SendQueued { behind, .. }, _) => {
                let funder = Funder::new(&self.node, &format!("operator-{who}"), call.signer);
                let mut sent = None;
                for _ in 0..TRIES {
                    sent = funder.again(&self.node, *behind).await;
                    if sent.is_some() {
                        break;
                    }
                    sleep(RETRY).await;
                }
                self.node.note(match sent {
                    Some(hash) => format!(
                        "the operator sent transaction number {behind} of {who} again, as {}",
                        &hash.to_string()[..10]
                    ),
                    None => format!(
                        "the operator could not send transaction number {behind} of {who} again"
                    ),
                });
            }
            _ => self
                .node
                .note(format!("nobody acts on the alert from {who}")),
        }
        self.told.lock().unwrap().remove(&key);
        let head = self.node.head().0;
        self.acted.lock().unwrap().entry(key).or_insert(head);
    }

    pub(crate) fn until(&self, horizon: Instant) -> Instant {
        let awaited = *self.awaited.lock().unwrap();
        awaited.map_or(horizon, |settled| settled.max(horizon))
    }

    pub(crate) fn failures(&self, unwatched: &[Address]) -> Vec<String> {
        let mut failures = self.wrong.lock().unwrap().clone();
        failures.dedup();
        failures.truncate(3);
        let head = self.node.head().0;
        let heard = self.heard.lock().unwrap();
        let acted = self.acted.lock().unwrap();
        let watched = self.watched.lock().unwrap();
        for queue in self.node.queues() {
            if !watched.contains(&queue.from) || unwatched.contains(&queue.from) {
                continue;
            }
            let said = heard.iter().find(|alert| match alert {
                Alert::SendQueued {
                    account, behind, ..
                } => (*account, *behind) == (queue.from, queue.behind),
                _ => false,
            });
            match said.map(|alert| acted.get(&alert.key())) {
                None if queue.repriced >= REPRICED => failures.push(format!(
                    "sends from {} went out again for {} blocks behind its unmined transaction number {}, and no alert said so",
                    queue.from, queue.repriced, queue.behind
                )),
                Some(Some(at)) if at + UNCLEARED <= head => failures.push(format!(
                    "sends from {} are still queued behind its transaction number {}, {} blocks after the operator acted",
                    queue.from,
                    queue.behind,
                    head - at
                )),
                _ => {}
            }
        }
        failures
    }
}

impl Operator {
    pub(crate) fn restarts(&mut self, who: &str, restart: impl Fn() + 'static) {
        self.restarts.insert(who.to_owned(), Rc::new(restart));
    }

    pub(crate) async fn tend(mut self) {
        while let Some(call) = self.calls.recv().await {
            let restart = self.restarts.get(&call.who).cloned();
            spawn_local(self.alarms.clone().act(call, restart));
        }
    }
}
