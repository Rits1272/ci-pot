# Draft proposal: prepaid billing for Grasp CI

**Status: a draft for discussion. Nothing here has been proposed upstream or agreed by anyone.**

This describes two small additions that would let a coordinator serve any repository whose CI has been paid for, without the operator adding it by hand. `ci-pot` is a working implementation of the operator's side that needs neither change, at the cost of a coordinator restart.

## What exists today

- NIP-C1's Coordinator Advertisement has a billing tag with two values: `["B", "not-required"]` and `["B", "out-of-band"]`. There is no value for billing a client can act on.
- ngit-ci admits repositories from a list read at start-up (`--repos`). Its documentation says "Controls do not dynamically expand `--repos` admission."
- The ngit roadmap names the goal: "Maintainers or benefactors could fund additional capacity through a subscription or shared pot", and for admission, "Providers a maintainer can select without an operator conversation".

## Addition 1: a billing value that points at a pot

A third value for the `B` tag, with the URL of the coordinator's pot service:

```json
["B", "prepaid", "https://pot.example"]
```

Meaning: this coordinator serves a repository while that repository's pot at the URL is funded. A client that understands the value can:

- read `GET <url>/info` for the price and the accepted mints,
- read `GET <url>/pots/<repository address>` for the balance and whether the repository is served now,
- send the maintainer or a supporter to `<url>` to fund it.

A client that does not understand `prepaid` treats it as any unknown billing value. The guidance already says an unknown value "is not permission to infer open, automatic, or free service".

This changes nothing about trust. Paying buys compute, not credibility: a result is still judged by who requested the coordinator and who signed it.

## Addition 2: a repository list that can change while running

An option for ngit-ci to read its admitted repositories from a file and notice when the file changes:

```text
--repos-file /etc/ngit-ci/repos
```

The file holds the same selectors `--repos` takes, one per line. On change, ngit-ci would start watching added repositories and stop taking new triggers for removed ones, while letting running jobs finish.

Until then, `ci-pot` writes `NGIT_CI_REPOS=...` to an environment file and runs an operator-supplied restart command. This relies on ngit-ci's existing graceful shutdown, which checkpoints queued jobs for a restart within `--queue-recovery-window-secs`.

## How charging works

Each finished job is charged from its Job Result (kind 9841):

- time is `created_at - started_at`,
- vCPUs come from the result's `f` tag, `vcpus:eq:<n>`,
- cost is time × vCPUs × the operator's rate.

Only results that name the operator's coordinator in their `q` tag and are signed by the coordinator or one of its approved providers are charged.

## Open questions

1. **Should the pot's balance be a Nostr event?** A replaceable event per repository, signed by the coordinator, would let clients show the balance without calling an HTTP service. It would also make the balance public on relays.
2. **Who should a deposit be payable to?** Here the operator's wallet takes Cashu tokens over HTTP. NIP-61 nutzaps tagged with the repository address would make each deposit a public, verifiable event and need no HTTP endpoint.
3. **What about separate compute providers?** A coordinator that delegates jobs to providers it does not own would need to pass on their share. This draft does not cover that.
4. **Is prepaid the right model?** A subscription with a compute allowance, which the roadmap also mentions, is a different shape and may suit maintainers better.
5. **Refunds.** A funder currently cannot take back unused sats.
