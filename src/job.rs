//! Reading one Job Result.
//!
//! A Job Result (kind 9841, defined by NIP-C1 "Grasp CI") is a compute
//! provider's signed claim that it ran one CI job. This module turns that
//! event into the few facts needed to charge for it, and nothing else.
//!
//! How long a job took comes straight from the specification: "A Job Result's
//! `created_at` is its completion timestamp", and `started_at` is "when
//! execution began". So a job's time is `created_at - started_at`.

use nostr::prelude::*;

/// Kind of a Job Result event.
pub const JOB_RESULT: u16 = 9841;
/// Kind of the Workflow Progress event a Job Result points back to.
const WORKFLOW_PROGRESS: &str = "39842";
/// Kind of a repository announcement, which is what an `a` tag points at.
const REPOSITORY: &str = "30617";
/// A job reported as longer than this is treated as a bad timestamp, not as usage.
pub const MAX_JOB_SECONDS: u64 = 24 * 60 * 60;

/// How long a job ran, as far as the event lets us tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ran {
    /// The job ran for this many seconds.
    For(u64),
    /// The event has no `started_at`: the job was skipped or failed before starting.
    NotStarted,
    /// The timestamps cannot be right: it ended before it began, or ran over a day.
    Implausible,
}

/// The facts taken from one Job Result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Event id, in hex.
    pub id: String,
    /// Who signed the result: the machine that claims to have run the job.
    pub executor: String,
    /// The coordinator that scheduled the job, when the event names one.
    pub coordinator: Option<String>,
    /// Every announcement of the repository this job ran for, as `30617:<pubkey>:<name>`.
    pub repositories: Vec<String>,
    /// When the job finished, in seconds since the Unix epoch.
    pub finished_at: u64,
    pub ran: Ran,
    /// vCPUs allocated to the job, when the provider declared them.
    pub vcpus: Option<u64>,
}

impl Job {
    /// Reads a Job Result. Returns why not when the event is something else or is malformed.
    ///
    /// The signature is not checked here; the store only holds verified events.
    pub fn read(event: &Event) -> Result<Job, String> {
        if event.kind != Kind::from(JOB_RESULT) {
            return Err(format!("kind {} is not a Job Result", event.kind.as_u16()));
        }
        let tags: Vec<&[String]> = event.tags.iter().map(|t| t.as_slice()).collect();
        let first = |name: &str| {
            tags.iter()
                .find(|t| t.first().map(String::as_str) == Some(name))
                .and_then(|t| t.get(1))
                .map(String::as_str)
        };

        let repositories: Vec<String> = tags
            .iter()
            .filter(|t| t.first().map(String::as_str) == Some("a"))
            .filter_map(|t| t.get(1))
            .filter(|address| is_repository_address(address))
            .cloned()
            .collect();
        if repositories.is_empty() {
            return Err("names no repository".into());
        }

        // `["q", "39842:<coordinator-pubkey>:<run-id>", ...]` names the coordinator.
        let coordinator = tags
            .iter()
            .filter(|t| t.first().map(String::as_str) == Some("q"))
            .filter_map(|t| t.get(1))
            .filter_map(|address| {
                let mut parts = address.splitn(3, ':');
                (parts.next() == Some(WORKFLOW_PROGRESS))
                    .then(|| parts.next())
                    .flatten()
            })
            .find(|pubkey| is_hex_key(pubkey))
            .map(str::to_string);

        let finished_at = event.created_at.as_secs();
        let ran = match first("started_at").map(str::parse::<u64>) {
            None => Ran::NotStarted,
            Some(Err(_)) => Ran::Implausible,
            Some(Ok(started)) if started > finished_at => Ran::Implausible,
            Some(Ok(started)) if finished_at - started > MAX_JOB_SECONDS => Ran::Implausible,
            Some(Ok(started)) => Ran::For(finished_at - started),
        };

        // `["f", "vcpus:eq:4"]` records the vCPUs actually allocated.
        let vcpus = tags
            .iter()
            .filter(|t| t.first().map(String::as_str) == Some("f"))
            .filter_map(|t| t.get(1))
            .find_map(|value| value.strip_prefix("vcpus:eq:"))
            .and_then(|n| n.parse().ok());

        Ok(Job {
            id: event.id.to_hex(),
            executor: event.pubkey.to_hex(),
            coordinator,
            repositories,
            finished_at,
            ran,
            vcpus,
        })
    }

    /// Seconds this job ran, or zero when it did not run or its timestamps are bad.
    pub fn seconds(&self) -> u64 {
        match self.ran {
            Ran::For(seconds) => seconds,
            Ran::NotStarted | Ran::Implausible => 0,
        }
    }
}

/// True for `30617:<64 hex>:<name>`.
pub fn is_repository_address(address: &str) -> bool {
    let mut parts = address.splitn(3, ':');
    parts.next() == Some(REPOSITORY)
        && parts.next().is_some_and(is_hex_key)
        && parts.next().is_some_and(|name| !name.is_empty())
}

fn is_hex_key(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// The owner's public key in a repository address.
pub fn owner_of(address: &str) -> &str {
    address.split(':').nth(1).unwrap_or_default()
}

/// The repository's name in a repository address.
pub fn name_of(address: &str) -> &str {
    address.splitn(3, ':').nth(2).unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn fixture(name: &str) -> Event {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    Event::from_json(std::fs::read_to_string(path).unwrap()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The fixtures are real events taken from relay.ngit.dev on 2026-11-25.

    #[test]
    fn a_native_job_gives_its_time_coordinator_and_vcpus() {
        let event = fixture("job_native.json");
        event
            .verify()
            .expect("the fixture is a genuine signed event");
        let job = Job::read(&event).unwrap();

        assert_eq!(job.ran, Ran::For(1795560363 - 1795559669));
        assert_eq!(job.seconds(), 694);
        assert_eq!(job.vcpus, Some(4));
        assert_eq!(job.executor, event.pubkey.to_hex());
        assert_eq!(
            job.coordinator.as_deref(),
            Some("8a80f346c4e140b489d622abc0d1a265d29632a957446a4e70e0da0409d46f1e")
        );
        assert_eq!(job.repositories.len(), 3);
        assert!(job.repositories.iter().all(|a| name_of(a) == "wyrd"));
    }

    #[test]
    fn an_act_job_declares_no_vcpus() {
        let job = Job::read(&fixture("job_act.json")).unwrap();
        assert_eq!(job.seconds(), 1795596386 - 1795596232);
        assert_eq!(job.vcpus, None);
        // Here the coordinator ran the job itself.
        assert_eq!(job.coordinator.as_deref(), Some(job.executor.as_str()));
    }

    #[test]
    fn a_job_that_never_started_counts_no_time() {
        let job = Job::read(&fixture("job_not_started.json")).unwrap();
        assert_eq!(job.ran, Ran::NotStarted);
        assert_eq!(job.seconds(), 0);
    }

    #[test]
    fn another_kind_of_event_is_not_a_job() {
        assert!(Job::read(&fixture("service_request.json")).is_err());
    }

    #[test]
    fn repository_addresses_are_checked() {
        let key = "a".repeat(64);
        assert!(is_repository_address(&format!("30617:{key}:ngit")));
        assert!(!is_repository_address(&format!("30617:{key}:")));
        assert!(!is_repository_address(&format!("1:{key}:ngit")));
        assert!(!is_repository_address("30617:short:ngit"));
        assert_eq!(name_of(&format!("30617:{key}:a:b")), "a:b");
        assert_eq!(owner_of(&format!("30617:{key}:ngit")), key);
    }
}
