# ADR-XXX: Cloud topology — orchestration / build / runtime

- **Status**: Proposed
- **Date**: YYYY-MM-DD

## Context

This project uses the three-role topology scaffolded by `lib-agents`
(see issue #141). The roles are:

| Role            | What it owns                                          |
|-----------------|-------------------------------------------------------|
| orchestration   | Agent SA (operator identity), custom IAM role, daily-deploy entry point |
| build           | Builder SA, Cloud Build, Artifact Registry, TF state |
| runtime         | Runtime SA, Cloud Run service, service-managed resources |

Every role can collapse to the same project (the 90% case) or split.
The split is a config edit, not a code refactor — bootstrap script,
Terraform, and operator commands all work identically; only IAM grants
branch on local-vs-cross-project.

## Decision

**Topology for this project**: <FILL IN — collapsed / partial-split / fully-split>

| Role            | Project                                | Region   |
|-----------------|----------------------------------------|----------|
| orchestration   | `<your-project>`                       | `<region>` |
| build           | `<your-project>`                       | `<region>` |
| runtime         | `<your-project>`                       | `<region>` |

**Rationale for the chosen split** (delete the cases that don't apply):

- *Collapsed (one project)*: personal sandbox, hackathon, single-tenant
  hobby project. No tenancy boundary needed; minimum IAM surface.
- *Build + runtime collapsed, orchestration split*: agent identity lives
  outside the deploy plane (e.g. operator runs from a workstation
  project that is separate from where services run).
- *Orchestration + build collapsed, runtime split*: production tenancy
  pattern — services run in a tenant-owned project; the build plane
  (where source + secrets exist transiently) stays in a separate
  ops-owned project.
- *Fully split*: regulated environments, large orgs, multi-tenant ops
  where each role belongs to a different team.

## TF / admin-cloud-init boundary (non-negotiable)

Per #141 lesson 1, Terraform does **not** mutate project scope. The
boundary is:

| Layer                | Owns                                                          |
|----------------------|---------------------------------------------------------------|
| `admin-cloud-init`   | API enablement; AR repo creation; TF state bucket creation; project-wide IAM of other principals (agent SA → custom role; builder SA → predefined functional roles; agent SA → actAs on builder SA) |
| Terraform (`main.tf`)| Resource construction: runtime SA, Cloud Run service, IAM bindings on Terraform's own resources (run.invoker, cross-project AR reader, LB/DNS) |

The generated TF works with a builder SA that holds ONLY the 6
predefined functional roles, no `projectIamAdmin` or
`serviceUsageAdmin`. Adding either to the builder defeats the agent's
least-privilege custom-role model — the agent can impersonate the
builder via Cloud Build, so anything granted to the builder becomes
part of the agent's effective authority.

If a project-scope concern needs automation, add a step to
`admin_cloud_init` (run as Owner once), not a Terraform resource.

## Consequences

**Positive**

- Operators see the same Make targets regardless of topology — the
  collapsed and split cases share one operator interface.
- The custom role is diff-reviewable; tightening it doesn't require
  re-pivoting the bootstrap script.
- 30-day expiry on agent → custom-role forces credential rotation
  gracefully (`make admin-cloud-init` refreshes idempotently).

**Negative**

- A split topology adds cross-project IAM grants the operator must
  understand (each `_grant_role` call branches local-vs-cross).
- The first-time bootstrap requires Owner on each distinct project
  in the topology (not necessarily the same person for every project).

## Alternatives considered

- *Single SA with broad project roles* — what the pre-#141 scaffold
  did, and what dex-arb-agent + onchain-markets started with. Rejected:
  daily-handling risk is unbounded because the agent is project-wide
  Owner-adjacent.
- *Two SAs, but the deployer SA carries projectIamAdmin so TF can
  self-escalate* — what the pre-H7 onchain-markets bootstrap did
  (#116 original proposal). Rejected: TF self-escalation requires the
  builder to hold the very roles the custom role was trying to keep
  off the agent.

## Reference

- Issue `kunallimaye/lib-agents#141` — three-role topology + IAM hardening
- Worked example: `kunal-labs/onchain-markets/docs/decisions/ADR-017-per-component-deployment-topology.md`
- Origin postmortem: `kunal-labs/onchain-markets#44` (epic; 4 restructure passes)
