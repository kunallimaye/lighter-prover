# ADR-0008: The witness delivery plane — how a cell obtains its witness slice

**Status**: Proposed
**Date**: 2026-06-14
**Verified-at-tip**: `ba65782aa286d94bd6ba725473388d85ea3fbb08`
**Issues**: refs #61 (operationalizes ADR-0006 §3; consumes ADR-0003 §D6 + its
2026-06-13 amendment; supplies the seam for ADR-0004 §3.1's `witness_move`)

> **Scope banner.** **Design-now, build-maybe-later.** This ADR designs the
> *delivery mechanism* of the witness plane and the *instrumentation seam*
> that makes witness-acquisition cost separately accountable. Those two
> halves are **not gated** — they are cheap to specify and (the seam) cheap
> to stub. The **distributed-fetch performance measurement and tuning are
> GATED** on (a) the G2 witness-generation work producing varied synthetic
> witnesses (epic #121 / reconstructor under `tools/witness-reconstructor/`)
> and (b) a running fleet to fetch into (#75). This ADR **invents no
> fetch-cost number**; it specifies *how* `witness_move` will be measured,
> not *what* it will be.

> **Numbering note.** This ADR takes **0008** — the next free number,
> verified by listing `docs/decisions/` at the tip above: `ADR-0001`
> (container-topology), `ADR-0003` (prover-cell streaming), `ADR-0004`
> (unified recursive distribution), `ADR-0005` (L6 inner-wrapper KZG
> sidecar), `ADR-0006` (distributed-prover conductor), `ADR-0007` (GCP fleet
> bench). `ADR-0002` is **reserved** for #10's `ADR-0002-l4-l8-driver.md` and
> is deliberately left free. The conductor is **ADR-0006** (the
> distributed-prover-conductor doc), not ADR-0005 — ADR-0005 is the
> L6-inner-wrapper/KZG-sidecar doc (verified at this tip; the #61 owner
> comment that calls the conductor "ADR-0005" predates the PR #115 → 0006
> renumber recorded in ADR-0006 lines 14-26).

---

## 0. What this ADR is, and what it is not

**ADR-0006 §3 (the conductor) designs the witness *seam*** — it names the
cell-side `{height, witness_index}` resolution interface, the
`witness_fetch_ms` BENCH_EVENT field, the "source is a PARAMETERIZED input"
rule, and the `witness_move`-stays-UNMODELED conclusion. It deliberately
**stops at the seam**: ADR-0006 §3 (lines 176-186) states the source is
"local mounted corpus today … possibly a Lighter service later — TBD-by-#83"
and designs an interface that "holds **regardless of source**."

**This ADR designs the delivery mechanism that sits behind that seam** — the
concrete addressing scheme, where the store lives, how a remote cell reaches
its slice without polluting the prove path, how the local `bench_test.json`
path is the k=1 degenerate case, and exactly where the `witness_fetch_ms`
measurement plugs into the code. It is a **delivery design, not an
architecture change**: it adds no machine class, alters no dispatch tier, and
moves nothing off the seams ADR-0006 already drew.

**It is not** the perf study. Real fetch cost needs varied witnesses (G2)
and a fleet (#75); §3 below specifies the measurement plan and stops.

---

## 1. The delivery model — how a cell gets its witness slice

**Question answered:** given a cell's assigned chunk, how does it identify,
request, and obtain the **exact** witness slice it needs, without the
acquisition dominating the proof? **Serves:** `lag` (the `witness_move`
term in ADR-0004 §3.1, line 173).

### 1.1 Addressing — `{height, witness_index}` over a partitioned block

The conductor's inner tier (ADR-0006 §1.2, line 93) has the coordinator
**SPLIT** its block into `k = ceil(tx/S)` chunks and **push**-dispatch them
to k cells (ADR-0006 §1.2, line 94: "the coordinator **owns its block's
chunk set and fans out** to k cells"). A cell therefore receives a **chunk
work item**, not a whole block.

The witness address is the pair the issue and ADR-0003 §D6 already name:

```
witness_key = { height, witness_index }
```

- **`height`** identifies the block (the same `height` already carried on the
  stream-mode BENCH_EVENTs `StreamArrival` / `ChunkProven`, `bench/src/events.rs:78,102`).
- **`witness_index`** identifies the **chunk's slice within that block's
  witnesses**. The ADR-0003 §D6 amendment (2026-06-13, "§D6 correction
  (witness partitioning)", line 149) makes this mandatory and concrete:
  > "A single block's witnesses must be **PARTITIONABLE across the cells**
  > proving its chunks; today's whole-block mounted corpus is the **k=1
  > case**. Witness RESOLUTION is now per-chunk-partitionable, not
  > whole-block-on-one-cell."

  So `witness_index` is the **chunk ordinal** in the coordinator's SPLIT — it
  ranges `0 .. k-1` and selects the contiguous `S`-tx slice of the block's
  witness the cell will prove at L1. The coordinator, which owns the SPLIT
  (ADR-0006 §1.2) and thus the chunk→slice mapping, is the authority that
  assigns each cell its `witness_index`.

This is the **real** addressing scheme the #61 corpus exposes: a
corpus/lookup keyed by `{height, witness_index}` resolves to the witness
bytes (+ seed roots) for exactly one chunk.

### 1.2 What the dispatch carries — references, not bytes

The composition with the two-tier dispatch is already decided by ADR-0006
and the §D6 amendment, and this ADR adopts it verbatim:

- **The dispatch carries witness *references*, not witness *bytes*.**
  ADR-0006 §3 (line 196-197): "dispatch carries **witness references**, not
  witness bytes (§1.2; #75 coordinator spec)." ADR-0003 §D6 amendment (line
  140): "The dispatch carries **chunk work items + witness references**."
  The witness **never travels through the trace or the message bus** —
  ADR-0003 §D6 (line 77) and its amendment (line 149), reaffirmed; the
  reference (`{height, witness_index}`) is what crosses the wire, and the
  cell **pulls the bytes from its local store** by resolving that reference.

This is the crux that keeps the design consistent with §D6: the bus moves a
tiny key, not a witness, so the data-hygiene rule ("never on the trace or the
bus") holds by construction.

### 1.3 Where the store lives + why the fetch cannot dominate the proof

The hard constraint is ADR-0003 §D6 (line 76), **reaffirmed as STILL
OPERATIVE** by the 2026-06-13 amendment (line 151):

> "GCS is showback-only … **Never in the per-proof critical path** (a
> 100-300 ms GCS round-trip is a ~100% tax on a 0.5 s fold step)."

A per-chunk synchronous GCS GET is therefore **prohibited**: a 100-300 ms
fetch against the merge step (`merge_step = 0.2751 s`, ADR-0004 §3.2 line
188) or a single L1 chunk prove is a ~40-100% tax. The delivery model
respects this with a **store-local-to-the-cell** design, in priority order:

1. **Mounted read-only corpus (primary, k=1 and small-k).** A read-only
   **image layer or volume** mounted on the cell, resolved by
   `{height, witness_index}` via **local indexed lookup** — ADR-0003 §D6
   (line 77) and ADR-0006 §3 (line 180-182). The lookup is a local file /
   mmap read, not a network round-trip, so it carries **no GCS tax**. This
   "approximates production's lookup-from-local-store keyed by height" (issue
   #61, Context).
2. **Pre-fetch / pre-position (sustained-load, large-k).** When the corpus is
   larger than a cell's mount or the source is remote, the witness slice for
   a cell's assigned chunk is **pre-positioned before the chunk enters the
   prove path** — the fetch is overlapped with dispatch/queue time and with
   the cell's prior chunk's prove, so the *critical-path* witness cost is the
   **local** resolve, not the remote pull. The reference-carrying dispatch
   (§1.2) is what makes pre-fetch possible: the cell knows its
   `{height, witness_index}` the moment it dequeues, before it needs the
   bytes.
3. **Locality / cache.** A cell proving consecutive chunks of the same height
   (k=1, or co-located chunks) reuses the resolved block witness; the corpus
   mount is itself the cache for the k=1 case.

The design **must never** place a synchronous remote GET on the L1/L2/L4
path. Whether pre-fetch fully hides remote cost is an **empirical question
deferred to §3** (it needs a fleet to measure); what is *designed* here is
that the **critical-path** witness operation is a **local resolve**, and the
remote operation, if any, is **off the critical path** by construction.

### 1.4 The k=1 special case — today's `bench_test.json`

The single-machine bench today reads the whole block from a local file:

- `get_test_block_json_file("bench_test.json")` at
  `bench/src/bin/bench.rs:3485-3490` — `Path::new(".").join(file_name)` +
  `fs::read_to_string` + `serde_json::from_str`. Called from
  `bench/src/bin/bench.rs:502` (standard path) and
  `bench/src/bin/bench.rs:897` (stream path).

This is **exactly the k=1 case of the general design**: a single cell mounts
a single whole-block witness on local disk, resolves it by a trivial
`{height, witness_index}` (one block, one slice), and reads it locally with
no network. ADR-0006 §3 (line 180) names this "the k=1 building block";
ADR-0003 §D6 amendment (line 149) names it "today's whole-block mounted
corpus is the k=1 case." The general design therefore **degrades cleanly** to
what exists today: the addressing collapses to a constant, the partitioning
collapses to identity, and the store is the bundled file. The acceptance
criterion in #61 — "Existing stdin streaming mode unaffected when the corpus
is absent (falls back to current recycled-witness behaviour)" — is honoured
because the resolver's k=1 fallback **is** the current file read.

---

## 2. The `witness_fetch_ms` instrumentation seam

**Question answered:** where in the prove path is witness acquisition
measured and emitted, so it is **always separately accountable and
subtractable** from prover metrics? **Serves:** the measurability of
`witness_move` (ADR-0004 §3.2, line 186: "implement #61's corpus +
`witness_fetch_ms` … then read it directly").

### 2.1 The measurement point (exact, verified)

Witness acquisition happens at the **load seam**:
`get_test_block_json_file` (`bench/src/bin/bench.rs:3485-3490`), invoked at
`bench.rs:502` (standard) and `bench.rs:897` (stream). The
`witness_fetch_ms` measurement is the **wall time of the resolve-and-read
operation at that seam** — i.e. wrap the `{height, witness_index}` →
witness-bytes resolution (today: the file read; tomorrow: the mount lookup or
the pre-fetched-slice handoff) in an `Instant::now()` / `.elapsed()` pair,
exactly as the prove steps already do (e.g. the `l4_t = Instant::now()` /
`l4_t.elapsed()` pattern at `bench.rs:2990`+).

### 2.2 The schema point (exact, verified)

The BENCH_EVENT schema is the `BenchEvent` enum in `bench/src/events.rs`
(emitted by `events::emit`, `bench/src/events.rs:224`). `witness_fetch_ms`
is added as a **dedicated `Option<u64>` field** to the per-chunk prove
variants, so it sits next to the prove walls and is trivially subtractable:

- **`ChunkProven`** (`bench/src/events.rs:90`) — the stream-mode per-chunk
  event. This is the **primary** site: it already carries `height`
  (`events.rs:102`), `lag_ms`, `wall_ms`, and `chunk_idx`/`chunk_total`, so
  `witness_fetch_ms` joins the per-chunk lag accounting directly. With it,
  `lag_ms − witness_fetch_ms` isolates the pure prover lag — exactly the
  "subtractable from prover metrics" property #61 asks for.
- **`LayerProve`** (`bench/src/events.rs:43`) — the non-stream per-chunk /
  one-shot prove event (emitted at `bench.rs:632,747,794,992`). The same
  `Option<u64>` field is added here for the standard (`bench.rs:502`) path so
  witness cost is accountable outside stream mode too.

`witness_fetch_ms` is `Option<u64>` (serialized as JSON `null` when absent),
matching the existing optional-field convention (`cpu_ms`, `rss_mb_*`) and
keeping every existing consumer (fleet parser, `s-calibrate`) unaffected —
new field, additive, `null` until populated. This mirrors the **additive**
discipline the `L4Check` split already follows (`events.rs:165-170`:
"ADDITIVE … existing consumers … are unaffected").

### 2.3 Should the seam be stubbed *now*? — Recommendation: **yes, stub it now.**

The seam is **cheap to specify and cheap to stub**, and stubbing it now buys
real value at near-zero risk:

- **What stubbing means:** add `witness_fetch_ms: Option<u64>` to
  `ChunkProven` + `LayerProve`, wrap the `get_test_block_json_file` resolve in
  a timer, and emit the measured local-read time (which today is a real,
  if small and local, number — **not invented**; it is the actual
  `fs::read_to_string` wall). Where the resolve hasn't been routed through
  the timer yet, emit `null`. **No fetch-cost number is fabricated**: the
  field reads the *real* local cost or `null`, never a guess.
- **Why it's worth it now (not gated):**
  1. It is the one piece ADR-0004 §3.2 (line 186) and ADR-0006 §3 (line 198)
     both say is the prerequisite to *ever* measuring `witness_move` — landing
     the field now means the day G2 witnesses + a fleet arrive, the
     measurement is **already wired**, not a new code change racing the
     experiment.
  2. It is **additive and consumer-safe** (§2.2) — it cannot regress existing
     parsers.
  3. It makes the k=1 local read **already accountable**, so even today's
     single-machine numbers separate witness-load from prove.
- **The honest caveat:** the *local* `witness_fetch_ms` today is **not**
  `witness_move` — `witness_move` is the *distributed* acquisition cost, which
  stays UNMODELED until §3's measurement runs on a fleet. Stubbing the seam
  measures the **floor** (local resolve), labels it as such, and leaves the
  distributed term UNMODELED. That distinction must be preserved in any
  reporting (Discussion #58 honesty norm: "designed-from-measured vs
  designed-from-assumption").

---

## 3. The gated part — distributed fetch cost, and the measurement plan

**Question answered:** what is deferred, what unblocks it, and how will it be
measured when unblocked? **Serves:** the eventual *closure* (not the closure
itself) of `witness_move`.

**Deferred (build nothing now):** the real distributed-fetch cost and any
tuning of mount vs pre-fetch vs cache. This is `witness_move` proper — the
acquisition cost a cell pays in a *distributed* fleet, across *varied*
witnesses.

**What unblocks it (both required):**

1. **Varied synthetic witnesses (G2).** The Go witness reconstructor (epic
   #121, `tools/witness-reconstructor/`) generates structurally-valid blocks
   of varied shape/mix (Cancel #123, Modify #124 landed at this tip). A
   corpus of *varied* witnesses keyed by `{height, witness_index}` is the
   input the fetch must range over — a single 500-tx `bench_test.json`
   (`bench/bench_test.json`, present at this tip) is n=1 and cannot exercise
   slice-size or corpus-size variation.
2. **A running fleet (#75).** There is **nothing to fetch *into*** until a
   cell fleet exists. The conductor build (#75) is itself design-gated
   (ADR-0006 §7a); the witness perf study sits behind it.

**The measurement plan (ready to execute when unblocked — do not run now):**

| Step | What to measure | How (using the §2 seam) |
|---|---|---|
| M1 | **Local-resolve floor (k=1).** | Read `witness_fetch_ms` from the stubbed seam on the single-machine bench over the varied G2 corpus. Establishes the floor and the per-slice-size curve. |
| M2 | **Mounted-corpus resolve, on a cell.** | Run a fleet cell against a mounted read-only corpus; read `witness_fetch_ms` per `ChunkProven`. Compare to M1 — quantifies the mount-vs-bundled-file delta. |
| M3 | **Pre-fetch effectiveness.** | With reference-carrying dispatch (§1.2), measure `witness_fetch_ms` *on the critical path* (local resolve of a pre-positioned slice) vs the *off-path* pull wall. The design claim (§1.3) is that the critical-path number stays ≪ the prove walls; M3 confirms or refutes it. |
| M4 | **`witness_move` as a lag term.** | Feed the measured critical-path `witness_fetch_ms` into ADR-0004 §3.1's `per_block_lag` as the `witness_move` term. Only after M2-M3 is this a *measured* number; until then it stays UNMODELED. |

**Acceptance for the gated study (when run):** `witness_fetch_ms` on the
critical path is a small, *bounded, machine-tagged* fraction of the per-chunk
prove wall (the design target is "never dominates the proof", §1.3) — and the
number is reported **machine-tagged** (`c4a-highcpu-64` vs anything else,
never mixed; ADR-0006 §8 honesty ledger). **No target value is set here** —
setting it before measurement would be inventing the number this ADR refuses
to invent.

---

## 4. Fit + UNMODELED closure

**How this closes (sets up the closure of) `witness_move`.** `witness_move`
is the **one UNMODELED term** in ADR-0004 §3.1's `per_block_lag` (line 173:
`per_block_lag ≈ witness_move + …`), named UNMODELED in §3.2 (line 186) with
the explicit unblock: "implement #61's corpus + `witness_fetch_ms` BENCH_EVENT
field, then read it directly." This ADR:

- **designs the corpus delivery** (§1) — the addressing, store, and
  partitioning that make a `{height, witness_index}` resolve real;
- **places the `witness_fetch_ms` field** (§2) — the exact schema + code seam
  that makes the term *readable*;
- **specifies the measurement plan** (§3) — the steps that turn the readable
  field into a *measured* `witness_move`, once G2 + fleet land.

It therefore **sets up the closure** of `witness_move` and **closes it
outright only after §3's gated study runs**. Until then, per ADR-0004 §3.2 /
ADR-0006 §3 (line 205-206): `witness_move` stays **UNMODELED — named, not
invented.** No number appears in this ADR.

**Fit with the conductor (ADR-0006).** This is a delivery design inside the
seams ADR-0006 already drew, confirmed point-by-point:

- It uses ADR-0006 §1.2's inner-tier push dispatch and coordinator-owned
  SPLIT to define `witness_index` (§1.1) — **no dispatch change**.
- It adopts ADR-0006 §3 / §D6-amendment "dispatch carries witness
  references" verbatim (§1.2) — **no new wire payload**.
- It honours ADR-0006 §3's "source is a PARAMETERIZED input" by designing the
  *mechanism* source-independently: mount today, pre-fetch/locality for
  remote, the resolver interface unchanged whether backed by the corpus or a
  future Lighter service (witness SOURCE remains **TBD-by-#83**, ADR-0006 §7c)
  — **no source decision pre-empted**.
- It adds **no machine class**: the witness store is a mount/volume on the
  existing cell, not a new tier (consistent with ADR-0006 §5's MIG/GKE
  substrate and the §D7 platform seam).

**Proposed-amendment check (Discussion #58 — name conflicts, do not silently
override).** This design surfaces **no conflict** with ADR-0003 §D6 or
ADR-0006: it operates entirely within §D6's reaffirmed "GCS showback-only,
never in the per-proof critical path" rule (it puts the *resolve* local and
the *pull*, if any, off the critical path — §1.3) and within the §D6
amendment's "witnesses partitionable across cells, never on the trace/bus"
rule (it carries references, not bytes — §1.2). **No §D6 sentence is
overridden; none is proposed for amendment.** If a *future* source (the #83
Lighter service) cannot satisfy "local resolve on the critical path", that
would be a §D6 conflict to raise **then**, on #83 — flagged here as a watch
item, not amended now.

---

## 5. Honesty ledger (Discussion #58 norms)

- **No fetch-cost number invented.** `witness_move` stays UNMODELED; §3
  specifies *how* it'll be measured, not *what* it is. The only number the
  seam emits today is the **real** local `fs::read_to_string` wall (§2.3),
  explicitly labelled the *local floor*, not `witness_move`.
- **Designed-from-fact vs proposed is distinguished.** Facts are cited to
  file:line or ADR section: the load seam (`bench.rs:3485-3490`, called from
  :502/:897), the BENCH_EVENT schema (`events.rs:43,90,224`), the §D6 rules
  (ADR-0003 §D6 lines 76-77 + amendment lines 140,149,151), the dispatch
  shape (ADR-0006 §1.2 lines 93-94, §3 lines 176-206), the UNMODELED term
  (ADR-0004 §3.1 line 173, §3.2 line 186). The *delivery mechanism* (mount /
  pre-fetch / locality, §1.3) and the *measurement plan* (§3) are labelled
  **proposed**.
- **Machine discipline carried forward.** Any future `witness_fetch_ms`
  number must be machine-tagged and never mixed (ADR-0006 §8); §3's
  acceptance step says so explicitly.
- **Design only.** No code is built, no benchmark is run, nothing is
  provisioned **by this ADR**. The now-safe implementation slice (the §2 seam
  + the §1 k=1 resolver interface) is gated on the **maintainer decision**
  recorded against #61, not assumed here.
