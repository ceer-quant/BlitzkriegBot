//! Dynamic strategy loader — dlopen user-layer strategy libraries (`.dylib` /
//! `.so` / `.dll`) implementing C ABI **v2** and wrap them as a full
//! [`crate::strategies::foreign::ForeignStrategy`] (which itself implements the
//! same `EngineStrategy` contract as an in-tree strategy).
//!
//! ARCHITECTURAL GUARANTEE: a loaded strategy only ever receives borrowed
//! read-only market/context views and returns intents as data. It has no handle
//! to credentials, the CLOB client, the order manager or the UDS socket — none
//! of those cross the dynamic boundary. [`policy_allows`] additionally refuses
//! credential-looking filenames before dlopen.
//!
//! ## TRUST MODEL — read this before promising anybody a "sandbox" (#188)
//!
//! A strategy library is **native code loaded into the kernel process** by
//! `dlopen`. It shares the kernel's address space, its file descriptors and its
//! environment — including the venue credentials the live executor reads. There
//! is no sandbox here and none can be built at this layer: a library that wants
//! to exfiltrate keys simply calls `getenv`. The controls below are therefore
//! about *where a library may come from* and *who approved it*, not about
//! limiting what an approved library can do:
//!
//!   1. **Path policy** ([`path_policy`]) — the library must live under an
//!      approved root (`<repo>/user_layer/strategies` by default, plus anything
//!      declared in `BLITZKRIEG_STRATEGY_ALLOW_DIRS`). Shared temp directories
//!      (`/tmp`, `/var/tmp`, `$TMPDIR`), `~/Downloads`, `~/Desktop` and
//!      `/Users/Shared` are never approved roots.
//!   2. **Approval manifest** ([`Manifest`]) — an operator-written list of
//!      `sha256 <path>` lines. A library outside an approved root, or produced by
//!      Shadow Evolution, loads ONLY when the manifest lists it AND the file's
//!      digest still matches. Swapping the bytes in place invalidates the
//!      approval.
//!   3. **Not world-writable** — a library any local user may rewrite is refused
//!      regardless of location.
//!
//! Residual risk, stated plainly: an attacker who can write into an approved
//! directory (or edit the manifest) is inside the trust boundary, because the
//! approval is a local file and not a signature. Process isolation (a strategy
//! child process with a restricted IPC channel) is the only real fix and is
//! tracked separately; until then this is a *gate*, not a sandbox.
//!
//! Negotiation order is deliberate: policy → dlopen → read the version symbol
//! → read the vtable. Version is settled BEFORE the vtable layout is trusted, so
//! a v1 library is rejected on version and never misread as v2.
//!
//! Runtime flow:
//!   1. [`policy_allows`] — static name policy; [`path_policy`] — trust policy.
//!   2. `dlopen` the library.
//!   3. `bk_strategy_abi_version()` — MUST equal 2 (clean break, no v1 shim).
//!   4. resolve `bk_strategy_free_string` and `bk_strategy_create` → vtable.
//!   5. verify vtable.abi_version/min_abi and the REQUIRED hooks.
//!   6. `create()` → handle, then wrap as `ForeignStrategy`.

#[cfg(feature = "strategy-loading")]
use blitzkrieg_strategy_api::{
    BK_ABI_VERSION, BK_BIND_EVAL_CTX_SYMBOL, BK_CONFIG_VIEW_SYMBOL, BK_CREATE_SYMBOL,
    BK_EVOLVABLE_KNOBS_SYMBOL, BK_FREE_STRING_SYMBOL, BK_GATE_EXEMPTIONS_SYMBOL,
    BK_MIN_ABI_VERSION, BK_VERSION_SYMBOL, BkStrategyVtable, bk_strategy_free_string,
};
use std::path::{Path, PathBuf};

/// Outcome of attempting to load a strategy library.
pub enum LoadOutcome {
    /// Policy rejected the path before any dlopen.
    Rejected { path: PathBuf, reason: String },
    /// Library opened, negotiated and registered.
    Loaded {
        path: PathBuf,
        name: String,
        version: String,
    },
    /// Policy passed but load/negotiate/create failed.
    Failed { path: PathBuf, reason: String },
}

impl std::fmt::Debug for LoadOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadOutcome::Rejected { path, reason } => {
                write!(
                    f,
                    "Rejected {{ path: {}, reason: {} }}",
                    path.display(),
                    reason
                )
            }
            LoadOutcome::Loaded {
                path,
                name,
                version,
            } => {
                write!(
                    f,
                    "Loaded {{ path: {}, name: {}, version: {} }}",
                    path.display(),
                    name,
                    version
                )
            }
            LoadOutcome::Failed { path, reason } => {
                write!(
                    f,
                    "Failed {{ path: {}, reason: {} }}",
                    path.display(),
                    reason
                )
            }
        }
    }
}

/// Policy decision for a candidate strategy library path. Pure and testable.
pub fn policy_allows(path: &Path) -> Result<(), String> {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    for bad in [".env", "private", "secret", "key", "credential"] {
        if name.to_lowercase().contains(bad) {
            return Err(format!(
                "strategy library name looks like it bundles credentials: {name}"
            ));
        }
    }
    let ext_ok = name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll");
    if !ext_ok {
        return Err(format!("not a shared library: {name}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Trust policy (#188)
// ---------------------------------------------------------------------------

/// Extra directories the operator trusts, `:`-separated (unix). Mirrors the
/// `--strategy-dir` escape hatch for deployments whose libraries live outside
/// the repository.
pub const ENV_ALLOW_DIRS: &str = "BLITZKRIEG_STRATEGY_ALLOW_DIRS";
/// Explicit approval-manifest path. Overrides the default location below.
pub const ENV_MANIFEST: &str = "BLITZKRIEG_STRATEGY_MANIFEST";
/// The primary strategy tree every repository ships.
pub const APPROVED_ROOT: &str = "user_layer/strategies";
/// Every approved root, relative to a repository root.
///
/// `user_layer/parity_strategy` is in the list because CI and `foreign_parity.rs`
/// build and load it as the reference C ABI v2 implementation; it is hand-written
/// source in the same checked-in tree as `user_layer/strategies`, so it carries
/// exactly the same trust and no more — a build of it that lands under a
/// machine-generated directory (Shadow Evolution output, soak artifacts) is still
/// quarantined by [`TrustPolicy::check`].
pub const APPROVED_ROOTS: [&str; 2] = [APPROVED_ROOT, "user_layer/parity_strategy"];
/// Operator-written approval list, looked up under each repository root.
pub const DEFAULT_MANIFEST: &str = "user_layer/strategies/approved.manifest";
/// Where machine-generated (Shadow Evolution, soak, download) artifacts land.
/// Never an approved root — see [`path_policy`].
const ARTIFACT_ROOTS: [&str; 2] = ["data", "shadow_evolution"];

/// The trust decision for one candidate library path, with the full policy
/// resolved once so a caller can inspect it (tests, receipts, logs).
#[derive(Debug, Clone, Default)]
pub struct TrustPolicy {
    /// Canonical directories that may hold loadable strategies.
    pub approved_dirs: Vec<PathBuf>,
    /// Explicitly untrusted directories (shared temp, downloads, …). Listed for
    /// the rejection message; a path here is refused unless an approved root is
    /// a prefix (a repository checked out under `$TMPDIR` is still a repository).
    pub untrusted_dirs: Vec<PathBuf>,
    /// Approval manifest, when one exists.
    pub manifest: Option<PathBuf>,
}

impl TrustPolicy {
    /// Resolve the policy from the environment, the process working directory
    /// and the compile-time repository location.
    pub fn from_env() -> Self {
        let mut approved_dirs = repo_roots()
            .into_iter()
            .flat_map(|r| APPROVED_ROOTS.into_iter().map(move |a| r.join(a)))
            .collect::<Vec<_>>();
        if let Ok(extra) = std::env::var(ENV_ALLOW_DIRS) {
            approved_dirs.extend(
                extra
                    .split(':')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from),
            );
        }
        let mut approved_dirs = approved_dirs
            .into_iter()
            .filter_map(|p| canonical_dir(&p))
            .collect::<Vec<_>>();
        approved_dirs.sort();
        approved_dirs.dedup();

        let manifest = std::env::var(ENV_MANIFEST)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                repo_roots()
                    .into_iter()
                    .map(|r| r.join(DEFAULT_MANIFEST))
                    .find(|p| p.is_file())
            });

        Self {
            approved_dirs,
            untrusted_dirs: untrusted_dirs(),
            manifest,
        }
    }

    /// Check one candidate library path.
    ///
    /// Order is load-bearing: a path inside an approved root short-circuits the
    /// untrusted check (developers and CI check the repository out wherever they
    /// like, including `$TMPDIR`), while everything else needs either an approved
    /// root or a matching manifest entry.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let absolute = absolutize(path);
        let inside_approved = self
            .approved_dirs
            .iter()
            .any(|root| absolute.starts_with(root));

        // 1. Never load a library any local user can rewrite.
        #[cfg(unix)]
        if let Ok(meta) = std::fs::metadata(&absolute) {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o002 != 0 {
                return Err(format!(
                    "strategy library is world-writable ({mode:04o}): {}",
                    absolute.display()
                ));
            }
        }

        // 2. Machine-generated artifacts (Shadow Evolution variants, soak
        //    outputs, downloaded files) are quarantined: an approved root alone
        //    is not enough, they need an explicit hash-pinned approval. This is
        //    the "进化产物必须人工审批" rule.
        if is_machine_generated(&absolute) {
            return self.approval_or_refuse(&absolute, "machine-generated artifact");
        }

        // 3. Shared temp / download directories.
        if !inside_approved
            && let Some(bad) = self.untrusted_dirs.iter().find(|d| absolute.starts_with(d))
        {
            return Err(format!(
                "{} lives under {} — a shared directory is never a strategy root; move the \
                 library under {} or declare its directory in {} (see SECURITY.md)",
                absolute.display(),
                bad.display(),
                self.approved_dirs
                    .first()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| APPROVED_ROOT.to_string()),
                ENV_ALLOW_DIRS
            ));
        }

        // 4. Approved root.
        if inside_approved {
            return Ok(());
        }

        // 5. Otherwise: an explicit, hash-pinned approval.
        self.approval_or_refuse(&absolute, "outside every approved strategy root")
    }

    /// Require a manifest entry whose digest matches the file on disk.
    fn approval_or_refuse(&self, absolute: &Path, why: &str) -> Result<(), String> {
        let Some(manifest) = &self.manifest else {
            return Err(format!(
                "{} is rejected ({why}) and no approval manifest exists; approve it by adding a \
                 `sha256 <path>` line to {} (or set {})",
                absolute.display(),
                DEFAULT_MANIFEST,
                ENV_MANIFEST
            ));
        };
        let text = std::fs::read_to_string(manifest)
            .map_err(|e| format!("cannot read approval manifest {}: {e}", manifest.display()))?;
        let Ok(actual) = sha256_file(absolute) else {
            return Err(format!(
                "cannot hash {} for approval against {}",
                absolute.display(),
                manifest.display()
            ));
        };
        for entry in parse_manifest(&text, manifest) {
            if entry.path == absolute {
                return if entry.sha256 == actual {
                    Ok(())
                } else {
                    Err(format!(
                        "{} is listed in {} but its sha256 changed (approved {}, found {actual}); \
                         re-review the library and update the manifest",
                        absolute.display(),
                        manifest.display(),
                        entry.sha256
                    ))
                };
            }
        }
        Err(format!(
            "{} is rejected ({why}) and is not listed in {}",
            absolute.display(),
            manifest.display()
        ))
    }
}

/// One `sha256 <path>` line of an approval manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub sha256: String,
    pub path: PathBuf,
}

/// Parse an approval manifest.
///
/// Deliberately tolerant about layout, strict about content: blank lines and
/// `#` comments are ignored, `sha256sum` order (`<hash> <path>`) and the reverse
/// are both accepted, a leading `*` (binary marker) is dropped, and relative
/// paths resolve against the manifest's own directory so a committed manifest
/// stays portable. A line with no 64-hex-char field is skipped rather than
/// quietly approving something.
pub fn parse_manifest(text: &str, manifest: &Path) -> Vec<ManifestEntry> {
    let base = manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(first), Some(second)) = (fields.next(), fields.next()) else {
            continue;
        };
        // `sha256sum` writes `<hash>  <path>` and, in binary mode, `<hash>  *<path>`.
        // The reverse order is accepted too, so a hand-written manifest cannot be
        // "wrongly formatted" in a way that silently approves nothing.
        let (hash, raw_path) = if is_sha256_hex(first) {
            (first.to_lowercase(), second)
        } else if is_sha256_hex(second) {
            (second.to_lowercase(), first)
        } else if is_sha256_hex(first.strip_prefix('*').unwrap_or(first)) {
            (
                first.strip_prefix('*').unwrap_or(first).to_lowercase(),
                second,
            )
        } else if is_sha256_hex(second.strip_prefix('*').unwrap_or(second)) {
            (
                second.strip_prefix('*').unwrap_or(second).to_lowercase(),
                first,
            )
        } else {
            continue;
        };
        let raw_path = raw_path.strip_prefix('*').unwrap_or(raw_path);
        if raw_path.is_empty() {
            continue;
        }
        let p = Path::new(raw_path);
        let path = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        out.push(ManifestEntry {
            sha256: hash,
            path: absolutize(&path),
        });
    }
    out
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether a path is a machine-generated artifact rather than a checked-in or
/// hand-built strategy: anything under a `data/` tree, or any component naming
/// Shadow Evolution. These never satisfy the approved-root rule on their own.
fn is_machine_generated(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        ARTIFACT_ROOTS.contains(&name.as_str())
            || name.starts_with("shadow")
            || name.starts_with("evolution")
    })
}

/// Candidate repository roots, canonicalized and de-duplicated.
///
/// Two anchors, because the two matter at different times: the compile-time
/// crate location (`<root>/core/blitzkrieg_core`) covers the build tree, and the
/// runtime working directory covers a binary that was built elsewhere and then
/// deployed — the upgrade path swaps a release binary into a *different* checkout.
fn repo_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(build_root) = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2) {
        roots.push(build_root.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut cur: Option<&Path> = Some(&cwd);
        // Bounded walk: a kernel started deep in a tree still finds its root,
        // and a kernel started at `/` does not scan the filesystem.
        for _ in 0..8 {
            let Some(dir) = cur else { break };
            if APPROVED_ROOTS.iter().any(|a| dir.join(a).is_dir()) || dir.join(".git").exists() {
                roots.push(dir.to_path_buf());
                break;
            }
            cur = dir.parent();
        }
        roots.push(cwd);
    }
    let mut out: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|p| canonical_dir(&p))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Make a path comparable with the canonical policy directories.
///
/// `canonicalize` needs every component to exist, and a rejected candidate often
/// does not — so the deepest existing ancestor is canonicalized and the missing
/// tail re-appended. On macOS this is what makes `$TMPDIR` (`/var/folders/…`)
/// match the canonical `/private/var/folders/…`; skipping it would leave the
/// temp-directory rule silently ineffective.
fn absolutize(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(d) => d.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur: &Path = &base;
    loop {
        if let Ok(canonical) = cur.canonicalize() {
            let mut out = canonical;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return base,
        }
    }
}

fn canonical_dir(p: &Path) -> Option<PathBuf> {
    let c = p.canonicalize().ok()?;
    c.is_dir().then_some(c)
}

/// Directories that are never strategy roots: shared scratch space and the two
/// folders a browser puts downloads in.
fn untrusted_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "/tmp",
        "/var/tmp",
        "/private/tmp",
        "/private/var/tmp",
        "/Users/Shared",
    ]
    .into_iter()
    .filter_map(|p| canonical_dir(Path::new(p)))
    .collect();
    if let Some(tmp) = canonical_dir(&std::env::temp_dir()) {
        dirs.push(tmp);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in ["Downloads", "Desktop", "Library/Mobile Documents"] {
            if let Some(d) = canonical_dir(&home.join(sub)) {
                dirs.push(d);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The trust gate for one candidate library. Thin wrapper over
/// [`TrustPolicy::from_env`] + [`TrustPolicy::check`] so every load path shares
/// one decision.
pub fn path_policy(path: &Path) -> Result<(), String> {
    TrustPolicy::from_env().check(path)
}

/// lowercase hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish_hex())
}

/// Minimal SHA-256 (FIPS 180-4). Dependency-free on purpose: the loader may not
/// grow the crate's dependency tree for an integrity check, and the algorithm is
/// fixed, public and testable against the standard vectors — which the unit
/// tests below do.
struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0u8; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (i, wi) in w.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(*wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, add) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(add);
        }
    }

    fn finish_hex(mut self) -> String {
        let bit_len = self.total_len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0x00]);
        }
        // `update` counted the padding; write the length of the MESSAGE only.
        let mut block = self.buf;
        block[56..64].copy_from_slice(&bit_len.to_be_bytes());
        self.compress(&block);
        self.state
            .iter()
            .map(|w| format!("{w:08x}"))
            .collect::<String>()
    }
}

/// A loaded, negotiated v2 strategy before registration.
#[cfg(feature = "strategy-loading")]
pub struct LoadedForeign {
    pub strategy: crate::strategies::foreign::ForeignStrategy,
    pub name: String,
    pub version: String,
    /// Shared entry gates this library declared it does not need (E2-b / #27).
    /// Default (nothing declared, or no such symbol) = fully gated.
    pub gate_exemptions: crate::strategies::GateExemptions,
    /// The knobs this library declared evolvable (E2-c / #28). Empty = **not
    /// evolvable** (no symbol, or nothing declared), which is an explicit
    /// declaration and is reported as such at registration.
    pub evolvable_knobs: Vec<crate::shadow_evolution::KnobSpec>,
}

/// Load and negotiate a v2 strategy library, returning it boxed as the full
/// `EngineStrategy` contract ready for `Engine::register_user_strategy`.
#[cfg(feature = "strategy-loading")]
pub fn load_foreign(path: &Path) -> Result<LoadedForeign, LoadOutcome> {
    // Both static policy stages run BEFORE dlopen: a rejected path must never
    // reach the dynamic linker, because opening the library already runs its
    // initialisers.
    let policy_error = |path: &Path| -> Option<String> {
        policy_allows(path)
            .err()
            .or_else(|| path_policy(path).err())
    };
    let fail = |reason: String| -> Result<LoadedForeign, LoadOutcome> {
        Err(if let Some(policy) = policy_error(path) {
            LoadOutcome::Rejected {
                path: path.to_path_buf(),
                reason: format!("{policy} (load failed: {reason})"),
            }
        } else {
            LoadOutcome::Failed {
                path: path.to_path_buf(),
                reason,
            }
        })
    };

    if let Err(reason) = policy_allows(path) {
        return Err(LoadOutcome::Rejected {
            path: path.to_path_buf(),
            reason,
        });
    }
    if let Err(reason) = path_policy(path) {
        return Err(LoadOutcome::Rejected {
            path: path.to_path_buf(),
            reason,
        });
    }

    // SAFETY: opening a shared library runs its initialisers. We only load
    // libraries the operator placed in the strategy directory after policy.
    let lib = match unsafe { libloading::Library::new(path) } {
        Ok(l) => l,
        Err(e) => return fail(format!("dlopen failed: {e}")),
    };

    // 1) Mandatory version symbol — negotiate before trusting the vtable layout.
    let version_fn = match unsafe { lib.get::<unsafe extern "C" fn() -> u32>(BK_VERSION_SYMBOL) } {
        Ok(f) => f,
        Err(_) => {
            return fail(format!(
                "missing symbol bk_strategy_abi_version; a v1/pre-v2 library? rebuild against strategy-api ABI v{BK_ABI_VERSION}"
            ));
        }
    };
    let reported = unsafe { version_fn() };
    if reported != BK_ABI_VERSION {
        return fail(format!(
            "ABI version mismatch: library exports {reported}, kernel requires {BK_ABI_VERSION}; \
             rebuild the strategy against strategy-api ABI v{BK_ABI_VERSION} (no v1 shim)"
        ));
    }

    // 2) JSON deallocator, resolved from THIS library.
    let free_string =
        unsafe { lib.get::<unsafe extern "C" fn(*mut std::ffi::c_char)>(BK_FREE_STRING_SYMBOL) }
            .ok()
            .map(|s| *s)
            .unwrap_or(bk_strategy_free_string as unsafe extern "C" fn(*mut std::ffi::c_char));

    // 2b) OPTIONAL per-strategy gate exemption declaration (E2-b / #27). Absent
    // symbol = nothing declared = fully gated, which is why adding this
    // capability needs no ABI bump (the vtable layout is untouched).
    let gate_exemptions_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkGateExemptionsFn>(BK_GATE_EXEMPTIONS_SYMBOL)
    }
    .ok()
    .map(|s| *s);

    // 2c) OPTIONAL per-strategy evolvable-knob declaration (E2-c / #28). Same rule:
    // absent symbol = declares nothing = NOT evolvable, so no ABI bump and older
    // libraries keep loading unchanged.
    let evolvable_knobs_fn = unsafe {
        lib.get::<blitzkrieg_strategy_api::BkEvolvableKnobsFn>(BK_EVOLVABLE_KNOBS_SYMBOL)
    }
    .ok()
    .map(|s| *s);

    // 3) Factory → vtable. Take the raw fn pointer out of the symbol so the
    // library handle can be moved into the shared wrapper below (the `Symbol`
    // itself borrows the `Library`).
    let create_fn: unsafe extern "C" fn() -> *const BkStrategyVtable = match unsafe {
        lib.get::<unsafe extern "C" fn() -> *const BkStrategyVtable>(BK_CREATE_SYMBOL)
    } {
        Ok(s) => *s,
        Err(e) => return fail(format!("missing symbol bk_strategy_create: {e}")),
    };
    let vt_ptr = unsafe { create_fn() };
    if vt_ptr.is_null() {
        return fail("bk_strategy_create returned null".into());
    }
    // SAFETY: library guarantees a pointer to a static vtable; copy by value.
    let vtable = unsafe { std::ptr::read(vt_ptr) };
    if vtable.abi_version != BK_ABI_VERSION {
        return fail(format!(
            "vtable ABI mismatch: vtable={}, kernel={BK_ABI_VERSION}",
            vtable.abi_version
        ));
    }
    if vtable.min_abi > BK_ABI_VERSION || vtable.abi_version < BK_MIN_ABI_VERSION {
        return fail(format!(
            "ABI range unsupported: library needs >= {}, speaks {}; kernel speaks {BK_ABI_VERSION}",
            vtable.min_abi, vtable.abi_version
        ));
    }

    // 4) Required hooks. Presence only — the copied vtable carries the pointers.
    let missing = |hook: &str| format!("v2 vtable missing required hook: {hook}");
    let required = [
        (vtable.create.is_some(), "create"),
        (vtable.destroy.is_some(), "destroy"),
        (vtable.on_book.is_some(), "on_book"),
        (vtable.on_round.is_some(), "on_round"),
        (vtable.evaluate.is_some(), "evaluate"),
    ];
    let missing_hooks: Vec<String> = required
        .into_iter()
        .filter(|(present, _)| !*present)
        .map(|(_, h)| missing(h))
        .collect();
    if !missing_hooks.is_empty() {
        return fail(missing_hooks.join("; "));
    }

    // 4b) OPTIONAL fresh-book binder (E-parity). Absent = the library keeps the
    // plain v2 contract and prices off its own `on_book` stream only.
    let bind_eval_ctx_fn =
        unsafe { lib.get::<blitzkrieg_strategy_api::BkBindEvalCtxFn>(BK_BIND_EVAL_CTX_SYMBOL) }
            .ok()
            .map(|s| *s);
    // 4c) OPTIONAL effective-config reporter (E-parity). Absent = the strategy
    // declares no config view; observability omits the field.
    let config_view_fn =
        unsafe { lib.get::<blitzkrieg_strategy_api::BkConfigViewFn>(BK_CONFIG_VIEW_SYMBOL) }
            .ok()
            .map(|s| *s);

    let name = unsafe { cstr_to_string(vtable.name) }.unwrap_or_else(|| "unnamed".into());
    let version = unsafe { cstr_to_string(vtable.version) }.unwrap_or_else(|| "0.0.0".into());

    // 5) The library handle is shared: the live instance and every shadow twin it
    // spawns hold the same Arc, so the mapping outlives them all.
    // SAFETY: negotiation above established a v2 library with a static vtable,
    // a matching allocator and the optional symbols resolved from it.
    let shared = unsafe {
        crate::strategies::foreign::LoadedLibrary::new(
            lib,
            create_fn,
            Some(free_string),
            gate_exemptions_fn,
            evolvable_knobs_fn,
            bind_eval_ctx_fn,
            config_view_fn,
        )
    };

    // SAFETY: as above — the shared library is valid, the vtable is static and the
    // instance is driven single-threaded and destroys its handle in Drop.
    let strategy = unsafe {
        crate::strategies::foreign::ForeignStrategy::from_loaded(
            shared,
            name.clone(),
            version.clone(),
        )
    };
    // Read the OPTIONAL declarations once, here, so the load report can state
    // them: an opt-out (E2-b) or "not evolvable" (E2-c) must be visible at
    // registration, not only inferred later.
    let gate_exemptions = crate::strategies::EngineStrategy::gate_exemptions(&strategy);
    let evolvable_knobs = crate::strategies::EngineStrategy::evolvable_knobs(&strategy);
    Ok(LoadedForeign {
        strategy,
        name,
        version,
        gate_exemptions,
        evolvable_knobs,
    })
}

/// Read a NUL-terminated C string into an owned `String` (None for null).
#[cfg(feature = "strategy-loading")]
unsafe fn cstr_to_string(p: *const std::ffi::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

/// Diagnostic summary of loading a library (does not register anywhere).
#[cfg(feature = "strategy-loading")]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    match load_foreign(path) {
        Ok(LoadedForeign { name, version, .. }) => LoadOutcome::Loaded {
            path: path.to_path_buf(),
            name,
            version,
        },
        Err(outcome) => outcome,
    }
}

/// Without the feature, report the library as unsupported (builtins still run).
#[cfg(not(feature = "strategy-loading"))]
pub fn load_strategy(path: &Path) -> LoadOutcome {
    // The trust policy is enforced with or without the loader feature: a build
    // that cannot load libraries must not report "not a library" for a path the
    // production build would refuse outright.
    if let Err(reason) = policy_allows(path).and_then(|()| path_policy(path)) {
        return LoadOutcome::Rejected {
            path: path.to_path_buf(),
            reason,
        };
    }
    LoadOutcome::Failed {
        path: path.to_path_buf(),
        reason: "dynamic strategy loading not compiled in (enable feature `strategy-loading`)"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_accepts_shared_libraries() {
        assert!(policy_allows(Path::new("strategies/dog_strategy.dylib")).is_ok());
        assert!(policy_allows(Path::new("strategies/dog_strategy.so")).is_ok());
    }

    #[test]
    fn policy_rejects_credential_like_and_non_libraries() {
        assert!(policy_allows(Path::new("strategies/private_key.dylib")).is_err());
        assert!(policy_allows(Path::new("strategies/secret.so")).is_err());
        assert!(policy_allows(Path::new("strategies/strategy.toml")).is_err());
    }

    /// A path that passes policy but has no file on disk must report a load
    /// FAILURE (dlopen could not open it), not a panic — and a path the policy
    /// refuses must be reported as REJECTED, before anything is opened.
    #[test]
    fn missing_library_reports_failure_not_panic() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repo root")
            .to_path_buf();
        let approved_missing =
            repo.join("user_layer/strategies/target/release/libdoes_not_exist.dylib");
        let out = load_strategy(&approved_missing);
        assert!(matches!(out, LoadOutcome::Failed { .. }), "got {out:?}");

        // Outside every approved root, the verdict is policy, not dlopen.
        let out = load_strategy(Path::new("/nonexistent/dir/foo.dylib"));
        assert!(matches!(out, LoadOutcome::Rejected { .. }), "got {out:?}");
    }

    // --- #188 trust policy ---------------------------------------------------

    /// FIPS 180-4 / RFC 6234 vectors. A hash that is wrong in one bit breaks
    /// every approval silently, so the implementation is pinned to the standard
    /// rather than to its own output.
    #[test]
    fn sha256_matches_the_standard_vectors() {
        let hex = |s: &str| {
            let mut h = Sha256::new();
            h.update(s.as_bytes());
            h.finish_hex()
        };
        assert_eq!(
            hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Multi-block input crosses the padding/compression boundary, where a
        // hand-rolled implementation is most likely to be wrong.
        let long = "a".repeat(1000);
        assert_eq!(
            hex(&long),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn manifest_parses_both_orders_and_resolves_relative_paths() {
        let text = "\
# approvals\n\
\n\
ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  target/release/libspread_arb_strategy.dylib\n\
target/release/libother.dylib  *248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1\n\
not-a-hash target/release/ignored.dylib\n";
        let manifest = Path::new("/srv/strategies/approved.manifest");
        let entries = parse_manifest(text, manifest);
        assert_eq!(
            entries.len(),
            2,
            "comment, blank and hashless lines skipped"
        );
        assert_eq!(
            entries[0].path,
            PathBuf::from("/srv/strategies/target/release/libspread_arb_strategy.dylib")
        );
        assert_eq!(
            entries[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Reversed order + binary marker still resolves to a path, relative to
        // the manifest's own directory.
        assert_eq!(
            entries[1].path,
            PathBuf::from("/srv/strategies/target/release/libother.dylib")
        );
        assert_eq!(
            entries[1].sha256,
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn machine_generated_paths_are_quarantined() {
        assert!(is_machine_generated(Path::new(
            "/repo/data/shadow_evolution/variant_7.dylib"
        )));
        assert!(is_machine_generated(Path::new(
            "/repo/user_layer/strategies/shadow_variant.dylib"
        )));
        assert!(is_machine_generated(Path::new("/repo/data/foo.dylib")));
        assert!(!is_machine_generated(Path::new(
            "/repo/user_layer/strategies/target/release/libdog_strategy.dylib"
        )));
    }

    /// The acceptance shape of #188: a library under the shared temp directory
    /// or under an artifact tree is refused, an approved root is not.
    #[test]
    fn policy_refuses_temp_and_artifacts_and_allows_the_strategy_tree() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repo root")
            .to_path_buf();
        let policy = TrustPolicy {
            approved_dirs: vec![
                repo.join(APPROVED_ROOT)
                    .canonicalize()
                    .unwrap_or_else(|_| repo.join(APPROVED_ROOT)),
            ],
            untrusted_dirs: untrusted_dirs(),
            manifest: None,
        };

        // Approved: the tree every strategy build lands in.
        let good = repo.join("user_layer/strategies/target/release/libdog_strategy.dylib");
        assert!(
            policy.check(&good).is_ok(),
            "the strategy tree must stay loadable: {:?}",
            policy.check(&good)
        );

        // Refused: /tmp, whatever the file is.
        let tmp = std::env::temp_dir().join("evil.dylib");
        let err = policy.check(&tmp).expect_err("/tmp must be refused");
        assert!(
            err.contains("shared directory") || err.contains("never a strategy root"),
            "rejection must name the reason, got: {err}"
        );

        // Refused: a Shadow Evolution artifact, even inside the approved tree —
        // this is the human-approval gate.
        let artifact = repo.join("user_layer/strategies/shadow_evolution/variant_9.dylib");
        let err = policy
            .check(&artifact)
            .expect_err("an evolution artifact needs explicit approval");
        assert!(
            err.contains("machine-generated") && err.contains("approval manifest"),
            "got: {err}"
        );

        // A path outside every root, with no manifest, is refused too.
        let stray = Path::new("/opt/somewhere/libx.dylib");
        assert!(policy.check(stray).is_err());
    }

    /// Hash-pinned approval: listed + matching digest passes, a byte changed
    /// afterwards does not. This is the acceptance for "哈希不匹配的库被拒绝".
    ///
    /// The probe lives under `$HOME` rather than `$TMPDIR` on purpose: a shared
    /// temp directory is refused *before* the manifest is consulted (see
    /// `policy_refuses_temp_and_artifacts_and_allows_the_strategy_tree`), so it
    /// could never demonstrate the manifest rule.
    #[test]
    fn manifest_approval_requires_a_matching_digest() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return; // no HOME to write a probe into; the rule is still covered below
        };
        let dir = home.join(format!(".bk-manifest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        if std::fs::create_dir_all(&dir).is_err() {
            return; // unwritable HOME (locked-down CI): nothing to assert here
        }
        let lib = dir.join("approved_lib.dylib");
        std::fs::write(&lib, b"first build").expect("write lib");
        let manifest = dir.join("approved.manifest");
        let digest = sha256_file(&lib).expect("hash");
        std::fs::write(&manifest, format!("{digest}  approved_lib.dylib\n"))
            .expect("write manifest");

        let policy = TrustPolicy {
            approved_dirs: vec![], // nothing is approved by location here
            untrusted_dirs: untrusted_dirs(),
            manifest: Some(manifest.clone()),
        };
        assert!(policy.check(&lib).is_ok(), "listed + matching digest loads");

        // The same path, different bytes: the approval does not carry over.
        std::fs::write(&lib, b"second build").expect("rewrite lib");
        let err = policy
            .check(&lib)
            .expect_err("a swapped library must lose its approval");
        assert!(err.contains("sha256 changed"), "got: {err}");

        // And an unlisted path stays refused.
        let other = dir.join("other.dylib");
        std::fs::write(&other, b"x").expect("write other");
        let err = policy.check(&other).expect_err("unlisted path is refused");
        assert!(err.contains("not listed"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A world-writable library is refused wherever it lives: any local user
    /// could replace it between approval and load.
    #[test]
    fn world_writable_libraries_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bk-worldrw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("libworld_writable.dylib");
        std::fs::write(&f, b"x").expect("write");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).expect("chmod");
        let policy = TrustPolicy {
            approved_dirs: vec![dir.canonicalize().unwrap()],
            untrusted_dirs: untrusted_dirs(),
            manifest: None,
        };
        let err = policy
            .check(&f)
            .expect_err("world-writable must be refused");
        assert!(err.contains("world-writable"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
