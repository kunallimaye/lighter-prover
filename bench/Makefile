
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
.PHONY: stream-record stream-replay stream-peak feeder-test

FEEDER := python3 feeder/feeder.py

stream-record: ## Record live trace (network): OUT=trace.jsonl [DURATION=15m]
	@test -n "$(OUT)" || { echo "error: OUT=<trace.jsonl> required"; exit 1; }
	$(FEEDER) record --out $(OUT) $(if $(DURATION),--duration $(DURATION),)

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
