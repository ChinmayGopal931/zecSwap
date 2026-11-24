use zecswap_sim::{End, Plan, Report, run, run_with};

fn seeds(var: &str, default: std::ops::Range<u64>) -> Vec<u64> {
    match std::env::var(var) {
        Ok(spec) => match spec.split_once("..") {
            Some((from, to)) => (from.parse().unwrap()..to.parse().unwrap()).collect(),
            None => spec.split(',').map(|seed| seed.parse().unwrap()).collect(),
        },
        Err(_) => default.collect(),
    }
}

fn regressions() -> Vec<u64> {
    include_str!("../regressions.txt")
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse().ok())
        .collect()
}

fn hold(reports: impl Iterator<Item = Report>) {
    let failed: Vec<Report> = reports
        .filter(|report| !report.failures.is_empty())
        .collect();
    for report in &failed {
        eprintln!("{}\n", report.story());
    }
    let seeds: Vec<u64> = failed.iter().map(|report| report.seed).collect();
    assert!(
        seeds.is_empty(),
        "seeds {seeds:?} broke an invariant: replay one with ZECSWAP_SIM_SEEDS=<seed>, and once fixed add it to regressions.txt"
    );
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
    hold(regressions().into_iter().map(run));
}

#[test]
#[ignore = "tells a seed's story: ZECSWAP_SIM_SEEDS=<seed> cargo test -p zecswap-sim story -- --ignored --nocapture"]
fn story() {
    for seed in seeds("ZECSWAP_SIM_SEEDS", 0..1) {
        let report = run(seed);
        println!("{}\n", report.story());
        if std::env::var("ZECSWAP_SIM_TRACE").is_ok() {
            println!("{}", report.trace.join("\n"));
        }
    }
}

#[test]
#[ignore = "one line a seed: ZECSWAP_SIM_SEEDS=0..200 cargo test -p zecswap-sim survey -- --ignored --nocapture"]
fn survey() {
    for seed in seeds("ZECSWAP_SIM_SEEDS", 0..24) {
        let report = run(seed);
        let count = |what: &str| {
            report
                .trace
                .iter()
                .filter(|line| line.contains(what))
                .count()
        };
        println!(
            "seed {seed}: {:?} {} {} fee {:?} -> {:?} stage {:?} | blocks {} reorgs {} crashes {} stops {} lost-rpc {} busy {} | {}",
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
            count("block "),
            count("reorg of"),
            count("wallet crashed"),
            count("wallet stopped"),
            count(" lost "),
            count("was busy"),
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
    for seed in seeds("ZECSWAP_SIM_REPLAY", 3..6) {
        let (first, second) = (run(seed), run(seed));
        assert_eq!(first.trace, second.trace, "seed {seed} ran two ways");
        assert_eq!(
            first.failures, second.failures,
            "seed {seed} judged two ways"
        );
    }
}
