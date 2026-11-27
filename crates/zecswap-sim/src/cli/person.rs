use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use alloy::primitives::B256;
use tokio::task::{JoinError, spawn_local};
use tokio::time::{Instant, sleep, sleep_until};
use zecswap_chain::evm::Settlement;
use zecswap_chain::zcash::{Prover, Wallet};
use zecswap_cli::store::Store;
use zecswap_cli::{Session, deposit, swap};
use zecswap_client::{DepositDeployment, Depositor, MakerApi, RelayerApi, Route, User};

use super::desk::{self, Aim, Kill, STATE, Tear};
use super::{MAX_FEE, World};
use crate::panics::{Killed, Unsent};
use crate::world::{CHAIN_ID, Skewed};

const PATIENCE: u32 = 6;
const MOST_TYPINGS: u32 = 40;
const CLAIM_RELAYER_FEE: u128 = 2_000_000;

static PROVER: LazyLock<Arc<Prover>> = LazyLock::new(Arc::default);

enum Typed {
    Ended(Result<(), String>),
    Killed,
    Unsent,
}

pub struct Person {
    world: Rc<World>,
}

fn ended(outcome: Result<Result<(), String>, JoinError>) -> Typed {
    match outcome {
        Ok(result) => Typed::Ended(result),
        Err(e) if e.is_panic() => {
            let payload = e.into_panic();
            match (payload.is::<Killed>(), payload.is::<Unsent>()) {
                (true, _) => Typed::Killed,
                (_, true) => Typed::Unsent,
                _ => Typed::Ended(Err("the command panicked".into())),
            }
        }
        Err(_) => Typed::Killed,
    }
}

impl Person {
    pub fn new(world: Rc<World>) -> Self {
        Self { world }
    }

    fn note(&self, what: String) {
        self.world.chain.node.note(what);
    }

    fn session(world: &World) -> Result<Session, String> {
        let path = world.scratch.0.join("wallet.sqlite");
        let wallet = Wallet::open(path, world.network).map_err(|e| e.to_string())?;
        Ok(Session {
            network: zecswap_api::Network::Testnet,
            store: Store::fused(&world.scratch.0, world.desk.fuse()),
            wallet,
            client: world.server.connect(),
            prover: PROVER.clone(),
        })
    }

    fn user(world: &World, typing: u32) -> User {
        let node = &world.chain.node;
        let name = format!("cli-{typing}");
        let maker = MakerApi::over("http://maker.sim", world.maker_wire.clone());
        let (settlement, route) = match world.plan.railgun {
            true => (
                Settlement::over(node.rpc(&name), world.chain.contract, None),
                Route::Railgun {
                    relayer: RelayerApi::over("http://relayer.sim", world.relayer_wire.clone()),
                    max_fee: CLAIM_RELAYER_FEE,
                },
            ),
            false => (
                Settlement::over(
                    node.rpc(&name),
                    world.chain.contract,
                    Some(world.user_key.clone()),
                ),
                Route::Account,
            ),
        };
        User::new(
            &world.cli_seed,
            &world.cli_seed,
            zecswap_api::Network::Testnet,
            settlement,
            maker,
            world.chain.token,
            route,
        )
        .with_max_open_fee(world.plan.max_open_fee)
    }

    fn depositor(world: &World, typing: u32) -> Option<Depositor> {
        let depositing = world.depositing.as_ref()?;
        let node = &world.chain.node;
        let name = format!("cli-deposit-{typing}");
        Some(Depositor::new(
            &world.cli_seed,
            &world.cli_seed,
            zecswap_api::Network::Testnet,
            Settlement::over(node.rpc(&name), depositing.contract, None),
            RelayerApi::over("http://deposit-relayer.sim", depositing.wire.clone()),
            DepositDeployment {
                chain_id: CHAIN_ID,
                contract: depositing.contract,
                token: world.chain.token,
            },
            MAX_FEE,
        ))
    }

    async fn init(&self) {
        let world = &self.world;
        let path = world.scratch.0.join("seed");
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(hex::encode(world.cli_seed).as_bytes()));
        written.expect("a seed file");
        for attempt in 0u64.. {
            let made = match Self::session(world) {
                Ok(mut ctx) => ctx
                    .wallet
                    .create_account(&mut ctx.client, &world.cli_seed, "zecswap-cli")
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            match made {
                Ok(()) => break,
                Err(e) => {
                    self.note(format!("init failed: {e}"));
                    sleep(Duration::from_secs(20 + 10 * attempt.min(6))).await;
                }
            }
        }
        self.note("the wallet has its own account".into());
        world.born.set(true);
    }

    fn rest(&self, typing: u32) -> u64 {
        self.world
            .seed
            .between("cli-wait", u64::from(typing), 20, 240)
    }

    fn arm(&self, kill: Option<Kill>) -> Option<(Tear, bool)> {
        let armed = match kill?.aim {
            Aim::Tear(tear) => (tear, false),
            Aim::Unsent(step) => (Tear { step, after: false }, true),
            _ => return None,
        };
        self.world.desk.armed.borrow_mut().push(armed);
        Some(armed)
    }

    fn disarm(&self, tear: Option<(Tear, bool)>) {
        let mut armed = self.world.desk.armed.borrow_mut();
        if let Some(at) = tear.and_then(|tear| armed.iter().position(|held| *held == tear)) {
            armed.remove(at);
        }
    }

    fn stage(&self) -> Option<(u8, bool)> {
        let run: desk::Run = self.world.desk.read(STATE)?;
        let swap_id = desk::swap_id(&run)?;
        let t0 = run.opened()?.t0;
        let stage = self.world.chain.recorded(swap_id).stage;
        let late = self.world.chain.node.head().1 >= t0;
        Some((stage, desk::deposit(&run).is_some() && late))
    }

    async fn aimed(&self, kill: Option<Kill>, typed: Instant) {
        let aim = kill.map(|kill| kill.aim);
        let watch = |wanted: fn((u8, bool)) -> bool| async move {
            while !self.stage().is_some_and(wanted) {
                sleep(Duration::from_secs(5)).await;
            }
        };
        match aim {
            Some(Aim::At(after)) => sleep_until(typed + Duration::from_secs(after)).await,
            Some(Aim::Ready(after)) => {
                watch(|(stage, late)| stage == 2 || (stage == 1 && late)).await;
                sleep(Duration::from_secs(after)).await;
            }
            Some(Aim::Refunded(after)) => {
                watch(|(stage, _)| stage == 4).await;
                sleep(Duration::from_secs(after)).await;
            }
            Some(Aim::Tear(_) | Aim::Unsent(_)) | None => std::future::pending().await,
        }
    }

    async fn typed(&self, kill: Option<Kill>, what: &'static str) -> Typed {
        let world = self.world.clone();
        let typing = world.typed.get() + 1;
        world.typed.set(typing);
        self.note(format!("the person types `{what}` (typing {typing})"));
        let tear = self.arm(kill);
        let skew = world.plan.skew;
        let units = world.plan.units;
        let command = {
            let world = world.clone();
            async move {
                let ran: anyhow::Result<()> = match what {
                    "deposit" => {
                        let store = Store::fused(&world.scratch.0, world.desk.fuse());
                        let depositor = Self::depositor(&world, typing).expect("a deposits world");
                        deposit::drive(&store, &depositor).await
                    }
                    _ => match Self::session(&world) {
                        Ok(mut ctx) => {
                            let user = Self::user(&world, typing);
                            swap::drive(&mut ctx, &user, units).await
                        }
                        Err(e) => Err(anyhow::Error::msg(e)),
                    },
                };
                ran.map_err(|e| format!("{e:#}"))
            }
        };
        let mut task = spawn_local(Skewed::new(skew, command));
        let aimed = self.aimed(kill, Instant::now());
        tokio::pin!(aimed);
        let typed = tokio::select! {
            biased;
            outcome = &mut task => ended(outcome),
            () = &mut aimed => {
                task.abort();
                let _ = task.await;
                let how = match kill.map(|kill| kill.aim) {
                    Some(Aim::Ready(_)) => "during the claim",
                    Some(Aim::Refunded(_)) => "during the sweep back",
                    _ => "by the clock",
                };
                self.note(format!("the cli was killed {how}"));
                Typed::Killed
            }
        };
        self.disarm(tear);
        match &typed {
            Typed::Ended(Ok(())) => self.note(format!("`{what}` ended")),
            Typed::Ended(Err(e)) => self.note(format!("`{what}` failed: {}", brief(e))),
            Typed::Killed => world.killed.set(world.killed.get() + 1),
            Typed::Unsent => self.note(format!("`{what}` failed: a broadcast was refused")),
        }
        typed
    }

    fn finished_quote(&self) -> Option<B256> {
        let run: desk::Run = self.world.desk.read(STATE)?;
        desk::finished(&run).then(|| desk::quote_id(&run)).flatten()
    }

    pub async fn swaps(self) {
        self.init().await;
        let world = self.world.clone();
        sleep_until(world.start + Duration::from_secs(world.plan.typed_at())).await;
        if world.plan.stale {
            let stale = br#"{"version": 1, "swap": "an older format"}"#;
            std::fs::write(world.scratch.0.join(STATE), stale).expect("a stale swap file");
            self.note("a swap.json of another version waits".into());
        }
        let mut kills = world.plan.kills.clone();
        let mut done = BTreeSet::new();
        for swap in 0..world.plan.swaps {
            let mut fruitless = 0;
            for _ in 0..MOST_TYPINGS {
                let kill = kills.first().copied();
                let typed = self.typed(kill, "swap").await;
                let rest = match typed {
                    Typed::Killed => {
                        kills.remove(0);
                        kill.map_or(0, |kill| kill.rest)
                    }
                    Typed::Unsent => {
                        kills.remove(0);
                        self.rest(world.typed.get())
                    }
                    Typed::Ended(Err(e)) if e.contains("another version's") => {
                        let finished = world.scratch.0.join(format!("{STATE}.finished-{swap}"));
                        let _ = std::fs::rename(world.scratch.0.join(STATE), finished);
                        self.note("the person renames the other version's swap.json".into());
                        5
                    }
                    Typed::Ended(_) => self.rest(world.typed.get()),
                };
                if let Some(quote) = self.finished_quote()
                    && done.insert(quote)
                {
                    break;
                }
                let open = self.world.desk.read::<desk::Run>(STATE).is_some_and(|run| {
                    let quote = desk::quote_id(&run);
                    !desk::finished(&run) && quote.is_none_or(|quote| !done.contains(&quote))
                });
                fruitless = match open {
                    true => 0,
                    false => fruitless + 1,
                };
                if fruitless >= PATIENCE {
                    self.note("the person gives up on this swap".into());
                    break;
                }
                sleep(Duration::from_secs(rest)).await;
            }
            let between = world.seed.between("cli-between", u64::from(swap), 30, 600);
            sleep(Duration::from_secs(between)).await;
        }
        self.note("the person is done swapping".into());
        world.settles.set(world.settles.get() + 1);
    }

    pub async fn deposits(self) {
        let world = self.world.clone();
        let Some(depositing) = &world.depositing else {
            return;
        };
        while !world.born.get() {
            sleep(Duration::from_secs(1)).await;
        }
        let mut kills = depositing.plan.kills.clone();
        let mut typed = depositing.plan.typed.clone();
        typed.sort_unstable();
        for at in typed {
            sleep_until(world.start + Duration::from_secs(at)).await;
            loop {
                let kill = kills.first().copied();
                match self.typed(kill, "deposit").await {
                    Typed::Killed => {
                        kills.remove(0);
                        sleep(Duration::from_secs(kill.map_or(0, |kill| kill.rest))).await;
                    }
                    Typed::Ended(_) | Typed::Unsent => break,
                }
            }
        }
        world.settles.set(world.settles.get() + 1);
    }

    pub async fn last_deposit(self) -> Result<(), String> {
        let mut last = Err("never typed".into());
        for _ in 0..3 {
            last = match self.typed(None, "deposit").await {
                Typed::Ended(result) => result,
                Typed::Killed | Typed::Unsent => Err("killed".into()),
            };
            if last.is_ok() {
                break;
            }
            sleep(Duration::from_secs(60)).await;
        }
        last
    }
}

fn brief(error: &str) -> String {
    let error = error.split(", source: ").next().unwrap_or(error);
    let words = error.split_whitespace().filter(|word| {
        let digits = word.chars().filter(char::is_ascii_hexdigit).count();
        word.len() < 24 || digits * 2 < word.len()
    });
    let words: Vec<&str> = words.take(24).collect();
    words.join(" ")
}
