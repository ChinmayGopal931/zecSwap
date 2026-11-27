use std::fmt::Debug;

use zecswap_sim::hunt::{Hunt, Kind, Told, folder, seated, spread};
use zecswap_sim::shrink::{self, Shrinks};
use zecswap_sim::{End, Plan, Report, deposits, guards, lightwallet, reverse, run, run_with};

fn listed(spec: &str) -> Vec<u64> {
    match spec.split_once("..") {
        Some((from, to)) => (from.parse().unwrap()..to.parse().unwrap()).collect(),
        None => spec.split(',').map(|seed| seed.parse().unwrap()).collect(),
    }
}

fn seeds(var: &str, default: std::ops::Range<u64>) -> Vec<u64> {
    match std::env::var(var) {
        Ok(spec) => listed(&spec),
        Err(_) => default.collect(),
    }
}

fn regressions(list: &str) -> Vec<u64> {
    list.lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

fn hold<P: Debug + Send, E: Debug + Send>(seeds: &[u64], run: fn(u64) -> Report<P, E>) {
    let mut failed = Vec::new();
    spread(seeds, run, |report| {
        if !report.failures.is_empty() {
            failed.push(report);
        }
    });
    for report in &failed {
        eprintln!("{}\n", report.story());
    }
    let seeds: Vec<u64> = failed.iter().map(|report| report.seed).collect();
    assert!(
        seeds.is_empty(),
        "seeds {seeds:?} broke an invariant: replay one with ZECSWAP_SIM_SEEDS=<seed>, and once fixed add it to its regressions list"
    );
}

fn digested<P: Debug, E: Debug>(report: &Report<P, E>)
where
    Report<P, E>: Told,
{
    if std::env::var("ZECSWAP_SIM_DIGEST").is_ok() {
        let kind = <Report<P, E>>::KIND.told();
        println!("digest of {kind} {}: {}", report.seed, report.digest());
    }
}

fn tell<P: Shrinks>()
where
    Report<P, P::Ended>: Told,
{
    let steps = std::env::var("ZECSWAP_SIM_STEPS").unwrap_or_default();
    for seed in seeds("ZECSWAP_SIM_SEEDS", 0..1) {
        let plan = shrink::stepped(P::of(seed), &steps);
        let report = P::run(seed, plan.unwrap_or_else(|refused| panic!("{refused}")));
        println!("{}\n", report.story());
        if std::env::var("ZECSWAP_SIM_TRACE").is_ok() {
            println!("{}", report.trace.join("\n"));
        }
        digested(&report);
    }
}

fn surveyed<P: Debug + Send, E: Debug + Send>(run: fn(u64) -> Report<P, E>)
where
    Report<P, E>: Told,
{
    spread(&seeds("ZECSWAP_SIM_SEEDS", 0..24), run, |report| {
        println!("{}", report.line());
        digested(&report);
    });
}

fn replays<P: Debug + Send, E: Debug + Send>(
    run: fn(u64) -> Report<P, E>,
    some: std::ops::Range<u64>,
) {
    let twice = |seed| (run(seed), run(seed));
    spread(
        &seeds("ZECSWAP_SIM_REPLAY", some),
        twice,
        |(first, second)| {
            let seed = first.seed;
            assert_eq!(first.trace, second.trace, "seed {seed} ran two ways");
            assert_eq!(
                first.failures, second.failures,
                "seed {seed} judged two ways"
            );
        },
    );
}

fn asked(all: &[u64]) -> Vec<(Kind, Vec<u64>)> {
    let run = |asked: &str| {
        let (name, own) = asked.split_once('=').unzip();
        let kind = Kind::named(name.unwrap_or(asked));
        let kind = kind.expect("a run: forward, reverse, deposits, zcash or guards");
        (kind, own.map_or_else(|| all.to_vec(), listed))
    };
    match std::env::var("ZECSWAP_SIM_RUNS") {
        Ok(runs) => runs.split(',').map(run).collect(),
        Err(_) => Kind::ALL.map(|kind| (kind, all.to_vec())).to_vec(),
    }
}

fn shrunk(kind: Kind, seed: u64, way: Option<&str>) {
    let shrunk = shrink::shrink(kind, seed, way, &folder());
    let shrunk = shrunk.expect("a folder to keep the shrunk plan in");
    if shrunk.is_empty() {
        let so = way.map_or(String::new(), |way| format!(" as [{way}]"));
        let run = kind.name();
        println!("{run} seed {seed} does not fail{so} on this tree: nothing to shrink\n");
    }
    for shrunk in shrunk {
        let kept = match shrunk.earlier {
            true => "kept from an earlier shrink on this tree in",
            false => "kept in",
        };
        println!("{}{kept} {}\n", shrunk.text, shrunk.kept.display());
    }
}

#[test]
#[ignore = "hunts seeds over the five runs, ZECSWAP_SIM_THREADS at once: ZECSWAP_SIM_SEEDS=0..4000 cargo test -p zecswap-sim hunt -- --exact --ignored --nocapture"]
fn hunt() {
    let asked = Hunt {
        runs: asked(&seeds("ZECSWAP_SIM_SEEDS", 0..200)),
        folder: folder(),
    };
    let summary = zecswap_sim::hunt::hunt(&asked).expect("a folder to keep the hunt in");
    println!("{summary}");
    if std::env::var("ZECSWAP_SIM_SHRINK").is_ok() {
        for (kind, seed, way) in &summary.new {
            shrunk(*kind, *seed, Some(way));
        }
    }
    let new: Vec<&str> = summary.new.iter().map(|(.., way)| way.as_str()).collect();
    assert!(
        new.is_empty(),
        "the hunt met ways of failing with no label in {}: {}",
        folder().join("labels.txt").display(),
        new.join(" ")
    );
}

#[test]
#[ignore = "shrinks failing seeds to the simplest plans that fail the same way: ZECSWAP_SIM_SEEDS=<seed> ZECSWAP_SIM_RUNS=<run> cargo test -p zecswap-sim shrink -- --exact --ignored --nocapture"]
fn shrink() {
    let seeds = std::env::var("ZECSWAP_SIM_SEEDS").expect("ZECSWAP_SIM_SEEDS, the seeds to shrink");
    for (kind, seeds) in asked(&listed(&seeds)) {
        for seed in seeds {
            shrunk(kind, seed, None);
        }
    }
}

#[test]
fn a_swap_with_nothing_wrong_pays_the_users_note() {
    let report = seated(|| run_with(1, Plan::plain()));
    assert!(report.failures.is_empty(), "{}", report.story());
    assert!(
        matches!(report.ended, Some(End::Paid(_))),
        "{}",
        report.story()
    );
}

#[test]
fn every_seed_keeps_the_invariants() {
    hold(&seeds("ZECSWAP_SIM_SEEDS", 0..32), run);
}

#[test]
fn seeds_that_once_failed_still_hold() {
    hold(&regressions(include_str!("../regressions.txt")), run);
}

#[test]
#[ignore = "tells a seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim story -- --ignored --nocapture"]
fn story() {
    tell::<Plan>();
}

#[test]
#[ignore = "one line a seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim survey -- --ignored --nocapture"]
fn survey() {
    surveyed(run);
}

#[test]
#[ignore = "tells the story of a swap with nothing wrong"]
fn plain_story() {
    let report = run_with(1, Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_seed_replays_to_the_same_trace() {
    replays(run, 3..6);
}

#[test]
fn a_reverse_swap_with_nothing_wrong_sends_the_zec() {
    let report = seated(|| reverse::run_with(1, reverse::Plan::plain()));
    assert!(report.failures.is_empty(), "{}", report.story());
    assert!(
        matches!(report.ended, Some(reverse::Ended::Received(_))),
        "{}",
        report.story()
    );
}

#[test]
fn every_reverse_seed_keeps_the_invariants() {
    hold(&seeds("ZECSWAP_SIM_SEEDS", 0..32), reverse::run);
}

#[test]
fn reverse_seeds_that_once_failed_still_hold() {
    let seeds = regressions(include_str!("../reverse-regressions.txt"));
    hold(&seeds, reverse::run);
}

#[test]
#[ignore = "tells a reverse seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim reverse_story -- --ignored --nocapture"]
fn reverse_story() {
    tell::<reverse::Plan>();
}

#[test]
#[ignore = "one line a reverse seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim reverse_survey -- --ignored --nocapture"]
fn reverse_survey() {
    surveyed(reverse::run);
}

#[test]
#[ignore = "tells the story of a reverse swap with nothing wrong"]
fn plain_reverse_story() {
    let report = reverse::run_with(1, reverse::Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_reverse_seed_replays_to_the_same_trace() {
    replays(reverse::run, 3..6);
}

#[test]
fn a_deposit_with_nothing_wrong_reaches_the_wallets_note() {
    let report = seated(|| deposits::run_with(1, deposits::Plan::plain()));
    assert!(report.failures.is_empty(), "{}", report.story());
    let ended = report.ended.as_ref().expect("a tally");
    assert!(
        ended.private > 0 && ended.waiting == 0 && ended.believed == 1,
        "{}",
        report.story()
    );
}

#[test]
fn every_deposit_seed_keeps_the_invariants() {
    hold(&seeds("ZECSWAP_SIM_SEEDS", 0..16), deposits::run);
}

#[test]
fn deposit_seeds_that_once_failed_still_hold() {
    let seeds = regressions(include_str!("../deposit-regressions.txt"));
    hold(&seeds, deposits::run);
}

#[test]
#[ignore = "tells a deposit seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim deposit_story -- --ignored --nocapture"]
fn deposit_story() {
    tell::<deposits::Plan>();
}

#[test]
#[ignore = "one line a deposit seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim deposit_survey -- --ignored --nocapture"]
fn deposit_survey() {
    surveyed(deposits::run);
}

#[test]
#[ignore = "tells the story of a deposit with nothing wrong"]
fn plain_deposit_story() {
    let report = deposits::run_with(1, deposits::Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_deposit_seed_replays_to_the_same_trace() {
    replays(deposits::run, 3..6);
}

#[test]
fn a_zcash_wallet_with_nothing_wrong_sweeps_its_deposit() {
    let report = seated(|| lightwallet::run_with(1, lightwallet::Plan::plain()));
    assert!(report.failures.is_empty(), "{}", report.story());
    let ended = report.ended.as_ref().expect("a tally");
    assert!(
        ended.errands == [Some(lightwallet::End::Swept(500_000))] && ended.own == 1_490_000,
        "{}",
        report.story()
    );
}

#[test]
fn every_zcash_seed_keeps_the_invariants() {
    hold(&seeds("ZECSWAP_SIM_SEEDS", 0..6), lightwallet::run);
}

#[test]
fn zcash_seeds_that_once_failed_still_hold() {
    let seeds = regressions(include_str!("../zcash-regressions.txt"));
    hold(&seeds, lightwallet::run);
}

#[test]
#[ignore = "tells a Zcash wallet seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim zcash_story -- --ignored --nocapture"]
fn zcash_story() {
    tell::<lightwallet::Plan>();
}

#[test]
#[ignore = "one line a Zcash wallet seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim zcash_survey -- --ignored --nocapture"]
fn zcash_survey() {
    surveyed(lightwallet::run);
}

#[test]
#[ignore = "tells the story of a Zcash wallet with nothing wrong"]
fn plain_zcash_story() {
    let report = lightwallet::run_with(1, lightwallet::Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_zcash_seed_replays_to_the_same_trace() {
    replays(lightwallet::run, 3..5);
}

#[test]
fn a_relayer_asked_nothing_wrong_serves_each_request() {
    let report = seated(|| guards::run_with(1, guards::Plan::plain()));
    assert!(report.failures.is_empty(), "{}", report.story());
    let ended = report.ended.as_ref().expect("a tally");
    assert!(
        ended.landed == 2 && ended.reverted == 0 && ended.refused == 0,
        "{}",
        report.story()
    );
}

#[test]
fn every_guard_seed_keeps_the_invariants() {
    hold(&seeds("ZECSWAP_SIM_SEEDS", 0..24), guards::run);
}

#[test]
fn guard_seeds_that_once_failed_still_hold() {
    let seeds = regressions(include_str!("../guard-regressions.txt"));
    hold(&seeds, guards::run);
}

#[test]
#[ignore = "tells a relayer seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim guard_story -- --ignored --nocapture"]
fn guard_story() {
    tell::<guards::Plan>();
}

#[test]
#[ignore = "one line a relayer seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim guard_survey -- --ignored --nocapture"]
fn guard_survey() {
    surveyed(guards::run);
}

#[test]
#[ignore = "tells the story of a relayer asked nothing wrong"]
fn plain_guard_story() {
    let report = guards::run_with(1, guards::Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_guard_seed_replays_to_the_same_trace() {
    replays(guards::run, 3..6);
}
