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
  parser.add_argument("--arch", default="", help="Silicon architecture override (c4a, c3d, t2d, c4d)")
  parser.add_argument("--blocks", type=int, default=2, help="Parallel pipeline blocks")
  parser.add_argument("--input", default="infra-as-code/kubernetes/prover_pod_unit.yaml", help="Input YAML")
  parser.add_argument("--output", default="infra-as-code/kubernetes/prover_pod_unit.rendered.yaml", help="Output YAML")
  parser.add_argument("--image", required=True, help="Container release tag or 'default'")
  parser.add_argument("--radix", type=int, default=2, help="Reduction tree radix")
  parser.add_argument("--benchmark-id", default="", help="Benchmark ID for GCS path isolation")
  args = parser.parse_args()

  if not args.image or args.image.strip() == "":
    sys.exit("ERROR: --image argument is required for Kubernetes deployment manifest rendering.")
  image_tag = args.image.strip()
  if image_tag == "default":
    image_tag = "0.0.3-distributed-proving"

  if not os.path.exists(args.config):
    print(f"Error: Config file {args.config} not found.", file=sys.stderr)
    sys.exit(1)

  try:
    with open(args.config, "rb") as f:
      data = tomllib.load(f)
  except (TypeError, AttributeError):
    with open(args.config, "r", encoding="utf-8") as f:
      data = tomllib.load(f)

  gcs_bucket = data.get("gcp", {}).get("bench", {}).get("bucket", "kunal-scratch-tfstate")
  gsa_email = data.get("gcp", {}).get("target", {}).get("runtime_sa", {}).get("email", "")
  if not gsa_email:
    gsa_email = data.get("gcp", {}).get("target", {}).get("build_sa", {}).get("email", "")

  # Dynamically resolve registry URI from config.toml
  project_id = data.get("gcp", {}).get("defaults", {}).get("project", "kunal-scratch")
  registry_cfg = data.get("gcp", {}).get("registry", {})
  registry_region = registry_cfg.get("region", "us")
  registry_repo = registry_cfg.get("repository", "lighter-prover-iac")
  image_uri = f"{registry_region}-docker.pkg.dev/{project_id}/{registry_repo}/zkp-prover:{image_tag}"

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
  
  # For Indexed Job, completions is the total number of chunks we need to prove (radix).
  # Parallelism is controlled by the blocks parameter (BLOCKS), capped at completions.
  completions = args.radix
  parallelism = min(args.blocks, completions)

  agg_cpu = str(agg_cfg.get("cpu_requests", "30" if arch in ("c3d", "c4d") else "16"))
  agg_mem = str(agg_cfg.get("memory_requests", "60Gi" if arch in ("c3d", "c4d") else "32Gi"))
  agg_replicas = 1 * args.blocks

  if not os.path.exists(args.input):
    print(f"Error: Input YAML {args.input} not found.", file=sys.stderr)
    sys.exit(1)

  mount_opts = "implicit-dirs"
  if args.benchmark_id:
    # GCS Fuse only-dir option mounts a subdirectory as the root of the volume
    mount_opts += f",only-dir={args.benchmark_id}"

  # Split rendering into Leaf and Tree Jobs
  leaf_rendered = f"""apiVersion: v1
kind: ServiceAccount
metadata:
  name: prover-sa
  namespace: default
  annotations:
    iam.gke.io/gcp-service-account: {gsa_email}
---
apiVersion: batch/v1
kind: Job
metadata:
  name: lighter-leaf-worker
  labels:
    app: zkp-prover
    role: leaf-worker
    silicon-arch: {arch}
spec:
  parallelism: {parallelism}
  completions: {completions}
  completionMode: Indexed
  template:
    metadata:
      annotations:
        gke-gcsfuse/volumes: "true"
      labels:
        role: leaf-worker
        silicon-arch: {arch}
    spec:
      serviceAccountName: prover-sa
      nodeSelector:
        role: leaf-worker
        silicon-arch: {arch}
      tolerations:
      - key: "dedicated"
        operator: "Equal"
        value: "zkp-prover"
        effect: "NoSchedule"
      restartPolicy: OnFailure
      containers:
      - name: prover
        image: {image_uri}
        command: ["sh", "-c", "prover-node leaf-worker --chunk-idx $JOB_COMPLETION_INDEX --tx-per-proof {leaf_chunk}"]
        resources:
          limits:
            cpu: "{leaf_cpu}"
            memory: "{leaf_mem}"
          requests:
            cpu: "{leaf_cpu}"
            memory: "{leaf_mem}"
        volumeMounts:
        - name: gcs-volume
          mountPath: /data/reports
      volumes:
      - name: gcs-volume
        csi:
          driver: gcsfuse.csi.storage.gke.io
          volumeAttributes:
            bucketName: "{gcs_bucket}"
            mountOptions: "{mount_opts}"
"""

  tree_rendered = f"""apiVersion: batch/v1
kind: Job
metadata:
  name: lighter-tree-aggregator
  labels:
    app: zkp-prover
    role: tree-node
    silicon-arch: {arch}
spec:
  template:
    metadata:
      annotations:
        gke-gcsfuse/volumes: "true"
      labels:
        role: tree-node
        silicon-arch: {arch}
    spec:
      serviceAccountName: prover-sa
      nodeSelector:
        role: tree-node
        silicon-arch: {arch}
      tolerations:
      - key: "dedicated"
        operator: "Equal"
        value: "zkp-prover"
        effect: "NoSchedule"
      restartPolicy: OnFailure
      containers:
      - name: aggregator
        image: {image_uri}
        command: ["prover-node", "tree-node", "--level", "1", "--node-idx", "0", "--radix", "{args.radix}", "--tx-per-proof", "{leaf_chunk}"]
        resources:
          limits:
            cpu: "{agg_cpu}"
            memory: "{agg_mem}"
          requests:
            cpu: "{agg_cpu}"
            memory: "{agg_mem}"
        volumeMounts:
        - name: gcs-volume
          mountPath: /data/reports
      volumes:
      - name: gcs-volume
        csi:
          driver: gcsfuse.csi.storage.gke.io
          volumeAttributes:
            bucketName: "{gcs_bucket}"
            mountOptions: "{mount_opts}"
"""

  leaf_output = args.output.replace(".rendered.yaml", "-leaf.rendered.yaml")
  tree_output = args.output.replace(".rendered.yaml", "-tree.rendered.yaml")

  with open(leaf_output, "w", encoding="utf-8") as f:
    f.write(leaf_rendered)
  with open(tree_output, "w", encoding="utf-8") as f:
    f.write(tree_rendered)

  print(f"[OK] Dynamically rendered K8s Proving Pod Jobs to {leaf_output} and {tree_output} (arch={arch}, blocks={args.blocks}, radix={args.radix}, leaf_chunk={leaf_chunk})")


if __name__ == "__main__":
  main()
