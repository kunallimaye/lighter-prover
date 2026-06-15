//! Proof-store transport — cells ship their REAL L2 leaf proof bytes to a
//! shared object store keyed by `{height}/{witness_index}` (issue #179, the
//! fan-IN half of the distributed prover).
//!
//! ## Why a proof store exists
//!
//! Pub/Sub messages are size-bounded; L2 leaf proofs are far too large to
//! inline on the results topic. So a cell, after it produces and verifies its
//! REAL `BlockTxChainCircuit` leaf proof, writes the proof BYTES to a shared
//! bucket and only the OBJECT KEY (a small string) crosses the wire on
//! [`ChunkResultMessage::proof_object`](super::pubsub::ChunkResultMessage).
//! The coordinator (a LATER slice of #179) fetches those bytes by key and
//! folds them with the existing `BlockTxChainMergeCircuit` tree.
//!
//! ## Two selectable transports (issue #206)
//!
//! This adapter supports two transports, chosen by configuration:
//!
//! 1. **`gcloud storage cp` CLI** (the original, issue #179) — drives the
//!    object store by invoking the **`gcloud storage` CLI**, exactly mirroring
//!    [`super::pubsub::GcloudPubSub`]'s decision to shell out to `gcloud
//!    pubsub`. The runtime image already ships `google-cloud-cli`; shelling
//!    out adds **no new Rust dependency** and reuses ADC/workload-identity
//!    auth the CLI already resolves.
//!
//! 2. **Mounted-volume file I/O** (issue #206, the near-term fix) — when a
//!    mount root is configured (`--proof-mount-path` / `LIGHTER_PROOF_MOUNT`,
//!    e.g. a gcsfuse mount of the proof bucket), `upload`/`download` become
//!    plain file write/read against `<mount>/<key>`. This mirrors how the
//!    **witness** plane resolves from a mounted corpus (#61). Mount mode is
//!    **selected over** the CLI when a mount path is set; otherwise the CLI
//!    path is used unchanged, so the change is additive and non-regressing.
//!
//! ### Why the mount (issue #206)
//!
//! After #203 removed the Pub/Sub poll latency, the per-`gcloud storage cp`
//! SUBPROCESS overhead (process spawn + auth re-resolve + TLS per copy) became
//! the dominant residual per-level barrier — *not* the ~422 KB payload (~2 ms
//! of wire time). Each merge does 2 downloads + 1 upload inside the barrier
//! against a ~0.5 s prove. Replacing the subprocess with a file write/read on
//! a mounted bucket removes that overhead — at the cost of a FUSE mount's
//! read-after-write visibility lag (write-on-A, cross-machine-read-on-B at the
//! per-level barrier), which we explicitly INSTRUMENT (see below) rather than
//! assume away. This is a stepping stone; #207 evaluates the permanent
//! substrate, informed by the numbers this instrumentation produces.
//!
//! ### Atomicity + honest-failure on the mount
//!
//! A mount write goes to a uniquely-named TEMP path under the mount, then is
//! atomically `rename`d into place, so a concurrent reader on another machine
//! never observes a partially-written proof — it either sees the complete
//! bytes or no file at all. A missing or not-yet-visible proof (file absent /
//! unreadable) returns `Err`, NEVER a fabricated proof (issue #179
//! honest-failure rule). This matters because a gcsfuse mount can have
//! read-after-write visibility lag across machines; callers that expect a
//! just-written proof poll via [`GcloudStorage::wait_for_object`].
//!
//! ## Opt-in / off by default
//!
//! The proof store is OFF unless a bucket OR a mount path is configured. With
//! neither, the cell behaves EXACTLY as it did before issue #179: prove, set
//! `proof_object: None`, publish — so existing benchmark runs are byte-for-byte
//! unchanged.
//!
//! ## What is unit-testable WITHOUT gcloud
//!
//! The object-key construction ([`proof_object_key`]), the `gcloud`-argument-
//! vector construction ([`StorageConfig::cp_to_gcs_argv`]), and the entire
//! mount-mode file I/O (which needs only a local temp dir, no bucket or auth)
//! are pure/hermetic and fully unit-tested here. The actual `cp` (which needs
//! a live bucket + auth) is exercised only behind an explicit env gate so CI
//! stays hermetic.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Construct the proof-store OBJECT KEY for an L2 leaf proof.
///
/// The key is `{height}/{witness_index}` — the EXACT scheme the plan
/// specifies (issue #179) and the identical scheme the future coordinator
/// slice must use to fetch the bytes. Keep this the single source of truth
/// for the key so the two sides can never drift.
pub fn proof_object_key(height: u64, witness_index: u64) -> String {
    format!("{height}/{witness_index}")
}

/// Construct the proof-store OBJECT KEY for an INTERMEDIATE MERGE proof
/// (issue #198, cross-machine fold fan-out).
///
/// In the distributed fold a merge proven on coordinator A must be readable
/// by coordinator B at the next tree level. Every merge OUTPUT is uploaded
/// to the proof store under this key so the next level's task can fetch it.
///
/// The key is `{height}/m/{level}/{index}`. The `/m/` segment guarantees it
/// can **never collide** with a LEAF key, which is `{height}/{witness_index}`
/// ([`proof_object_key`]): a leaf key's second path segment is always a
/// decimal number, never the literal `m`, so the two namespaces are disjoint.
///
/// `level` is the 1-based merge level (level 1 folds the leaves, level 2
/// folds level-1 outputs, …) and `index` is the stable in-level pair index
/// (the same index the #193 determinism re-sort keys on). Keep this the
/// single source of truth for the key so the producing and consuming
/// coordinators can never drift — exactly like [`proof_object_key`].
pub fn merge_object_key(height: u64, level: u64, index: u64) -> String {
    format!("{height}/m/{level}/{index}")
}

/// Resolved configuration for the gcloud-backed proof store. Mirrors
/// [`super::pubsub::PubSubConfig`]: the binary resolves the values from a
/// flag/env var and hands the struct over, so the transport reads no globals.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Target bucket NAME (no `gs://` prefix), e.g.
    /// `kunal-scratch-lighter-prover-proofs`. Used by the `gcloud storage cp`
    /// CLI transport. Empty AND no `mount_path` means "disabled" — the cell
    /// must not attempt any upload.
    pub bucket: String,
    /// `gcloud` binary path (default `gcloud`; overridable for a vendored
    /// CLI). Reuses the same binary the Pub/Sub transport uses.
    pub gcloud_bin: String,
    /// Issue #206: filesystem root of the MOUNTED proof bucket (e.g. a gcsfuse
    /// mount of `bucket`). Empty = mount mode OFF (use the `gcloud storage cp`
    /// CLI transport). Non-empty = mount mode ON: `upload`/`download` become
    /// plain file write/read against `<mount_path>/<key>`, with atomic
    /// temp+rename writes. Mount mode is SELECTED OVER the CLI when set, so
    /// the CLI path is preserved as a non-regressing fallback.
    pub mount_path: String,
}

impl StorageConfig {
    /// Whether the proof store is enabled. Off only when NEITHER a bucket NOR
    /// a mount path is configured.
    pub fn enabled(&self) -> bool {
        !self.bucket.trim().is_empty() || self.mount_enabled()
    }

    /// Issue #206: whether MOUNT mode is selected (a non-empty mount path).
    /// When true, `upload`/`download` use mounted-file I/O instead of the
    /// `gcloud storage cp` CLI.
    pub fn mount_enabled(&self) -> bool {
        !self.mount_path.trim().is_empty()
    }

    /// Issue #206: the on-mount filesystem PATH for an object key. The key
    /// (`{height}/{witness_index}` or `{height}/m/{level}/{index}`) maps
    /// directly to a relative path under the mount root, so the SAME key
    /// scheme that crosses Pub/Sub addresses the file on the mount.
    pub fn mount_object_path(&self, key: &str) -> PathBuf {
        Path::new(self.mount_path.trim()).join(key)
    }

    /// The full `gs://<bucket>/<key>` destination URI for an object key.
    pub fn gcs_uri(&self, key: &str) -> String {
        format!("gs://{}/{}", self.bucket, key)
    }

    /// Build the `gcloud storage cp <local_path> gs://<bucket>/<key>` argv
    /// (pure — no process spawned), so it is unit-testable. `--quiet`
    /// suppresses the interactive progress UI in non-tty pods.
    pub fn cp_to_gcs_argv(&self, local_path: &str, key: &str) -> Vec<String> {
        vec![
            "storage".into(),
            "cp".into(),
            "--quiet".into(),
            local_path.into(),
            self.gcs_uri(key),
        ]
    }

    /// Build the `gcloud storage cp gs://<bucket>/<key> <local_path>` argv
    /// (pure — no process spawned), so it is unit-testable. This is the
    /// EXACT mirror of [`cp_to_gcs_argv`](Self::cp_to_gcs_argv) with source
    /// and destination swapped: it FETCHES an object the cell uploaded back
    /// to a local path for the coordinator to deserialize + fold (issue #179
    /// fan-IN). `--quiet` suppresses the interactive progress UI in non-tty
    /// pods.
    pub fn cp_from_gcs_argv(&self, key: &str, local_path: &str) -> Vec<String> {
        vec![
            "storage".into(),
            "cp".into(),
            "--quiet".into(),
            self.gcs_uri(key),
            local_path.into(),
        ]
    }
}

/// The proof store. Holds the resolved [`StorageConfig`] and drives EITHER
/// mounted-file I/O (issue #206, when `mount_path` is set) OR the `gcloud
/// storage cp` CLI (issue #179, the fallback). The selected transport is
/// fixed by config at construction; the `upload`/`download` SURFACE is
/// identical so callers (the fold transport, the cell, the worker) are
/// unchanged regardless of which transport is active.
#[derive(Debug, Clone)]
pub struct GcloudStorage {
    cfg: StorageConfig,
}

impl GcloudStorage {
    pub fn new(cfg: StorageConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &StorageConfig {
        &self.cfg
    }

    /// Upload raw `bytes` to the object `key`, returning the object key on
    /// success. Routes to the mounted-file transport (issue #206) when a
    /// mount path is configured, else to the `gcloud storage cp` CLI (issue
    /// #179). Errors are honest: a failed upload returns `Err` — the caller
    /// keeps `proof_object: None` and never fabricates a stored-bytes claim.
    pub fn upload(&self, key: &str, bytes: &[u8]) -> std::io::Result<String> {
        if !self.cfg.enabled() {
            return Err(std::io::Error::other(
                "proof store disabled (no bucket or mount configured); refusing to upload",
            ));
        }
        if self.cfg.mount_enabled() {
            return self.upload_mount(key, bytes);
        }

        // Stage to a uniquely-named temp file. The key contains a '/', so
        // flatten it for the temp filename to avoid creating subdirs in TMP.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "lighter-proof-{}-{}.bin",
            key.replace('/', "_"),
            std::process::id()
        ));
        std::fs::write(&tmp, bytes)?;

        let local_path = tmp.to_string_lossy().to_string();
        let argv = self.cfg.cp_to_gcs_argv(&local_path, key);
        let result = Command::new(&self.cfg.gcloud_bin).args(&argv).output();

        // Best-effort cleanup regardless of upload outcome.
        let _ = std::fs::remove_file(&tmp);

        let out = result?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "gcloud storage cp to {} failed: {}",
                self.cfg.gcs_uri(key),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(key.to_string())
    }

    /// Issue #206: ATOMIC mount-mode upload. Writes the bytes to a uniquely-
    /// named TEMP file in the SAME mount directory as the destination (so the
    /// final `rename` is a same-filesystem atomic move — a reader on any
    /// machine sees either the complete proof or no file, never a partial
    /// write), then renames it into place at `<mount>/<key>`. Parent
    /// directories (the `{height}/m/{level}/` namespace) are created as needed.
    ///
    /// Honest-failure: any write/rename error propagates as `Err` (no
    /// fabricated stored-bytes claim, issue #179). The temp file is cleaned up
    /// best-effort on the error path so a failed write leaves no junk on the
    /// mount.
    fn upload_mount(&self, key: &str, bytes: &[u8]) -> std::io::Result<String> {
        let dest = self.cfg.mount_object_path(key);
        let parent = dest.parent().ok_or_else(|| {
            std::io::Error::other(format!("mount upload: key '{key}' has no parent dir"))
        })?;
        std::fs::create_dir_all(parent)?;

        // Temp file in the SAME directory as the destination so `rename` is an
        // atomic same-filesystem move (a cross-fs rename would copy, breaking
        // atomicity). Unique per process + key to avoid collisions between
        // concurrent uploads of different keys on the same worker.
        let tmp = parent.join(format!(
            ".lighter-proof-tmp-{}-{}",
            key.rsplit('/').next().unwrap_or("k"),
            std::process::id()
        ));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(key.to_string())
    }

    /// Download the object stored under `key` in the configured bucket and
    /// return its raw bytes — the coordinator's fan-IN read (issue #179).
    /// Mirrors [`upload`](Self::upload)'s `gcloud storage cp` pattern with
    /// source/destination reversed: it `cp`s `gs://<bucket>/<key>` to a
    /// uniquely-named temp file (the CLI writes to a path, not stdout),
    /// reads the bytes back, then removes the temp file.
    ///
    /// Errors are honest: a missing object or a failed `cp` returns `Err`
    /// — the coordinator must NOT fabricate a proof when bytes are absent
    /// (issue #179 honest-failure rule). The caller deserializes the bytes
    /// into `ProofWithPublicInputs` with the SAME `serde_json` representation
    /// the cell uploaded (issue #117 export format), so the two sides never
    /// drift.
    pub fn download(&self, key: &str) -> std::io::Result<Vec<u8>> {
        if !self.cfg.enabled() {
            return Err(std::io::Error::other(
                "proof store disabled (no bucket or mount configured); refusing to download",
            ));
        }
        if self.cfg.mount_enabled() {
            return self.download_mount(key);
        }

        // Stage to a uniquely-named temp file. The key contains a '/', so
        // flatten it for the temp filename to avoid creating subdirs in TMP.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "lighter-proof-dl-{}-{}.bin",
            key.replace('/', "_"),
            std::process::id()
        ));
        let local_path = tmp.to_string_lossy().to_string();
        let argv = self.cfg.cp_from_gcs_argv(key, &local_path);
        let result = Command::new(&self.cfg.gcloud_bin).args(&argv).output();

        let out = match result {
            Ok(out) => out,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        if !out.status.success() {
            let _ = std::fs::remove_file(&tmp);
            return Err(std::io::Error::other(format!(
                "gcloud storage cp from {} failed: {}",
                self.cfg.gcs_uri(key),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        // Read the fetched bytes, then clean up regardless of read outcome.
        let bytes = std::fs::read(&tmp);
        let _ = std::fs::remove_file(&tmp);
        bytes
    }

    /// Issue #206: mount-mode download — read `<mount>/<key>` directly. A
    /// missing or unreadable file (not present, or not-yet-visible through the
    /// mount) returns `Err`, never a fabricated proof (issue #179). Because
    /// `upload_mount` writes atomically (temp + rename), a successful read
    /// always returns the COMPLETE bytes — there is no partial-read window.
    fn download_mount(&self, key: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.cfg.mount_object_path(key))
    }

    /// Issue #206: timed `upload` — returns `(object_key, wall)`. The wall is
    /// the `storage_upload_ms` instrumentation point: the time to write a proof
    /// OUT (mount: temp-write + atomic rename; CLI: temp-write + `gcloud cp`).
    pub fn upload_timed(&self, key: &str, bytes: &[u8]) -> std::io::Result<(String, Duration)> {
        let t = Instant::now();
        let k = self.upload(key, bytes)?;
        Ok((k, t.elapsed()))
    }

    /// Issue #206: timed `download` — returns `(bytes, wall)`. The wall is the
    /// `storage_download_ms` instrumentation point: the time to read an input
    /// proof IN (mount: a file read; CLI: `gcloud cp` + temp read).
    pub fn download_timed(&self, key: &str) -> std::io::Result<(Vec<u8>, Duration)> {
        let t = Instant::now();
        let bytes = self.download(key)?;
        Ok((bytes, t.elapsed()))
    }

    /// Issue #206: the READ-AFTER-WRITE BARRIER WAIT instrumentation point.
    ///
    /// Poll for `key` to become readable, returning the proof bytes plus a
    /// [`VisibilityWait`] that decomposes the wait into the time SPENT waiting
    /// for the object to appear (`wait`) and the final successful read
    /// (`read`). A next-level fold worker calls this for each merge input: the
    /// input was written on (possibly) ANOTHER machine at the previous level,
    /// and a gcsfuse mount can have read-after-write visibility lag, so the
    /// proof may not be readable the instant the task arrives. This is the
    /// gcsfuse-specific risk the issue calls out — the number that decides
    /// "mount is fine" vs "the mount's consistency lag is the new bottleneck"
    /// (feeds #207).
    ///
    /// `wait.wait` is `0` when the object is already visible on the first
    /// attempt (the common case once the level barrier has settled). Honest-
    /// failure: if the object never becomes readable before `deadline`, the
    /// LAST read error is returned as `Err` — never a fabricated proof.
    pub fn wait_for_object(
        &self,
        key: &str,
        deadline: Duration,
        poll: Duration,
    ) -> std::io::Result<(Vec<u8>, VisibilityWait)> {
        let started = Instant::now();
        let mut attempts: u64 = 0;
        loop {
            attempts += 1;
            // The wait is the time from the first poll up to the START of the
            // FINAL (successful) read — i.e. the read-after-write visibility
            // lag, NOT the read itself. On the first attempt this is the
            // elapsed time before any read began, which is ~0 (no lag observed).
            let read_started = Instant::now();
            let wait = read_started.saturating_duration_since(started);
            match self.download(key) {
                Ok(bytes) => {
                    let read = read_started.elapsed();
                    return Ok((
                        bytes,
                        VisibilityWait {
                            wait,
                            read,
                            attempts,
                        },
                    ));
                }
                Err(e) => {
                    if started.elapsed() >= deadline {
                        return Err(std::io::Error::other(format!(
                            "proof '{key}' not visible after {} attempt(s) over {:?} \
                             (read-after-write visibility wait exceeded; honest-failure, \
                             no fabricated proof): {e}",
                            attempts, deadline
                        )));
                    }
                    std::thread::sleep(poll);
                }
            }
        }
    }
}

/// Issue #206: the decomposed read-after-write barrier wait for one merge
/// input — emitted as `proof_visibility_wait_ms` / `storage_download_ms`
/// instrumentation. Returned by [`GcloudStorage::wait_for_object`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibilityWait {
    /// Time spent waiting for the just-written (on another machine) proof to
    /// become VISIBLE through the mount — the read-after-write consistency lag.
    /// `0` when the object was readable on the first attempt.
    pub wait: Duration,
    /// Wall of the final SUCCESSFUL read (`storage_download_ms`).
    pub read: Duration,
    /// Number of read attempts (1 = visible immediately, no lag observed).
    pub attempts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StorageConfig {
        StorageConfig {
            bucket: "kunal-scratch-lighter-prover-proofs".into(),
            gcloud_bin: "gcloud".into(),
            mount_path: String::new(),
        }
    }

    #[test]
    fn object_key_is_height_slash_witness_index() {
        // The EXACT key scheme the coordinator slice must mirror. Guard it.
        assert_eq!(proof_object_key(186_974_616, 3), "186974616/3");
        assert_eq!(proof_object_key(0, 0), "0/0");
        assert_eq!(proof_object_key(100, 55), "100/55");
    }

    #[test]
    fn merge_object_key_is_height_slash_m_slash_level_slash_index() {
        // The EXACT merge-transit key scheme both producing and consuming
        // coordinators must mirror (issue #198). Guard it.
        assert_eq!(merge_object_key(186_974_616, 1, 0), "186974616/m/1/0");
        assert_eq!(merge_object_key(0, 1, 0), "0/m/1/0");
        assert_eq!(merge_object_key(100, 3, 7), "100/m/3/7");
    }

    #[test]
    fn merge_key_never_collides_with_leaf_key() {
        // The `/m/` segment makes the merge namespace disjoint from the leaf
        // namespace: a leaf key's second segment is always a decimal index,
        // never the literal `m`. Exhaustively spot-check the boundary cases
        // that a naive `{height}/{level}/{index}` scheme would have collided
        // on (e.g. leaf {height}/1 vs a merge at level 1).
        let height = 42u64;
        // Every leaf key for this height.
        let leaf_keys: Vec<String> = (0..32).map(|i| proof_object_key(height, i)).collect();
        // Every merge key across several levels/indices for this height.
        let mut merge_keys: Vec<String> = Vec::new();
        for level in 1..=6u64 {
            for index in 0..32u64 {
                merge_keys.push(merge_object_key(height, level, index));
            }
        }
        for lk in &leaf_keys {
            assert!(
                !merge_keys.contains(lk),
                "leaf key {lk} collided with a merge key — namespaces must be disjoint"
            );
            // Structural guarantee: a leaf key has exactly one '/'; a merge
            // key has exactly three and its second segment is the literal 'm'.
            assert_eq!(lk.matches('/').count(), 1, "leaf key shape: {lk}");
        }
        for mk in &merge_keys {
            assert_eq!(mk.matches('/').count(), 3, "merge key shape: {mk}");
            assert_eq!(
                mk.split('/').nth(1),
                Some("m"),
                "merge key's second segment must be the literal 'm': {mk}"
            );
        }
    }

    #[test]
    fn disabled_when_bucket_empty() {
        let c = StorageConfig {
            bucket: String::new(),
            gcloud_bin: "gcloud".into(),
            mount_path: String::new(),
        };
        assert!(!c.enabled());
        let c2 = StorageConfig {
            bucket: "   ".into(),
            gcloud_bin: "gcloud".into(),
            mount_path: String::new(),
        };
        assert!(!c2.enabled(), "whitespace-only bucket is still disabled");
    }

    #[test]
    fn enabled_when_bucket_set() {
        assert!(cfg().enabled());
    }

    #[test]
    fn gcs_uri_prefixes_bucket() {
        let key = proof_object_key(100, 2);
        assert_eq!(
            cfg().gcs_uri(&key),
            "gs://kunal-scratch-lighter-prover-proofs/100/2"
        );
    }

    #[test]
    fn cp_argv_is_well_formed() {
        let key = proof_object_key(100, 2);
        let argv = cfg().cp_to_gcs_argv("/tmp/proof.bin", &key);
        assert_eq!(
            argv,
            vec![
                "storage".to_string(),
                "cp".to_string(),
                "--quiet".to_string(),
                "/tmp/proof.bin".to_string(),
                "gs://kunal-scratch-lighter-prover-proofs/100/2".to_string(),
            ]
        );
    }

    #[test]
    fn upload_refused_when_disabled() {
        let store = GcloudStorage::new(StorageConfig {
            bucket: String::new(),
            gcloud_bin: "gcloud".into(),
            mount_path: String::new(),
        });
        // No bucket → upload must error WITHOUT shelling out, so the cell
        // path stays None and never claims bytes were stored.
        let err = store.upload("100/2", b"bytes").unwrap_err();
        assert!(err.to_string().contains("disabled"));
    }

    #[test]
    fn cp_from_argv_is_well_formed_and_mirrors_upload() {
        // The coordinator's fan-IN fetch (issue #179): EXACT mirror of the
        // upload argv with src/dst swapped. Guard the swap so the two sides
        // never drift.
        let key = proof_object_key(186_974_616, 3);
        let argv = cfg().cp_from_gcs_argv(&key, "/tmp/proof.bin");
        assert_eq!(
            argv,
            vec![
                "storage".to_string(),
                "cp".to_string(),
                "--quiet".to_string(),
                "gs://kunal-scratch-lighter-prover-proofs/186974616/3".to_string(),
                "/tmp/proof.bin".to_string(),
            ]
        );
        // Round-trip invariant: the download argv's source URI is the same
        // URI the upload argv writes to (same key → same object).
        let up = cfg().cp_to_gcs_argv("/tmp/proof.bin", &key);
        assert_eq!(up[4], argv[3], "upload dest URI == download src URI");
    }

    #[test]
    fn download_refused_when_disabled() {
        let store = GcloudStorage::new(StorageConfig {
            bucket: String::new(),
            gcloud_bin: "gcloud".into(),
            mount_path: String::new(),
        });
        // No bucket → download must error WITHOUT shelling out, so the
        // coordinator never invents bytes for a fold.
        let err = store.download("100/2").unwrap_err();
        assert!(err.to_string().contains("disabled"));
    }

    // ─── Issue #206: mount-mode transport (hermetic — local temp dir, no
    // bucket, no gcloud, no auth) ──────────────────────────────────────────

    fn mount_cfg(root: &std::path::Path) -> StorageConfig {
        StorageConfig {
            // Bucket left set to prove mount mode is SELECTED OVER the CLI even
            // when a bucket is also configured (mount wins).
            bucket: "kunal-scratch-lighter-prover-proofs".into(),
            gcloud_bin: "gcloud".into(),
            mount_path: root.to_string_lossy().to_string(),
        }
    }

    fn unique_mount_root(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "lighter-mount-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    #[test]
    fn mount_enabled_selects_mount_mode() {
        let root = unique_mount_root("sel");
        let c = mount_cfg(&root);
        assert!(c.enabled());
        assert!(c.mount_enabled(), "a set mount path turns mount mode on");
        // Plain bucket-only config is NOT mount mode (CLI fallback path).
        assert!(!cfg().mount_enabled());
    }

    #[test]
    fn mount_object_path_maps_key_under_root() {
        let root = std::path::Path::new("/mnt/proofs");
        let c = StorageConfig {
            bucket: String::new(),
            gcloud_bin: "gcloud".into(),
            mount_path: "/mnt/proofs".into(),
        };
        // Leaf and merge keys both map directly under the mount root, EXACTLY
        // mirroring the unchanged key scheme that crosses Pub/Sub.
        assert_eq!(
            c.mount_object_path(&proof_object_key(100, 2)),
            root.join("100/2")
        );
        assert_eq!(
            c.mount_object_path(&merge_object_key(100, 1, 3)),
            root.join("100/m/1/3")
        );
    }

    #[test]
    fn mount_upload_then_download_round_trips() {
        let root = unique_mount_root("rt");
        let store = GcloudStorage::new(mount_cfg(&root));
        // A merge key exercises nested-dir creation (`{height}/m/{level}/`).
        let key = merge_object_key(186_974_616, 2, 5);
        let bytes = b"real-proof-bytes-\x00\x01\x02".to_vec();
        let returned = store.upload(&key, &bytes).expect("mount upload");
        assert_eq!(returned, key, "upload returns the object key unchanged");
        // The file lives at <root>/<key> — the key IS the path.
        assert!(root.join(&key).is_file(), "proof written under the mount");
        let got = store.download(&key).expect("mount download");
        assert_eq!(got, bytes, "round-trip is byte-identical");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mount_download_missing_is_honest_err() {
        let root = unique_mount_root("missing");
        let store = GcloudStorage::new(mount_cfg(&root));
        // A not-yet-written (or not-yet-visible) proof must be an Err, never a
        // fabricated proof (issue #179 honest-failure rule).
        let err = store.download(&merge_object_key(1, 1, 0)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mount_write_is_atomic_no_temp_leftovers() {
        // After a successful upload only the FINAL object exists in its dir —
        // the temp file was renamed into place, not left behind. A reader can
        // therefore never observe a partial `.lighter-proof-tmp-*` file as the
        // object (it has a distinct name AND is gone post-rename).
        let root = unique_mount_root("atomic");
        let store = GcloudStorage::new(mount_cfg(&root));
        let key = merge_object_key(7, 1, 0);
        store.upload(&key, b"abc").unwrap();
        let dir = root.join("7/m/1");
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["0".to_string()], "only the final object remains");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mount_overwrite_is_idempotent() {
        // A redelivered merge task re-uploads under the SAME key (issue #203
        // at-least-once). The atomic rename must overwrite cleanly so the
        // leader's #193 re-sort still reads the (identical) latest bytes.
        let root = unique_mount_root("overwrite");
        let store = GcloudStorage::new(mount_cfg(&root));
        let key = proof_object_key(9, 0);
        store.upload(&key, b"first").unwrap();
        store.upload(&key, b"second").unwrap();
        assert_eq!(store.download(&key).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_for_object_returns_immediately_when_present() {
        let root = unique_mount_root("wait-present");
        let store = GcloudStorage::new(mount_cfg(&root));
        let key = proof_object_key(3, 1);
        store.upload(&key, b"xyz").unwrap();
        let (bytes, wait) = store
            .wait_for_object(&key, Duration::from_secs(1), Duration::from_millis(5))
            .expect("already-visible object");
        assert_eq!(bytes, b"xyz");
        assert_eq!(wait.attempts, 1, "visible on first attempt → no lag");
        // No polling happened, so the wait is sub-millisecond — it rounds to
        // `proof_visibility_wait_ms=0` in the emitted metric (no observable lag).
        assert!(
            wait.wait < Duration::from_millis(1),
            "no read-after-write wait (sub-ms): {:?}",
            wait.wait
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_for_object_polls_until_visible_then_reports_wait() {
        // Simulate read-after-write lag: a writer thread creates the object
        // after a short delay; wait_for_object must poll, succeed, and report a
        // NON-ZERO visibility wait + >1 attempt (the gcsfuse-lag instrumentation).
        let root = unique_mount_root("wait-lag");
        std::fs::create_dir_all(&root).unwrap();
        let store = GcloudStorage::new(mount_cfg(&root));
        let key = proof_object_key(4, 2);
        let writer_store = store.clone();
        let writer_key = key.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            writer_store.upload(&writer_key, b"late").unwrap();
        });
        let (bytes, wait) = store
            .wait_for_object(&key, Duration::from_secs(5), Duration::from_millis(10))
            .expect("object becomes visible within deadline");
        writer.join().unwrap();
        assert_eq!(bytes, b"late");
        assert!(wait.attempts > 1, "had to poll: attempts={}", wait.attempts);
        assert!(
            wait.wait > Duration::ZERO,
            "non-zero read-after-write visibility wait recorded"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wait_for_object_honest_timeout_when_never_visible() {
        let root = unique_mount_root("wait-timeout");
        std::fs::create_dir_all(&root).unwrap();
        let store = GcloudStorage::new(mount_cfg(&root));
        let err = store
            .wait_for_object(
                &proof_object_key(5, 0),
                Duration::from_millis(40),
                Duration::from_millis(10),
            )
            .expect_err("never-written object must time out honestly");
        assert!(
            err.to_string().contains("not visible"),
            "honest visibility-timeout error, never a fabricated proof: {err}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_timed_and_download_timed_round_trip_with_walls() {
        let root = unique_mount_root("timed");
        let store = GcloudStorage::new(mount_cfg(&root));
        let key = proof_object_key(6, 0);
        let (k, _up_wall) = store.upload_timed(&key, b"timed-bytes").unwrap();
        assert_eq!(k, key);
        let (bytes, _dl_wall) = store.download_timed(&key).unwrap();
        assert_eq!(bytes, b"timed-bytes");
        let _ = std::fs::remove_dir_all(&root);
    }
}
