use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Display, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use alloy::primitives::keccak256;
use serde::{Deserialize, Serialize};

use crate::run::Report;
use crate::{End, Plan, deposits, lightwallet, reach, reverse};

const TOLD_EVERY: Duration = Duration::from_secs(5);
const SHOWN: usize = 12;
const ALSO_SHOWN: usize = 15;

type Failing<'a> = (String, Vec<&'a Outcome>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Forward,
    Reverse,
    Deposits,
    Zcash,
}

impl Kind {
    pub const ALL: [Self; 4] = [Self::Forward, Self::Reverse, Self::Deposits, Self::Zcash];

    pub fn name(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Reverse => "reverse",
            Self::Deposits => "deposits",
            Self::Zcash => "zcash",
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    pub fn told(self) -> &'static str {
        match self {
            Self::Forward => "seed",
            Self::Reverse => "reverse seed",
            Self::Deposits => "deposit seed",
            Self::Zcash => "zcash seed",
        }
    }

    pub fn replay(self, seed: u64) -> String {
        let story = match self {
            Self::Forward => "story",
            Self::Reverse => "reverse_story",
            Self::Deposits => "deposit_story",
            Self::Zcash => "zcash_story",
        };
        format!(
            "ZECSWAP_SIM_SEEDS={seed} ZECSWAP_SIM_TRACE=1 cargo test -p zecswap-sim {story} -- --exact --ignored --nocapture"
        )
    }

    pub fn run(self, seed: u64) -> Outcome {
        match self {
            Self::Forward => Outcome::of(&crate::run(seed)),
            Self::Reverse => Outcome::of(&reverse::run(seed)),
            Self::Deposits => Outcome::of(&deposits::run(seed)),
            Self::Zcash => Outcome::of(&lightwallet::run(seed)),
        }
    }

    pub fn reachable(self) -> Vec<String> {
        match self {
            Self::Forward => reach::forward_all(),
            Self::Reverse => reach::reverse_all(),
            Self::Deposits => reach::deposits_all(),
            Self::Zcash => reach::zcash_all(),
        }
    }
}

pub trait Told {
    const KIND: Kind;

    fn line(&self) -> String;

    fn reached(&self) -> (Vec<u16>, Vec<String>);
}

fn tally(trace: &[String]) -> String {
    let count = |what: &str| trace.iter().filter(|line| line.contains(what)).count();
    let alerts = match count("alert from ") {
        0 => String::new(),
        alerts => format!(" alerts {alerts}"),
    };
    format!(
        "blocks {} reorgs {} crashes {} stops {} lost-rpc {} busy {}{alerts}",
        count("block "),
        count("reorg of"),
        count("wallet crashed"),
        count("wallet stopped"),
        count(" lost "),
        count("was busy"),
    )
}

impl Told for Report<Plan, End> {
    const KIND: Kind = Kind::Forward;

    fn line(&self) -> String {
        format!(
            "seed {}: {:?} {} {} fee {:?} -> {:?} stage {:?} | {} | {}",
            self.seed,
            self.plan.mood,
            if self.plan.railgun {
                "railgun"
            } else {
                "account"
            },
            if self.plan.private {
                "private"
            } else {
                "public"
            },
            self.plan.fee,
            self.ended,
            self.stage,
            tally(&self.trace),
            self.failures.join("; "),
        )
    }

    fn reached(&self) -> (Vec<u16>, Vec<String>) {
        reach::forward(self)
    }
}

impl Told for Report<reverse::Plan, reverse::Ended> {
    const KIND: Kind = Kind::Reverse;

    fn line(&self) -> String {
        format!(
            "reverse seed {}: {:?} {:?} {} -> {:?} stage {:?} | {} | {}",
            self.seed,
            self.plan.mood,
            self.plan.service,
            if self.plan.private {
                "private"
            } else {
                "public"
            },
            self.ended,
            self.stage,
            tally(&self.trace),
            self.failures.join("; "),
        )
    }

    fn reached(&self) -> (Vec<u16>, Vec<String>) {
        reach::reverse(self)
    }
}

impl Told for Report<deposits::Plan, deposits::Ended> {
    const KIND: Kind = Kind::Deposits;

    fn line(&self) -> String {
        format!(
            "deposit seed {}: {:?}{} {} arrivals, {} in 1000 returned, {:?} -> {:?} | {} | {}",
            self.seed,
            self.plan.service,
            match self.plan.fees_stay {
                true => " while fees stay",
                false => "",
            },
            self.plan.arrivals.len(),
            self.plan.returned,
            self.plan.payout,
            self.ended,
            tally(&self.trace),
            self.failures.join("; "),
        )
    }

    fn reached(&self) -> (Vec<u16>, Vec<String>) {
        reach::deposits(self)
    }
}

impl Told for Report<lightwallet::Plan, lightwallet::Ended> {
    const KIND: Kind = Kind::Zcash;

    fn line(&self) -> String {
        format!(
            "zcash seed {}: {} -> {:?} | {} | {}",
            self.seed,
            self.plan.brief(),
            self.ended,
            tally(&self.trace),
            self.failures.join("; "),
        )
    }

    fn reached(&self) -> (Vec<u16>, Vec<String>) {
        reach::zcash(self)
    }
}

impl<P: Debug, E: Debug> Report<P, E> {
    pub fn digest(&self) -> String {
        let mut text = format!(
            "plan {:?}\nended {:?} stage {:?}\n",
            self.plan, self.ended, self.stage
        );
        for (what, lines) in [("trace", &self.trace), ("failures", &self.failures)] {
            let _ = writeln!(text, "{what} {}", lines.len());
            for line in lines {
                let _ = writeln!(text, "{line}");
            }
        }
        let hash = keccak256(text.as_bytes());
        format!(
            "{} trace {} failures {}",
            &hash.to_string()[2..34],
            self.trace.len(),
            self.failures.len()
        )
    }
}

pub fn threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        let asked = std::env::var("ZECSWAP_SIM_THREADS").ok();
        let asked = asked.and_then(|threads| threads.parse().ok());
        let cores = std::thread::available_parallelism().map_or(4, usize::from);
        asked.filter(|threads| *threads > 0).unwrap_or(cores)
    })
}

struct Seats {
    free: Mutex<usize>,
    freed: Condvar,
}

struct Seat(&'static Seats);

impl Drop for Seat {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.freed.notify_one();
    }
}

fn seat() -> Seat {
    static SEATS: OnceLock<Seats> = OnceLock::new();
    let seats = SEATS.get_or_init(|| Seats {
        free: Mutex::new(threads()),
        freed: Condvar::new(),
    });
    let mut free = seats.free.lock().unwrap();
    while *free == 0 {
        free = seats.freed.wait(free).unwrap();
    }
    *free -= 1;
    Seat(seats)
}

pub fn spread<T: Send>(seeds: &[u64], run: impl Fn(u64) -> T + Sync, mut each: impl FnMut(T)) {
    let seated = |seed: u64| {
        let _seat = seat();
        run(seed)
    };
    let workers = threads().min(seeds.len());
    if workers <= 1 {
        return seeds.iter().for_each(|seed| each(seated(*seed)));
    }
    let next = AtomicUsize::new(0);
    let (done, results) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (done, next, seated) = (done.clone(), &next, &seated);
            scope.spawn(move || {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(seed) = seeds.get(at) else { break };
                    if done.send((at, seated(*seed))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(done);
        let (mut early, mut due) = (BTreeMap::new(), 0);
        for (at, ran) in results {
            early.insert(at, ran);
            while let Some(ran) = early.remove(&due) {
                each(ran);
                due += 1;
            }
        }
    });
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub kind: Kind,
    pub seed: u64,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub line: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    pub reached: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub said: Vec<String>,
}

impl Outcome {
    fn of<P: Debug, E: Debug>(report: &Report<P, E>) -> Self
    where
        Report<P, E>: Told,
    {
        let (line, plan) = match report.failures.is_empty() {
            true => Default::default(),
            false => (report.line(), format!("{:?}", report.plan)),
        };
        let (reached, said) = report.reached();
        Self {
            kind: <Report<P, E>>::KIND,
            seed: report.seed,
            digest: report.digest(),
            failures: report.failures.clone(),
            line,
            plan,
            reached,
            said,
        }
    }
}

pub fn signature(failure: &str) -> String {
    let here = format!("{}/", workspace().display());
    let failure = failure.replace(&here, "");
    let mut out = String::new();
    let mut rest = failure.as_str();
    while !rest.is_empty() {
        let wordy = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let end = rest.find(|c| !wordy(c)).unwrap_or(rest.len());
        if end == 0 {
            let other = rest.chars().next().expect("a character");
            out.push(other);
            rest = &rest[other.len_utf8()..];
            continue;
        }
        let word = &rest[..end];
        rest = &rest[end..];
        let hex =
            |digits: &str| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit());
        let numbered = word.bytes().any(|b| b.is_ascii_digit());
        let named = word.starts_with(|c: char| c.is_ascii_alphabetic()) && word.len() <= 3;
        if !numbered || named {
            out.push_str(word);
        } else if word.strip_prefix("0x").is_some_and(hex) || (word.len() >= 8 && hex(word)) {
            out.push('#');
        } else {
            let mut last = ' ';
            for c in word.chars() {
                let c = if c.is_ascii_digit() { '#' } else { c };
                if c != '#' || last != '#' {
                    out.push(c);
                }
                last = c;
            }
        }
    }
    while let Some(at) = out.find("#, #") {
        out.replace_range(at..at + 4, "#");
    }
    out
}

fn named(signature: &str) -> String {
    keccak256(signature.as_bytes()).to_string()[2..10].to_owned()
}

#[derive(Clone, Debug)]
pub struct Tree {
    pub commit: String,
    pub dirty: Option<String>,
}

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let asked = Command::new("git").arg("-C").arg(root).args(args).output();
    asked
        .ok()
        .filter(|out| out.status.success())
        .map(|out| out.stdout)
}

impl Tree {
    pub fn read(root: &Path) -> Self {
        let commit = git(root, &["rev-parse", "HEAD"]);
        let commit = commit.map(|out| String::from_utf8_lossy(&out).trim().to_owned());
        let changed = git(root, &["status", "--porcelain", "--untracked-files=all"]);
        let changed = changed.unwrap_or_default();
        let dirty = (!changed.is_empty()).then(|| {
            let mut all = changed.clone();
            all.extend(git(root, &["diff", "HEAD"]).unwrap_or_default());
            for line in String::from_utf8_lossy(&changed).lines() {
                if let Some(new) = line.strip_prefix("?? ") {
                    all.extend(fs::read(root.join(new)).unwrap_or_default());
                }
            }
            keccak256(&all).to_string()[2..10].to_owned()
        });
        Self {
            commit: commit.unwrap_or_else(|| "unknown".into()),
            dirty,
        }
    }

    pub fn name(&self) -> String {
        let commit = &self.commit[..self.commit.len().min(12)];
        match &self.dirty {
            Some(changes) => format!("{commit}-dirty-{changes}"),
            None => commit.to_owned(),
        }
    }
}

impl Display for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.dirty {
            Some(changes) => write!(f, "{}, dirty (changes {changes})", self.commit),
            None => write!(f, "{}, clean", self.commit),
        }
    }
}

pub fn workspace() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        root.canonicalize().unwrap_or(root)
    })
}

pub fn folder() -> PathBuf {
    match std::env::var_os("ZECSWAP_SIM_HUNTS") {
        Some(folder) => PathBuf::from(folder),
        None => workspace().join("docs/local/sim-hunts"),
    }
}

pub struct Hunt {
    pub runs: Vec<(Kind, Vec<u64>)>,
    pub folder: PathBuf,
}

struct Journal {
    file: File,
    held: BTreeMap<(Kind, u64), Outcome>,
}

impl Journal {
    fn open(path: &Path) -> std::io::Result<Self> {
        let mut held = BTreeMap::new();
        if let Ok(earlier) = File::open(path) {
            for line in BufReader::new(earlier).lines() {
                if let Ok(outcome) = serde_json::from_str::<Outcome>(&line?) {
                    held.insert((outcome.kind, outcome.seed), outcome);
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file, held })
    }

    fn keep(&mut self, outcome: Outcome) -> std::io::Result<()> {
        let mut line = serde_json::to_string(&outcome).expect("an outcome in JSON");
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.held.insert((outcome.kind, outcome.seed), outcome);
        Ok(())
    }
}

struct Pace {
    kind: Kind,
    ran: usize,
    all: usize,
    failing: usize,
    started: Instant,
    told: Instant,
}

impl Pace {
    fn rate(&self) -> f64 {
        self.ran as f64 / self.started.elapsed().as_secs_f64().max(0.001)
    }

    fn tell(&mut self) {
        self.told = Instant::now();
        eprintln!(
            "hunt {}: {} of {} seeds, {:.1} a second, {} failing",
            self.kind.name(),
            self.ran,
            self.all,
            self.rate(),
            self.failing
        );
    }
}

pub struct Summary {
    pub text: String,
    pub failing: usize,
    pub unnamed: usize,
}

impl Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn labels(folder: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(folder.join("labels.txt")).unwrap_or_default();
    let labelled = text.lines().filter_map(|line| {
        let (name, label) = line.trim().split_once(char::is_whitespace)?;
        Some((name.to_owned(), label.trim().to_owned()))
    });
    labelled.collect()
}

pub fn hunt(asked: &Hunt) -> std::io::Result<Summary> {
    let tree = Tree::read(workspace());
    let home = asked.folder.join(tree.name());
    fs::create_dir_all(&home)?;
    fs::write(home.join("tree.txt"), format!("{tree}\n"))?;
    let mut journal = Journal::open(&home.join("seeds.jsonl"))?;
    let mut text = format!(
        "hunt on tree {tree}, kept in {}\n{} threads\n",
        home.display(),
        threads()
    );
    let mut every = String::new();
    for (kind, seeds) in &asked.runs {
        let fresh = |seed: &&u64| !journal.held.contains_key(&(*kind, **seed));
        let todo: Vec<u64> = seeds.iter().filter(fresh).copied().collect();
        let mut pace = Pace {
            kind: *kind,
            ran: 0,
            all: todo.len(),
            failing: 0,
            started: Instant::now(),
            told: Instant::now(),
        };
        let mut lost = Ok(());
        spread(
            &todo,
            |seed| kind.run(seed),
            |outcome| {
                pace.ran += 1;
                if !outcome.failures.is_empty() {
                    pace.failing += 1;
                    eprintln!("hunt {}: {}", kind.name(), outcome.line);
                }
                if let Err(e) = journal.keep(outcome) {
                    lost = Err(e);
                }
                if pace.told.elapsed() >= TOLD_EVERY {
                    pace.tell();
                }
            },
        );
        lost?;
        pace.tell();
        let _ = writeln!(
            text,
            "\n{}: {} seeds asked, {} hunted before on this tree, {} run now in {:.1} s ({:.1} a second)",
            kind.name(),
            seeds.len(),
            seeds.len() - todo.len(),
            todo.len(),
            pace.started.elapsed().as_secs_f64(),
            pace.rate(),
        );
        let hunted = seeds.iter();
        let hunted: Vec<&Outcome> = hunted
            .filter_map(|seed| journal.held.get(&(*kind, *seed)))
            .collect();
        let (reached, also) = reached(*kind, &hunted);
        text.push_str(&reached);
        let _ = writeln!(every, "{}\n{reached}{also}", kind.name());
    }
    let hunted = asked.runs.iter().flat_map(|(kind, seeds)| {
        let (kind, held) = (*kind, &journal.held);
        seeds
            .iter()
            .filter_map(move |seed| held.get(&(kind, *seed)))
    });
    let failed: Vec<&Outcome> = hunted
        .filter(|outcome| !outcome.failures.is_empty())
        .collect();
    let labels = labels(&asked.folder);
    let (told, unnamed) = failures(&failed, &labels);
    text.push_str(&told);
    fs::write(home.join("summary.txt"), &text)?;
    fs::write(home.join("reach.txt"), every)?;
    fs::write(home.join("failures.txt"), stories(&failed))?;
    Ok(Summary {
        text,
        failing: failed.len(),
        unnamed,
    })
}

fn reached(kind: Kind, hunted: &[&Outcome]) -> (String, String) {
    let known = kind.reachable();
    let mut met = vec![0; known.len()];
    let mut said: BTreeMap<&str, usize> = BTreeMap::new();
    for outcome in hunted {
        for index in &outcome.reached {
            if let Some(seeds) = met.get_mut(usize::from(*index)) {
                *seeds += 1;
            }
        }
        for what in &outcome.said {
            *said.entry(what.as_str()).or_default() += 1;
        }
    }
    let never = known.iter().zip(&met).filter(|(_, seeds)| **seeds == 0);
    let never: Vec<&str> = never.map(|(tag, _)| tag.as_str()).collect();
    let failing = hunted.iter().filter(|outcome| !outcome.failures.is_empty());
    let mut text = format!(
        "  {} failing; {} of {} situations reached\n",
        failing.count(),
        known.len() - never.len(),
        known.len()
    );
    if !never.is_empty() {
        let _ = writeln!(text, "  NEVER REACHED: {}", never.join("; "));
    }
    for (tag, seeds) in known.iter().zip(&met) {
        let _ = writeln!(text, "  {seeds:>7}  {tag}");
    }
    let mut said: Vec<(&str, usize)> = said.into_iter().collect();
    said.sort_by_key(|(what, seeds)| (std::cmp::Reverse(*seeds), *what));
    let mut also = String::new();
    for (what, seeds) in &said {
        let _ = writeln!(also, "  {seeds:>7}  {what}");
    }
    let shown = said.iter().take(ALSO_SHOWN);
    let shown: Vec<String> = shown
        .map(|(what, seeds)| format!("{seeds} {what}"))
        .collect();
    if !said.is_empty() {
        let _ = writeln!(
            text,
            "  and {} kinds of refusal said (all in reach.txt), the commonest: {}",
            said.len(),
            shown.join("; ")
        );
    }
    (text, also)
}

fn failures(failed: &[&Outcome], labels: &BTreeMap<String, String>) -> (String, usize) {
    let mut kinds: BTreeMap<String, Failing> = BTreeMap::new();
    for outcome in failed {
        let signatures = outcome.failures.iter().map(|failure| signature(failure));
        let signatures: BTreeSet<String> = signatures.collect();
        for signature in signatures {
            let kind = kinds.entry(named(&signature)).or_default();
            kind.0 = signature;
            kind.1.push(*outcome);
        }
    }
    let mut text = format!(
        "\nfailing seeds: {}, failing in {} ways\n",
        failed.len(),
        kinds.len()
    );
    let mut unnamed = 0;
    let mut ordered: Vec<(&String, &Failing)> = kinds.iter().collect();
    ordered.sort_by_key(|(name, (_, seeds))| (std::cmp::Reverse(seeds.len()), *name));
    for (name, (signature, seeds)) in ordered {
        let label = match labels.get(name) {
            Some(label) => format!("known: {label}"),
            None => {
                unnamed += 1;
                "NEW".to_owned()
            }
        };
        let some = seeds.iter().take(SHOWN);
        let some: Vec<String> = some
            .map(|outcome| format!("{} {}", outcome.kind.name(), outcome.seed))
            .collect();
        let more = match seeds.len() > SHOWN {
            true => ", ...",
            false => "",
        };
        let first = seeds[0];
        let _ = writeln!(
            text,
            "  [{name}] {label}: {} failing ({}{more})\n      {signature}\n      {}",
            seeds.len(),
            some.join(", "),
            first.kind.replay(first.seed),
        );
    }
    (text, unnamed)
}

fn stories(failed: &[&Outcome]) -> String {
    let mut text = String::new();
    for outcome in failed {
        let _ = writeln!(
            text,
            "{} seed {}\n  {}\n  plan {}",
            outcome.kind.name(),
            outcome.seed,
            outcome.line,
            outcome.plan
        );
        for failure in &outcome.failures {
            let _ = writeln!(text, "  [{}] {failure}", named(&signature(failure)));
        }
        let _ = writeln!(text, "  {}\n", outcome.kind.replay(outcome.seed));
    }
    text
}
