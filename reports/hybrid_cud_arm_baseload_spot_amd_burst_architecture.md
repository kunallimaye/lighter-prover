# Enterprise Architecture Blueprint: Hybrid CUD ARM Baseload + Spot AMD Burst (`10 BPS`)

## Executive Summary & Vision
You have formulated the definitive systems and financial topology for institutional zero-knowledge settlement on Google Cloud Platform.

Attempting to run $100\%$ Spot infrastructure exposes exchange baseload settlement to regional GCP preemption shocks during cloud capacity crunches. Conversely, running $100\%$ On-Demand silicon incurs catastrophic commercial billing burn ($\$1.42\text{M/month}$). This blueprint codifies a **Dual-Architecture Hybrid Triad**: locking baseline exchange traffic under **3-Year Committed Use Discounts (CUD) on ARM Neoverse V2 (`c4a`)** while absorbing daytime market volatility spikes via **Elastic Spot MIGs on AMD EPYC Milan Tau (`t2d`)**.

---

## Part 1: Swapping Tree Aggregators to `t2d` (Pure x86_64 Pods) 🖥️⚡

Substituting **`t2d-standard-16`** ($16\text{ AMD Milan vCPUs}$ @ $\$0.067\text{/hr}$) for **`c4a-highcpu-16`** ($16\text{ ARM cores}$ @ $\$0.178\text{/hr}$) in the Reduction Tree Aggregator role unlocks two major architectural lifts:

1.  **Eliminating Emulated Cross-Compilation**: When pods pair AMD leaf provers with ARM aggregators, CI/CD pipelines must run `docker buildx build --platform linux/amd64,linux/arm64`. Compiling Rust STARK circuits for `aarch64` inside an `amd64` builder requires heavy QEMU emulation (~4 minutes). Making the entire hypothesis pod **$100\%$ pure `linux/amd64`** allows `cloudbuild-zkp.yaml` to execute native single-target builds in $30\text{ seconds}$.
2.  **Pure Spot Arbitrage**: Slashing aggregator spot rates drops total hourly pod burn from $\$0.934 \rightarrow \mathbf{\$0.823\text{ / hr / pod}}$ ($64.4\%$ savings vs all-ARM!). Across 120 pods, this refinement saves Lighter an additional **$\mathbf{\$116,683\text{ per year}}$**.

```mermaid
graph TD
    classDef cud fill:#7c3aed,stroke:#ddd6fe,stroke-width:2px,color:#fff;
    classDef spot fill:#0284c7,stroke:#bae6fd,stroke-width:2px,color:#fff;

    DEX["Lighter Sequencer Traffic: Bimodal 24-Hour Curve"]

    subgraph Tier 1: Ironclad Core Baseload Fleet (24 Pods of c4a-64 under 3-Yr CUD)
    BASE["Continuous Baseload @ 2 BPS (1,000 TPS) --> 100% Guaranteed SLA | Wall Time = 12.00s"]:::cud
    end

    subgraph Tier 2: Elastic Burst Fleet (0..96 Pods of t2d-60 on Spot MIG)
    BURST["Daytime Volatility Spikes @ +8 BPS (+4,000 TPS) --> $0.82/hr/pod Spot Arbitrage"]:::spot
    end

    DEX --> BASE
    DEX -->|"Daytime Burst Spikes"| BURST
```

---

## Part 2: Blended Financial Economics (`10 BPS` Peak) 💰📊

Google Cloud provides an approximate **$55\%$ discount** off On-Demand rates for 3-Year Committed Use Discounts on ARM Axion (`c4a`). This brings dedicated flagship silicon ($\$1.098\text{/hr}$) within striking distance of Spot pricing while guaranteeing $100\%$ immunity to reclamation!

| Operating Fleet Tier | Assigned Paradigm & Silicon Pod Shape | Active Pod Quantity | Tenancy & Billing Governance | Blended Pod Hourly Rate | Blended Tier Hourly Burn | Annual Tier Expenditure | Finality Final Wall Time | Operational & Security Verdict |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :--- |
| **Tier 1: Ironclad Core Baseload** | `c4a-highcpu-64` leaves $+ \text{tree}$ | $24\text{ pods}$ *(2 BPS)* | 3-Year Dedicated CUD | $\$3.570\text{ / pod}$ | $\$85.68\text{ / hr}$ | $\$750,556$ | **$12.005\text{ seconds}$** | 🛡️ **Ironclad SLA**: Zero preemption risk for core DEX operations. |
| **Tier 2: Elastic Burst MIG** | `t2d-standard-60` leaves $+ \text{t2d-16 tree}$ | $96\text{ pods}$ *(+8 BPS)* | Preemptible Spot MIG | $\$0.823\text{ / pod}$ | $\$79.00\text{ / hr}$ | $\$692,040$ *(Max burst)* | $12.962\text{ seconds}$ | 🌊 **Elastic Surge**: Absorbs market volatility spikes at pure spot rates. |
| **BLENDED ENTERPRISE FLEET** | **Hybrid Axion-Tau Matrix** | **120 Pods** *(10 BPS)* | **Bimodal CUD + Spot** | **$\$1.372\text{ / pod}$** | **$\mathbf{\$164.68\text{ / hr}}$** | **$\mathbf{\$1,442,596}$** | **Blended $\sim 12.7\text{s}$** | 🏆 **$\mathbf{\$0.0000091\text{ / tx}}$** *(Sub-0.001 cents!)* |

---

## User Review Required 🛑

> [!IMPORTANT]
> **CUD Commitment Authorization**: Executing this blueprint requires CFO / VP authorization to commit to a 3-Year Google Cloud CUD contract for **72 instances of `c4a-highcpu-64`** ($4,608\text{ ARM cores}$). This guarantees institutional exchange finality under $12.01\text{s}$ for $\$750K\text{/yr}$.

---

## Open Questions ❓

> [!CAUTION]
> **Pacer Failover Logic**: If an unexpected regional spot crunch preempts 10 pods in Tier 2 during a burst period, do your systems engineers prefer **Dynamic Pacing Throttle** (temporarily buffering sequencer transaction batches into Redis/PubSub to slow burst TPS down to active pod capacity) or **On-Demand Burst Fallback** (auto-scaling up On-Demand `t2d-60` VMs for 10 minutes until spot liquidity returns)? *(Recommended default: Dynamic Pacing Throttle)*.
