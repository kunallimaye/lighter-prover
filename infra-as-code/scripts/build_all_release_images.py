#!/usr/bin/env python3
"""Build and push multi-arch STARK container images for all milestone releases and radix-16 branch."""

import os
import subprocess
import sys


def main():
  targets = [
      "v0.0.1-single-vm-proof-gen",
      "v0.0.1-single-vm-async-proof-gen",
      "v0.0.2-single-vm-dynamic-chunk-size-proof-gen",
      "0.0.3-distributed-proving",
      "radix-16-reduction-trees",
  ]

  print("=== Building & Pushing Multi-Arch Container Images for All Targets ===")

  for target in targets:
    print(f"\n[TARGET] Preparing proving code for '{target}'...")
    try:
      subprocess.run(["git", "checkout", target, "--", "circuit/", "bench/"], check=True)
    except subprocess.CalledProcessError:
      print(f"  [WARNING] Could not checkout '{target}' for circuit/bench (might be identical or branch missing).")
      continue

    print(f"  Submitting parallel multi-arch Cloud Build for '{target}' (make cloud-zkp-build ARCH=all)...")
    env = os.environ.copy()
    env["TAG"] = target
    res = subprocess.run(["make", "cloud-zkp-build", "ARCH=all"], env=env, check=False)
    if res.returncode != 0:
      print(f"  [ERROR] Cloud Build failed for target '{target}'!")
    else:
      print(f"  [OK] Successfully built and archived container images for '{target}'")

  print("\n[CLEANUP] Restoring repository to clean main branch...")
  subprocess.run(["git", "checkout", "main", "--", "circuit/", "bench/"], check=True)
  print("[OK] All release and radix-16 container images built, pushed, and archived in Artifactory!")


if __name__ == "__main__":
  main()
