Once `make build` is run and the `bench` executable is generated, that and `bench_test.json` can be copied across machines being placed at the same level, and run independently from the repository.

## Supported `--tx-per-proof` range

This bench is validated for `--tx-per-proof ∈ {1, 2, 3, 4, 5, 6}` on
upstream commit `5bbb307`. Larger values trigger an unrelated bug in
the chain-recursion circuit sizing (`log_gates = 14` is insufficient
for the resulting verifier). See
[issue #8](https://github.com/kunallimaye/lighter-prover/issues/8)
for the analysis and proposed fixes.

The default `--tx-per-proof 4` matches upstream's production setting
(`bench/src/bin/bench.rs:33`, `build_circuits.sh:21`).
