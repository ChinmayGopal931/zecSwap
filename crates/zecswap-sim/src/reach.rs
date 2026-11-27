use std::collections::BTreeSet;

use crate::hunt::signature;
use crate::maker::{Fee, Mood, REVERSE_T0_TO_T1, REVERSE_TO_T0, ReverseMood, TO_T0};
use crate::relayer::{ReverseService, Service};
use crate::run::Report;
use crate::seed::Seed;
use crate::wallet::End;
use crate::{Plan, deposits, lightwallet, reverse};

const CRASH_REST: u64 = 300;
const NEAR: u64 = 3 * 60;
const SAID: usize = 110;

type Forward = Report<Plan, End>;
type Reverse = Report<reverse::Plan, reverse::Ended>;
type Deposits = Report<deposits::Plan, deposits::Ended>;
type Zcash = Report<lightwallet::Plan, lightwallet::Ended>;
type Situation<R> = (&'static str, fn(&R) -> bool);

fn has<P, E>(report: &Report<P, E>, what: &str) -> bool {
    report.trace.iter().any(|line| line.contains(what))
}

fn reverted<P, E>(report: &Report<P, E>) -> bool {
    let mut blocks = report
        .trace
        .iter()
        .filter(|line| line.starts_with("block "));
    blocks.any(|line| line.contains('!'))
}

fn away(crashes: &[(u64, u64)]) -> Option<u64> {
    let long = crashes.iter().find(|(_, rest)| *rest > CRASH_REST);
    long.map(|(at, rest)| at + rest)
}

fn killed<P, E>(report: &Report<P, E>, after: bool) -> bool {
    let at = |what: &str| report.trace.iter().position(|line| line.contains(what));
    let (deposited, ended) = (at("wallet deposited"), at("wallet ended"));
    let mut crashes = report.trace.iter().enumerate();
    crashes.any(|(index, line)| {
        let within = deposited.is_some_and(|deposited| index > deposited)
            && ended.is_none_or(|ended| index < ended);
        line.contains("the wallet crashed") && within == after
    })
}

fn both<P, E>(report: &Report<P, E>, what: &str, how: &str) -> bool {
    let mut lines = report.trace.iter();
    lines.any(|line| line.contains(what) && line.contains(how))
}

fn told(line: &str) -> &str {
    let numbered = line.strip_prefix('#').and_then(|line| line.split_once(' '));
    numbered.map_or(line, |(_, told)| told)
}

fn reached<P, E>(
    report: &Report<P, E>,
    situations: &[Situation<Report<P, E>>],
    said: &[&str],
) -> (Vec<u16>, Vec<String>) {
    let met = situations.iter().zip(0..);
    let met = met.filter(|((_, met), _)| met(report));
    let mut voiced = BTreeSet::new();
    for line in report.trace.iter().map(|line| told(line)) {
        if said.iter().any(|said| line.contains(said)) {
            voiced.insert(signature(line).chars().take(SAID).collect());
        }
    }
    (
        met.map(|(_, index)| index).collect(),
        voiced.into_iter().collect(),
    )
}

fn all<R>(situations: &[Situation<R>]) -> Vec<String> {
    situations
        .iter()
        .map(|(tag, _)| (*tag).to_owned())
        .collect()
}

fn forward_focus(report: &Forward) -> u64 {
    Seed(report.seed).below("focus", 0, 10)
}

const FORWARD: &[Situation<Forward>] = &[
    ("maker Honest", |r| r.plan.mood == Mood::Honest),
    ("maker NeverReadies", |r| r.plan.mood == Mood::NeverReadies),
    ("maker Vanishes", |r| r.plan.mood == Mood::Vanishes),
    ("maker CancelsEarly", |r| r.plan.mood == Mood::CancelsEarly),
    ("maker KeepsFee", |r| r.plan.mood == Mood::KeepsFee),
    ("maker OpensLess", |r| r.plan.mood == Mood::OpensLess),
    ("maker OpensAnotherNote", |r| {
        r.plan.mood == Mood::OpensAnotherNote
    }),
    ("maker OpensFar", |r| r.plan.mood == Mood::OpensFar),
    ("maker LiesAboutDeadlines", |r| {
        r.plan.mood == Mood::LiesAboutDeadlines
    }),
    ("open fee: none", |r| r.plan.fee == Fee::None),
    (
        "open fee: asked, within the cap",
        |r| matches!(r.plan.fee, Fee::Asked(fee) if fee <= r.plan.max_open_fee),
    ),
    (
        "open fee: asked, over the cap",
        |r| matches!(r.plan.fee, Fee::Asked(fee) if fee > r.plan.max_open_fee),
    ),
    ("open fee: payable outside Orchard", |r| {
        matches!(r.plan.fee, Fee::Transparent(_))
    }),
    ("route: Railgun", |r| r.plan.railgun),
    ("route: an account", |r| !r.plan.railgun),
    ("reveals sent privately", |r| r.plan.private),
    ("reveals sent publicly", |r| !r.plan.private),
    ("relayer skips payouts", |r| {
        r.plan.service == Service::SkipsPayouts
    }),
    ("with Multicall3", |r| r.plan.multicall),
    ("without Multicall3", |r| !r.plan.multicall),
    ("calm weather", |r| r.plan.calm()),
    ("focus: the claim margin", |r| forward_focus(r) == 0),
    ("focus: the ready depth", |r| forward_focus(r) == 1),
    ("wallet away until around t0", |r| {
        away(&r.plan.crashes).is_some_and(|back| back <= TO_T0 + NEAR)
    }),
    ("wallet away until around t1", |r| {
        away(&r.plan.crashes).is_some_and(|back| back > TO_T0 + NEAR)
    }),
    ("wallet lingers before verifying", |r| {
        r.plan.linger_verify.is_some()
    }),
    ("wallet lingers before claiming", |r| {
        r.plan.linger_claim.is_some()
    }),
    ("device clock skewed", |r| r.plan.skew != 0),
    ("two units", |r| r.plan.units == 2),
    ("ends paid", |r| matches!(r.ended, Some(End::Paid(_)))),
    ("ends with its deposit swept back", |r| {
        matches!(r.ended, Some(End::SweptBack(_)))
    }),
    ("ends cancelled", |r| r.ended == Some(End::Cancelled)),
    ("ends abandoned", |r| {
        matches!(r.ended, Some(End::Abandoned(_)))
    }),
    ("ends nowhere", |r| r.ended.is_none()),
    ("no swap opened", |r| r.stage.is_none()),
    ("swap left open", |r| r.stage == Some(1)),
    ("swap left ready", |r| r.stage == Some(2)),
    ("swap claimed", |r| r.stage == Some(3)),
    ("swap refunded", |r| r.stage == Some(4)),
    ("a reorg", |r| has(r, "reorg of ")),
    ("a reorg two deep", |r| has(r, "reorg of 2 from")),
    ("a missed slot", |r| has(r, "missed the slot")),
    ("a transaction reverted", reverted),
    ("a private send", |r| has(r, "private tx ")),
    ("wallet killed before its deposit", |r| killed(r, false)),
    ("wallet killed after its deposit", |r| killed(r, true)),
    ("wallet stopped on an error", |r| has(r, "wallet stopped: ")),
    ("wallet lingered", |r| has(r, "the wallet lingers ")),
    ("a node call stalled", |r| has(r, " stalled on ")),
    ("a node request lost", |r| has(r, " lost its eth_")),
    ("a node answer lost", |r| has(r, " lost the answer to ")),
    ("maker busy", |r| has(r, "maker was busy for ")),
    ("relayer busy", |r| has(r, "relayer was busy for ")),
    ("a service hung", |r| has(r, " hung on ")),
    ("a service never heard", |r| has(r, " never heard ")),
    ("a service's answer lost", |r| {
        has(r, " lost its answer to ")
    }),
    ("alert: private sends stuck", |r| {
        has(r, ": Private sends from ")
    }),
    ("alert: sends queued", |r| has(r, " are queued. ")),
    ("operator restarted a sender", |r| {
        has(r, "the operator restarted ")
    }),
    ("operator sent a transaction again", |r| {
        has(r, "the operator sent transaction ")
    }),
    ("operator could not send again", |r| {
        has(r, "the operator could not send ")
    }),
    ("nobody acted on an alert", |r| {
        has(r, "nobody acts on the alert")
    }),
];

const FORWARD_SAID: &[&str] = &["wallet stopped: ", "wallet ended: Abandoned("];

pub(crate) fn forward(report: &Forward) -> (Vec<u16>, Vec<String>) {
    reached(report, FORWARD, FORWARD_SAID)
}

pub(crate) fn forward_all() -> Vec<String> {
    all(FORWARD)
}

fn reverse_focus(report: &Reverse) -> u64 {
    Seed(report.seed).below("reverse-focus", 0, 12)
}

const REVERSE_T1: u64 = REVERSE_TO_T0 + REVERSE_T0_TO_T1;

const REVERSE: &[Situation<Reverse>] = &[
    ("maker Honest", |r| r.plan.mood == ReverseMood::Honest),
    ("maker NeverDeposits", |r| {
        r.plan.mood == ReverseMood::NeverDeposits
    }),
    ("maker DepositsLess", |r| {
        r.plan.mood == ReverseMood::DepositsLess
    }),
    ("maker DepositsLate", |r| {
        matches!(r.plan.mood, ReverseMood::DepositsLate(_))
    }),
    ("maker NeverClaims", |r| {
        r.plan.mood == ReverseMood::NeverClaims
    }),
    ("maker ClaimsLate", |r| {
        matches!(r.plan.mood, ReverseMood::ClaimsLate(_))
    }),
    ("maker ClaimsAtTheWire", |r| {
        r.plan.mood == ReverseMood::ClaimsAtTheWire
    }),
    ("maker LocksLong", |r| r.plan.mood == ReverseMood::LocksLong),
    ("maker LiesAboutDeadlines", |r| {
        matches!(r.plan.mood, ReverseMood::LiesAboutDeadlines(_))
    }),
    ("maker LiesAboutNote", |r| {
        r.plan.mood == ReverseMood::LiesAboutNote
    }),
    ("maker LiesAboutUser", |r| {
        r.plan.mood == ReverseMood::LiesAboutUser
    }),
    ("maker LiesAboutAmounts", |r| {
        matches!(r.plan.mood, ReverseMood::LiesAboutAmounts(_))
    }),
    ("maker LiesAboutDeployment", |r| {
        matches!(r.plan.mood, ReverseMood::LiesAboutDeployment(_))
    }),
    ("maker BadProof", |r| r.plan.mood == ReverseMood::BadProof),
    ("maker ReportsAnotherSwap", |r| {
        r.plan.mood == ReverseMood::ReportsAnotherSwap
    }),
    ("relayer Honest", |r| {
        r.plan.service == ReverseService::Honest
    }),
    ("relayer SkipsPayouts", |r| {
        r.plan.service == ReverseService::SkipsPayouts
    }),
    ("relayer SkipsOpen", |r| {
        r.plan.service == ReverseService::SkipsOpen
    }),
    ("relayer Careless", |r| {
        r.plan.service == ReverseService::Careless
    }),
    ("relayer BusyWithRefunds", |r| {
        r.plan.service == ReverseService::BusyWithRefunds
    }),
    ("relayer LosesReadyAnswers", |r| {
        r.plan.service == ReverseService::LosesReadyAnswers
    }),
    ("reveals sent privately", |r| r.plan.private),
    ("reveals sent publicly", |r| !r.plan.private),
    ("calm weather", |r| r.plan.calm()),
    ("focus: a refund asked just after confirming the ZEC", |r| {
        reverse_focus(r) == 0
    }),
    ("focus: a refund asked before the ZEC arrives", |r| {
        reverse_focus(r) == 1
    }),
    ("focus: the deposit spendable around the last ready", |r| {
        reverse_focus(r) == 2
    }),
    ("focus: a short deposit", |r| reverse_focus(r) == 3),
    ("focus: a reorg behind every transaction", |r| {
        reverse_focus(r) == 4
    }),
    ("focus: a claim seconds before t1", |r| {
        reverse_focus(r) == 5
    }),
    ("focus: a lying quote", |r| reverse_focus(r) == 6),
    ("wallet wants its USDC back some time after funding", |r| {
        r.plan.patience.is_some()
    }),
    ("wallet wants its USDC back after confirming the ZEC", |r| {
        r.plan.regret.is_some()
    }),
    ("wallet lingers before readying", |r| {
        r.plan.linger_ready.is_some()
    }),
    ("wallet lingers before refunding", |r| {
        r.plan.linger_refund.is_some()
    }),
    ("wallet away until around t0", |r| {
        away(&r.plan.crashes).is_some_and(|back| back <= REVERSE_TO_T0 + NEAR)
    }),
    ("wallet away until around t1", |r| {
        let back = away(&r.plan.crashes);
        back.is_some_and(|back| back > REVERSE_TO_T0 + NEAR && back <= REVERSE_T1 + NEAR)
    }),
    ("wallet away until long after t1", |r| {
        away(&r.plan.crashes).is_some_and(|back| back > REVERSE_T1 + NEAR)
    }),
    ("device clock skewed", |r| r.plan.skew != 0),
    ("two units", |r| r.plan.units == 2),
    ("ends with the ZEC received", |r| {
        matches!(r.ended, Some(reverse::Ended::Received(_)))
    }),
    ("ends refunded", |r| {
        r.ended == Some(reverse::Ended::Refunded)
    }),
    ("ends abandoned", |r| {
        matches!(r.ended, Some(reverse::Ended::Abandoned(_)))
    }),
    ("ends nowhere", |r| r.ended.is_none()),
    ("no escrow agreed", |r| r.stage.is_none()),
    ("escrow never funded", |r| r.stage == Some(0)),
    ("escrow left open", |r| r.stage == Some(1)),
    ("escrow left ready", |r| r.stage == Some(2)),
    ("escrow claimed", |r| r.stage == Some(3)),
    ("escrow refunded", |r| r.stage == Some(4)),
    ("the maker paid its ZEC in", |r| has(r, "the maker paid ")),
    ("the wallet wanted its USDC back", |r| {
        has(r, "the wallet wants its USDC back")
    }),
    ("a ready refused or failed", |r| has(r, " no ready: ")),
    ("a refund refused or failed", |r| has(r, " no refund: ")),
    ("funding not taken", |r| has(r, "the funding was not taken")),
    ("a reorg", |r| has(r, "reorg of ")),
    ("a reorg two deep", |r| has(r, "reorg of 2 from")),
    ("a missed slot", |r| has(r, "missed the slot")),
    ("a transaction reverted", reverted),
    ("a private send", |r| has(r, "private tx ")),
    ("wallet killed", |r| has(r, "the wallet crashed")),
    ("wallet stopped on an error", |r| has(r, "wallet stopped: ")),
    ("wallet lingered", |r| has(r, "the wallet lingers ")),
    ("a node call stalled", |r| has(r, " stalled on ")),
    ("a node request lost", |r| has(r, " lost its eth_")),
    ("a node answer lost", |r| has(r, " lost the answer to ")),
    ("maker busy", |r| has(r, "maker was busy for ")),
    ("relayer busy", |r| has(r, "relayer was busy for ")),
    ("a service hung", |r| has(r, " hung on ")),
    ("a service never heard", |r| has(r, " never heard ")),
    ("a service's answer lost", |r| {
        has(r, " lost its answer to ")
    }),
    ("alert: private sends stuck", |r| {
        has(r, ": Private sends from ")
    }),
    ("alert: sends queued", |r| has(r, " are queued. ")),
    ("operator restarted a sender", |r| {
        has(r, "the operator restarted ")
    }),
    ("operator sent a transaction again", |r| {
        has(r, "the operator sent transaction ")
    }),
    ("operator could not send again", |r| {
        has(r, "the operator could not send ")
    }),
];

const REVERSE_SAID: &[&str] = &[
    "wallet stopped: ",
    "wallet ended: Abandoned(",
    "no ready: ",
    "no refund: ",
    "the funding was not taken: ",
];

pub(crate) fn reverse(report: &Reverse) -> (Vec<u16>, Vec<String>) {
    reached(report, REVERSE, REVERSE_SAID)
}

pub(crate) fn reverse_all() -> Vec<String> {
    all(REVERSE)
}

fn deposits_focus(report: &Deposits) -> Option<u64> {
    let focus = Seed(report.seed).below("deposit-focus", 0, 16);
    (!report.plan.fees_stay).then_some(focus)
}

fn arrives(report: &Deposits, at: fn(&deposits::Where) -> bool) -> bool {
    report.plan.arrivals.iter().any(|arrival| at(&arrival.to))
}

fn left(report: &Deposits, how: fn(&deposits::Ended) -> bool) -> bool {
    report.ended.as_ref().is_some_and(how)
}

const DEPOSITS: &[Situation<Deposits>] = &[
    ("relayer Honest", |r| {
        r.plan.service == deposits::Service::Honest
    }),
    ("relayer Overcharges", |r| {
        matches!(r.plan.service, deposits::Service::Overcharges(_))
    }),
    ("relayer AnotherDeployment", |r| {
        matches!(r.plan.service, deposits::Service::AnotherDeployment(_))
    }),
    ("relayer TakesNone", |r| {
        r.plan.service == deposits::Service::TakesNone
    }),
    ("relayer LowMinimum", |r| {
        r.plan.service == deposits::Service::LowMinimum
    }),
    ("relayer StaleTerms", |r| {
        matches!(r.plan.service, deposits::Service::StaleTerms(_))
    }),
    ("relayer Sits", |r| {
        matches!(r.plan.service, deposits::Service::Sits(_))
    }),
    ("relayer NeverSends", |r| {
        r.plan.service == deposits::Service::NeverSends
    }),
    ("relayer LosesAnswers", |r| {
        r.plan.service == deposits::Service::LosesAnswers
    }),
    ("relayer Tampers", |r| {
        matches!(r.plan.service, deposits::Service::Tampers(_))
    }),
    ("relayer NamesAnother", |r| {
        matches!(r.plan.service, deposits::Service::NamesAnother(_))
    }),
    ("relayer takes requests valid under a day only", |r| {
        r.plan.offer.max_validity < 24 * 60 * 60
    }),
    ("fee repriced within the cap", |r| {
        r.plan.reprice.is_some_and(|(_, fee)| fee <= r.plan.cap.fee)
    }),
    ("fee repriced over the cap", |r| {
        r.plan.reprice.is_some_and(|(_, fee)| fee > r.plan.cap.fee)
    }),
    ("wallet's cap is the relayer's fee", |r| {
        r.plan.cap.fee == r.plan.offer.fee
    }),
    ("with Multicall3", |r| r.plan.multicall),
    ("without Multicall3", |r| !r.plan.multicall),
    ("arrival at the address shown", |r| {
        arrives(r, |to| *to == deposits::Where::Shown)
    }),
    ("arrival at an address used before", |r| {
        arrives(r, |to| matches!(to, deposits::Where::Reused(_)))
    }),
    ("arrival some addresses ahead", |r| {
        arrives(r, |to| matches!(to, deposits::Where::Ahead(_)))
    }),
    ("arrival past the gap", |r| {
        arrives(r, |to| matches!(to, deposits::Where::Far(_)))
    }),
    ("arrival at a set address", |r| {
        arrives(r, |to| matches!(to, deposits::Where::Index(_)))
    }),
    ("one arrival", |r| r.plan.arrivals.len() == 1),
    ("several arrivals", |r| r.plan.arrivals.len() > 1),
    ("some shields sent back", |r| {
        (1..1000).contains(&r.plan.returned)
    }),
    ("every shield sent back", |r| r.plan.returned == 1000),
    ("a forward swap's payout sent back", |r| {
        r.plan.payout.is_some_and(|payout| !payout.reverse)
    }),
    ("a reverse swap's payout sent back", |r| {
        r.plan.payout.is_some_and(|payout| payout.reverse)
    }),
    ("a swap's payout sent back twice", |r| {
        r.plan.payout.is_some_and(|payout| payout.returns == 2)
    }),
    ("fees stay where the weather left them", |r| {
        r.plan.fees_stay
    }),
    ("three payments at once under steady fees", |r| {
        r.plan.fees_stay && r.plan.arrivals.len() == 3
    }),
    ("focus: a short validity on a relayer that sits", |r| {
        deposits_focus(r) == Some(0)
    }),
    ("focus: lagging terms after a new fee", |r| {
        deposits_focus(r) == Some(1)
    }),
    ("focus: an older transaction of the same address", |r| {
        deposits_focus(r) == Some(2)
    }),
    ("focus: a transaction of another address", |r| {
        deposits_focus(r) == Some(3)
    }),
    ("focus: a gap closed late", |r| deposits_focus(r) == Some(4)),
    ("focus: every shield sent back", |r| {
        deposits_focus(r) == Some(5)
    }),
    ("focus: the minimum", |r| deposits_focus(r) == Some(6)),
    ("focus: a second arrival seconds after the first", |r| {
        deposits_focus(r) == Some(7)
    }),
    ("focus: a fee over the cap", |r| {
        deposits_focus(r) == Some(8)
    }),
    ("focus: another deployment", |r| {
        deposits_focus(r) == Some(9)
    }),
    ("focus: a tampered send", |r| deposits_focus(r) == Some(10)),
    ("focus: a relayer gone silent", |r| {
        deposits_focus(r) == Some(11)
    }),
    ("wallet away ten minutes or more", |r| {
        away(&r.plan.crashes).is_some()
    }),
    ("device clock skewed", |r| r.plan.skew != 0),
    ("nothing left waiting", |r| {
        left(r, |ended| ended.waiting == 0)
    }),
    ("something left waiting", |r| {
        left(r, |ended| ended.waiting > 0)
    }),
    ("something sent back", |r| {
        left(r, |ended| ended.returned > 0)
    }),
    ("no deposit believed", |r| {
        left(r, |ended| ended.believed == 0)
    }),
    ("several deposits believed", |r| {
        left(r, |ended| ended.believed > 1)
    }),
    ("the scan at the end failed", |r| {
        left(r, |ended| !ended.audited)
    }),
    ("the wallet believed a deposit", |r| {
        has(r, "the wallet believes ")
    }),
    ("Railgun sent a shield back", |r| {
        has(r, " back to the vault of #")
    }),
    ("Railgun sent the swap's payout back", |r| {
        has(r, " back to the vault of the swap")
    }),
    ("a deposit refused or failed", |r| {
        has(r, " no deposit from ")
    }),
    ("a rescue refused or failed", |r| has(r, " no rescue for ")),
    ("the swap's rescue refused or failed", |r| {
        has(r, " no rescue of the swap's payout")
    }),
    ("a scan failed", |r| has(r, " no scan: ")),
    ("a reorg", |r| has(r, "reorg of ")),
    ("a reorg two deep", |r| has(r, "reorg of 2 from")),
    ("a missed slot", |r| has(r, "missed the slot")),
    ("a transaction reverted", reverted),
    ("wallet killed", |r| has(r, "the wallet crashed")),
    ("a node call stalled", |r| has(r, " stalled on ")),
    ("a node request lost", |r| has(r, " lost its eth_")),
    ("a node answer lost", |r| has(r, " lost the answer to ")),
    ("relayer busy", |r| has(r, "relayer was busy for ")),
    ("a service hung", |r| has(r, " hung on ")),
    ("a service never heard", |r| has(r, " never heard ")),
    ("a service's answer lost", |r| {
        has(r, " lost its answer to ")
    }),
    ("alert: sends queued", |r| has(r, " are queued. ")),
    ("operator sent a transaction again", |r| {
        has(r, "the operator sent transaction ")
    }),
    ("operator could not send again", |r| {
        has(r, "the operator could not send ")
    }),
];

const DEPOSITS_SAID: &[&str] = &[
    "no deposit from ",
    "no rescue for ",
    "no rescue of the swap's payout: ",
    "no scan: ",
    "no look at ",
    "no address to show: ",
];

pub(crate) fn deposits(report: &Deposits) -> (Vec<u16>, Vec<String>) {
    reached(report, DEPOSITS, DEPOSITS_SAID)
}

pub(crate) fn deposits_all() -> Vec<String> {
    all(DEPOSITS)
}

fn zcash_focus(report: &Zcash) -> Option<u64> {
    let focus = Seed(report.seed).below("zcash-focus", 0, 16);
    let own = !matches!(focus, 3 | 8 | 10 | 11) || report.plan.own;
    (own && report.plan.sky.dropped == 0).then_some(focus)
}

fn errand(report: &Zcash, like: fn(&lightwallet::Errand) -> bool) -> bool {
    report.plan.errands.iter().any(like)
}

fn held(report: &Zcash, then: fn(&lightwallet::Ahead) -> bool) -> bool {
    report.plan.errands.iter().any(|errand| match errand {
        lightwallet::Errand::Pay {
            ahead: Some(ahead), ..
        } => then(ahead),
        _ => false,
    })
}

fn signs(report: &Zcash, like: fn(lightwallet::Trick, lightwallet::View, bool) -> bool) -> bool {
    report.plan.errands.iter().any(|errand| match errand {
        lightwallet::Errand::Sign {
            trick,
            view,
            proves,
            ..
        } => like(*trick, *view, *proves),
        _ => false,
    })
}

fn errand_ends(report: &Zcash, how: fn(&Option<lightwallet::End>) -> bool) -> bool {
    let ended = report.ended.as_ref();
    ended.is_some_and(|ended| ended.errands.iter().any(how))
}

fn tallied(report: &Zcash, how: fn(&lightwallet::Ended) -> bool) -> bool {
    report.ended.as_ref().is_some_and(how)
}

const ZCASH: &[Situation<Zcash>] = &[
    ("mainnet", |r| r.plan.mainnet),
    ("testnet", |r| !r.plan.mainnet),
    ("before NU7", |r| r.plan.era == lightwallet::Era::Before),
    ("across NU7's activation", |r| {
        matches!(r.plan.era, lightwallet::Era::Across(_))
    }),
    ("after NU7", |r| r.plan.era == lightwallet::Era::After),
    ("one confirmation", |r| r.plan.confirmations == 1),
    ("two confirmations", |r| r.plan.confirmations == 2),
    ("three confirmations", |r| r.plan.confirmations == 3),
    ("ten confirmations", |r| r.plan.confirmations == 10),
    ("wallet with an account from its seed", |r| {
        r.plan.own && !r.plan.inventory && r.plan.sky.steady > 0
    }),
    ("wallet with an inventory account", |r| {
        r.plan.own && r.plan.inventory && r.plan.sky.steady > 0
    }),
    ("wallet of joint accounts only", |r| {
        !r.plan.own && r.plan.sky.steady > 0
    }),
    ("a fresh wallet", |r| r.plan.sky.steady == 0),
    ("lightwalletd of another network", |r| {
        r.plan.service.another
    }),
    ("lightwalletd names tips backwards", |r| {
        r.plan.service.backwards
    }),
    ("payments lost with their reorged block", |r| {
        r.plan.sky.dropped > 0
    }),
    ("calm chain", |r| r.plan.calm()),
    ("focus: reorgs behind every transaction", |r| {
        zcash_focus(r) == Some(0)
    }),
    ("focus: built before the upgrade, delayed past it", |r| {
        zcash_focus(r) == Some(1)
    }),
    ("focus: a first transaction lost", |r| {
        zcash_focus(r) == Some(2)
    }),
    ("focus: killed while holding a proved payment", |r| {
        zcash_focus(r) == Some(3)
    }),
    ("focus: 65 to 68 notes in a joint account", |r| {
        zcash_focus(r) == Some(4)
    }),
    ("focus: joint accounts one after another", |r| {
        zcash_focus(r) == Some(5)
    }),
    ("focus: a stalling lightwalletd", |r| {
        zcash_focus(r) == Some(6)
    }),
    ("focus: lost answers to broadcasts", |r| {
        zcash_focus(r) == Some(7)
    }),
    ("focus: each way a held payment ends", |r| {
        zcash_focus(r) == Some(8)
    }),
    ("focus: hostile sweeps for the signer", |r| {
        zcash_focus(r) == Some(9)
    }),
    ("focus: fee addresses under reorgs", |r| {
        zcash_focus(r) == Some(10)
    }),
    ("focus: restarts of an inventory or seed account", |r| {
        zcash_focus(r) == Some(11)
    }),
    ("errand: watch a joint account", |r| {
        errand(r, |errand| {
            matches!(errand, lightwallet::Errand::Receive { sweep: None, .. })
        })
    }),
    ("errand: a joint account after another", |r| {
        errand(r, |errand| {
            matches!(errand, lightwallet::Errand::Receive { chained: true, .. })
        })
    }),
    ("errand: sweep to an address of its own", |r| {
        errand(r, |errand| {
            let home = Some(lightwallet::Home::Own);
            matches!(errand, lightwallet::Errand::Receive { sweep, .. } if *sweep == home)
        })
    }),
    ("errand: sweep to another wallet", |r| {
        errand(r, |errand| {
            let home = Some(lightwallet::Home::Stranger);
            matches!(errand, lightwallet::Errand::Receive { sweep, .. } if *sweep == home)
        })
    }),
    ("errand: sweep of over 64 notes", |r| {
        errand(
            r,
            |errand| matches!(errand, lightwallet::Errand::Receive { notes, .. } if notes.len() > 64),
        )
    }),
    ("errand: pay", |r| {
        errand(r, |errand| {
            matches!(errand, lightwallet::Errand::Pay { ahead: None, .. })
        })
    }),
    ("errand: pay proved ahead, then sent", |r| {
        held(r, |ahead| matches!(ahead.then, lightwallet::Then::Send(_)))
    }),
    ("errand: pay proved ahead, then released", |r| {
        held(r, |ahead| {
            matches!(ahead.then, lightwallet::Then::Release(_))
        })
    }),
    (
        "errand: pay proved ahead, then signed with another key",
        |r| {
            held(r, |ahead| {
                matches!(ahead.then, lightwallet::Then::WrongKey(_))
            })
        },
    ),
    ("errand: pay proved ahead, then held too long", |r| {
        held(r, |ahead| ahead.then == lightwallet::Then::Late)
    }),
    ("errand: pay proved ahead, killed while held", |r| {
        held(r, |ahead| ahead.crash.is_some())
    }),
    ("errand: watch a fee address", |r| {
        errand(r, |errand| {
            matches!(errand, lightwallet::Errand::Fee { .. })
        })
    }),
    ("errand: sign an honest sweep", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::None)
    }),
    ("errand: sign and prove an honest sweep", |r| {
        signs(r, |_, _, proves| proves)
    }),
    ("errand: sign a sweep paying another receiver", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Elsewhere)
    }),
    ("errand: sign a sweep that skims", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Skims)
    }),
    ("errand: sign a sweep spending part", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Partial)
    }),
    ("errand: sign a sweep overpaying the network", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Overpays)
    }),
    ("errand: sign a sweep its receiver cannot read", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Garbles)
    }),
    ("errand: sign a sweep with its value hidden", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Hides)
    }),
    ("errand: sign a sweep already signed", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::Presigned)
    }),
    (
        "errand: sign a sweep claiming a receiver it does not pay",
        |r| signs(r, |trick, _, _| trick == lightwallet::Trick::Forges),
    ),
    ("errand: sign a sweep for another key", |r| {
        signs(r, |trick, _, _| trick == lightwallet::Trick::AnothersKey)
    }),
    ("signer shown the whole sweep", |r| {
        signs(r, |_, view, _| view == lightwallet::View::Whole)
    }),
    ("signer shown a signer's view", |r| {
        signs(r, |_, view, _| view == lightwallet::View::Signers)
    }),
    ("signer shown a compact view", |r| {
        signs(r, |_, view, _| view == lightwallet::View::Compact)
    }),
    ("an errand ends swept", |r| {
        errand_ends(r, |end| matches!(end, Some(lightwallet::End::Swept(_))))
    }),
    ("an errand ends watched", |r| {
        errand_ends(r, |end| matches!(end, Some(lightwallet::End::Watched(_))))
    }),
    ("an errand ends refused for its fee", |r| {
        errand_ends(r, |end| matches!(end, Some(lightwallet::End::Refused(_))))
    }),
    ("an errand ends paid", |r| {
        errand_ends(r, |end| matches!(end, Some(lightwallet::End::Paid(_))))
    }),
    ("an errand ends with its fee seen", |r| {
        errand_ends(r, |end| matches!(end, Some(lightwallet::End::Seen(_))))
    }),
    ("an errand ends signed", |r| {
        errand_ends(r, |end| matches!(end, Some(lightwallet::End::Signed(_))))
    }),
    ("an errand ends with the signer balking", |r| {
        errand_ends(r, |end| *end == Some(lightwallet::End::Balked))
    }),
    ("an errand left undone", |r| errand_ends(r, Option::is_none)),
    ("the wallet could not be read at the end", |r| {
        tallied(r, |ended| !ended.audited)
    }),
    ("a transaction sent", |r| tallied(r, |ended| ended.sent > 0)),
    ("a transaction left unmined", |r| {
        tallied(r, |ended| ended.mined < ended.sent)
    }),
    ("a reorg", |r| has(r, "reorg of ")),
    ("a reorg two deep", |r| has(r, "reorg of 2 from")),
    ("a reorg three deep", |r| has(r, "reorg of 3 from")),
    ("a payment gone with its block", |r| {
        has(r, " is gone with its block")
    }),
    ("another holder of a joint key spent a sweep's notes", |r| {
        has(r, "another holder of the key spends")
    }),
    ("a transaction left the pool", |r| {
        has(r, " left the pool: ")
    }),
    ("a transaction lost from the pool", |r| {
        has(r, " was lost from the pool")
    }),
    ("a broadcast refused by the node", |r| {
        has(r, " was refused: ")
    }),
    ("lightwalletd hung up", |r| has(r, "lightwalletd hung up ")),
    ("lightwalletd failed a call", |r| {
        has(r, "lightwalletd failed ")
    }),
    ("lightwalletd never answered", |r| {
        has(r, "lightwalletd never answered ")
    }),
    ("lightwalletd went silent mid-answer", |r| {
        has(r, " in way 1")
    }),
    ("lightwalletd trickled bytes", |r| has(r, " in way 2")),
    ("lightwalletd cut a stream of blocks", |r| {
        has(r, "lightwalletd cut ")
    }),
    ("lightwalletd named a tip behind", |r| {
        has(r, "lightwalletd named a tip ")
    }),
    ("lightwalletd answered another height's tree state", |r| {
        has(r, "the tree state of another height")
    }),
    ("lightwalletd lost its answer to a broadcast", |r| {
        has(r, "lost its answer to the broadcast")
    }),
    ("wallet killed", |r| has(r, "the wallet crashed")),
    ("the wallet's alert", |r| has(r, "alert from the wallet")),
    ("a transaction expired unmined", |r| {
        has(r, " expired unmined")
    }),
    ("a payment proved ahead", |r| {
        both(r, "the wallet proved ", " ahead")
    }),
    ("a payment proved again", |r| {
        both(r, "the wallet proved ", " again")
    }),
    ("a held payment released", |r| {
        has(r, "the wallet released ")
    }),
    ("a held payment signed with another key", |r| {
        has(r, " with another key")
    }),
    ("a held payment not sent", |r| {
        has(r, "the wallet would not send ")
    }),
    ("a sweep refused for its fee", |r| {
        has(r, "the wallet refused to sweep ")
    }),
    ("the signer refused a sweep", |r| {
        has(r, "the signer refused ")
    }),
    ("the signer signed a sweep", |r| {
        has(r, "the signer signed ")
    }),
    ("the wallet saw a fee", |r| {
        has(r, "the wallet saw the fee ")
    }),
    ("the wallet forgot an earlier swap", |r| {
        has(r, "forgot the earlier swap")
    }),
    ("a sync failed", |r| has(r, ": no sync: ")),
    ("a broadcast failed", |r| has(r, " not out: ")),
];

const ZCASH_SAID: &[&str] = &[
    ": no ",
    ": not ",
    " not out: ",
    "the signer refused ",
    "no sweep of ",
    "the wallet would not send ",
    " left the pool: ",
    " was refused: ",
];

pub(crate) fn zcash(report: &Zcash) -> (Vec<u16>, Vec<String>) {
    reached(report, ZCASH, ZCASH_SAID)
}

pub(crate) fn zcash_all() -> Vec<String> {
    all(ZCASH)
}
