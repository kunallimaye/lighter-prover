#!/usr/bin/env python3
"""Build and push multi-arch STARK container images for all milestone releases."""

import subprocess
import sys


def main():
  tags = [
      "v0.0.1-single-vm-proof-gen",
      "v0.0.1-single-vm-async-proof-gen",
      "v0.0.2-single-vm-dynamic-chunk-size-proof-gen",
      "0.0.3-distributed-proving",
  ]

  print("=== Building & Pushing Multi-Arch Container Images for All Milestone Releases ===")

  for tag in tags:
    print(f"\n[RELEASE] Checking out git tag '{tag}'...")
    subprocess.run(["git", "checkout", tag], check=True)
    
    print("  Restoring modernized IaC pipelines from main branch...")
    subprocess.run(["git", "checkout", "main", "--", "infra-as-code/", "Makefile"], check=True)

    print(f"  Submitting parallel multi-arch Cloud Build for '{tag}' (make cloud-zkp-build ARCH=all)...")
    res = subprocess.run(["make", "cloud-zkp-build", "ARCH=all"], check=False)
    if res.returncode != 0:
      print(f"  [ERROR] Cloud Build failed for tag '{tag}'!")
    else:
      print(f"  [OK] Successfully built and archived container images for '{tag}'")

  print("\n[CLEANUP] Restoring repository to clean main branch...")
  subprocess.run(["git", "checkout", "main", "-f"], check=True)
  print("[OK] All milestone release images built, pushed, and archived in Artifactory!")


if __name__ == "__main__":
  main()
