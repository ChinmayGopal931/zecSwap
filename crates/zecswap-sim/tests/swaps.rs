use std::fmt::Debug;

use zecswap_sim::{End, Plan, Report, reverse, run, run_with};

fn seeds(var: &str, default: std::ops::Range<u64>) -> Vec<u64> {
    match std::env::var(var) {
        Ok(spec) => match spec.split_once("..") {
            Some((from, to)) => (from.parse().unwrap()..to.parse().unwrap()).collect(),
            None => spec.split(',').map(|seed| seed.parse().unwrap()).collect(),
        },
        Err(_) => default.collect(),
    }
}

fn regressions(list: &str) -> Vec<u64> {
    list.lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

fn hold<P: Debug, E: Debug>(reports: impl Iterator<Item = Report<P, E>>) {
    let failed: Vec<Report<P, E>> = reports
        .filter(|report| !report.failures.is_empty())
        .collect();
    for report in &failed {
        eprintln!("{}\n", report.story());
    }
    let seeds: Vec<u64> = failed.iter().map(|report| report.seed).collect();
    assert!(
        seeds.is_empty(),
        "seeds {seeds:?} broke an invariant: replay one with ZECSWAP_SIM_SEEDS=<seed>, and once fixed add it to its regressions list"
    );
}

fn tell<P: Debug, E: Debug>(run: fn(u64) -> Report<P, E>) {
    for seed in seeds("ZECSWAP_SIM_SEEDS", 0..1) {
        let report = run(seed);
        println!("{}\n", report.story());
        if std::env::var("ZECSWAP_SIM_TRACE").is_ok() {
            println!("{}", report.trace.join("\n"));
        }
    }
}

fn tally(trace: &[String]) -> String {
    let count = |what: &str| trace.iter().filter(|line| line.contains(what)).count();
    format!(
        "blocks {} reorgs {} crashes {} stops {} lost-rpc {} busy {}",
        count("block "),
        count("reorg of"),
        count("wallet crashed"),
        count("wallet stopped"),
        count(" lost "),
        count("was busy"),
    )
}

fn replays<P: Debug, E: Debug>(run: fn(u64) -> Report<P, E>) {
    for seed in seeds("ZECSWAP_SIM_REPLAY", 3..6) {
        let (first, second) = (run(seed), run(seed));
        assert_eq!(first.trace, second.trace, "seed {seed} ran two ways");
        assert_eq!(
            first.failures, second.failures,
            "seed {seed} judged two ways"
        );
    }
}

#[test]
fn a_swap_with_nothing_wrong_pays_the_users_note() {
    let report = run_with(1, Plan::plain());
    assert!(report.failures.is_empty(), "{}", report.story());
    assert!(
        matches!(report.ended, Some(End::Paid(_))),
        "{}",
        report.story()
    );
}

#[test]
fn every_seed_keeps_the_invariants() {
    hold(seeds("ZECSWAP_SIM_SEEDS", 0..32).into_iter().map(run));
}

#[test]
fn seeds_that_once_failed_still_hold() {
    hold(
        regressions(include_str!("../regressions.txt"))
            .into_iter()
            .map(run),
    );
}

#[test]
#[ignore = "tells a seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim story -- --ignored --nocapture"]
fn story() {
    tell(run);
}

#[test]
#[ignore = "one line a seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim survey -- --ignored --nocapture"]
fn survey() {
    for seed in seeds("ZECSWAP_SIM_SEEDS", 0..24) {
        let report = run(seed);
        println!(
            "seed {seed}: {:?} {} {} fee {:?} -> {:?} stage {:?} | {} | {}",
            report.plan.mood,
            if report.plan.railgun {
                "railgun"
            } else {
                "account"
            },
            if report.plan.private {
                "private"
            } else {
                "public"
            },
            report.plan.fee,
            report.ended,
            report.stage,
            tally(&report.trace),
            report.failures.join("; "),
        );
    }
}

#[test]
#[ignore = "tells the story of a swap with nothing wrong"]
fn plain_story() {
    let report = run_with(1, Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_seed_replays_to_the_same_trace() {
    replays(run);
}

#[test]
fn a_reverse_swap_with_nothing_wrong_sends_the_zec() {
    let report = reverse::run_with(1, reverse::Plan::plain());
    assert!(report.failures.is_empty(), "{}", report.story());
    assert!(
        matches!(report.ended, Some(reverse::Ended::Received(_))),
        "{}",
        report.story()
    );
}

#[test]
fn every_reverse_seed_keeps_the_invariants() {
    hold(
        seeds("ZECSWAP_SIM_SEEDS", 0..32)
            .into_iter()
            .map(reverse::run),
    );
}

#[test]
fn reverse_seeds_that_once_failed_still_hold() {
    hold(
        regressions(include_str!("../reverse-regressions.txt"))
            .into_iter()
            .map(reverse::run),
    );
}

#[test]
#[ignore = "tells a reverse seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim reverse_story -- --ignored --nocapture"]
fn reverse_story() {
    tell(reverse::run);
}

#[test]
#[ignore = "one line a reverse seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim reverse_survey -- --ignored --nocapture"]
fn reverse_survey() {
    for seed in seeds("ZECSWAP_SIM_SEEDS", 0..24) {
        let report = reverse::run(seed);
        println!(
            "reverse seed {seed}: {:?} {:?} {} -> {:?} stage {:?} | {} | {}",
            report.plan.mood,
            report.plan.service,
            if report.plan.private {
                "private"
            } else {
                "public"
            },
            report.ended,
            report.stage,
            tally(&report.trace),
            report.failures.join("; "),
        );
    }
}

#[test]
#[ignore = "tells the story of a reverse swap with nothing wrong"]
fn plain_reverse_story() {
    let report = reverse::run_with(1, reverse::Plan::plain());
    println!("{}\n{}", report.story(), report.trace.join("\n"));
}

#[test]
fn a_reverse_seed_replays_to_the_same_trace() {
    replays(reverse::run);
}
