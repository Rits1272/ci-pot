//! What a job costs.
//!
//! One rule: a job costs its running time, times its vCPUs, times the
//! operator's rate. A job that does not say how many vCPUs it was given is
//! charged for the operator's default.
//!
//! Amounts are kept in millisatoshis so that short jobs are not rounded to
//! nothing, and each job's cost is rounded up to a whole millisatoshi.

use crate::job::Job;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Price {
    /// Millisatoshis for one vCPU for one hour.
    pub msat_per_vcpu_hour: u64,
    /// vCPUs assumed for a job that declares none.
    pub default_vcpus: u64,
}

impl Price {
    pub fn per_vcpu_hour(sat: u64, default_vcpus: u64) -> Price {
        Price {
            msat_per_vcpu_hour: sat * 1000,
            default_vcpus,
        }
    }

    /// The cost of one job, in millisatoshis.
    pub fn of(&self, job: &Job) -> u64 {
        let vcpus = job.vcpus.unwrap_or(self.default_vcpus);
        let vcpu_seconds = job.seconds().saturating_mul(vcpus);
        vcpu_seconds
            .saturating_mul(self.msat_per_vcpu_hour)
            .div_ceil(3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{fixture, Job, Ran};

    fn job(seconds: u64, vcpus: Option<u64>) -> Job {
        Job {
            id: "1".into(),
            executor: "e".repeat(64),
            coordinator: None,
            repositories: vec![format!("30617:{}:app", "a".repeat(64))],
            finished_at: 0,
            ran: Ran::For(seconds),
            vcpus,
        }
    }

    #[test]
    fn an_hour_on_one_vcpu_costs_the_rate() {
        let price = Price::per_vcpu_hour(60, 4);
        assert_eq!(price.of(&job(3600, Some(1))), 60_000);
    }

    #[test]
    fn more_vcpus_cost_proportionally_more() {
        let price = Price::per_vcpu_hour(60, 4);
        assert_eq!(price.of(&job(1800, Some(8))), 240_000);
    }

    #[test]
    fn a_job_that_declares_no_vcpus_is_charged_the_default() {
        let price = Price::per_vcpu_hour(60, 4);
        assert_eq!(price.of(&job(3600, None)), 240_000);
    }

    #[test]
    fn a_short_job_is_rounded_up_not_down_to_nothing() {
        let price = Price::per_vcpu_hour(1, 1);
        assert_eq!(price.of(&job(1, Some(1))), 1);
    }

    #[test]
    fn a_job_that_never_ran_costs_nothing() {
        let mut skipped = job(0, Some(4));
        skipped.ran = Ran::NotStarted;
        assert_eq!(Price::per_vcpu_hour(60, 4).of(&skipped), 0);
    }

    #[test]
    fn a_real_job_is_priced_from_its_own_event() {
        // 694 seconds on 4 vCPUs at 60 sat per vCPU-hour.
        let job = Job::read(&fixture("job_native.json")).unwrap();
        assert_eq!(Price::per_vcpu_hour(60, 2).of(&job), 46_267);
    }
}
