#!/usr/bin/env python3
"""Renders prover_pod_unit.rendered.yaml dynamically from config.toml proving_pod profiles."""

import argparse
import os
import sys

try:
  import tomllib  # Python 3.11+
except ImportError:
  try:
    import tomli as tomllib
  except ImportError:
    import toml as tomllib


def main():
  parser = argparse.ArgumentParser(description="Render dynamic K8s Proving Pod manifest")
  parser.add_argument("--config", default="config.toml", help="Path to config.toml")
  parser.add_argument("--arch", default="", help="Silicon architecture override (c4a, c3d, t2d)")
  parser.add_argument("--blocks", type=int, default=2, help="Parallel pipeline blocks")
  parser.add_argument("--input", default="infra-as-code/kubernetes/prover_pod_unit.yaml", help="Input YAML")
  parser.add_argument("--output", default="infra-as-code/kubernetes/prover_pod_unit.rendered.yaml", help="Output YAML")
  args = parser.parse_args()

  if not os.path.exists(args.config):
    print(f"Error: Config file {args.config} not found.", file=sys.stderr)
    sys.exit(1)

  try:
    with open(args.config, "rb") as f:
      data = tomllib.load(f)
  except (TypeError, AttributeError):
    with open(args.config, "r", encoding="utf-8") as f:
      data = tomllib.load(f)

  pod_cfg = data.get("proving_pod", {})
  defaults = pod_cfg.get("defaults", {})
  
  arch = args.arch if args.arch else str(defaults.get("arch", "c3d"))
  arch_cfg = pod_cfg.get(arch, {})
  if not arch_cfg:
    print(f"Warning: No [proving_pod.{arch}] found in {args.config}, using fallbacks.", file=sys.stderr)
    arch_cfg = {}

  leaf_cfg = arch_cfg.get("leaf_worker", {})
  agg_cfg = arch_cfg.get("aggregator", {})

  kube_arch = "arm64" if arch == "c4a" else "amd64"

  leaf_cpu = str(leaf_cfg.get("cpu_requests", "30" if arch in ("c3d", "t2d", "c4d") else "64"))
  leaf_mem = str(leaf_cfg.get("memory_requests", "60Gi" if arch in ("c3d", "c4d") else "128Gi"))
  leaf_chunk = int(leaf_cfg.get("chunk_size", 1 if arch in ("c3d", "c4d") else 4))
  leaf_replicas = 3 * args.blocks

  agg_cpu = str(agg_cfg.get("cpu_requests", "30" if arch in ("c3d", "c4d") else "16"))
  agg_mem = str(agg_cfg.get("memory_requests", "60Gi" if arch in ("c3d", "c4d") else "32Gi"))
  agg_replicas = 1 * args.blocks

  if not os.path.exists(args.input):
    print(f"Error: Input YAML {args.input} not found.", file=sys.stderr)
    sys.exit(1)

  with open(args.input, "r", encoding="utf-8") as f:
    yaml_content = f.read()

  # Render Leaf Worker Deployment
  rendered = f"""apiVersion: apps/v1
kind: Deployment
metadata:
  name: lighter-leaf-worker
  labels:
    app: zkp-prover
    role: leaf-worker
    silicon-arch: {arch}
spec:
  replicas: {leaf_replicas}
  selector:
    matchLabels:
      role: leaf-worker
  template:
    metadata:
      labels:
        role: leaf-worker
        silicon-arch: {arch}
    spec:
      nodeSelector:
        cloud.google.com/gke-spot: "true"
        cloud.google.com/compute-class: "{arch}"
        kubernetes.io/arch: {kube_arch}
      containers:
      - name: prover
        image: us-docker.pkg.dev/lighter-prover/zkp-prover:multiarch
        command: ["prover-node", "leaf-worker", "--tx-per-proof", "{leaf_chunk}"]
        resources:
          limits:
            cpu: "{leaf_cpu}"
            memory: "{leaf_mem}"
          requests:
            cpu: "{leaf_cpu}"
            memory: "{leaf_mem}"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: lighter-tree-aggregator
  labels:
    app: zkp-prover
    role: tree-node
    silicon-arch: {arch}
spec:
  replicas: {agg_replicas}
  selector:
    matchLabels:
      role: tree-node
  template:
    metadata:
      labels:
        role: tree-node
        silicon-arch: {arch}
    spec:
      nodeSelector:
        cloud.google.com/gke-spot: "true"
        cloud.google.com/compute-class: "{arch}"
        kubernetes.io/arch: {kube_arch}
      containers:
      - name: aggregator
        image: us-docker.pkg.dev/lighter-prover/zkp-prover:multiarch
        command: ["prover-node", "tree-node"]
        resources:
          limits:
            cpu: "{agg_cpu}"
            memory: "{agg_mem}"
          requests:
            cpu: "{agg_cpu}"
            memory: "{agg_mem}"
"""

  with open(args.output, "w", encoding="utf-8") as f:
    f.write(rendered)

  print(f"[OK] Dynamically rendered K8s Proving Pod manifest to {args.output} (arch={arch}, blocks={args.blocks}, leaf_chunk={leaf_chunk})")


if __name__ == "__main__":
  main()
