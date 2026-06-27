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


def tree_depth(n, radix):
  """ceil(log_radix(n)): number of node levels above N leaves.

  Mirrors `tree_depth` in bench/src/bin/prover_node.rs. Computed iteratively to
  avoid float rounding hazards near exact powers (e.g. log2(8) == 3 exactly).
  """
  if radix < 2:
    raise ValueError("radix must be >= 2")
  if n <= 1:
    return 0
  depth = 0
  span = 1  # radix**depth
  while span < n:
    span *= radix
    depth += 1
  return depth


def nodes_at_level(n, radix, level):
  """ceil(n / radix**level): node count at a 1-indexed tree level.

  Mirrors `nodes_at_level` in bench/src/bin/prover_node.rs. The root level always
  has exactly one node.
  """
  if level < 1:
    raise ValueError("tree levels are 1-indexed")
  divisor = radix ** level
  return max((n + divisor - 1) // divisor, 1)


def main():
  parser = argparse.ArgumentParser(description="Render dynamic K8s Proving Pod manifest")
  parser.add_argument("--config", default="config.toml", help="Path to config.toml")
  parser.add_argument("--arch", default="", help="Silicon architecture override (c4a, c3d, t2d, c4d)")
  parser.add_argument("--blocks", type=int, default=2, help="Parallel pipeline blocks")
  parser.add_argument("--input", default="infra-as-code/kubernetes/prover_pod_unit.yaml", help="Input YAML")
  parser.add_argument("--output", default="infra-as-code/kubernetes/prover_pod_unit.rendered.yaml", help="Output YAML")
  parser.add_argument("--image", required=True, help="Container release tag or 'default'")
  parser.add_argument("--radix", type=int, default=2, help="Reduction tree radix (fan-in per node)")
  parser.add_argument(
      "--leaf-count",
      type=int,
      default=0,
      help="Total number of leaf proofs N (decoupled from radix). Defaults to "
           "radix for back-compat with the single-level (N == radix) pipeline.",
  )
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
  
  # Leaf count N is DECOUPLED from radix (fan-in). For back-compat, when
  # --leaf-count is omitted (0) it defaults to radix (the old single-level
  # N == radix pipeline). For an Indexed leaf Job, completions == N: we prove one
  # leaf per completion index. Parallelism is capped by BLOCKS and by N.
  leaf_count = args.leaf_count if args.leaf_count and args.leaf_count > 0 else args.radix
  if leaf_count < 1:
    sys.exit("ERROR: --leaf-count (or --radix fallback) must be >= 1.")

  completions = leaf_count
  parallelism = min(args.blocks, completions)

  # Dynamic tree geometry: depth = ceil(log_radix(N)); each level L has
  # ceil(N / radix^L) tree-node Jobs. depth == 0 (N == 1) means a lone leaf with
  # no folding — still emit one trivial level-1 Job for a uniform pipeline.
  depth = max(tree_depth(leaf_count, args.radix), 1)

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

  # One Indexed tree-node Job PER LEVEL (1..=depth). At level L the Job runs
  # `nodes_at_level(N, radix, L)` completions, each folding up to `radix` children
  # read from the transport ($JOB_COMPLETION_INDEX -> --node-idx). The level-L
  # binary now derives node geometry from --leaf-count, so the same image folds
  # any level. The final (root) level always has exactly one completion.
  #
  # TODO(#291 follow-up — GKE cross-level ordering): These per-level Jobs are
  # emitted with correct geometry, but Kubernetes batch/v1 Jobs have no native
  # inter-Job completion gating. A real multi-level GKE run must NOT start level
  # L+1 until every level-L node proof exists in the transport. That ordering
  # (Argo Workflows / a controller / an initContainer poll-loop on the
  # transport) is intentionally NOT wired here so we don't fake a working
  # multi-level cluster pipeline. The binary-side dynamic depth + the
  # filesystem/local end-to-end path are fully implemented and validated; this
  # manifest is render-correct and ready for that orchestration layer.
  level_jobs = []
  for level in range(1, depth + 1):
    level_nodes = nodes_at_level(leaf_count, args.radix, level)
    is_root = (level == depth)
    level_parallelism = min(args.blocks, level_nodes)
    level_jobs.append(f"""apiVersion: batch/v1
kind: Job
metadata:
  name: lighter-tree-aggregator-l{level}
  labels:
    app: zkp-prover
    role: tree-node
    tree-level: "{level}"
    is-root-level: "{str(is_root).lower()}"
    silicon-arch: {arch}
spec:
  parallelism: {level_parallelism}
  completions: {level_nodes}
  completionMode: Indexed
  template:
    metadata:
      annotations:
        gke-gcsfuse/volumes: "true"
      labels:
        role: tree-node
        tree-level: "{level}"
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
        command: ["sh", "-c", "prover-node tree-node --level {level} --node-idx $JOB_COMPLETION_INDEX --radix {args.radix} --leaf-count {leaf_count} --tx-per-proof {leaf_chunk}"]
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
""")

  # RootCoordinator Job: harvests + verifies the single root proof at the
  # dynamically-computed root level (the binary derives root_level from
  # --leaf-count, so no level is hardcoded here either).
  root_job = f"""apiVersion: batch/v1
kind: Job
metadata:
  name: lighter-root-coordinator
  labels:
    app: zkp-prover
    role: root-coordinator
    silicon-arch: {arch}
spec:
  template:
    metadata:
      annotations:
        gke-gcsfuse/volumes: "true"
      labels:
        role: root-coordinator
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
      - name: coordinator
        image: {image_uri}
        command: ["prover-node", "root-coordinator", "--radix", "{args.radix}", "--leaf-count", "{leaf_count}", "--node-idx", "0", "--tx-per-proof", "{leaf_chunk}"]
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

  tree_rendered = "---\n".join(level_jobs) + "---\n" + root_job

  leaf_output = args.output.replace(".rendered.yaml", "-leaf.rendered.yaml")
  tree_output = args.output.replace(".rendered.yaml", "-tree.rendered.yaml")

  with open(leaf_output, "w", encoding="utf-8") as f:
    f.write(leaf_rendered)
  with open(tree_output, "w", encoding="utf-8") as f:
    f.write(tree_rendered)

  print(
      f"[OK] Dynamically rendered K8s Proving Pod Jobs to {leaf_output} and "
      f"{tree_output} (arch={arch}, blocks={args.blocks}, radix={args.radix}, "
      f"leaf_count={leaf_count}, depth={depth}, leaf_chunk={leaf_chunk})"
  )


if __name__ == "__main__":
  main()
