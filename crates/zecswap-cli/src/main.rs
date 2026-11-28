use std::path::PathBuf;
use std::sync::Arc;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use zcash_address::ZcashAddress;
use zecswap_api::Network;
use zecswap_chain::zcash::{Prover, Wallet, connect};
use zecswap_cli::store::Store;
use zecswap_cli::{Session, deposit, swap};

#[derive(Parser)]
#[command(about = "Test client for ZecSwap: a seed wallet plus the user side of a swap")]
struct Cli {
    #[arg(long, default_value_t = Network::Testnet, global = true)]
    network: Network,
    /// The network's own (testnet.zec.rocks or zec.rocks) unless given.
    #[arg(long, env = "LIGHTWALLETD_URL", global = true)]
    lightwalletd: Option<String>,
    #[arg(
        long,
        env = "ZECSWAP_CLI_DIR",
        default_value = "zecswap-cli-data",
        global = true
    )]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Creates the wallet with a fresh seed and one account born at the chain tip.
    Init,
    /// Syncs, then prints the account's address and balance.
    Status,
    /// Pays `zatoshis` to `address`.
    Send { address: String, zatoshis: u64 },
    /// Pays each `address=zatoshis` in one transaction, so a single note funds them all.
    SendMany {
        #[arg(required = true)]
        payments: Vec<String>,
    },
    /// Runs the user side of a swap against a maker, resuming an unfinished one. It pays the
    /// account whose key is in `USER_PRIVATE_KEY`, or with `--relayer`, this seed's Railgun
    /// wallet.
    Swap {
        #[arg(
            long,
            env = "ZECSWAP_MAKER_URL",
            default_value = "http://127.0.0.1:8787"
        )]
        maker: String,
        /// The settlement chain's RPC.
        #[arg(long, env = "EVM_RPC_URL")]
        rpc: String,
        #[arg(long, env = "ZECSWAP_CONTRACT")]
        contract: Address,
        #[arg(long, env = "ZECSWAP_TOKEN")]
        token: Address,
        #[arg(long, default_value_t = 1)]
        units: u32,
        /// Pay into Railgun, with this relayer sending the transactions.
        #[arg(long, env = "ZECSWAP_RELAYER_URL")]
        relayer: Option<String>,
        /// The most the relayer may keep, in token base units.
        #[arg(long, default_value_t = 2_000_000)]
        max_fee: u128,
        /// The most the maker may ask before it opens the swap, in zatoshis: its gas for the
        /// open, kept whether or not a deposit follows. A quote asking more is refused.
        #[arg(long, default_value_t = 500_000)]
        max_open_fee: u64,
    },
    /// Shields this seed's deposit addresses' tokens into its Railgun wallet through a relayer,
    /// resuming an unfinished shield, then shows the address to deposit to next.
    Deposit {
        #[arg(long, env = "ZECSWAP_RELAYER_URL")]
        relayer: String,
        /// The chain's RPC.
        #[arg(long, env = "EVM_RPC_URL")]
        rpc: String,
        /// The Deposits contract.
        #[arg(long, env = "ZECSWAP_DEPOSITS")]
        contract: Address,
        #[arg(long, env = "ZECSWAP_TOKEN")]
        token: Address,
        /// The most the relayer may keep, in token base units, before its percentage.
        #[arg(long, default_value_t = 1_000_000)]
        max_fee: u128,
        /// The most the relayer may keep of a deposit besides, in basis points.
        #[arg(long, default_value_t = 50)]
        max_fee_bps: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let network = cli.network;
    std::fs::create_dir_all(&cli.data_dir)?;
    if let Command::Deposit {
        relayer,
        rpc,
        contract,
        token,
        max_fee,
        max_fee_bps,
    } = cli.command
    {
        let args = deposit::DepositArgs {
            relayer,
            rpc,
            contract,
            token,
            max_fee,
            max_fee_bps,
        };
        return deposit::run(&Store::new(&cli.data_dir), network, args).await;
    }
    let lightwalletd = cli.lightwalletd.as_deref();
    let mut ctx = Session {
        network,
        store: Store::new(&cli.data_dir),
        wallet: Wallet::open(cli.data_dir.join("wallet.sqlite"), network.zcash())?,
        client: connect(lightwalletd.unwrap_or(network.lightwalletd()))
            .await
            .context("connecting to lightwalletd")?,
        prover: Arc::new(Prover::default()),
    };

    match cli.command {
        Command::Init => init(&mut ctx).await,
        Command::Status => status(&mut ctx).await,
        Command::Send { address, zatoshis } => send(&mut ctx, &address, zatoshis).await,
        Command::SendMany { payments } => send_many(&mut ctx, &payments).await,
        Command::Swap {
            maker,
            rpc,
            contract,
            token,
            units,
            relayer,
            max_fee,
            max_open_fee,
        } => {
            let payee = match relayer {
                Some(relayer) => swap::Payee::Railgun { relayer, max_fee },
                None => swap::Payee::Account(
                    std::env::var("USER_PRIVATE_KEY")
                        .context("USER_PRIVATE_KEY is not set")?
                        .parse()
                        .context("USER_PRIVATE_KEY")?,
                ),
            };
            let args = swap::SwapArgs {
                maker,
                rpc,
                contract,
                token,
                units,
                payee,
                max_open_fee,
            };
            swap::run(&mut ctx, args).await
        }
        Command::Deposit { .. } => unreachable!(),
    }
}

async fn init(ctx: &mut Session) -> Result<()> {
    let seed = ctx.store.create_seed()?;
    let (account, _) = ctx
        .wallet
        .create_account(&mut ctx.client, &seed, "zecswap-cli")
        .await?;
    println!("address: {}", ctx.wallet.address(account)?);
    Ok(())
}

async fn status(ctx: &mut Session) -> Result<()> {
    ctx.sync().await?;
    let account = ctx.account()?;
    let funds = ctx.wallet.funds(account)?;
    println!("address:   {}", ctx.wallet.address(account)?);
    println!("total:     {} zat", funds.total);
    println!("spendable: {} zat", funds.spendable);
    Ok(())
}

async fn send(ctx: &mut Session, address: &str, zatoshis: u64) -> Result<()> {
    let to: ZcashAddress = address.parse().context("parsing the recipient")?;
    ctx.sync().await?;
    let txid = ctx.pay(&to, zatoshis)?;
    ctx.broadcast(txid).await?;
    println!("sent: {txid}");
    Ok(())
}

async fn send_many(ctx: &mut Session, payments: &[String]) -> Result<()> {
    let payments = payments
        .iter()
        .map(|payment| {
            let (address, zatoshis) = payment
                .split_once('=')
                .with_context(|| format!("{payment} is not address=zatoshis"))?;
            let to: ZcashAddress = address.parse().context("parsing a recipient")?;
            Ok((to, zatoshis.parse().context("parsing zatoshis")?))
        })
        .collect::<Result<Vec<_>>>()?;
    ctx.sync().await?;
    let txid = ctx.pay_all(&payments)?;
    ctx.broadcast(txid).await?;
    println!("sent: {txid}");
    Ok(())
}
