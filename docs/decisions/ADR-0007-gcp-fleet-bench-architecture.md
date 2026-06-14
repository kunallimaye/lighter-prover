# ADR-0007: GCP fleet bench architecture

- **Status**: Accepted
- **Date**: 2026-06-10
- **Issue**: #11
- **Supersedes**: —
- **Superseded by**: —

> **Renumbering note (#68).** This ADR was originally filed as `ADR-0001`,
> colliding with `ADR-0001-container-topology.md`. Per #68 the collision was
> resolved by keeping container-topology at 0001 (it is the canonical
> `ADR-0001` referenced from the README) and renumbering this file to the
> next free number, **0007** (0002 is reserved for #10's
> `ADR-0002-l4-l8-driver.md`; 0003–0006 are taken). Content is unchanged.

## Context

We need to compare `bench` (the zk-rollup prover benchmark) performance
across multiple GCP machine shapes to inform the homogeneous-worker
deployment model planned in issue #2 (containerization) and #3 (work
sharding). The local AMD EPYC 7B13 baseline (32c / 125 GiB) at
Discussion #6 establishes reference numbers; we now need cross-shape data
across 3 CPU architectures: Google Axion (`c4a-*`, `n4a-*`), Ampere Altra
(`t2a-*`), and AMD Turin (`n4d-*`, `c4d-*`).

The work is embarrassingly parallel (each shape is independent), runs for
a known bounded duration (≤4h per S, ~1h total per VM), and produces
text-format log artifacts plus a `machine-info.txt` summary.

Constraints from the `kl-ai-workstation` project:

- No `default` VPC. All VMs must attach to `ai-workstation-ws-net` /
  `ai-workstation-ws-subnet` with `--no-address` (Cloud NAT egress only).
- Shielded VM is enforced by org policy
  (`--shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring`
  mandatory).
- Cloud NAT is IPv4-only; `apt-get update` hangs unless `Acquire::ForceIPv4`
  is set.
- The orchestrator runs as a workstation SA that cannot directly create
  Compute resources; it must impersonate `bench-sweep@...` for everything.
- GCE T2A had a stockout in `us-central1-a` during pilot (`us-central1-b`
  worked); other shapes have varied zone availability.

## Decisions

### Decision 1: Ephemeral parallel VMs (not Cloud Run Jobs, not GKE, not a long-lived shared cluster)

Each machine shape gets a freshly provisioned GCE VM. All shapes provision
in parallel. Each VM is single-use and auto-deletes on completion.

**Rationale**: We need machine-type-specific shapes, which Cloud Run Jobs
(24h max, no shape control) and Kubernetes (forces a shared node pool or
heavy taint/toleration plumbing) make awkward. The workload is bounded
and embarrassingly parallel, so the spin-up/spin-down overhead is amortized
against ~1h of useful work per VM.

**Alternatives considered**:
- *Cloud Run Jobs*: rejected — 24h cap is fine but no per-job machine-type
  selection; we'd be limited to whatever Cloud Run's worker pool runs.
- *GKE*: rejected — overkill for 10 single-use machines; node taints +
  affinity for 10 different shapes would be ugly.
- *Single long-lived multi-shape cluster*: rejected — no way to mix
  arm64 + x86_64 + multiple sizes cleanly without per-shape node pools,
  which is just N parallel VMs with extra ceremony.

### Decision 2: Orchestrator-side teardown (not in-VM self-delete)

When a VM finishes its sweep, it uploads results + writes a `_DONE`
sentinel to GCS. The orchestrator polls GCS every 60s; when it sees a
sentinel it issues `gcloud compute instances delete` from outside the VM.

**Rationale**: The pilot proved that in-VM self-delete via
`gcloud compute instances delete $(hostname)` requires the VM's runtime SA
to have `roles/compute.instances.delete`. The default Compute SA does not
have this role, and `bench-sweep` (which provisions VMs) cannot grant it —
`bench-sweep` lacks `setIamPolicy` permission. The orchestrator already
has `compute.instanceAdmin.v1` (granted to `bench-sweep`) so it can do
the delete trivially.

The `--max-run-duration=8h --instance-termination-action=DELETE` flag
on each instance is a belt-and-suspenders safety net: even if the
orchestrator process dies and never sees the sentinel, GCE will delete
the VM at the 8h mark.

### Decision 3: Build-on-VM with `RUSTFLAGS="-C target-cpu=native"`

Each VM does its own cold `cargo build --release -p bench --bin bench`.
We do NOT cross-compile centrally or pre-bake images.

**Rationale**:
- **Most honest per-shape numbers**: `target-cpu=native` lets the
  compiler tune for the actual CPU (Axion vs Altra vs Turin have very
  different SIMD/vector capabilities, instruction latencies, cache sizes).
  Cross-compiling against a generic `aarch64-unknown-linux-gnu` or
  `x86_64-v3` baseline would understate the strongest shapes.
- **No image-registry plumbing**: we'd need a multi-arch registry, image
  promotion pipeline, and per-shape image refresh hook for SHA changes.
  Build-on-VM eliminates all of this.
- **Affordable**: cold build is ~70s on the pilot t2a-standard-32 and
  should be ≤90s on stronger shapes. At ~$0.02/min that's negligible
  vs the ~50min sweep that follows.

**Alternatives considered**:
- *Central cross-compile + scp binary*: rejected — three architectures
  × five CPU sub-families would mean cross-compile harnesses for every
  combination; lose `target-cpu=native` advantage.
- *Pre-baked images per shape*: rejected — adds image-management
  overhead; image refresh becomes a separate workflow gated on SHA
  changes; defeats the "one command runs the whole fleet at any SHA"
  goal.

### Decision 4: Text log parser (not JSONL adapter)

`parse-bench-log.sh` targets the current `info!`-emitted text format
(regex on `TOTAL`/`AVERAGE`/`BENCH_META` lines and Rust `Duration`
debug-format tokens).

**Rationale**: That is what `bench` emits today on `main`. Issue #9
proposes a structured JSONL `BENCH_EVENT` format but has not landed;
designing the parser around an unland format would block this toolkit.

**Migration plan**: When #9 lands, add a JSONL parser path that
auto-detects format and emits the same TSV row schema. The renderer
takes TSV in and is format-agnostic.

### Decision 5: Single Discussion in `Show and tell` + back-link comment on #6 (not per-machine, not edit-in-place on #6)

`publish` creates **one** Discussion containing all 10 shapes' results
with a single cross-shape comparison table, then posts a comment on
Discussion #6 with the new Discussion's URL.

**Rationale**:
- *Cross-shape comparison is the headline*. A single Discussion lets a
  reader scan all shapes against the EPYC baseline in one table.
- *Per-machine Discussion spam*: rejected — 10 new Discussions per run
  would flood `Show and tell` and lose the comparison angle.
- *Edit Discussion #6 in place*: rejected — #6 is the canonical local
  baseline; mixing fleet results into it loses provenance. The
  back-link comment provides discoverability without overwriting.

### Decision 6: Shell scripts with `gcloud` (not Terraform)

`run-fleet.sh` + `lib/*.sh` use raw `gcloud` calls wrapped in an
impersonation helper.

**Rationale**: Every VM is single-use; there is no long-lived state to
manage. Terraform's strengths (state file, drift detection, declarative
diffs) buy nothing when the desired state is "no resources exist
anymore" 1h after the run starts. The toolkit's state file is the GCS
bucket + the per-run state directory under `/tmp/bench-fleet-runs/`.

**Alternatives considered**:
- *Terraform with ephemeral workspaces*: rejected — state lifecycle
  mismatched to workload; we'd be `terraform apply`-ing and immediately
  `terraform destroy`-ing; the state file becomes an artifact to garbage-
  collect with no value.
- *Pulumi / CDK*: same critique as Terraform.

### Decision 7: `bench-sweep` SA + impersonation (not direct grants to workstation SA)

Every `gcloud` / `gcloud storage` call in the toolkit goes through
`--impersonate-service-account=bench-sweep@...`. The workstation SA itself
has no Compute / Storage permissions; it only has
`roles/iam.serviceAccountTokenCreator` on `bench-sweep`.

**Rationale**: The workstation SA is broad-use (interactive IDE,
arbitrary scripts). Granting it `compute.instanceAdmin.v1` directly
would blast-radius any compromise across all of Compute. `bench-sweep`
is purpose-built: it can create + delete VMs in this project and write to
the bench bucket, nothing else. Token-creator delegation keeps the
workstation SA narrowly scoped.

### Decision 8: `us-central1-*` zones, IPv4 apt fix, shielded VM flags, custom VPC flags

The toolkit hardcodes (or auto-applies) the project-specific:
- VPC: `--network=ai-workstation-ws-net --subnet=ai-workstation-ws-subnet --no-address`
- Shielded: `--shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring`
- IPv4 apt fix: startup script writes `Acquire::ForceIPv4 "true";` to
  `/etc/apt/apt.conf.d/99force-ipv4` before any `apt-get update`.
- Zones: preferred zones per shape are all `us-central1-*` (with one
  fallback to `us-east*` available in the TSV column).

**Rationale**: All four are forced by the `kl-ai-workstation` project's
org policies and network topology. Discovering them on the first run
cost real wall-clock during the pilot (apt hung for ~10 minutes before
diagnosis). Documenting them in code AND in this ADR ensures future
operators don't relearn the same lessons.

## Consequences

### Positive

- One command (`run-fleet.sh run --yes`) provisions, monitors, and
  collects all 10 shapes.
- Zero long-lived GCP resources between runs. Costs only what's spent
  during the ~1h fleet wall-clock.
- Trivial to extend to a 4th architecture (Intel Sapphire/Granite
  Rapids) by adding rows to `machines.tsv`.
- The parser + renderer have fixture-based tests so format changes
  break loudly in CI rather than silently in production.

### Negative

- Per-VM cold build wastes ~70-90s of vCPU time. (Mitigated by build
  being ≤2% of total wall.)
- Discussion bodies have a 65KB GitHub cap; `tests/test-render.sh`
  enforces a 60KB warning ceiling. If we ever add a 4th or 5th
  architecture the per-machine `<details>` blocks will need trimming.
- `bench-sweep` SA + impersonation adds one extra hop to debug ("why
  didn't this gcloud call work?" → "did you remember to impersonate?").

### Follow-ups

- When #9 lands (JSONL `BENCH_EVENT` output), add a JSONL parser path
  to `parse-bench-log.sh` and gate on `head -1 | grep -q BENCH_EVENT`.
- When #8 lands (chain circuit `log_gates=15`), add S∈{8,16,32} to
  `DEFAULT_SVALUES` in `run-fleet.sh`.
- Consider spot-VM mode (`--provisioning-model=SPOT`) for ~60%
  savings. Requires monitor.sh to handle preemption gracefully (retry
  on new instance, possibly different zone).

## Addendum: Org-policy relaxations (post-initial-scaffold)

After the initial scaffolding (commit `b0c6bb5`), the user relaxed two project-level org policies on `kl-ai-workstation` to simplify the toolkit:

| Constraint | Previous state | New state | Effect on toolkit |
|---|---|---|---|
| `constraints/compute.requireShieldedVm` | enforced | disabled | Dropped `--shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring` from `provision.sh` |
| `constraints/compute.vmExternalIpAccess` | "Deny All" | "Allow All" | Dropped `--no-address` from `provision.sh`; dropped `Acquire::ForceIPv4` hack from `vm-startup.sh.tmpl` (apt now reaches mirrors directly over the VM's public IP, no Cloud NAT hop) |

The custom VPC (`ai-workstation-ws-net`/`ai-workstation-ws-subnet`) remains the only VPC in the project — `constraints/compute.skipDefaultNetworkCreation` is a one-shot policy enforced at project-creation time and relaxing it now does not retroactively create a `default` VPC. Toolkit continues to pass `--network` and `--subnet` flags.

If this toolkit is ever ported to a different GCP project where the org policies are still enforced, restore the dropped flags from git history (commit `b0c6bb5`).

## Addendum: Makefile operator interface

Added a root-level `Makefile` with `fleet-*` targets wrapping `run-fleet.sh`:
`fleet-quota-check`, `fleet-run-dry`, `fleet-run`, `fleet-status`,
`fleet-collect`, `fleet-publish`, `fleet-teardown`. This is the recommended
operator interface; `run-fleet.sh` remains directly callable for non-default
flows (subset of machines, alternate git refs, custom confirmation behavior).
The Makefile passes `--yes` to `fleet-run` to skip the prompt — the underlying
script's cost-estimate print is the safety gate.

## Addendum: Container pivot + move to kunal-scratch (#33, June 2026)

**Status: supersedes Decision 3 (build-on-VM), Decision 7 (bench-sweep
impersonation), and the kl-ai-workstation-specific parts of Decision 8.**

### All infrastructure moves to `kunal-scratch`

The `kl-ai-workstation` environment broke irrecoverably (#32: the
`bench-sweep` SA was deleted and the results bucket removed). Rather
than restore it, ALL fleet infrastructure — Cloud Build, Artifact
Registry (`us-central1-docker.pkg.dev/kunal-scratch/lighter-prover`),
the results bucket (`gs://kunal-scratch-bench-fleet-runs`), and the VMs
themselves — now lives in `kunal-scratch`. Configuration flows through
the repo-root `config.toml` ([gcp.defaults] + the new `[fleet]`
section); the toolkit derives every project-specific value from it and
hardcodes nothing. The fleet's IAM needs are bootstrapped by idempotent
steps appended to `admin-cloud-init` (scripts/cloud.sh). Impersonation
is gone: `BENCH_SWEEP_SA` defaults to empty and the orchestrator's
active account is the acting identity. kunal-scratch uses the AUTO-mode
`default` VPC (no custom network flags needed beyond
`--network=default --subnet=default`).

### Containers replace on-VM builds

Decision 3 (build on each VM with `-C target-cpu=native`) is reversed.
The fleet now runs Container-Optimized OS VMs that pull prebuilt images
from Artifact Registry:

* **Kills the on-VM build failure class entirely** — toolchain install
  flakes, apt mirror issues, build-vs-binary inconsistencies (#24's
  instant rc=101 mystery), and the ~15-20 min per-VM toolchain+compile
  tax all disappear. What ran in CI is bit-for-bit what runs on the VM
  (image digest recorded in machine-info.txt).
* **Native-codegen honesty is preserved** via explicit per-microarch
  tags instead of `native`: the 10 GCE shapes collapse onto 3
  microarchitectures, each with its own image variant —
  `:<sha>-znver5` (c4d/n4d, AMD Turin), `:<sha>-neoverse-v2` (c4a/n4a,
  Google Axion), `:<sha>-neoverse-n1` (t2a, Ampere Altra). A portable
  multi-arch `:<sha>`/`:latest` manifest serves dev/Cloud Run.
* **The per-S collection contract moves into the container**: each
  worker container uploads its own `bench.log` + `bench.jsonl` + DONE
  to `<run-id>/<machine>/S<N>/` (the #25 entrypoint contract). The VM
  writes only fleet-level artifacts and the `_DONE` sentinel the
  monitor polls (layout unchanged).

### Cross-compilation, not QEMU

Cloud Build has **no ARM workers** (verified June 2026: the default
pool offers e2/n1 x86 only; private pools offer e2/n2d/c3 x86 only).
The PR #30 pipeline compiled the arm64 half of the manifest under QEMU
emulation — a 3-5× wall-time penalty on a Rust workload this heavy.
The #33 pipeline cross-compiles instead: the builder stage always runs
natively on the x86 worker (`FROM --platform=$BUILDPLATFORM`, rustup
aarch64 target, `gcc-aarch64-linux-gnu` linker) and emits aarch64 code
at native compile speed. QEMU is retained ONLY for the arm64 runtime
stage (debian-slim + tini + google-cloud-cli apt installs — minutes,
cached, and shared across all arm64 variants). Two arch gates prevent
a repeat of the pre-#30 `exec format error` fleet wipeout: a `file(1)`
assertion inside the builder stage, and a verify step in
cicd/cloudbuild.yaml that extracts the binary from every pushed tag
(docker create/cp — no execution) and fails on ELF-arch mismatch.
