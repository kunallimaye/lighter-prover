
build:
	@echo ""
	@export RUSTFLAGS="-C target-cpu=native"
	cargo build --release --bin bench;
	@mv ../target/release/bench bench;

run: 
	@echo ""
	@./bench

build-and-run: build run

# --- feeder (producer, #48) ---
# Trace producer for the streaming bench. Contract: trace-format.md.
# stream-record / stream-replay live emission need network or a trace;
# feeder-test is fully offline (<1 min, no network, no sleeps).
.PHONY: stream-record stream-replay stream-peak stream-tx-mix feeder-test

FEEDER := python3 feeder/feeder.py

stream-record: ## Record live trace (network): OUT=trace.jsonl [DURATION=15m]
	@test -n "$(OUT)" || { echo "error: OUT=<trace.jsonl> required"; exit 1; }
	$(FEEDER) record --out $(OUT) $(if $(DURATION),--duration $(DURATION),)

stream-tx-mix: ## Capture mainnet tx-type mix (#128): [BLOCKS=N | HEIGHTS="LO HI"] | SAMPLE=1
	$(FEEDER) tx-mix \
	  $(if $(SAMPLE),--sample-block,) \
	  $(if $(BLOCKS),--blocks $(BLOCKS),) \
	  $(if $(HEIGHTS),--heights $(HEIGHTS),)

stream-replay: ## Replay a trace: TRACE=... [SPEED=N | RATE=TXS] [DURATION=15m]
	@test -n "$(TRACE)" || { echo "error: TRACE=<trace.jsonl> required"; exit 1; }
	$(FEEDER) replay --in $(TRACE) \
	  $(if $(SPEED),--speed $(SPEED),) \
	  $(if $(RATE),--target-rate $(RATE),) \
	  $(if $(DURATION),--duration $(DURATION),)

stream-peak: ## Synthesize peak-load trace: RATE=TXS DURATION=15m
	@test -n "$(RATE)" || { echo "error: RATE=<tx/s> required"; exit 1; }
	@test -n "$(DURATION)" || { echo "error: DURATION=<15m|900s> required"; exit 1; }
	$(FEEDER) synth-peak --rate $(RATE) --duration $(DURATION)

feeder-test: ## Run the offline feeder test suite (no network, <1 min)
	python3 -m unittest discover -s feeder/tests -v
# --- end feeder (producer, #48) ---

# --- synthetic block corpus (#165; Refs #128 #121 #144) ---
# Regenerate the {height, witness_index} MountedCorpus layout + manifest from
# the REAL mainnet load distribution. Band weights are recomputed by running
# scripts/trace-distribution/analyze.py on TRACE (no invented weights).
# Generation only — no proving, no cloud, no matching engine.
.PHONY: corpus-gen corpus-test

corpus-gen: ## Regenerate corpus: TRACE=/path/trace.jsonl [N=100 S=100] (or DOC=1 for #163 fallback)
	@test -n "$(TRACE)$(DOC)" || { echo "error: TRACE=<trace.jsonl> required (or DOC=1)"; \
	  echo "  fetch the real trace first, e.g.:"; \
	  echo "  gcloud storage cat gs://kunal-scratch-bench-fleet-runs/traces/2026-06-11T0204Z-15m-offpeak/trace_15m.jsonl > /tmp/trace_15m.jsonl"; \
	  exit 1; }
	python3 ../tools/corpus-gen/gen_corpus.py \
	  $(if $(TRACE),--trace $(TRACE),) \
	  $(if $(DOC),--from-doc,) \
	  $(if $(N),--n-blocks $(N),) \
	  $(if $(S),--chunk-size $(S),)

corpus-test: ## Verify the committed corpus loads via the MountedCorpus resolver
	cargo test --manifest-path Cargo.toml --test corpus_mount
# --- end synthetic block corpus (#165) ---

# --- streaming consumer (#49) ---
# stream-smoke: MANUAL real-proving smoke test for `bench --stream`.
# NOT part of any automated test target: circuit define alone takes
# minutes and each L1+L2 proof seconds. It pipes a tiny inline trace
# (3 small blocks at tx_per_proof=1) into the stream consumer and
# should produce stream_arrival, chunk_proven, and stream_summary
# events plus real proofs before exiting at trace EOF (--duration 120s
# is only a safety cap).
stream-smoke:
	@printf '%s\n' \
	  '{"provenance": {"generator": "replay --target-rate", "params": {"note": "inline stream-smoke"}, "generated_at": "2026-01-01T00:00:00Z"}}' \
	  '{"ts_ms": 1, "height": 1001, "tx_count": 2}' \
	  '{"ts_ms": 2, "height": 1002, "tx_count": 1}' \
	  '{"ts_ms": 3, "height": 1003, "tx_count": 1}' \
	| cargo run --release --bin bench -- --stream --tx-per-proof 1 --duration 120s

# stream-sweep: rate-ladder sweep driving `feeder.py replay` (issue
# #48, sibling PR) into `bench --stream`. Untestable until both PRs
# merge; see scripts/stream-sweep.sh for knobs (TRACE, RATES, ...).
stream-sweep:
	@bash ../scripts/stream-sweep.sh
# --- end streaming consumer (#49) ---
