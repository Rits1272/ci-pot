//! Admission: turning funded pots into the list of repositories a coordinator serves.
//!
//! ngit-ci reads the repositories it serves from `NGIT_CI_REPOS` when it
//! starts. This module writes that setting to a file whenever the set of
//! funded repositories changes, and can then run a command of the operator's
//! choosing, normally one that restarts the coordinator. ngit-ci keeps queued
//! jobs across a short graceful restart, so this works with it unmodified.
//!
//! A repository is admitted while its pot holds more than the operator's
//! minimum balance. Repositories the operator always serves (their own, for
//! example) are listed first and never depend on a pot.

use std::path::Path;

use anyhow::{Context, Result};
use nostr::prelude::*;

use crate::job::{name_of, owner_of};
use crate::ledger::Pot;

/// The value for `NGIT_CI_REPOS`: `npub1.../name` entries, comma separated.
///
/// `always` entries come first, unchanged. Funded pots follow in a stable order.
pub fn repos_value(always: &[String], pots: &[Pot], min_balance_msat: i64) -> String {
    let mut entries: Vec<String> = always.to_vec();
    let mut funded: Vec<String> = pots
        .iter()
        .filter(|pot| pot.balance_msat() > min_balance_msat)
        .flat_map(|pot| pot.addresses.iter())
        .filter_map(|address| selector(address))
        .collect();
    funded.sort();
    for entry in funded {
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(",")
}

/// `30617:<hex pubkey>:<name>` as ngit-ci's `npub1.../name`.
///
/// Names with characters that would break the list (a comma, `#`, whitespace)
/// are left out rather than written in a form ngit-ci would misread.
fn selector(address: &str) -> Option<String> {
    let name = name_of(address);
    if name.is_empty()
        || name
            .chars()
            .any(|c| c == ',' || c == '#' || c.is_whitespace())
    {
        return None;
    }
    let npub = PublicKey::from_hex(owner_of(address))
        .ok()?
        .to_bech32()
        .ok()?;
    Some(format!("{npub}/{name}"))
}

/// Writes `NGIT_CI_REPOS=<value>` to `path` if it differs from what is there.
/// Returns whether the file changed.
pub fn write_if_changed(path: &Path, value: &str) -> Result<bool> {
    let contents = format!("NGIT_CI_REPOS={value}\n");
    if std::fs::read_to_string(path).is_ok_and(|current| current == contents) {
        return Ok(false);
    }
    // Write beside the file and rename, so a reader never sees half a list.
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, &contents)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(true)
}

/// Runs the operator's command after the list changed. Returns its exit status as text.
pub async fn run_on_change(command: &str) -> String {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .await
    {
        Ok(status) if status.success() => "ok".into(),
        Ok(status) => format!("exited with {status}"),
        Err(e) => format!("could not run: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "8a80f346c4e140b489d622abc0d1a265d29632a957446a4e70e0da0409d46f1e";
    const BOB: &str = "a008def15796fba9a0d6fab04e8fd57089285d9fd505da5a83fe8aad57a3564d";

    fn npub(hex: &str) -> String {
        PublicKey::from_hex(hex).unwrap().to_bech32().unwrap()
    }

    fn pot(id: i64, owners: &[&str], name: &str, deposited: u64, charged: u64) -> Pot {
        let addresses: Vec<String> = owners
            .iter()
            .map(|owner| format!("30617:{owner}:{name}"))
            .collect();
        Pot {
            id,
            address: addresses[0].clone(),
            addresses,
            created_at: 0,
            deposited_msat: deposited,
            charged_msat: charged,
            jobs: 0,
            pending_deposits: 0,
        }
    }

    #[test]
    fn a_funded_pot_is_admitted_under_every_maintainers_address() {
        let pots = [pot(1, &[ALICE, BOB], "app", 5_000, 1_000)];
        let mut expected = [format!("{}/app", npub(ALICE)), format!("{}/app", npub(BOB))];
        expected.sort();
        assert_eq!(repos_value(&[], &pots, 0), expected.join(","));
    }

    #[test]
    fn an_empty_or_overrun_pot_is_not_admitted() {
        let pots = [
            pot(1, &[ALICE], "spent", 1_000, 1_000),
            pot(2, &[ALICE], "overrun", 1_000, 4_000),
        ];
        assert_eq!(repos_value(&[], &pots, 0), "");
    }

    #[test]
    fn a_pot_must_hold_more_than_the_minimum() {
        let pots = [pot(1, &[ALICE], "app", 100_000, 0)];
        assert_eq!(repos_value(&[], &pots, 100_000), "");
        assert_ne!(repos_value(&[], &pots, 99_999), "");
    }

    #[test]
    fn repositories_the_operator_always_serves_come_first_and_need_no_pot() {
        let own = format!("{}/mine", npub(BOB));
        let pots = [pot(1, &[ALICE], "app", 5_000, 0)];
        let value = repos_value(std::slice::from_ref(&own), &pots, 0);
        assert_eq!(value, format!("{own},{}/app", npub(ALICE)));
    }

    #[test]
    fn a_name_that_would_break_the_list_is_left_out() {
        let pots = [
            pot(1, &[ALICE], "evil,npub1attacker", 5_000, 0),
            pot(2, &[ALICE], "evil#ALIAS", 5_000, 0),
            pot(3, &[ALICE], "fine", 5_000, 0),
        ];
        assert_eq!(repos_value(&[], &pots, 0), format!("{}/fine", npub(ALICE)));
    }

    #[test]
    fn the_file_is_rewritten_only_when_the_list_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.env");
        assert!(write_if_changed(&path, "a").unwrap());
        assert!(!write_if_changed(&path, "a").unwrap());
        assert!(write_if_changed(&path, "a,b").unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "NGIT_CI_REPOS=a,b\n"
        );
    }

    #[tokio::test]
    async fn the_operators_command_reports_how_it_went() {
        assert_eq!(run_on_change("true").await, "ok");
        assert!(run_on_change("exit 3").await.starts_with("exited"));
    }
}
