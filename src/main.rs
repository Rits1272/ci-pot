//! ci-pot: a funding pot for CI on Nostr.
//!
//! Anyone pays sats into a repository's pot. The coordinator serves the
//! repository while the pot is funded, and each finished job is charged to it.
//!
//! - [`job`] reads a Job Result; [`relay`] and [`store`] fetch and keep them.
//! - [`price`] says what a job costs.
//! - [`ledger`] records every deposit and every charge.
//! - [`cashier`] redeems and holds the Cashu ecash.
//! - [`admit`] writes the list of funded repositories for the coordinator.
//! - [`pot`] ties those into deposit, settle and admit; [`server`] is its HTTP side.

mod admit;
mod cashier;
mod job;
mod ledger;
mod pot;
mod price;
mod relay;
mod server;
mod store;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use nostr::prelude::*;

use cashier::Cashier;
use ledger::Ledger;
use price::Price;
use store::Store;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Where the ledger, the wallet and fetched events are kept.
    #[arg(
        long,
        global = true,
        env = "CI_POT_DATA_DIR",
        default_value = "ci-pot-data"
    )]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the pot: take deposits, charge finished jobs, keep the admission list current.
    Serve(ServeArgs),
    /// Show every pot and its balance.
    Status,
    /// See what the wallet holds, or withdraw what has been earned.
    Wallet(WalletArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Your coordinator's public key (npub or hex). Only its jobs are charged.
    #[arg(long, env = "CI_POT_COORDINATOR")]
    coordinator: String,
    /// A compute provider your coordinator uses (npub or hex). Repeat for each.
    /// The coordinator's own key is always included.
    #[arg(long = "provider", env = "CI_POT_PROVIDERS", value_delimiter = ',')]
    providers: Vec<String>,
    /// Cashu mint to accept payment from. Repeat for more than one.
    #[arg(
        long = "mint",
        env = "CI_POT_MINTS",
        value_delimiter = ',',
        required = true
    )]
    mints: Vec<String>,
    /// Price of one vCPU for one hour, in sats.
    #[arg(long, env = "CI_POT_SAT_PER_VCPU_HOUR", default_value_t = 50)]
    sat_per_vcpu_hour: u64,
    /// vCPUs charged for a job that does not declare any.
    #[arg(long, env = "CI_POT_DEFAULT_VCPUS", default_value_t = 4)]
    default_vcpus: u64,
    /// A repository is served while its pot holds more than this many sats.
    #[arg(long, env = "CI_POT_MIN_BALANCE_SAT", default_value_t = 0)]
    min_balance_sat: u64,
    /// A repository to serve whatever its pot holds, as `npub1.../name`. Repeat for each.
    #[arg(long = "always", env = "CI_POT_ALWAYS", value_delimiter = ',')]
    always: Vec<String>,
    /// File to write `NGIT_CI_REPOS=...` to. Without it no admission list is written.
    #[arg(long, env = "CI_POT_REPOS_FILE")]
    repos_file: Option<PathBuf>,
    /// Shell command to run after the admission list changed, such as a coordinator restart.
    #[arg(long, env = "CI_POT_ON_CHANGE")]
    on_change: Option<String>,
    /// Relay to read Job Results from. Repeat for more than one.
    #[arg(
        long = "relay",
        env = "CI_POT_RELAYS",
        value_delimiter = ',',
        default_value = "wss://relay.ngit.dev"
    )]
    relays: Vec<String>,
    /// Address to listen on.
    #[arg(long, env = "CI_POT_LISTEN", default_value = "127.0.0.1:4050")]
    listen: SocketAddr,
    /// Seconds between passes over the relays.
    #[arg(long, env = "CI_POT_POLL_SECONDS", default_value_t = 60)]
    poll_seconds: u64,
}

#[derive(Args)]
struct WalletArgs {
    /// Mint to look at. Repeat for more than one.
    #[arg(
        long = "mint",
        env = "CI_POT_MINTS",
        value_delimiter = ',',
        required = true
    )]
    mints: Vec<String>,
    #[command(subcommand)]
    action: WalletAction,
}

#[derive(Subcommand)]
enum WalletAction {
    /// Show what the wallet holds, how much of it belongs to pots, and what is yours.
    Balance,
    /// Print a Cashu token for this many sats, to paste into any Cashu wallet.
    Withdraw {
        sat: u64,
        /// Allow withdrawing sats that funders paid in and jobs have not used yet.
        #[arg(long)]
        allow_unearned: bool,
    },
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("error: {e:#}");
        std::process::exit(2);
    }
}

async fn run(cli: Cli) -> Result<()> {
    // TLS for relay and mint connections needs its cryptography chosen once, up front.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let data_dir = cli.data_dir;

    match cli.command {
        Command::Serve(args) => serve(&data_dir, args).await,

        Command::Status => {
            cashier::make_private_dir(&data_dir)?;
            let pots = Ledger::open(&data_dir.join("ledger.sqlite"))?.pots()?;
            if pots.is_empty() {
                println!("no pots yet");
            }
            for pot in pots {
                println!(
                    "{:>9} sat  {} jobs  {}",
                    pot.balance_msat() / 1000,
                    pot.jobs,
                    pot.address
                );
                if pot.pending_deposits > 0 {
                    println!(
                        "           {} deposit(s) started but never confirmed: check the wallet",
                        pot.pending_deposits
                    );
                }
            }
            Ok(())
        }

        Command::Wallet(args) => {
            let cashier = Cashier::open(&data_dir, &args.mints).await?;
            let ledger = Ledger::open(&data_dir.join("ledger.sqlite"))?;
            let balances = cashier.balances().await?;
            let total: u64 = balances.iter().map(|(_, sat)| sat).sum();
            let held = ledger.held_for_pots_msat()?.div_ceil(1000);
            let earned = total.saturating_sub(held);
            match args.action {
                WalletAction::Balance => {
                    for (mint, sat) in &balances {
                        println!("{sat:>9} sat  {mint}");
                    }
                    println!("{total:>9} sat  in the wallet");
                    println!("{held:>9} sat  held for pots (paid in, not yet used)");
                    println!("{earned:>9} sat  earned, free to withdraw");
                }
                WalletAction::Withdraw {
                    sat,
                    allow_unearned,
                } => {
                    if args.mints.len() != 1 {
                        bail!("withdraw from one mint at a time: pass a single --mint");
                    }
                    if sat > earned && !allow_unearned {
                        bail!(
                            "{sat} sat is more than the {earned} sat earned so far. The rest was \
                             paid in by funders for jobs not run yet. Pass --allow-unearned to take it anyway."
                        );
                    }
                    println!("{}", cashier.withdraw(&args.mints[0], sat).await?);
                }
            }
            Ok(())
        }
    }
}

async fn serve(data_dir: &Path, args: ServeArgs) -> Result<()> {
    let coordinator = hex_key(&args.coordinator)?;
    let mut executors = vec![coordinator.clone()];
    for provider in &args.providers {
        executors.push(hex_key(provider)?);
    }

    let cashier = Cashier::open(data_dir, &args.mints).await?;
    let service = Arc::new(pot::Service::new(
        pot::Config {
            coordinator,
            executors,
            price: Price::per_vcpu_hour(args.sat_per_vcpu_hour, args.default_vcpus),
            min_balance_msat: (args.min_balance_sat * 1000) as i64,
            always: args.always,
            repos_file: args.repos_file,
            on_change: args.on_change,
            relays: args.relays,
        },
        cashier,
        Ledger::open(&data_dir.join("ledger.sqlite"))?,
        Store::open(&data_dir.join("events.sqlite"))?,
    ));

    // The background pass: read relays, charge finished jobs, update admission.
    let worker = service.clone();
    let pause = Duration::from_secs(args.poll_seconds.max(5));
    tokio::spawn(async move {
        loop {
            worker.pass(pot::now()).await;
            tokio::time::sleep(pause).await;
        }
    });

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("cannot listen on {}", args.listen))?;
    eprintln!("ci-pot listening on http://{}", args.listen);
    eprintln!(
        "charging jobs of coordinator {} at {} sat per vCPU-hour",
        args.coordinator, args.sat_per_vcpu_hour
    );
    axum::serve(listener, server::router(service)).await?;
    Ok(())
}

/// A public key given as `npub1...` or hex, as hex.
fn hex_key(text: &str) -> Result<String> {
    PublicKey::parse(text.trim())
        .map(|key| key.to_hex())
        .map_err(|e| anyhow!("`{text}` is not a public key: {e}"))
}
