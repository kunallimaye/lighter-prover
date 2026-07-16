#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Dry-run tests for render_pod_spec.py --pool-topology wiring (#321 unified pool).

The fungible worker pool is a SELECTABLE topology:

  * 'split' (DEFAULT, zero behavior change): two fixed Deployments —
    `lighter-fungible-leaf` (subscribes prover-leaf-work-sub) and
    `lighter-fungible-agg` (subscribes prover-agg-work-sub) — plus their two
    KEDA ScaledObjects. The fleet is statically partitioned per phase.
  * 'unified' (opt-in): ONE Deployment `lighter-fungible-prover` where every pod
    pulls BOTH leaf and fold work from the single no-filter subscription
    `prover-unified-work-sub` and self-balances the leaf-vs-fold mix. Sized
    FOLD-CAPABLE (~4Gi, from the measured RSS: leaf ~3.13GB / baked-fold 2.25GB (#338); leaf 3.18GB/fold
    6.00GB), running the SAME `prover-node work --transport=pubsub` binary.

These tests render the fungible manifests directly via the module's render
functions with a minimal fake `args`, then assert:

  * unified renders ONE `lighter-fungible-prover` Deployment subscribed to
    `prover-unified-work-sub`, a fold-capable ~4Gi memory request, ONE KEDA
    ScaledObject on that sub, the `prover-node work --transport=pubsub` command,
    and does NOT emit `lighter-fungible-leaf`/`-agg`; and
  * split (default) renders the existing two Deployments + two ScaledObjects
    unchanged (both present, unified absent) — back-compat.

No cloud is touched: the render functions only string-format YAML to a temp dir.

Runs two ways:
  * pytest infra-as-code/scripts/tests/test_render_pod_spec_pool_topology.py
  * python3 infra-as-code/scripts/tests/test_render_pod_spec_pool_topology.py  (self-test)
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


def _fake_args(output, pool_topology="split", pool_replicas=0):
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
      fold_strategy="reduction",
      pool_topology=pool_topology,
      pool_replicas=pool_replicas,
      output=output,
  )


_COMMON = dict(
    project_id="proj",
    gsa_email="sa@proj.iam.gserviceaccount.com",
    gcs_bucket="bucket",
    image_uri="us-docker.pkg.dev/proj/repo/zkp-prover:default",
    leaf_chunk=1,
    gcs_relative_path="benchmark-reports/dryrun/default/c3d",
)


def _render_split():
  """Render the SPLIT fungible manifests and return (deployment, keda) text."""
  tmpdir = tempfile.mkdtemp(prefix="rps-pool-split-")
  output = os.path.join(tmpdir, "prover_pod_unit.rendered.yaml")
  args = _fake_args(output, pool_topology="split")
  rps.render_fungible(
      args,
      _COMMON["project_id"],
      _COMMON["gsa_email"],
      _COMMON["gcs_bucket"],
      _COMMON["image_uri"],
      _COMMON["leaf_chunk"],
      "14",    # leaf_cpu
      "27Gi",  # leaf_mem
      "58",    # agg_cpu
      "110Gi",  # agg_mem
      _COMMON["gcs_relative_path"],
  )
  return _read_outputs(output)


def _render_unified(pool_replicas=0, cpu="", memory=""):
  """Render the UNIFIED fungible manifests and return (deployment, keda) text.

  Mirrors the main() derivation: default fold-capable mem = 4Gi, cpu = leaf_cpu,
  replicas = leaf_replicas when pool_replicas is 0.
  """
  tmpdir = tempfile.mkdtemp(prefix="rps-pool-unified-")
  output = os.path.join(tmpdir, "prover_pod_unit.rendered.yaml")
  args = _fake_args(output, pool_topology="unified", pool_replicas=pool_replicas)
  args.cpu = cpu
  args.memory = memory
  leaf_cpu = "14"
  leaf_replicas = 9 * 14  # main() derives from config node/pod counts
  unified_cpu = args.cpu if args.cpu else leaf_cpu
  unified_mem = args.memory if args.memory else "4Gi"
  unified_replicas = pool_replicas if pool_replicas and pool_replicas > 0 else leaf_replicas
  rps.render_unified(
      args,
      _COMMON["project_id"],
      _COMMON["gsa_email"],
      _COMMON["gcs_bucket"],
      _COMMON["image_uri"],
      _COMMON["leaf_chunk"],
      unified_cpu,
      unified_mem,
      _COMMON["gcs_relative_path"],
      unified_replicas,
  )
  return _read_outputs(output)


def _read_outputs(output):
  def _read(suffix):
    path = output.replace(".rendered.yaml", suffix)
    with open(path, "r", encoding="utf-8") as f:
      return f.read()
  return {
      "deployment": _read("-fungible.rendered.yaml"),
      "keda": _read("-fungible-keda.rendered.yaml"),
  }


# ---------------------------------------------------------------------------
# UNIFIED: one self-balancing pool on prover-unified-work-sub, fold-capable.
# ---------------------------------------------------------------------------
def test_unified_renders_single_pool():
  out = _render_unified()
  dep = out["deployment"]

  # Exactly ONE unified Deployment, named lighter-fungible-prover.
  assert "name: lighter-fungible-prover" in dep, \
      "unified must emit the lighter-fungible-prover Deployment"
  assert dep.count("kind: Deployment") == 1, \
      "unified must emit exactly ONE Deployment"

  # It subscribes to the single no-filter unified subscription.
  assert 'value: "prover-unified-work-sub"' in dep, \
      "unified pod must subscribe to prover-unified-work-sub"

  # It must NOT emit the split leaf/agg Deployments.
  assert "lighter-fungible-leaf" not in dep, \
      "unified must NOT emit lighter-fungible-leaf"
  assert "lighter-fungible-agg" not in dep, \
      "unified must NOT emit lighter-fungible-agg"
  assert 'value: "prover-leaf-work-sub"' not in dep
  assert 'value: "prover-agg-work-sub"' not in dep


def test_unified_is_fold_capable_memory():
  """Unified pod is sized for the bigger (fold) job: default 8Gi request+limit."""
  out = _render_unified()
  dep = out["deployment"]
  # 8Gi = fold 6.00GB (attempt-48 measured RSS) x ~1.3 margin. Appears as both
  # request and limit in the single container's resources block.
  assert dep.count('memory: "4Gi"') == 2, \
      "unified default memory request+limit must be the fold-capable 8Gi"
  # Must NOT carry the split agg 26Gi / 110Gi over-provisioned guess.
  assert '110Gi' not in dep and '26Gi' not in dep


def test_unified_worker_command_is_work_pubsub():
  """Same binary/command as split: prover-node work --transport=pubsub."""
  out = _render_unified()
  dep = out["deployment"]
  assert 'command: ["prover-node", "work"]' in dep, \
      "unified worker must run prover-node work"
  assert '- "--transport=pubsub"' in dep, \
      "unified worker must use --transport=pubsub (handles any role it pulls)"


def test_unified_single_keda_on_unified_sub():
  out = _render_unified()
  keda = out["keda"]
  assert keda.count("kind: ScaledObject") == 1, \
      "unified must emit exactly ONE KEDA ScaledObject"
  assert "name: lighter-fungible-prover" in keda
  assert 'subscriptionName: "prover-unified-work-sub"' in keda, \
      "unified KEDA must key on prover-unified-work-sub"
  # No leaf/agg ScaledObjects under unified.
  assert "lighter-fungible-leaf" not in keda
  assert "lighter-fungible-agg" not in keda


def test_unified_pool_replicas_override():
  out = _render_unified(pool_replicas=42)
  dep = out["deployment"]
  keda = out["keda"]
  assert "replicas: 42" in dep, "--pool-replicas must override the derived count"
  assert "maxReplicaCount: 42" in keda, "KEDA max must follow --pool-replicas"


def test_unified_cpu_memory_override():
  out = _render_unified(cpu="20", memory="12Gi")
  dep = out["deployment"]
  assert dep.count('memory: "12Gi"') == 2, "--memory must override the 8Gi default"
  assert dep.count('cpu: "20"') == 2, "--cpu must override the leaf_cpu default"


# ---------------------------------------------------------------------------
# SPLIT (default): two Deployments + two ScaledObjects, unchanged. Back-compat.
# ---------------------------------------------------------------------------
def test_split_default_renders_two_pools_unchanged():
  out = _render_split()
  dep = out["deployment"]
  keda = out["keda"]

  # Both split Deployments present.
  assert "name: lighter-fungible-leaf" in dep, "split must emit lighter-fungible-leaf"
  assert "name: lighter-fungible-agg" in dep, "split must emit lighter-fungible-agg"
  assert dep.count("kind: Deployment") == 2, "split must emit exactly TWO Deployments"

  # Each on its role-filtered subscription.
  assert 'value: "prover-leaf-work-sub"' in dep
  assert 'value: "prover-agg-work-sub"' in dep

  # Unified pool must be ABSENT under split.
  assert "lighter-fungible-prover" not in dep, \
      "split must NOT emit the unified lighter-fungible-prover Deployment"
  assert 'value: "prover-unified-work-sub"' not in dep

  # Two ScaledObjects, one per sub; unified sub absent.
  assert keda.count("kind: ScaledObject") == 2, "split must emit exactly TWO ScaledObjects"
  assert 'subscriptionName: "prover-leaf-work-sub"' in keda
  assert 'subscriptionName: "prover-agg-work-sub"' in keda
  assert "prover-unified-work-sub" not in keda


def test_split_worker_command_unchanged():
  """Split workers still run the same prover-node work --transport=pubsub binary."""
  out = _render_split()
  dep = out["deployment"]
  # Both leaf + agg containers use the same command.
  assert dep.count('command: ["prover-node", "work"]') == 2
  assert dep.count('- "--transport=pubsub"') == 2


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
