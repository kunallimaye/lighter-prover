#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Dry-run tests for render_pod_spec.py ROOT-PROOF-WAIT wiring (#321 Phase 9).

BUG B2: the coordinator hardcoded the HEX root proof name `tree_L{depth}_N0.proof`
in its `while [ ! -f ... ]` wait loop (and in the ROOT_PROOF_NAME plan var). A
REDUCTION run (the GKE default since #321 Phase 8) never produces that file — it
commits `reduction_0_{padded-1}.proof` (the interval [0, padded-1] the gate treats
as RootReached) — so the coordinator spun forever (the attempt-46 GKE timeout).

These tests render the coordinator manifest directly via the module's render
function with a minimal fake `args`, then assert the root-wait references the
CORRECT key per strategy:

  * --fold-strategy reduction -> `reduction_0_{padded-1}.proof`
    (N=500 -> reduction_0_511.proof; N=125 -> reduction_0_127.proof), and
  * --fold-strategy hex       -> `tree_L{depth}_N0.proof` (UNCHANGED).

Also unit-tests the pure `padded_leaf_count` / `root_proof_name` helpers against
the Rust `bench::transport::reduction_root_key` spec.

No cloud is touched: the render function only string-formats YAML to a temp dir.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_render_pod_spec_root_wait.py
  * python3 infra-as-code/scripts/tests/test_render_pod_spec_root_wait.py  (self-test)
"""

import importlib.util
import os
import sys
import tempfile
from types import SimpleNamespace

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "render_pod_spec.py")

# Load render_pod_spec by path (it is a script, not a package).
_spec = importlib.util.spec_from_file_location("render_pod_spec", _SCRIPT)
rps = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rps)


def _fake_args(fold_strategy, output):
  """A minimal args namespace covering the fields render_coordinator touches."""
  return SimpleNamespace(
      arch="c3d",
      radix=16,
      fold_strategy=fold_strategy,
      output=output,
  )


def _render_coordinator(fold_strategy, leaf_count, depth):
  """Render the coordinator manifest and return its text."""
  tmpdir = tempfile.mkdtemp(prefix="rps-rootwait-test-")
  output = os.path.join(tmpdir, "prover_pod_unit.rendered.yaml")
  args = _fake_args(fold_strategy, output)
  rps.render_coordinator(
      args,
      "proj",                                              # project_id
      "sa@proj.iam.gserviceaccount.com",                   # gsa_email
      "bucket",                                            # gcs_bucket
      "us-docker.pkg.dev/proj/repo/zkp-prover:default",    # image_uri
      1,                                                   # leaf_chunk
      "benchmark-reports/dryrun/default/c3d",              # gcs_relative_path
      leaf_count,
      depth,
  )
  # render_coordinator writes the coordinator wait job to the -coordinator
  # rendered file (see render_coordinator's out_path logic).
  out_path = output.replace(".rendered.yaml", "-coordinator.rendered.yaml")
  with open(out_path, "r", encoding="utf-8") as f:
    return f.read()


# ---------------------------------------------------------------------------
# Pure helper unit tests: match bench::transport::reduction_root_key spec.
# ---------------------------------------------------------------------------
def test_padded_leaf_count_matches_rust_spec():
  cases = [(1, 1), (2, 2), (4, 4), (5, 8), (8, 8), (125, 128), (500, 512)]
  for n, expected in cases:
    assert rps.padded_leaf_count(n) == expected, \
        f"padded_leaf_count({n}) must be {expected}"


def test_root_proof_name_reduction_and_hex():
  # Reduction: reduction_0_{padded-1}.proof
  assert rps.root_proof_name("reduction", depth=3, leaf_count=500) == \
      "reduction_0_511.proof"
  assert rps.root_proof_name("reduction", depth=2, leaf_count=125) == \
      "reduction_0_127.proof"
  assert rps.root_proof_name("reduction", depth=1, leaf_count=4) == \
      "reduction_0_3.proof"
  # Hex: tree_L{depth}_N0.proof (unchanged)
  assert rps.root_proof_name("hex", depth=3, leaf_count=500) == \
      "tree_L3_N0.proof"
  assert rps.root_proof_name("hex", depth=2, leaf_count=125) == \
      "tree_L2_N0.proof"


# ---------------------------------------------------------------------------
# Rendered coordinator manifest: root-wait references the right key by strategy.
# ---------------------------------------------------------------------------
def test_reduction_coordinator_waits_for_reduction_root():
  # N=500 (500 txs, chunk 1) -> padded 512 -> reduction_0_511.proof
  depth = rps.tree_depth(500, 16)
  out = _render_coordinator("reduction", leaf_count=500, depth=depth)
  assert "reduction_0_511.proof" in out, \
      "reduction coordinator must wait for reduction_0_511.proof (N=500)"
  # It must NOT wait for the hex root that a reduction run never produces.
  assert f"tree_L{depth}_N0.proof" not in out, \
      "reduction coordinator must NOT wait for the hex tree_L{depth}_N0.proof"
  # Also thread --fold-strategy reduction to the root-coordinator harvest.
  assert "--fold-strategy reduction" in out, \
      "root-coordinator invocation must carry --fold-strategy reduction"


def test_reduction_coordinator_waits_for_reduction_root_125():
  # N=125 -> padded 128 -> reduction_0_127.proof
  depth = rps.tree_depth(125, 16)
  out = _render_coordinator("reduction", leaf_count=125, depth=depth)
  assert "reduction_0_127.proof" in out, \
      "reduction coordinator must wait for reduction_0_127.proof (N=125)"


def test_hex_coordinator_waits_for_hex_root_unchanged():
  depth = rps.tree_depth(500, 16)
  out = _render_coordinator("hex", leaf_count=500, depth=depth)
  assert f"tree_L{depth}_N0.proof" in out, \
      "hex coordinator must wait for tree_L{depth}_N0.proof (unchanged)"
  assert "reduction_0_" not in out, \
      "hex coordinator must NOT reference any reduction root key"
  assert "--fold-strategy hex" in out, \
      "root-coordinator invocation must carry --fold-strategy hex"


def _run_self_test():
  tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
  failures = 0
  for t in tests:
    try:
      t()
      print(f"[PASS] {t.__name__}")
    except AssertionError as e:
      failures += 1
      print(f"[FAIL] {t.__name__}: {e}")
    except Exception as e:  # noqa: BLE001
      failures += 1
      print(f"[ERROR] {t.__name__}: {e!r}")
  print(f"\n{len(tests) - failures}/{len(tests)} passed")
  return 1 if failures else 0


if __name__ == "__main__":
  sys.exit(_run_self_test())
