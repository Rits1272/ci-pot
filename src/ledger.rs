//! The ledger: every sat that entered a pot and every sat a job took out.
//!
//! A pot belongs to one repository. Its balance is never stored; it is the
//! sum of the pot's entries, so the ledger cannot drift from its own history.
//!
//! Two things keep the ledger honest when something is retried:
//!
//! - **A job is charged once.** The job's event id is the charge's reference,
//!   and references are unique.
//! - **A token is credited once.** A deposit is written as *pending* before
//!   the token is redeemed, keyed by the token's hash, and only then
//!   confirmed with the amount the mint paid out. A crash in between leaves a
//!   pending entry for the operator to look at, never a silent loss.
//!
//! A repository is announced under several addresses, one per maintainer.
//! When a job lists a funded address next to others, the others become
//! aliases of the same pot.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::job::Job;

pub struct Ledger {
    database: Connection,
}

/// What happened when a job was offered to the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charged {
    /// The job was charged to this pot.
    Yes { pot: i64 },
    /// None of the job's repository addresses has a pot.
    NoPot,
    /// This job was charged before.
    Already,
    /// The job finished before its pot existed, so it was never paid for in advance.
    BeforePot,
}

/// One pot, with its totals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pot {
    pub id: i64,
    /// The address the pot was first funded under, as `30617:<pubkey>:<name>`.
    pub address: String,
    /// Every address known to be this repository, including `address`.
    pub addresses: Vec<String>,
    /// When the pot was first funded, in seconds since the Unix epoch.
    pub created_at: u64,
    /// Total paid in, in millisatoshis.
    pub deposited_msat: u64,
    /// Total charged for jobs, in millisatoshis.
    pub charged_msat: u64,
    /// How many jobs were charged.
    pub jobs: u64,
    /// Deposits that were started and never confirmed.
    pub pending_deposits: u64,
}

impl Pot {
    /// What is left, in millisatoshis. Negative when jobs overran the pot.
    pub fn balance_msat(&self) -> i64 {
        self.deposited_msat as i64 - self.charged_msat as i64
    }
}

/// One line of a pot's history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// `deposit` or `charge`.
    pub kind: String,
    /// Millisatoshis in (deposit) or out (charge).
    pub msat: u64,
    /// The token's hash for a deposit, the job's event id for a charge.
    pub reference: String,
    pub at: u64,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Ledger> {
        let database =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        database.execute_batch(
            "CREATE TABLE IF NOT EXISTS pots (
                 id         INTEGER PRIMARY KEY,
                 address    TEXT NOT NULL UNIQUE,
                 created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS aliases (
                 address TEXT PRIMARY KEY,
                 pot     INTEGER NOT NULL REFERENCES pots (id)
             );
             CREATE TABLE IF NOT EXISTS entries (
                 id        INTEGER PRIMARY KEY,
                 pot       INTEGER NOT NULL REFERENCES pots (id),
                 kind      TEXT NOT NULL CHECK (kind IN ('deposit', 'charge')),
                 pending   INTEGER NOT NULL DEFAULT 0,
                 msat      INTEGER NOT NULL,
                 reference TEXT NOT NULL UNIQUE,
                 at        INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS entries_by_pot ON entries (pot, at);",
        )?;
        Ok(Ledger { database })
    }

    /// The pot for a repository address, creating it when the address has none.
    pub fn pot_for(&self, address: &str, now: u64) -> Result<i64> {
        if let Some(pot) = self.find(address)? {
            return Ok(pot);
        }
        self.database.execute(
            "INSERT INTO pots (address, created_at) VALUES (?1, ?2)",
            params![address, now as i64],
        )?;
        let pot = self.database.last_insert_rowid();
        self.database.execute(
            "INSERT INTO aliases (address, pot) VALUES (?1, ?2)",
            params![address, pot],
        )?;
        Ok(pot)
    }

    /// Records that a deposit is about to be redeemed. Returns `None` when
    /// this token was seen before, in which case nothing must be redeemed.
    pub fn begin_deposit(&self, pot: i64, token_hash: &str, now: u64) -> Result<Option<i64>> {
        let added = self.database.execute(
            "INSERT OR IGNORE INTO entries (pot, kind, pending, msat, reference, at)
             VALUES (?1, 'deposit', 1, 0, ?2, ?3)",
            params![pot, token_hash, now as i64],
        )?;
        Ok((added == 1).then(|| self.database.last_insert_rowid()))
    }

    /// Records what the mint paid out for a pending deposit.
    pub fn confirm_deposit(&self, entry: i64, msat: u64) -> Result<()> {
        self.database.execute(
            "UPDATE entries SET pending = 0, msat = ?2 WHERE id = ?1 AND pending = 1",
            params![entry, msat as i64],
        )?;
        Ok(())
    }

    /// Forgets a pending deposit whose token the mint refused.
    pub fn cancel_deposit(&self, entry: i64) -> Result<()> {
        self.database.execute(
            "DELETE FROM entries WHERE id = ?1 AND pending = 1",
            params![entry],
        )?;
        Ok(())
    }

    /// Removes a pot that has no entries at all, so a refused first deposit leaves nothing behind.
    pub fn drop_if_unused(&self, pot: i64) -> Result<()> {
        let entries: i64 = self.database.query_row(
            "SELECT COUNT(*) FROM entries WHERE pot = ?1",
            params![pot],
            |row| row.get(0),
        )?;
        if entries == 0 {
            self.database
                .execute("DELETE FROM aliases WHERE pot = ?1", params![pot])?;
            self.database
                .execute("DELETE FROM pots WHERE id = ?1", params![pot])?;
        }
        Ok(())
    }

    /// Charges a job to its repository's pot, once.
    pub fn charge(&self, job: &Job, msat: u64) -> Result<Charged> {
        let mut pot = None;
        for address in &job.repositories {
            if let Some(found) = self.find(address)? {
                pot = Some(found);
                break;
            }
        }
        let Some(pot) = pot else {
            return Ok(Charged::NoPot);
        };

        // The job's other addresses are the same repository under other maintainers.
        for address in &job.repositories {
            self.database.execute(
                "INSERT OR IGNORE INTO aliases (address, pot) VALUES (?1, ?2)",
                params![address, pot],
            )?;
        }

        let created_at: i64 = self.database.query_row(
            "SELECT created_at FROM pots WHERE id = ?1",
            params![pot],
            |row| row.get(0),
        )?;
        if (job.finished_at as i64) < created_at {
            return Ok(Charged::BeforePot);
        }

        let added = self.database.execute(
            "INSERT OR IGNORE INTO entries (pot, kind, msat, reference, at)
             VALUES (?1, 'charge', ?2, ?3, ?4)",
            params![pot, msat as i64, job.id, job.finished_at as i64],
        )?;
        Ok(if added == 1 {
            Charged::Yes { pot }
        } else {
            Charged::Already
        })
    }

    /// Every pot, oldest first.
    pub fn pots(&self) -> Result<Vec<Pot>> {
        let mut query = self.database.prepare(
            "SELECT p.id, p.address, p.created_at,
                    COALESCE(SUM(CASE WHEN e.kind = 'deposit' AND e.pending = 0 THEN e.msat END), 0),
                    COALESCE(SUM(CASE WHEN e.kind = 'charge' THEN e.msat END), 0),
                    COUNT(CASE WHEN e.kind = 'charge' THEN 1 END),
                    COUNT(CASE WHEN e.pending = 1 THEN 1 END)
             FROM pots p LEFT JOIN entries e ON e.pot = p.id
             GROUP BY p.id ORDER BY p.id",
        )?;
        let rows = query.query_map([], |row| {
            Ok(Pot {
                id: row.get(0)?,
                address: row.get(1)?,
                addresses: Vec::new(),
                created_at: row.get::<_, i64>(2)? as u64,
                deposited_msat: row.get::<_, i64>(3)? as u64,
                charged_msat: row.get::<_, i64>(4)? as u64,
                jobs: row.get::<_, i64>(5)? as u64,
                pending_deposits: row.get::<_, i64>(6)? as u64,
            })
        })?;
        let mut pots = Vec::new();
        for pot in rows {
            let mut pot = pot?;
            let mut aliases = self
                .database
                .prepare("SELECT address FROM aliases WHERE pot = ?1 ORDER BY address")?;
            pot.addresses = aliases
                .query_map(params![pot.id], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            pots.push(pot);
        }
        Ok(pots)
    }

    /// The pot a repository address belongs to, with its totals.
    pub fn pot(&self, address: &str) -> Result<Option<Pot>> {
        let Some(id) = self.find(address)? else {
            return Ok(None);
        };
        Ok(self.pots()?.into_iter().find(|pot| pot.id == id))
    }

    /// A pot's most recent entries, newest first.
    pub fn entries(&self, pot: i64, limit: u32) -> Result<Vec<Entry>> {
        let mut query = self.database.prepare(
            "SELECT kind, msat, reference, at FROM entries
             WHERE pot = ?1 AND pending = 0 ORDER BY at DESC, id DESC LIMIT ?2",
        )?;
        let rows = query.query_map(params![pot, limit], |row| {
            Ok(Entry {
                kind: row.get(0)?,
                msat: row.get::<_, i64>(1)? as u64,
                reference: row.get(2)?,
                at: row.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Millisatoshis still owed to funders: the sum of every pot's positive balance.
    ///
    /// This is money the operator holds but has not earned yet.
    pub fn held_for_pots_msat(&self) -> Result<u64> {
        Ok(self
            .pots()?
            .iter()
            .map(|pot| pot.balance_msat().max(0) as u64)
            .sum())
    }

    fn find(&self, address: &str) -> Result<Option<i64>> {
        Ok(self
            .database
            .query_row(
                "SELECT pot FROM aliases WHERE address = ?1",
                params![address],
                |row| row.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Ran;

    fn ledger() -> (Ledger, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            Ledger::open(&dir.path().join("ledger.sqlite")).unwrap(),
            dir,
        )
    }

    fn address(owner: char) -> String {
        format!("30617:{}:app", owner.to_string().repeat(64))
    }

    fn job(id: &str, addresses: &[String], finished_at: u64) -> Job {
        Job {
            id: id.into(),
            executor: "e".repeat(64),
            coordinator: Some("c".repeat(64)),
            repositories: addresses.to_vec(),
            finished_at,
            ran: Ran::For(60),
            vcpus: Some(2),
        }
    }

    /// Puts `msat` into the pot for `address`, as a confirmed deposit.
    fn fund(ledger: &Ledger, address: &str, token: &str, msat: u64, now: u64) -> i64 {
        let pot = ledger.pot_for(address, now).unwrap();
        let entry = ledger.begin_deposit(pot, token, now).unwrap().unwrap();
        ledger.confirm_deposit(entry, msat).unwrap();
        pot
    }

    #[test]
    fn a_deposit_then_a_charge_leaves_the_difference() {
        let (ledger, _dir) = ledger();
        let pot = fund(&ledger, &address('a'), "token-1", 10_000, 100);

        let charged = ledger
            .charge(&job("job-1", &[address('a')], 200), 3_000)
            .unwrap();
        assert_eq!(charged, Charged::Yes { pot });

        let pot = ledger.pot(&address('a')).unwrap().unwrap();
        assert_eq!(pot.deposited_msat, 10_000);
        assert_eq!(pot.charged_msat, 3_000);
        assert_eq!(pot.balance_msat(), 7_000);
        assert_eq!(pot.jobs, 1);
    }

    #[test]
    fn the_same_job_is_charged_once() {
        let (ledger, _dir) = ledger();
        fund(&ledger, &address('a'), "token-1", 10_000, 100);
        let job = job("job-1", &[address('a')], 200);

        assert!(matches!(
            ledger.charge(&job, 3_000).unwrap(),
            Charged::Yes { .. }
        ));
        assert_eq!(ledger.charge(&job, 3_000).unwrap(), Charged::Already);
        assert_eq!(
            ledger.pot(&address('a')).unwrap().unwrap().charged_msat,
            3_000
        );
    }

    #[test]
    fn the_same_token_is_credited_once() {
        let (ledger, _dir) = ledger();
        let pot = fund(&ledger, &address('a'), "token-1", 10_000, 100);
        assert_eq!(ledger.begin_deposit(pot, "token-1", 101).unwrap(), None);
        assert_eq!(
            ledger.pot(&address('a')).unwrap().unwrap().deposited_msat,
            10_000
        );
    }

    #[test]
    fn a_refused_token_leaves_no_trace_and_can_be_retried() {
        let (ledger, _dir) = ledger();
        let pot = ledger.pot_for(&address('a'), 100).unwrap();
        let entry = ledger.begin_deposit(pot, "token-1", 100).unwrap().unwrap();
        ledger.cancel_deposit(entry).unwrap();

        let status = ledger.pot(&address('a')).unwrap().unwrap();
        assert_eq!((status.deposited_msat, status.pending_deposits), (0, 0));
        assert!(ledger.begin_deposit(pot, "token-1", 101).unwrap().is_some());
    }

    #[test]
    fn an_unconfirmed_deposit_is_visible_but_not_spendable() {
        let (ledger, _dir) = ledger();
        let pot = ledger.pot_for(&address('a'), 100).unwrap();
        ledger.begin_deposit(pot, "token-1", 100).unwrap().unwrap();

        let status = ledger.pot(&address('a')).unwrap().unwrap();
        assert_eq!(status.pending_deposits, 1);
        assert_eq!(status.balance_msat(), 0);
    }

    #[test]
    fn a_repository_without_a_pot_is_not_charged() {
        let (ledger, _dir) = ledger();
        let charged = ledger
            .charge(&job("job-1", &[address('a')], 200), 3_000)
            .unwrap();
        assert_eq!(charged, Charged::NoPot);
        assert!(ledger.pots().unwrap().is_empty());
    }

    #[test]
    fn a_job_that_finished_before_the_pot_existed_is_not_charged() {
        let (ledger, _dir) = ledger();
        fund(&ledger, &address('a'), "token-1", 10_000, 500);
        let charged = ledger
            .charge(&job("old", &[address('a')], 499), 3_000)
            .unwrap();
        assert_eq!(charged, Charged::BeforePot);
        assert_eq!(ledger.pot(&address('a')).unwrap().unwrap().charged_msat, 0);
    }

    #[test]
    fn other_maintainers_addresses_become_aliases_of_the_pot() {
        let (ledger, _dir) = ledger();
        let pot = fund(&ledger, &address('a'), "token-1", 10_000, 100);

        // The first job lists both announcements; the second only Bob's.
        ledger
            .charge(&job("job-1", &[address('b'), address('a')], 200), 1_000)
            .unwrap();
        let second = ledger
            .charge(&job("job-2", &[address('b')], 300), 1_000)
            .unwrap();

        assert_eq!(second, Charged::Yes { pot });
        let status = ledger.pot(&address('b')).unwrap().unwrap();
        assert_eq!(status.addresses, vec![address('a'), address('b')]);
        assert_eq!(status.charged_msat, 2_000);
        // Funding under Bob's address now reaches the same pot.
        assert_eq!(ledger.pot_for(&address('b'), 400).unwrap(), pot);
    }

    #[test]
    fn a_pot_can_be_overrun_and_shows_the_debt() {
        let (ledger, _dir) = ledger();
        fund(&ledger, &address('a'), "token-1", 1_000, 100);
        ledger
            .charge(&job("job-1", &[address('a')], 200), 2_500)
            .unwrap();
        assert_eq!(
            ledger.pot(&address('a')).unwrap().unwrap().balance_msat(),
            -1_500
        );
        // An overrun pot owes funders nothing.
        assert_eq!(ledger.held_for_pots_msat().unwrap(), 0);
    }

    #[test]
    fn money_held_for_funders_is_the_sum_of_positive_balances() {
        let (ledger, _dir) = ledger();
        fund(&ledger, &address('a'), "token-1", 5_000, 100);
        fund(&ledger, &address('b'), "token-2", 2_000, 100);
        ledger
            .charge(&job("job-1", &[address('a')], 200), 1_000)
            .unwrap();
        assert_eq!(ledger.held_for_pots_msat().unwrap(), 6_000);
    }

    #[test]
    fn history_lists_newest_first() {
        let (ledger, _dir) = ledger();
        let pot = fund(&ledger, &address('a'), "token-1", 10_000, 100);
        ledger
            .charge(&job("job-1", &[address('a')], 200), 1_000)
            .unwrap();
        let entries = ledger.entries(pot, 10).unwrap();
        assert_eq!(entries[0].kind, "charge");
        assert_eq!(entries[0].reference, "job-1");
        assert_eq!(entries[1].kind, "deposit");
    }
}
