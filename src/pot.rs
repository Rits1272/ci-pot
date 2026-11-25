//! The pot service: what happens when money comes in and when jobs finish.
//!
//! Three operations, and nothing else changes state:
//!
//! 1. [`Service::deposit`]: a funder's token is redeemed and credited to a repository's pot.
//! 2. [`Service::settle`]: finished jobs this coordinator ran are charged to their pots.
//! 3. [`Service::admit`]: the list of funded repositories is written for the coordinator.
//!
//! Only jobs the operator's own machines ran are charged. A Job Result is a
//! public event anyone can publish, so a job counts only when it names this
//! coordinator *and* is signed by a key the operator listed. Without the
//! second check, a stranger could publish made-up results and drain a pot.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Serialize;

use crate::admit;
use crate::cashier::{token_hash, Cashier};
use crate::job::{is_repository_address, Job, JOB_RESULT};
use crate::ledger::{Charged, Ledger, Pot};
use crate::price::Price;
use crate::relay;
use crate::store::Store;

/// How far back to re-read job results on each pass. Charging is idempotent,
/// so re-reading costs nothing and catches events that arrived late.
const LOOKBACK_SECONDS: u64 = 24 * 60 * 60;

/// The current time, in seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub struct Config {
    /// This operator's coordinator, as a hex public key.
    pub coordinator: String,
    /// Keys whose Job Results are believed: the coordinator and its approved providers.
    pub executors: Vec<String>,
    pub price: Price,
    /// A repository is admitted while its pot holds more than this.
    pub min_balance_msat: i64,
    /// Repositories always served, as ngit-ci selectors (`npub1.../name`).
    pub always: Vec<String>,
    /// Where to write `NGIT_CI_REPOS=...`. `None` writes nothing.
    pub repos_file: Option<PathBuf>,
    /// Command to run after the list changed.
    pub on_change: Option<String>,
    /// Relays to read Job Results from.
    pub relays: Vec<String>,
}

pub struct Service {
    pub config: Config,
    pub cashier: Cashier,
    ledger: Mutex<Ledger>,
    store: Mutex<Store>,
}

/// Why a deposit was not accepted.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The request itself is wrong: bad address, not a token, wrong mint.
    Invalid(String),
    /// This exact token was already paid in.
    AlreadyUsed,
    /// The mint would not redeem the token (usually: already spent).
    Mint(String),
    /// Something failed on this side. The funder's token was not redeemed,
    /// or was redeemed and is recorded as a pending deposit.
    Internal(String),
}

/// A pot as shown to the outside.
#[derive(Clone, Debug, Serialize)]
pub struct PotView {
    #[serde(flatten)]
    pub pot: Pot,
    pub balance_msat: i64,
    /// Whether the coordinator is told to serve this repository right now.
    pub admitted: bool,
}

/// What one settling pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    pub charged: u64,
    pub charged_msat: u64,
}

impl Service {
    pub fn new(config: Config, cashier: Cashier, ledger: Ledger, store: Store) -> Service {
        Service {
            config,
            cashier,
            ledger: Mutex::new(ledger),
            store: Mutex::new(store),
        }
    }

    /// Credits a token to the pot of the repository at `address`.
    pub async fn deposit(&self, address: &str, token: &str, now: u64) -> Result<PotView, Refused> {
        let internal = |e: anyhow::Error| Refused::Internal(e.to_string());

        // 1. Refuse what can be refused without touching money.
        if !is_repository_address(address) {
            return Err(Refused::Invalid(
                "the address must look like 30617:<64 hex characters>:<repository name>".into(),
            ));
        }
        self.cashier
            .inspect(token)
            .map_err(|e| Refused::Invalid(e.to_string()))?;

        // 2. Write down that this token is being redeemed, before redeeming it.
        let (pot, entry) = {
            let ledger = self.ledger.lock().unwrap();
            let pot = ledger.pot_for(address, now).map_err(internal)?;
            match ledger
                .begin_deposit(pot, &token_hash(token), now)
                .map_err(internal)?
            {
                Some(entry) => (pot, entry),
                None => return Err(Refused::AlreadyUsed),
            }
        };

        // 3. Redeem. From here the money is either ours or still the funder's.
        match self.cashier.redeem(token).await {
            Ok(sat) => {
                let ledger = self.ledger.lock().unwrap();
                ledger
                    .confirm_deposit(entry, sat * 1000)
                    .map_err(internal)?;
            }
            Err(e) => {
                let ledger = self.ledger.lock().unwrap();
                ledger.cancel_deposit(entry).map_err(internal)?;
                ledger.drop_if_unused(pot).map_err(internal)?;
                return Err(Refused::Mint(e.to_string()));
            }
        }

        // 4. A newly funded repository should be served without waiting for the next pass.
        if let Err(e) = self.admit().await {
            eprintln!("admission: {e:#}");
        }
        self.pot(address)
            .map_err(internal)?
            .ok_or_else(|| Refused::Internal("the pot is missing after a deposit".into()))
    }

    /// Fetches new Job Results from the relays into the local store.
    pub async fn fetch(&self, now: u64) {
        let since = now.saturating_sub(LOOKBACK_SECONDS);
        for url in &self.config.relays {
            let mut events = Vec::new();
            let outcome = relay::fetch(url, &[JOB_RESULT], since, |event| {
                events.push(event);
                Ok(())
            })
            .await;
            if let Err(e) = outcome {
                eprintln!("{url}: {e:#}");
            }
            let store = self.store.lock().unwrap();
            for event in &events {
                if let Err(e) = store.keep(event) {
                    eprintln!("store: {e:#}");
                }
            }
        }
    }

    /// Charges every stored job this coordinator ran that has not been charged yet.
    pub fn settle(&self, now: u64) -> Result<Settled> {
        let since = now.saturating_sub(LOOKBACK_SECONDS);
        let events = self.store.lock().unwrap().events(JOB_RESULT, since)?;
        let ledger = self.ledger.lock().unwrap();
        let mut settled = Settled::default();
        for event in &events {
            let Ok(job) = Job::read(event) else { continue };
            if !self.is_ours(&job) {
                continue;
            }
            let cost = self.config.price.of(&job);
            if let Charged::Yes { .. } = ledger.charge(&job, cost)? {
                settled.charged += 1;
                settled.charged_msat += cost;
            }
        }
        Ok(settled)
    }

    /// True when this coordinator scheduled the job and a listed key signed its result.
    fn is_ours(&self, job: &Job) -> bool {
        job.coordinator.as_deref() == Some(self.config.coordinator.as_str())
            && self.config.executors.contains(&job.executor)
    }

    /// Writes the list of repositories to serve, and runs the operator's command if it changed.
    pub async fn admit(&self) -> Result<bool> {
        let Some(path) = &self.config.repos_file else {
            return Ok(false);
        };
        let pots = self.ledger.lock().unwrap().pots()?;
        let value = admit::repos_value(&self.config.always, &pots, self.config.min_balance_msat);
        let changed = admit::write_if_changed(path, &value)?;
        if changed {
            eprintln!("admission list changed: {}", path.display());
            if let Some(command) = &self.config.on_change {
                eprintln!("ran `{command}`: {}", admit::run_on_change(command).await);
            }
        }
        Ok(changed)
    }

    /// One full pass: fetch, charge, then update admission.
    pub async fn pass(&self, now: u64) {
        self.fetch(now).await;
        match self.settle(now) {
            Ok(settled) if settled.charged > 0 => eprintln!(
                "charged {} jobs, {} sat",
                settled.charged,
                settled.charged_msat.div_ceil(1000)
            ),
            Ok(_) => {}
            Err(e) => eprintln!("settling: {e:#}"),
        }
        if let Err(e) = self.admit().await {
            eprintln!("admission: {e:#}");
        }
    }

    pub fn pots(&self) -> Result<Vec<PotView>> {
        let pots = self.ledger.lock().unwrap().pots()?;
        Ok(pots.into_iter().map(|pot| self.view(pot)).collect())
    }

    pub fn pot(&self, address: &str) -> Result<Option<PotView>> {
        let pot = self.ledger.lock().unwrap().pot(address)?;
        Ok(pot.map(|pot| self.view(pot)))
    }

    pub fn entries(&self, pot: i64) -> Result<Vec<crate::ledger::Entry>> {
        self.ledger.lock().unwrap().entries(pot, 50)
    }

    fn view(&self, pot: Pot) -> PotView {
        let balance_msat = pot.balance_msat();
        PotView {
            admitted: balance_msat > self.config.min_balance_msat,
            balance_msat,
            pot,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use nostr::prelude::*;
    use tokio_tungstenite::tungstenite::Message;

    pub const NOW: u64 = 1_800_000_000;

    /// A service with one coordinator key, which is also its only executor.
    pub async fn service(dir: &std::path::Path, relays: Vec<String>) -> (Service, Keys) {
        let coordinator = Keys::generate();
        let config = Config {
            coordinator: coordinator.public_key().to_hex(),
            executors: vec![coordinator.public_key().to_hex()],
            price: Price::per_vcpu_hour(60, 4),
            min_balance_msat: 0,
            always: Vec::new(),
            repos_file: Some(dir.join("repos.env")),
            on_change: None,
            relays,
        };
        let service = Service::new(
            config,
            Cashier::open(dir, &["https://mint.example".into()])
                .await
                .unwrap(),
            Ledger::open(&dir.join("ledger.sqlite")).unwrap(),
            Store::open(&dir.join("events.sqlite")).unwrap(),
        );
        (service, coordinator)
    }

    pub fn address(owner: &Keys, name: &str) -> String {
        format!("30617:{}:{name}", owner.public_key().to_hex())
    }

    /// A signed Job Result: `executor` ran one hour on one vCPU for `address`,
    /// scheduled by `coordinator`, finishing at `finished_at`.
    pub fn job_result(
        executor: &Keys,
        coordinator: &Keys,
        address: &str,
        finished_at: u64,
    ) -> Event {
        let run = format!("39842:{}:run-1", coordinator.public_key().to_hex());
        let started = (finished_at - 3600).to_string();
        let tags = [
            vec!["a", address],
            vec!["q", &run],
            vec!["job", "test"],
            vec!["conclusion", "success"],
            vec!["started_at", &started],
            vec!["f", "vcpus:eq:1"],
        ];
        EventBuilder::new(Kind::from(JOB_RESULT), "")
            .tags(tags.iter().map(|t| Tag::parse(t.iter().copied()).unwrap()))
            .custom_created_at(Timestamp::from(finished_at))
            .finalize(executor)
            .unwrap()
    }

    /// Puts sats into a pot without a mint, by writing the ledger directly.
    pub fn fund(service: &Service, address: &str, sat: u64, at: u64) {
        let ledger = service.ledger.lock().unwrap();
        let pot = ledger.pot_for(address, at).unwrap();
        let entry = ledger
            .begin_deposit(pot, &format!("test-{address}-{sat}-{at}"), at)
            .unwrap()
            .unwrap();
        ledger.confirm_deposit(entry, sat * 1000).unwrap();
    }

    fn keep(service: &Service, event: &Event) {
        assert!(service.store.lock().unwrap().keep(event).unwrap());
    }

    fn repos_file(service: &Service) -> String {
        std::fs::read_to_string(service.config.repos_file.as_ref().unwrap()).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_job_this_coordinator_ran_is_charged_to_the_pot() {
        let dir = tempfile::tempdir().unwrap();
        let (service, coordinator) = service(dir.path(), vec![]).await;
        let repo = address(&Keys::generate(), "app");
        fund(&service, &repo, 100, NOW - 7200);
        keep(
            &service,
            &job_result(&coordinator, &coordinator, &repo, NOW - 60),
        );

        let settled = service.settle(NOW).unwrap();
        // One hour on one vCPU at 60 sat per vCPU-hour.
        assert_eq!(
            settled,
            Settled {
                charged: 1,
                charged_msat: 60_000
            }
        );
        assert_eq!(service.pot(&repo).unwrap().unwrap().balance_msat, 40_000);
        // Settling again charges nothing more.
        assert_eq!(service.settle(NOW).unwrap(), Settled::default());
    }

    #[tokio::test]
    async fn a_result_signed_by_a_stranger_cannot_drain_a_pot() {
        let dir = tempfile::tempdir().unwrap();
        let (service, coordinator) = service(dir.path(), vec![]).await;
        let repo = address(&Keys::generate(), "app");
        fund(&service, &repo, 100, NOW - 7200);

        // It names our coordinator, but our coordinator never listed this key.
        let stranger = Keys::generate();
        keep(
            &service,
            &job_result(&stranger, &coordinator, &repo, NOW - 60),
        );

        assert_eq!(service.settle(NOW).unwrap(), Settled::default());
        assert_eq!(service.pot(&repo).unwrap().unwrap().balance_msat, 100_000);
    }

    #[tokio::test]
    async fn another_coordinators_job_is_not_charged() {
        let dir = tempfile::tempdir().unwrap();
        let (service, coordinator) = service(dir.path(), vec![]).await;
        let repo = address(&Keys::generate(), "app");
        fund(&service, &repo, 100, NOW - 7200);

        // Our own machine signed it, but another coordinator scheduled it.
        let other = Keys::generate();
        keep(&service, &job_result(&coordinator, &other, &repo, NOW - 60));

        assert_eq!(service.settle(NOW).unwrap(), Settled::default());
    }

    #[tokio::test]
    async fn a_repository_is_admitted_while_funded_and_dropped_when_spent() {
        let dir = tempfile::tempdir().unwrap();
        let (service, coordinator) = service(dir.path(), vec![]).await;
        let owner = Keys::generate();
        let repo = address(&owner, "app");
        let selector = format!("{}/app", owner.public_key().to_bech32().unwrap());

        fund(&service, &repo, 100, NOW - 3 * 3600);
        assert!(service.admit().await.unwrap());
        assert_eq!(repos_file(&service), format!("NGIT_CI_REPOS={selector}\n"));

        // Two one-hour jobs cost 120 sat and overrun the 100 sat pot.
        keep(
            &service,
            &job_result(&coordinator, &coordinator, &repo, NOW - 3600),
        );
        keep(
            &service,
            &job_result(&coordinator, &coordinator, &repo, NOW - 60),
        );
        service.settle(NOW).unwrap();

        assert!(service.admit().await.unwrap());
        assert_eq!(repos_file(&service), "NGIT_CI_REPOS=\n");
        let pot = service.pot(&repo).unwrap().unwrap();
        assert_eq!(pot.balance_msat, -20_000);
        assert!(!pot.admitted);
    }

    #[tokio::test]
    async fn a_deposit_with_a_bad_address_or_token_is_refused_and_leaves_no_pot() {
        let dir = tempfile::tempdir().unwrap();
        let (service, _) = service(dir.path(), vec![]).await;
        let repo = address(&Keys::generate(), "app");

        let bad_address = service.deposit("not-an-address", "cashuBabc", NOW).await;
        assert!(matches!(bad_address, Err(Refused::Invalid(_))));
        let bad_token = service.deposit(&repo, "not a token", NOW).await;
        assert!(matches!(bad_token, Err(Refused::Invalid(_))));
        assert!(service.pots().unwrap().is_empty());
    }

    /// A relay that answers every request with the same events, then end-of-stored-events.
    async fn mock_relay(events: Vec<Event>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let events = events.clone();
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = socket.next().await {
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if request[0] != "REQ" {
                            continue;
                        }
                        for event in &events {
                            let line = format!(r#"["EVENT",{},{}]"#, request[1], event.as_json());
                            socket.send(Message::Text(line.into())).await.unwrap();
                        }
                        let done = format!(r#"["EOSE",{}]"#, request[1]);
                        socket.send(Message::Text(done.into())).await.unwrap();
                    }
                });
            }
        });
        url
    }

    #[tokio::test]
    async fn a_full_pass_reads_a_relay_charges_the_job_and_updates_admission() {
        let dir = tempfile::tempdir().unwrap();
        let coordinator_for_relay = Keys::generate();
        let repo = address(&Keys::generate(), "app");

        // The relay is set up first, so the service is built around the same coordinator key.
        let result = job_result(
            &coordinator_for_relay,
            &coordinator_for_relay,
            &repo,
            NOW - 60,
        );
        let url = mock_relay(vec![result]).await;
        let (mut service, _) = service(dir.path(), vec![url]).await;
        service.config.coordinator = coordinator_for_relay.public_key().to_hex();
        service.config.executors = vec![coordinator_for_relay.public_key().to_hex()];

        fund(&service, &repo, 50, NOW - 7200);
        service.pass(NOW).await;

        // 60 sat charged against 50: the pot is overrun and the repository is no longer admitted.
        let pot = service.pot(&repo).unwrap().unwrap();
        assert_eq!(pot.pot.jobs, 1);
        assert_eq!(pot.balance_msat, -10_000);
        assert_eq!(repos_file(&service), "NGIT_CI_REPOS=\n");
    }
}
