#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Dry-run tests for render_pod_spec.py --fold-strategy wiring (#321 Phase 8).

The order-free REDUCTION path is the DEFAULT on GKE. These tests render the
fungible-pool manifests (seeder + leaf/agg workers) directly via the module's
render functions with a minimal fake `args`, then assert:

  * the DEFAULT (reduction) renders `--fold-strategy=reduction` (arg templated
    via `$(FOLD_STRATEGY)`) and a `FOLD_STRATEGY: "reduction"` env in the seeder
    and both fungible worker Deployments, and
  * the HEX opt-out (`--fold-strategy hex`) renders `FOLD_STRATEGY: "hex"`.

No cloud is touched: the render functions only string-format YAML to a temp dir.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_render_pod_spec_fold_strategy.py
  * python3 infra-as-code/scripts/tests/test_render_pod_spec_fold_strategy.py  (self-test)
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
  """A minimal args namespace covering the fields the render fns touch."""
  return SimpleNamespace(
      arch="c3d",
      radix=16,
      blocks=1,
      image="default",
      benchmark_id="dryrun",
      topic="work-topic",
      subscription="work-sub",
      event_topic="events-topic",
      event_subscription="events-sub",
      baseload=10,
      burst=0,
      ack_deadline=180,
      cpu="",
      memory="",
      fold_strategy=fold_strategy,
      output=output,
  )


def _render_seeder_and_fungible(fold_strategy):
  """Render the seeder + fungible manifests and return their text, keyed."""
  tmpdir = tempfile.mkdtemp(prefix="rps-fold-test-")
  output = os.path.join(tmpdir, "prover_pod_unit.rendered.yaml")
  args = _fake_args(fold_strategy, output)

  common = dict(
      project_id="proj",
      gsa_email="sa@proj.iam.gserviceaccount.com",
      gcs_bucket="bucket",
      image_uri="us-docker.pkg.dev/proj/repo/zkp-prover:default",
      leaf_chunk=1,
      gcs_relative_path="benchmark-reports/dryrun/default/c3d",
  )

  rps.render_seeder(args, **common)
  rps.render_fungible(
      args,
      common["project_id"],
      common["gsa_email"],
      common["gcs_bucket"],
      common["image_uri"],
      common["leaf_chunk"],
      "14",   # leaf_cpu
      "27Gi",  # leaf_mem
      "58",   # agg_cpu
      "110Gi",  # agg_mem
      common["gcs_relative_path"],
  )

  def _read(suffix):
    path = output.replace(".rendered.yaml", suffix)
    with open(path, "r", encoding="utf-8") as f:
      return f.read()

  return {
      "seeder": _read("-seeder.rendered.yaml"),
      "fungible": _read("-fungible.rendered.yaml"),
  }


# ---------------------------------------------------------------------------
# DEFAULT: reduction is threaded through the seeder + both fungible workers.
# ---------------------------------------------------------------------------
def test_reduction_default_renders_reduction():
  out = _render_seeder_and_fungible("reduction")

  # Seeder: templated arg + concrete env value.
  assert '- "--fold-strategy=$(FOLD_STRATEGY)"' in out["seeder"], \
      "seeder must pass --fold-strategy=$(FOLD_STRATEGY)"
  assert 'name: FOLD_STRATEGY' in out["seeder"]
  assert 'value: "reduction"' in out["seeder"], \
      "seeder FOLD_STRATEGY env must default to reduction"

  # Fungible leaf + agg workers: templated arg + concrete env value (twice).
  assert out["fungible"].count('- "--fold-strategy=$(FOLD_STRATEGY)"') == 2, \
      "both leaf + agg fungible workers must pass --fold-strategy=$(FOLD_STRATEGY)"
  assert out["fungible"].count('name: FOLD_STRATEGY') == 2, \
      "both fungible workers must declare a FOLD_STRATEGY env"
  assert out["fungible"].count('value: "reduction"') == 2, \
      "both fungible workers must default FOLD_STRATEGY to reduction"


# ---------------------------------------------------------------------------
# HEX opt-out: --fold-strategy hex renders hex end-to-end (opt-out preserved).
# ---------------------------------------------------------------------------
def test_hex_opt_out_renders_hex():
  out = _render_seeder_and_fungible("hex")

  assert 'name: FOLD_STRATEGY' in out["seeder"]
  assert 'value: "hex"' in out["seeder"], \
      "seeder FOLD_STRATEGY env must be hex when opted out"
  assert '- "--fold-strategy=$(FOLD_STRATEGY)"' in out["seeder"]

  assert out["fungible"].count('value: "hex"') == 2, \
      "both fungible workers must render FOLD_STRATEGY hex on opt-out"
  # Sanity: no lingering reduction default when hex is requested.
  assert out["seeder"].count('value: "reduction"') == 0
  assert out["fungible"].count('value: "reduction"') == 0


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
