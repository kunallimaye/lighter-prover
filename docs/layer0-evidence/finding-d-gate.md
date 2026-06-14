# Layer 0 — FINDING D gate REAL output (issue #177)

`LAYER0_FINDING_D=1 cargo test -p bench --release --test prestate_finding_d -- --nocapture --test-threads=1`

Host: AMD EPYC 7B13, 32 cores, 125 GiB. 862 REAL L1 proves, no stubbing.

```
test finding_d_per_tx_positional_prestate_all_chunks_prove ... [layer0] loaded bench_test.json: height=186974592 tx_count=500
[layer0] building S=1 L1 circuit for the sweep...
[layer0] starting S=1 sweep over 500 txs (REAL L1 proves; this is the long pole)...
[layer0]   sweep pos 0/500 (638 ms)  elapsed=639.786795ms
[layer0]   sweep pos 25/500 (600 ms)  elapsed=15.700285149s
[layer0]   sweep pos 50/500 (597 ms)  elapsed=30.693911325s
[layer0]   sweep pos 75/500 (598 ms)  elapsed=45.802431529s
[layer0]   sweep pos 100/500 (595 ms)  elapsed=60.869429854s
[layer0]   sweep pos 125/500 (601 ms)  elapsed=75.934916239s
[layer0]   sweep pos 150/500 (589 ms)  elapsed=90.832497175s
[layer0]   sweep pos 175/500 (601 ms)  elapsed=105.89817201s
[layer0]   sweep pos 200/500 (599 ms)  elapsed=121.015871314s
[layer0]   sweep pos 225/500 (606 ms)  elapsed=136.052379209s
[layer0]   sweep pos 250/500 (602 ms)  elapsed=151.085898894s
[layer0]   sweep pos 275/500 (613 ms)  elapsed=166.06109942s
[layer0]   sweep pos 300/500 (597 ms)  elapsed=181.128704665s
[layer0]   sweep pos 325/500 (636 ms)  elapsed=196.331148108s
[layer0]   sweep pos 350/500 (640 ms)  elapsed=212.763127161s
[layer0]   sweep pos 375/500 (601 ms)  elapsed=227.840525716s
[layer0]   sweep pos 400/500 (604 ms)  elapsed=242.844301461s
[layer0]   sweep pos 425/500 (597 ms)  elapsed=257.809943867s
[layer0]   sweep pos 450/500 (603 ms)  elapsed=272.771759082s
[layer0]   sweep pos 475/500 (590 ms)  elapsed=287.810573648s
[layer0]   sweep pos 499/500 (590 ms)  elapsed=302.278627368s
[layer0] sweep DONE: 501 snapshots (500 proves) in 302.278769008s
[layer0] === S=9 gate: k=55 chunks (effective_limit=495) ===
[layer0]   S=9 chunk 0/55: positional == known-good ✓
[layer0]   S=9 chunk 10/55: positional == known-good ✓
[layer0]   S=9 chunk 20/55: positional == known-good ✓
[layer0]   S=9 chunk 30/55: positional == known-good ✓
[layer0]   S=9 chunk 40/55: positional == known-good ✓
[layer0]   S=9 chunk 50/55: positional == known-good ✓
[layer0]   S=9 chunk 54/55: positional == known-good ✓
[layer0] === S=9 gate PASSED: all 55 chunks prove + match-known-good ===
[layer0] === S=4 gate: k=125 chunks (effective_limit=500) ===
[layer0]   S=4 chunk 0/125: positional == known-good ✓
[layer0]   S=4 chunk 10/125: positional == known-good ✓
[layer0]   S=4 chunk 20/125: positional == known-good ✓
[layer0]   S=4 chunk 30/125: positional == known-good ✓
[layer0]   S=4 chunk 40/125: positional == known-good ✓
[layer0]   S=4 chunk 50/125: positional == known-good ✓
[layer0]   S=4 chunk 60/125: positional == known-good ✓
[layer0]   S=4 chunk 70/125: positional == known-good ✓
[layer0]   S=4 chunk 80/125: positional == known-good ✓
[layer0]   S=4 chunk 90/125: positional == known-good ✓
[layer0]   S=4 chunk 100/125: positional == known-good ✓
[layer0]   S=4 chunk 110/125: positional == known-good ✓
[layer0]   S=4 chunk 120/125: positional == known-good ✓
[layer0]   S=4 chunk 124/125: positional == known-good ✓
[layer0] === S=4 gate PASSED: all 125 chunks prove + match-known-good ===
[layer0] FINDING D GATE PASSED for S=9 (k=56) AND S=4 (k=125) in 1470.955733747s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1471.10s
GATE_EXIT=0
```
