#!/usr/bin/env python3
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1

"""Dry-run tests for render_pod_spec.py --chunk plumbing.

Regression guard for the plumbing gap where Cloud Build `_CHUNK_SIZE` (and the
Makefile `GKE_CHUNK`) was echoed for logging but NEVER reached the rendered pods
-- `leaf_chunk` silently fell back to the config.toml default (1 for c3d), so
attempt-47 requested C=4 but actually ran C=1 (500 leaves instead of 125).

The fix: a `--chunk` arg that OVERRIDES config.toml when > 0, threaded from
cloudbuild-bench.yaml as `--chunk="${_CHUNK_SIZE}"`.

Run:
  * pytest infra-as-code/scripts/tests/test_render_pod_spec_chunk.py
  * python3 infra-as-code/scripts/tests/test_render_pod_spec_chunk.py  (self-test)
"""

import importlib.util
import os
import subprocess
import sys
import tempfile

_HERE = os.path.dirname(os.path.abspath(__file__))
_SCRIPT = os.path.join(_HERE, "..", "render_pod_spec.py")
_REPO = os.path.abspath(os.path.join(_HERE, "..", "..", ".."))
_CONFIG = os.path.join(_REPO, "config.toml.example")


def _render(chunk_arg):
  """Render the fungible manifests with an optional --chunk; return combined YAML."""
  out = os.path.join(tempfile.mkdtemp(), "out.rendered.yaml")
  cmd = [
      sys.executable, _SCRIPT,
      "--config", _CONFIG,
      "--arch", "c3d",
      "--image", "default",
      "--radix", "16",
      "--fold-strategy", "hex",
      "--emit-fungible",
      "--benchmark-id", "chunk-test",
      "--output", out,
  ]
  if chunk_arg is not None:
    cmd += ["--chunk", str(chunk_arg)]
  r = subprocess.run(cmd, capture_output=True, text=True, cwd=_REPO)
  assert r.returncode == 0, f"render failed: {r.stderr}\n{r.stdout}"
  # Collect all rendered sibling files.
  base = out.replace(".rendered.yaml", "")
  combined = ""
  d = os.path.dirname(out)
  for f in os.listdir(d):
    if f.endswith(".rendered.yaml"):
      combined += open(os.path.join(d, f)).read()
  return combined


def _tx_per_proof(yaml_text):
  """Extract the resolved TX_PER_PROOF env value from rendered YAML."""
  lines = yaml_text.splitlines()
  for i, l in enumerate(lines):
    if "name: TX_PER_PROOF" in l:
      # value is on the next line: `          value: "4"`
      for j in range(i + 1, min(i + 3, len(lines))):
        if "value:" in lines[j]:
          return lines[j].split("value:")[1].strip().strip('"')
  return None


def test_chunk_override_applies():
  """--chunk 4 must set TX_PER_PROOF=4 and leaf-count=125 (500/4), NOT the c3d config default of 1."""
  y = _render(4)
  assert _tx_per_proof(y) == "4", f"TX_PER_PROOF should be 4, got {_tx_per_proof(y)}"
  assert "leaf-count 125" in y, "radix-16 C=4 must render leaf-count 125 (500/4)"
  print("[PASS] test_chunk_override_applies")


def test_no_chunk_falls_back_to_config():
  """Without --chunk, TX_PER_PROOF falls back to the config.toml.example c3d default (1)."""
  y = _render(None)
  assert _tx_per_proof(y) == "1", f"fallback TX_PER_PROOF should be 1, got {_tx_per_proof(y)}"
  print("[PASS] test_no_chunk_falls_back_to_config")


def test_chunk_zero_falls_back():
  """--chunk 0 is the explicit 'use config' sentinel (same as omitting it)."""
  y = _render(0)
  assert _tx_per_proof(y) == "1", f"--chunk 0 should fall back to config (1), got {_tx_per_proof(y)}"
  print("[PASS] test_chunk_zero_falls_back")


if __name__ == "__main__":
  fns = [test_chunk_override_applies, test_no_chunk_falls_back_to_config, test_chunk_zero_falls_back]
  passed = 0
  for fn in fns:
    fn(); passed += 1
  print(f"\n{passed}/{len(fns)} passed")
