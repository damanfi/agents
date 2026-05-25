//! Filesystem-backed credit-mutual-aid inbox.
//!
//! Bust bees cannot afford gas to submit their own loan requests on Arc (Arc
//! uses USDC as native gas; `with_recommended_fillers` pre-deducts
//! `gas_limit * max_fee_per_gas` before the tx ever lands). The recovery loop
//! is mutual: a bust bee signs an EIP-712 `LoanRequest` locally (no gas), writes
//! the signed payload into a shared filesystem inbox, and a relief peer polls
//! the inbox on its own tick and submits via `requestLoanWithSignature` on the
//! bust bee's behalf.
//!
//! This module is the transport. It is intentionally filesystem-only because
//! hum's `chi:"gossip-subscribe"` bee-side bridge is not wired yet (see
//! `factories::tool_subscribe_to_role_events`). Once the bridge lands, the
//! same `SignedLoanRequest` shape can ride a gossip topic verbatim.
//!
//! Layout:
//!
//! ```text
//! $XDG_STATE_HOME/hum/daman/credit-p2p/
//!   <borrower_eoa>-<nonce>.signed.json            # unprocessed
//!   <borrower_eoa>-<nonce>.submitted-<tx>.json    # already relayed
//! ```
//!
//! All filenames are lowercase 0x-hex for the borrower address and decimal for
//! the nonce, so the directory is grep-able by humans. Files older than
//! `GC_WINDOW_SECS` are garbage-collected on list (whether they're pending or
//! submitted) so a stuck inbox doesn't grow without bound.
//!
//! Directory is mode 0700; the inbox is per-host shared state across the 27
//! persona processes that all run under the same uid.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Files older than this (in seconds, measured against `signed_at_ts`) are
/// garbage-collected on the next `list_pending` call. 4 hours is generous; the
/// EIP-712 deadline is the authoritative expiry, but a stale request the
/// borrower never followed up on is just noise after a few hours.
pub const GC_WINDOW_SECS: u64 = 4 * 60 * 60;

/// Signed loan-request payload. Mirrors the EIP-712 `LoanRequest` body returned
/// by `tool_sign_loan_request`, plus a couple of provenance fields so a relief
/// peer can show the operator what it picked up before submitting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedLoanRequest {
    /// Borrower EOA, 0x-prefixed lowercase hex.
    pub borrower: String,
    /// USDC base-units (decimal string).
    pub amount: String,
    /// uint256 nonce, decimal string. Reads from `benevolence.nonceOf(borrower)`.
    pub nonce: String,
    /// Signature expiry, unix seconds, decimal string.
    pub deadline: String,
    /// 65-byte EIP-712 signature, 0x-prefixed hex.
    pub signature: String,
    /// Unix seconds the borrower signed at. Provenance only.
    pub signed_at_ts: u64,
    /// bee_name of the borrower; populated by the publisher for log clarity.
    pub by_bee: String,
    /// Short human-readable reason (e.g. "gas top-up after subscribe revert").
    pub reason: String,
}

/// One pending inbox entry surfaced to the relief peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    /// File basename (NOT full path). Pass back to `mark_submitted` after
    /// relay so the loop is idempotent across ticks.
    pub filename: String,
    pub request: SignedLoanRequest,
    /// Seconds elapsed since `signed_at_ts`. Useful for the relief persona's
    /// reasoning ("this is fresh" vs "this has been hanging for an hour").
    pub age_seconds: u64,
}

/// Resolve `$XDG_STATE_HOME/hum/daman/credit-p2p`, defaulting to
/// `$HOME/.local/state/hum/daman/credit-p2p` per the XDG Base Directory spec.
/// Does NOT create the directory; callers that need it created (the publish
/// path) call `ensure_inbox_dir` instead.
pub fn inbox_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("hum/daman/credit-p2p");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".local/state/hum/daman/credit-p2p")
}

/// Create `inbox_dir()` (and parents) with mode 0700 if missing. Idempotent.
fn ensure_inbox_dir() -> Result<PathBuf, String> {
    let dir = inbox_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    apply_owner_only_mode(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn apply_owner_only_mode(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)
        .map_err(|e| format!("stat {}: {e}", p.display()))?
        .permissions();
    if perm.mode() & 0o777 != 0o700 {
        perm.set_mode(0o700);
        fs::set_permissions(p, perm).map_err(|e| format!("chmod {}: {e}", p.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_owner_only_mode(_p: &Path) -> Result<(), String> {
    Ok(())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Normalize a borrower 0x-address to lowercase hex (no checksum) so two
/// different bees that happen to format the same address with different case
/// don't collide the inbox filename.
fn normalize_addr(addr: &str) -> String {
    let s = addr.trim();
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        format!("0x{}", rest.to_ascii_lowercase())
    } else {
        s.to_ascii_lowercase()
    }
}

/// Write the signed payload atomically: serialize to JSON, write to a `.tmp`
/// sibling, fsync, rename into place. Two relief peers that happened to read
/// the directory mid-write would never see a half-written file.
pub fn publish_request(req: &SignedLoanRequest) -> Result<PathBuf, String> {
    let dir = ensure_inbox_dir()?;
    let borrower = normalize_addr(&req.borrower);
    let basename = format!("{borrower}-{nonce}.signed.json", nonce = req.nonce);
    let final_path = dir.join(&basename);
    let tmp_path = dir.join(format!(".{basename}.tmp"));

    let body =
        serde_json::to_vec_pretty(req).map_err(|e| format!("serialize signed request: {e}"))?;

    {
        let mut f = fs::File::create(&tmp_path)
            .map_err(|e| format!("create {}: {e}", tmp_path.display()))?;
        f.write_all(&body)
            .map_err(|e| format!("write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, &final_path).map_err(|e| {
        format!(
            "rename {} -> {}: {e}",
            tmp_path.display(),
            final_path.display()
        )
    })?;
    Ok(final_path)
}

/// List unprocessed `*.signed.json` entries, oldest-first. Also opportunistic
/// garbage collection: any file older than `GC_WINDOW_SECS` (whether signed
/// or already submitted) is removed before the list is returned.
pub fn list_pending() -> Result<Vec<PendingRequest>, String> {
    let dir = inbox_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let now = now_unix_secs();
    let mut out: Vec<PendingRequest> = Vec::new();
    let entries =
        fs::read_dir(&dir).map_err(|e| format!("readdir {}: {e}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Skip the temp-files used by atomic write.
        if file_name.starts_with('.') {
            continue;
        }

        // Garbage-collect both .signed.json and .submitted-*.json once they're
        // older than the GC window. We use mtime as the floor; even a file
        // with a bogus signed_at_ts will get collected eventually.
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(dur) = SystemTime::now().duration_since(mtime) {
                    if dur.as_secs() > GC_WINDOW_SECS {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                }
            }
        }

        if !file_name.ends_with(".signed.json") {
            continue;
        }

        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let request: SignedLoanRequest = match serde_json::from_slice(&bytes) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let age = now.saturating_sub(request.signed_at_ts);
        out.push(PendingRequest {
            filename: file_name.to_string(),
            request,
            age_seconds: age,
        });
    }

    out.sort_by_key(|p| p.request.signed_at_ts);
    Ok(out)
}

/// Rename `<filename>.signed.json` to `<filename>.submitted-<tx_hash>.json`.
/// Same directory; the rename is the only state change. Strips any `0x` prefix
/// from tx_hash so the filename pattern is uniform.
pub fn mark_submitted(filename: &str, tx_hash: &str) -> Result<(), String> {
    if filename.contains('/') || filename.contains('\\') {
        return Err(format!("filename must be a basename, got {filename:?}"));
    }
    let stem = filename
        .strip_suffix(".signed.json")
        .ok_or_else(|| format!("filename {filename:?} not in *.signed.json form"))?;
    let tx_trim = tx_hash.trim_start_matches("0x").trim_start_matches("0X");
    let dir = inbox_dir();
    let from = dir.join(filename);
    let to = dir.join(format!("{stem}.submitted-{tx_trim}.json"));
    fs::rename(&from, &to)
        .map_err(|e| format!("rename {} -> {}: {e}", from.display(), to.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that mutate the process-global `XDG_STATE_HOME` env
    /// var. Cargo runs `#[test]`s on a thread pool by default and the env
    /// var is shared state; without this, parallel runs see each other's
    /// inbox dirs and the rename assertions race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
        _dir: tempfile::TempDir,
    }

    fn isolate_state_home() -> EnvGuard<'static> {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("XDG_STATE_HOME", dir.path());
        EnvGuard {
            _lock: lock,
            _dir: dir,
        }
    }

    fn fixture(nonce: u64) -> SignedLoanRequest {
        SignedLoanRequest {
            borrower: "0xABCdef0000000000000000000000000000000001".into(),
            amount: "2000000".into(),
            nonce: nonce.to_string(),
            deadline: "9999999999".into(),
            signature: format!("0x{}", "ab".repeat(65)),
            signed_at_ts: now_unix_secs(),
            by_bee: "daman-leader-alpha".into(),
            reason: "gas top-up".into(),
        }
    }

    #[test]
    fn publish_creates_dir_with_owner_only_mode() {
        let _g = isolate_state_home();
        let req = fixture(1);
        let path = publish_request(&req).expect("publish");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let parent = path.parent().unwrap();
            let mode = fs::metadata(parent).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn publish_uses_lowercase_borrower_filename() {
        let _g = isolate_state_home();
        let req = fixture(2);
        let path = publish_request(&req).expect("publish");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("0xabcdef"), "got {name}");
        assert!(name.ends_with("-2.signed.json"), "got {name}");
    }

    #[test]
    fn list_pending_returns_published_entries_sorted() {
        let _g = isolate_state_home();
        let mut a = fixture(10);
        a.signed_at_ts = 100;
        let mut b = fixture(11);
        b.signed_at_ts = 50;
        publish_request(&a).unwrap();
        publish_request(&b).unwrap();
        let pending = list_pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].request.nonce, "11");
        assert_eq!(pending[1].request.nonce, "10");
    }

    #[test]
    fn mark_submitted_renames_to_submitted_form() {
        let _g = isolate_state_home();
        let req = fixture(7);
        let path = publish_request(&req).unwrap();
        let filename = path.file_name().unwrap().to_str().unwrap().to_string();
        mark_submitted(&filename, "0xdeadbeef").unwrap();
        // Pending now has no signed.json entries.
        let pending = list_pending().unwrap();
        assert!(pending.is_empty());
        // The submitted-form file exists.
        let dir = inbox_dir();
        let entries: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect();
        assert!(
            entries.iter().any(|e| e.ends_with(".submitted-deadbeef.json")),
            "got {entries:?}"
        );
    }

    #[test]
    fn mark_submitted_rejects_filename_with_path_sep() {
        let _g = isolate_state_home();
        let err = mark_submitted("foo/bar.signed.json", "0xabc").unwrap_err();
        assert!(err.contains("basename"), "got {err}");
    }

    #[test]
    fn list_pending_skips_non_signed_files() {
        let _g = isolate_state_home();
        let dir = ensure_inbox_dir().unwrap();
        fs::write(dir.join("loose-note.txt"), "ignore me").unwrap();
        fs::write(dir.join("0xfeed-1.submitted-aa.json"), "{}").unwrap();
        let pending = list_pending().unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn list_pending_handles_missing_dir() {
        let _g = isolate_state_home();
        // Do not create the inbox dir; list should return empty Ok.
        let pending = list_pending().unwrap();
        assert!(pending.is_empty());
    }
}
