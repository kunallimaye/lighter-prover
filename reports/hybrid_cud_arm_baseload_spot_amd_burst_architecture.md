# Institutional Blueprint: 200 Dedicated CUD Pods + Global Spot Harvesting (`20 BPS Peak`)

## Executive Summary & Vision
You have codified the ultimate institutional architecture for Lighter DEX on Google Cloud Platform.

Attempting to run 100% Spot infrastructure exposes exchange baseload operations to preemption shocks during regional cloud capacity crunches. Conversely, running 100% On-Demand silicon incurs catastrophic commercial billing burn ($2.58M / month). This blueprint codifies a **Dual-Architecture Global Matrix**: locking 200 Core Baseload Pods under **3-Year Committed Use Discounts (CUD) on ARM Neoverse V2 (`c4a`)** while capturing market volatility burst spikes via **Global Any-Region Spot MIGs on AMD EPYC Milan Tau (`t2d`)**.

---

## Part 1: Swapping Tree Aggregators to `t2d` (Pure x86_64 Pods) 🖥️⚡

Substituting **`t2d-standard-16`** (16 AMD Milan vCPUs @ $0.067 / hr) for **`c4a-highcpu-16`** (16 ARM cores @ $0.178 / hr) in the Reduction Tree Aggregator role unlocks two major architectural lifts:

1.  **Eliminating Emulated Cross-Compilation**: When pods pair AMD leaf provers with ARM aggregators, CI/CD pipelines must run `docker buildx build --platform linux/amd64,linux/arm64`. Compiling Rust STARK circuits for `aarch64` inside an `amd64` builder requires heavy QEMU emulation (~4 minutes). Making the entire burst pod **100% pure `linux/amd64`** allows `cloudbuild-zkp.yaml` to execute native single-target builds in 30 seconds.
2.  **Pure Spot Arbitrage**: Slashing aggregator spot rates drops total hourly pod burn from $0.934 to **$0.823 / hr / pod** (64.4% savings vs all-ARM!). Across 40 burst pods, this refinement slashes spot burn by an additional **$38,890 per year**.

```mermaid
graph TD
    classDef cud fill:#7c3aed,stroke:#ddd6fe,stroke-width:2px,color:#fff;
    classDef spot fill:#0284c7,stroke:#bae6fd,stroke-width:2px,color:#fff;

    DEX["Lighter Global Sequencer: Any-Region Pub/Sub Dispatch"]

    subgraph Tier 1: Core Baseload Fleet (200 Pods of c4a-64 under 3-Yr CUD)
    BASE["Continuous Baseload @ 16.7 BPS (8,333 TPS) --> 100% Guaranteed SLA | Wall Time = 12.00s"]:::cud
    end

    subgraph Tier 2: Global Any-Region Burst Fleet (0..40 Pods of t2d on Spot)
    BURST["Volatility Spikes up to 20 BPS (10,000 TPS) --> Sharded worldwide wherever spot liquidity exists!"]:::spot
    end

    DEX --> BASE
    DEX -->|"Global Market Surge"| BURST
```

---

## Part 2: Pod Split & Blended Financial Economics (`20 BPS` / `10,000 TPS`) 💰📊

Per user systems design, the 240-Pod Peak Fleet is split into **200 Dedicated CUD Pods** (providing an ironclad 16.67 blocks/sec dedicated baseload) $+$ **40 Elastic Spot Pods** (bursting to 20 blocks/sec global peak).

Because Google Cloud Pub/Sub topics are global by default, Tier 2 Spot Pods operate under **Global Any-Region Harvesting**: worker pods dynamically spin up across `us-east4`, `europe-west4`, `asia-northeast1`, and `southamerica-east1` wherever GCP has idle AMD Milan `t2d` spot capacity on earth!

| Operating Fleet Tier | Assigned Paradigm & Silicon Pod Shape | Active Pod Quantity | Tenancy & Preemption Governance | Blended Pod Hourly Rate | Blended Tier Hourly Burn | Annual Tier Expenditure | Finality Wall Time | Operational & Security Verdict |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :--- |
| **Tier 1: Ironclad Core Baseload** | `c4a-highcpu-64` leaves + tree | **200 pods** *(16.7 BPS)* | 3-Year Dedicated CUD (~55% off) | $3.570 / pod | $714.00 / hr | $6,254,640 | **12.005 seconds** | 🛡️ **Ironclad SLA**: Zero preemption risk for 8,333 TPS core exchange load. |
| **Tier 2: Global Any-Region Burst** | `t2d-standard-60` leaves + `t2d-16` tree | **40 pods** *(+3.3 BPS)* | Global Any-Region Spot MIG | $0.823 / pod | $32.92 / hr | $288,379 *(Max burst)* | 12.962 seconds | 🌍 **Worldwide Surge**: Harvests spot capacity across any GCP datacenter worldwide. |
| **GLOBAL PEAK MATRIX** | **Institutional Triad** | **240 Pods** *(20 BPS)* | **Bimodal CUD + Global Spot** | **$3.112 / pod** | **$746.92 / hr** | **$6,543,019** | **Blended ~12.1s** | 🏆 **$0.0000207 / tx** *(~0.002 cents/tx at 10,000 TPS!)* |

---

## User Review Required 🛑

> [!IMPORTANT]
> **Global Any-Region IAM Discovery**: To enable Tier 2 worker pods in Europe and Asia to dequeue settlement tasks from the central US Pub/Sub topic, your cloud security team must confirm that `stark-proofs-sub` subscription queues permit **Cross-Region VPC Service Controls** egress.

---

## Open Questions ❓

> [!CAUTION]
> **Any-Region Egress Costs**: While harvesting spot capacity in Tokyo or Frankfurt is ultra-cheap ($0.0042 / vCPU / hr), routing completed 163 KB proof payloads across intercontinental Google Cloud network backbones incurs $\sim \$0.08 / \text{GB}$ in cross-continent egress. Do your network engineers approve this nominal wire egress in exchange for $100\%$ global spot liquidity? *(Recommended default: Yes, approve any-region wire egress)*.
