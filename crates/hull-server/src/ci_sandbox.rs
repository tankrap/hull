//! Sandboxed CI runner (Phase 3 — in-process hardening).
//!
//! [`default_local_ci`](hull_plugin::default_local_ci) is the one place the OSS server executes
//! **attacker-controlled repo code** (a repo's `build.rs`/proc-macro, its `package.json` test
//! script, or an explicit `HULL_CI_CMD`), and today it does so wide open: the full host environment
//! is inherited (leaking `HULL_CI_SECRET`, `GITHUB_*`, AI keys, the `~/.hull/*` stores), there is no
//! memory/CPU/pids/file-size cap, only a wall-clock `child.kill()` that never reaps grandchildren,
//! and no filesystem confinement (a planted `evil -> /etc` symlink is followed at run time).
//!
//! [`SandboxedCiRunner`] closes all four holes without new infrastructure. It reproduces
//! `default_local_ci`'s command selection but spawns the command **through a re-exec of the current
//! binary** as a `ci-sandbox` helper:
//!
//! ```text
//! <hull-server> ci-sandbox --mem-mb N --cpu-secs N --nproc N --fsize-mb N \
//!     --allow-rw <checkout> --allow-rw <scratch> --allow-ro <toolchain-dir>... -- <program> <args>...
//! ```
//!
//! **Why re-exec instead of `pre_exec`.** The server is multi-threaded (tokio). Applying Landlock or
//! touching allocator state in a `pre_exec` closure risks a malloc-after-fork deadlock, and Landlock
//! `restrict_self` is *per-thread* — a multi-threaded process would only confine one thread. The
//! helper is a **fresh process** that runs single-threaded up to the point it `execvp`s the real
//! command (see [`dispatch_if_invoked`], called as the very first line of `main` before any tokio
//! runtime or thread exists), so `setrlimit`/Landlock apply to the whole process safely.
//!
//! Split of responsibilities:
//! - **Server side** ([`SandboxedCiRunner::run`]) does the fork-safe `Command` setup: `current_dir`,
//!   `env_clear` + a curated allow-list env, `process_group(0)` (helper + its exec'd grandchildren
//!   share a fresh pgid), and the poll-loop timeout — replacing `child.kill()` with
//!   `kill(-pgid, SIGKILL)` so the whole group is reaped.
//! - **Helper side** ([`dispatch_if_invoked`] → `run_helper`) drops the `setrlimit` caps, builds and
//!   applies the Landlock ruleset (Linux only, best-effort so old kernels degrade to a warning), then
//!   `execvp`s the real command (which inherits the rlimits + Landlock domain + pgid + curated env).
//!
//! Gated on `HULL_CI_SANDBOX` (default **on** on unix; `off` falls back to the raw `default_local_ci`
//! for debugging). Network is intentionally left open (cargo/npm need to fetch deps) — a documented
//! deferred increment. See `docs/deploy.md`.

use hull_plugin::{CiOutcome, CiRequest, CiRunner, CiStatus};

/// Sandboxing runner installed by default on unix. Reproduces `default_local_ci`'s command selection
/// but jails the spawn (scrubbed env, process group, resource limits, and — on Linux — a Landlock
/// filesystem sandbox). Falls back to the raw runner when `HULL_CI_SANDBOX=off` or off-unix.
#[derive(Default)]
pub struct SandboxedCiRunner;

impl SandboxedCiRunner {
    pub fn new() -> Self {
        SandboxedCiRunner
    }
}

impl CiRunner for SandboxedCiRunner {
    fn run(&self, req: &CiRequest) -> CiOutcome {
        // Explicit off-switch, and the raw fallback on non-unix (no `setrlimit`/pgroup/Landlock).
        if std::env::var("HULL_CI_SANDBOX").as_deref() == Ok("off") {
            return hull_plugin::default_local_ci(req);
        }
        #[cfg(unix)]
        {
            unix::run_sandboxed(req)
        }
        #[cfg(not(unix))]
        {
            eprintln!(
                "hull-ci-sandbox: unsupported on this platform — running CI UNSANDBOXED (set HULL_CI_SANDBOX=off to silence)"
            );
            hull_plugin::default_local_ci(req)
        }
    }
}

/// Command selection, identical to [`hull_plugin::default_local_ci`]: `HULL_CI_CMD` → `sh -c`; else
/// `Cargo.toml` → `cargo test --quiet`; else `package.json` → `npm test --silent`; else none.
fn select_command(dir: &std::path::Path) -> Option<(String, Vec<String>)> {
    if let Ok(cmd) = std::env::var("HULL_CI_CMD") {
        Some(("sh".into(), vec!["-c".into(), cmd]))
    } else if dir.join("Cargo.toml").is_file() {
        Some(("cargo".into(), vec!["test".into(), "--quiet".into()]))
    } else if dir.join("package.json").is_file() {
        Some(("npm".into(), vec!["test".into(), "--silent".into()]))
    } else {
        None
    }
}

fn errored(summary: impl Into<String>) -> CiOutcome {
    CiOutcome { status: CiStatus::Errored, summary: summary.into(), memoized: false }
}

/// Read a `u64` limit from `key`, falling back to `default` on unset/garbage.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

// ── Server side (fork-safe Command setup + timeout group-kill) ────────────────────────────────────

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Default caps (MiB / seconds). Builds are hungry, so memory is generous; CPU is aligned to the
    /// wall-clock timeout. All overridable via `HULL_CI_*`.
    const DEFAULT_MEM_MB: u64 = 4096;
    const DEFAULT_NPROC: u64 = 512;
    const DEFAULT_FSIZE_MB: u64 = 2048;
    const DEFAULT_TIMEOUT_SECS: u64 = 600;

    pub fn run_sandboxed(req: &CiRequest) -> CiOutcome {
        let dir = &req.workdir;
        let Some((program, args)) = select_command(dir) else {
            return errored("no test command detected (no Cargo.toml/package.json, no HULL_CI_CMD)");
        };

        let helper = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return errored(format!("could not resolve sandbox helper (current_exe): {e}")),
        };

        // Per-run scratch dir — the child's HOME / CARGO_HOME / npm cache / TMPDIR live here (NOT
        // `/var/lib/hull`, so a malicious job can't reach the server's `~/.hull/*` stores). Sibling of
        // the checkout under the temp dir; removed after the run.
        let scratch = scratch_dir(req);
        let cargo_home = scratch.join("cargo");
        let npm_cache = scratch.join("npm");
        let tmp = scratch.join("tmp");
        for d in [&scratch, &cargo_home, &npm_cache, &tmp] {
            if let Err(e) = std::fs::create_dir_all(d) {
                return errored(format!("could not create sandbox scratch dir {}: {e}", d.display()));
            }
        }

        // Toolchain roots the child needs to READ+EXECUTE (Landlock allow-ro): the dir holding the
        // resolved program, plus the rustup/cargo install roots (so `cargo`/`rustc` can read std).
        // The fixed system dirs (`/usr`, `/lib`, `/etc`, …) are added by the helper itself.
        let allow_ro = toolchain_dirs(&program);

        let mem_mb = env_u64("HULL_CI_MEM_MB", DEFAULT_MEM_MB);
        let timeout_secs = env_u64("HULL_CI_TIMEOUT", DEFAULT_TIMEOUT_SECS);
        // CPU seconds default to the wall-clock timeout so a busy-loop is caught by the cheaper cap.
        let cpu_secs = env_u64("HULL_CI_CPU_SECS", timeout_secs);
        let nproc = env_u64("HULL_CI_NPROC", DEFAULT_NPROC);
        let fsize_mb = env_u64("HULL_CI_FSIZE_MB", DEFAULT_FSIZE_MB);

        let mut cmd = Command::new(&helper);
        cmd.arg("ci-sandbox")
            .arg("--mem-mb")
            .arg(mem_mb.to_string())
            .arg("--cpu-secs")
            .arg(cpu_secs.to_string())
            .arg("--nproc")
            .arg(nproc.to_string())
            .arg("--fsize-mb")
            .arg(fsize_mb.to_string())
            .arg("--allow-rw")
            .arg(dir)
            .arg("--allow-rw")
            .arg(&scratch);
        for ro in &allow_ro {
            cmd.arg("--allow-ro").arg(ro);
        }
        cmd.arg("--").arg(&program);
        for a in &args {
            cmd.arg(a);
        }

        // ── Fork-safe setup (applied at exec by the Command machinery, safe from a threaded parent).
        cmd.current_dir(dir);
        // Scrub the environment: NO `HULL_*`, `GITHUB_*`, `*_API_KEY`, `HULL_CI_SECRET`, etc. reach the
        // child. Only a curated allow-list is re-added below.
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path); // locate cargo/npm/sh/node/rustc
        }
        cmd.env("HOME", &scratch);
        cmd.env("CARGO_HOME", &cargo_home);
        cmd.env("npm_config_cache", &npm_cache);
        // Keep temp writes inside the jail (Landlock only allows the checkout + scratch; a build tool
        // writing to the default `/tmp` would otherwise be denied — or, worse, read a sibling tenant's
        // checkout under `/tmp`).
        cmd.env("TMPDIR", &tmp);
        cmd.env("TERM", "dumb");
        for pass in ["LANG", "LC_ALL"] {
            if let Some(v) = std::env::var_os(pass) {
                cmd.env(pass, v);
            }
        }
        // rustup resolves its toolchains via RUSTUP_HOME (defaulting to `$HOME/.rustup`). Because HOME
        // is scrubbed to the empty scratch dir, a rustup-based `cargo`/`rustc` would fail to find its
        // toolchain unless RUSTUP_HOME is pinned to the real install. Not a secret; needed for builds.
        if let Some(ru) = rustup_home() {
            cmd.env("RUSTUP_HOME", ru);
        }

        // A fresh process group so the timeout can reap the whole tree (helper + exec'd grandchildren
        // share this pgid unless one deliberately calls setpgid).
        cmd.process_group(0);
        // Don't hand the child the server's stdin; let its stdout/stderr flow to the logs.
        cmd.stdin(Stdio::null());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return errored(format!("could not spawn sandbox helper: {e}"));
            }
        };
        // With `process_group(0)` the child is the group leader, so pgid == child pid.
        let pgid = child.id() as i32;

        let start = Instant::now();
        let outcome = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status_to_outcome(status, &program, &args),
                Ok(None) => {
                    if start.elapsed().as_secs() >= timeout_secs {
                        // Kill the whole group, not just the leader — reaps backgrounded grandchildren
                        // the old `child.kill()` left orphaned.
                        kill_group(pgid);
                        let _ = child.wait();
                        break errored(format!("timed out after {timeout_secs}s"));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => break errored(format!("wait failed: {e}")),
            }
        };

        // Reap any stragglers the command backgrounded even on a clean exit, then drop the scratch.
        kill_group(pgid);
        let _ = std::fs::remove_dir_all(&scratch);
        outcome
    }

    /// Map an exit status to a verdict exactly like `default_local_ci`: 0 → green, else red.
    fn status_to_outcome(status: std::process::ExitStatus, program: &str, args: &[String]) -> CiOutcome {
        let cmd = format!("{program} {}", args.join(" "));
        if status.success() {
            CiOutcome { status: CiStatus::Green, summary: format!("`{cmd}` passed"), memoized: false }
        } else {
            CiOutcome { status: CiStatus::Red, summary: format!("`{cmd}` failed ({status})"), memoized: false }
        }
    }

    /// SIGKILL an entire process group. `kill(-pgid, …)` targets the group; `ESRCH` (already gone) is
    /// ignored.
    fn kill_group(pgid: i32) {
        if pgid > 1 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }

    fn scratch_dir(req: &CiRequest) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tag = &req.tree_id[..req.tree_id.len().min(16)];
        std::env::temp_dir().join(format!("hull-ci-scratch-{tag}-{}-{seq}", std::process::id()))
    }

    /// The directories the child must READ+EXECUTE to run the toolchain — kept deliberately generous
    /// (better to over-allow the toolchain than break a build); the *sensitive* targets are denied by
    /// omission (`~/.hull`, other tenants' checkouts, `/` at large). The fixed system set (`/usr`,
    /// `/lib`, `/etc`, …) is added helper-side, so this returns only the dynamic, install-specific
    /// roots: the resolved program's directory and the rustup/cargo roots.
    fn toolchain_dirs(program: &str) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(resolved) = resolve_program(program) {
            // Canonicalize so a rustup/symlinked shim contributes its real directory too.
            let real = std::fs::canonicalize(&resolved).unwrap_or(resolved.clone());
            for p in [resolved.parent(), real.parent()].into_iter().flatten() {
                dirs.push(p.to_path_buf());
            }
        }
        if let Some(ru) = rustup_home() {
            dirs.push(ru);
        }
        if let Some(ch) = real_cargo_home() {
            dirs.push(ch);
        }
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Locate `program` on the server's `PATH` (a tiny `which`), so its install dir can be allow-listed.
    fn resolve_program(program: &str) -> Option<PathBuf> {
        let p = Path::new(program);
        if p.is_absolute() {
            return p.is_file().then(|| p.to_path_buf());
        }
        if program.contains('/') {
            return std::fs::canonicalize(program).ok();
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|d| d.join(program)).find(|c| is_executable(c))
    }

    fn is_executable(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }

    /// The rustup home to grant read (and pin for the child): `RUSTUP_HOME`, else `$HOME/.rustup`.
    fn rustup_home() -> Option<PathBuf> {
        if let Some(v) = std::env::var_os("RUSTUP_HOME") {
            let p = PathBuf::from(v);
            if p.is_dir() {
                return Some(p);
            }
        }
        let home = std::env::var_os("HOME")?;
        let p = PathBuf::from(home).join(".rustup");
        p.is_dir().then_some(p)
    }

    /// The server's *real* cargo home (for read access to installed subcommands): `CARGO_HOME`, else
    /// `$HOME/.cargo`. Distinct from the child's scrubbed `CARGO_HOME` (the scratch dir).
    fn real_cargo_home() -> Option<PathBuf> {
        if let Some(v) = std::env::var_os("CARGO_HOME") {
            let p = PathBuf::from(v);
            if p.is_dir() {
                return Some(p);
            }
        }
        let home = std::env::var_os("HOME")?;
        let p = PathBuf::from(home).join(".cargo");
        p.is_dir().then_some(p)
    }
}

// ── Helper side (single-threaded post-exec: setrlimit + Landlock + execvp) ────────────────────────

/// Intercept the `ci-sandbox` helper subcommand. **Must be the first line of `main`**, before any
/// tokio runtime or thread is spawned, so the jail is applied single-threaded. Returns immediately if
/// this process was not invoked as the helper; otherwise it applies the jail and `execvp`s the real
/// command — **never returning** (it `exit`s on any failure).
pub fn dispatch_if_invoked() {
    #[cfg(unix)]
    {
        let args = unix_raw_args();
        if args.get(1).map(String::as_str) != Some("ci-sandbox") {
            return;
        }
        // argv after `ci-sandbox`.
        let helper_args: Vec<String> = args.into_iter().skip(2).collect();
        let code = helper::run_helper(helper_args);
        std::process::exit(code);
    }
    #[cfg(not(unix))]
    {
        if std::env::args_os().nth(1).and_then(|a| a.into_string().ok()).as_deref() == Some("ci-sandbox") {
            eprintln!("ci-sandbox: unsupported on this platform");
            std::process::exit(1);
        }
    }
}

/// Read this process's argv robustly. `dispatch_if_invoked` runs at the very start of `main` — but the
/// sandbox tests also drive it from a pre-`main` constructor (to intercept the `ci-sandbox` re-exec of
/// the *test* binary before libtest's harness swallows the argv). `std::env::args()` is unreliable that
/// early on Linux (the stdlib may not have captured argv yet), so read `/proc/self/cmdline` there;
/// elsewhere (and on the normal `main` path) `args_os` is fine.
#[cfg(unix)]
fn unix_raw_args() -> Vec<String> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Ok(raw) = std::fs::read("/proc/self/cmdline") {
            let mut parts: Vec<String> =
                raw.split(|b| *b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
            // `/proc/self/cmdline` ends in a trailing NUL → drop the trailing empty element only
            // (interior args are preserved verbatim).
            if matches!(parts.last(), Some(s) if s.is_empty()) {
                parts.pop();
            }
            if !parts.is_empty() {
                return parts;
            }
        }
    }
    std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect()
}

#[cfg(unix)]
mod helper {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    #[derive(Default)]
    struct Parsed {
        mem_mb: u64,
        cpu_secs: u64,
        nproc: u64,
        fsize_mb: u64,
        allow_rw: Vec<PathBuf>,
        allow_ro: Vec<PathBuf>,
        program: String,
        prog_args: Vec<String>,
    }

    fn parse(args: Vec<String>) -> Result<Parsed, String> {
        let mut p = Parsed::default();
        let mut it = args.into_iter();
        while let Some(tok) = it.next() {
            match tok.as_str() {
                "--mem-mb" => p.mem_mb = next_u64(&mut it, "--mem-mb")?,
                "--cpu-secs" => p.cpu_secs = next_u64(&mut it, "--cpu-secs")?,
                "--nproc" => p.nproc = next_u64(&mut it, "--nproc")?,
                "--fsize-mb" => p.fsize_mb = next_u64(&mut it, "--fsize-mb")?,
                "--allow-rw" => p.allow_rw.push(next_val(&mut it, "--allow-rw")?.into()),
                "--allow-ro" => p.allow_ro.push(next_val(&mut it, "--allow-ro")?.into()),
                "--" => {
                    p.program = it.next().ok_or("missing program after --")?;
                    p.prog_args = it.collect();
                    return Ok(p);
                }
                other => return Err(format!("unexpected argument: {other}")),
            }
        }
        Err("missing `--` program separator".into())
    }

    fn next_val(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
        it.next().ok_or_else(|| format!("missing value for {flag}"))
    }
    fn next_u64(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
        next_val(it, flag)?.parse().map_err(|_| format!("invalid number for {flag}"))
    }

    /// Apply the jail and `execvp`. Returns a nonzero exit code only on failure (on success it never
    /// returns — the process image is replaced).
    pub fn run_helper(args: Vec<String>) -> i32 {
        let parsed = match parse(args) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("ci-sandbox: {e}");
                return 2;
            }
        };

        set_rlimits(&parsed);

        #[cfg(target_os = "linux")]
        apply_landlock(&parsed.allow_rw, &parsed.allow_ro);

        // Replace the process image. `exec()` runs execvp (PATH search) with the curated env, cwd,
        // process group, rlimits, and Landlock domain this process already holds — all inherited
        // across execve.
        let err = Command::new(&parsed.program).args(&parsed.prog_args).exec();
        eprintln!("ci-sandbox: exec `{}` failed: {err}", parsed.program);
        127
    }

    /// Drop the four resource caps in-process via `setrlimit`. Best-effort: a failing cap is logged,
    /// not fatal (we still want the other caps + Landlock). The macro expands the resource *constant*
    /// inline so its platform-specific type (Linux `__rlimit_resource_t` vs macOS `c_int`) is inferred.
    fn set_rlimits(p: &Parsed) {
        macro_rules! set {
            ($res:expr, $val:expr, $name:literal) => {{
                let v = $val as libc::rlim_t;
                let lim = libc::rlimit { rlim_cur: v, rlim_max: v };
                if unsafe { libc::setrlimit($res, &lim) } != 0 {
                    eprintln!("ci-sandbox: setrlimit({}) failed: {}", $name, std::io::Error::last_os_error());
                }
            }};
        }
        // Address space (virtual memory). RLIMIT_AS is reliably enforced on Linux; macOS ignores it
        // (the pgroup + wall-clock timeout remain the backstop there).
        set!(libc::RLIMIT_AS, p.mem_mb.saturating_mul(1024 * 1024), "RLIMIT_AS");
        // CPU seconds — SIGXCPU then SIGKILL when exceeded.
        set!(libc::RLIMIT_CPU, p.cpu_secs, "RLIMIT_CPU");
        // Max processes for the runtime uid — caps fork bombs. Note: this counts ALL of the uid's
        // processes host-wide, so a shared-uid host with many server processes may need it raised.
        set!(libc::RLIMIT_NPROC, p.nproc, "RLIMIT_NPROC");
        // Max file size a single write may create — SIGXFSZ when exceeded (caps disk-fill).
        set!(libc::RLIMIT_FSIZE, p.fsize_mb.saturating_mul(1024 * 1024), "RLIMIT_FSIZE");
    }

    /// Build + apply the Landlock filesystem ruleset (Linux only). Grants read+write beneath the
    /// checkout + scratch, read+execute beneath the toolchain + a fixed system set, read+write beneath
    /// `/dev` (for `/dev/null` etc.), and denies **everything else** — the `~/.hull/*` stores, other
    /// tenants' checkouts, and the rest of `/`.
    ///
    /// Best-effort compatibility: on kernels <5.13 or with the Landlock LSM disabled, the ruleset is a
    /// logged no-op (`NotEnforced`) rather than a hard error — the env-scrub, pgroup, and rlimits still
    /// apply. Only existing paths are added (a missing dir would otherwise fail the rule).
    #[cfg(target_os = "linux")]
    fn apply_landlock(allow_rw: &[PathBuf], allow_ro: &[PathBuf]) {
        use landlock::{
            path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, ABI,
        };

        // ABI V1 is the original Landlock (kernel 5.13). Handling only V1 accesses maximizes
        // compatibility while still governing every read/write/execute path — exactly what confines
        // the checkout. Higher ABIs (Refer/Truncate/IoctlDev) are not needed here.
        let abi = ABI::V1;

        // Fixed system roots the toolchain reads/executes: libraries, interpreters, TLS certs
        // (`/etc/ssl`), DNS config (`/etc/resolv.conf`), and the cpu-topology pseudo-fs. `/dev` is
        // read+write for `/dev/null` `/dev/urandom` `/dev/zero` `/dev/tty`.
        let sys_ro = ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/proc", "/sys"];
        let sys_rw = ["/dev"];

        let rw: Vec<PathBuf> = allow_rw
            .iter()
            .cloned()
            .chain(sys_rw.iter().map(PathBuf::from))
            .filter(|p| p.exists())
            .collect();
        let ro: Vec<PathBuf> = allow_ro
            .iter()
            .cloned()
            .chain(sys_ro.iter().map(PathBuf::from))
            .filter(|p| p.exists())
            .collect();

        let build = || {
            Ruleset::default()
                .set_compatibility(CompatLevel::BestEffort)
                .handle_access(AccessFs::from_all(abi))?
                .create()?
                .add_rules(path_beneath_rules(&rw, AccessFs::from_all(abi)))?
                .add_rules(path_beneath_rules(&ro, AccessFs::from_read(abi)))?
                .restrict_self()
        };

        match build() {
            Ok(status) => match status.ruleset {
                RulesetStatus::FullyEnforced => {}
                RulesetStatus::PartiallyEnforced => {
                    eprintln!("ci-sandbox: Landlock partially enforced (kernel supports only an older ABI)");
                }
                RulesetStatus::NotEnforced => {
                    eprintln!(
                        "ci-sandbox: WARNING Landlock NOT enforced (kernel <5.13 or Landlock LSM disabled) — \
                         filesystem confinement is OFF; env-scrub + process-group + rlimits still apply"
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "ci-sandbox: WARNING Landlock setup failed ({e}) — continuing without filesystem confinement"
                );
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────────────────────────
//
// The sandbox tests re-exec the *test* binary as the `ci-sandbox` helper (`current_exe`). A
// constructor lets that helper dispatch before libtest's `main` would otherwise treat `ci-sandbox` as
// a test-name filter. They prefer `HULL_CI_CMD` with `sh`, so they need no cargo/npm toolchain.
#[cfg(all(test, unix))]
#[ctor::ctor]
fn ci_sandbox_test_ctor() {
    // Runs at test-binary startup. Returns immediately for a normal test run (argv[1] != "ci-sandbox");
    // for the re-exec it applies the jail and execvp's, never returning to libtest.
    dispatch_if_invoked();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    // `HULL_CI_*` config is read from the process environment, which is global. Serialize the tests
    // that mutate it so parallel runs don't clobber each other's config.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn workdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hull-ci-sbx-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req_for(dir: &Path) -> CiRequest {
        CiRequest {
            repo: "t/r".into(),
            change: "c".into(),
            // The scratch dir name slices this; a hex-ish id keeps it filesystem-safe.
            tree_id: "deadbeefcafef00d1234".into(),
            workdir: dir.to_path_buf(),
        }
    }

    /// Run a command through the sandbox with `HULL_CI_CMD` set to `cmd` plus any extra env, holding
    /// the env lock only for the duration of the spawn, then scrubbing the vars back out.
    fn run(dir: &Path, cmd: &str, extra: &[(&str, &str)]) -> CiOutcome {
        let _g = ENV_LOCK.lock().unwrap();
        // Default-on: make sure a stray HULL_CI_SANDBOX=off from another test doesn't leak in.
        std::env::remove_var("HULL_CI_SANDBOX");
        std::env::set_var("HULL_CI_CMD", cmd);
        for (k, v) in extra {
            std::env::set_var(k, v);
        }
        let outcome = SandboxedCiRunner::new().run(&req_for(dir));
        std::env::remove_var("HULL_CI_CMD");
        for (k, _) in extra {
            std::env::remove_var(k);
        }
        outcome
    }

    fn pid_alive(pid: i32) -> bool {
        // kill(pid, 0): 0 ⇒ signalable (alive); -1/ESRCH ⇒ gone.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// A normal command still runs green through the default-on sandbox (env/pgroup/rlimit path — and,
    /// on Linux, Landlock). Guards against the sandbox breaking a legitimate build. A red is red too.
    #[test]
    fn sandbox_runs_normal_commands() {
        let dir = workdir("normal");
        assert!(matches!(run(&dir, "exit 0", &[]).status, CiStatus::Green), "exit 0 ⇒ green");
        assert!(matches!(run(&dir, "exit 3", &[]).status, CiStatus::Red), "exit 3 ⇒ red");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The child must NOT see the server's secrets: `env_clear` + a curated allow-list. A fake secret
    /// set in THIS process is absent from the child's environment; the curated `TERM=dumb`/`HOME` are
    /// present (proving the command actually ran with the scrubbed env, not that it simply failed).
    #[test]
    fn env_is_scrubbed_of_secrets() {
        let dir = workdir("envscrub");
        std::env::set_var("HULL_CI_SECRET", "leak-me-hull-secret");
        std::env::set_var("MY_FAKE_API_KEY", "leak-me-api-key");
        let outcome = run(&dir, "env > env_dump.txt", &[]);
        std::env::remove_var("HULL_CI_SECRET");
        std::env::remove_var("MY_FAKE_API_KEY");
        assert!(matches!(outcome.status, CiStatus::Green), "env dump ran: {}", outcome.summary);

        let dump = std::fs::read_to_string(dir.join("env_dump.txt")).expect("env dump written");
        assert!(!dump.contains("leak-me-hull-secret"), "HULL_CI_SECRET leaked into child env:\n{dump}");
        assert!(!dump.contains("leak-me-api-key"), "API key leaked into child env:\n{dump}");
        assert!(!dump.contains("HULL_CI_SECRET"), "HULL_* var name leaked:\n{dump}");
        // Curated env applied.
        assert!(dump.contains("TERM=dumb"), "curated TERM missing:\n{dump}");
        assert!(dump.contains("HOME="), "curated HOME missing:\n{dump}");
        // HOME points at the per-run scratch, NOT the server's real home / /var/lib/hull.
        assert!(dump.contains("hull-ci-scratch-"), "HOME should be the per-run scratch:\n{dump}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backgrounded grandchild is reaped by the process-group kill on timeout — the old
    /// `child.kill()` left it orphaned. The inner shell writes its pid (which `exec sleep` inherits)
    /// then sleeps far longer than the short timeout.
    #[test]
    fn timeout_kills_the_whole_process_group() {
        let dir = workdir("pgroup");
        let cmd = "sh -c 'echo $$ > gc.pid; exec sleep 300' & sleep 300";
        let outcome = run(&dir, cmd, &[("HULL_CI_TIMEOUT", "2")]);
        assert!(matches!(outcome.status, CiStatus::Errored), "should time out: {}", outcome.summary);
        assert!(outcome.summary.contains("timed out"), "summary: {}", outcome.summary);

        let pid: i32 = std::fs::read_to_string(dir.join("gc.pid"))
            .expect("grandchild wrote its pid")
            .trim()
            .parse()
            .expect("pid parses");
        assert!(pid > 1, "sane pid");
        // Give the group-kill a moment to take effect, then the grandchild must be dead.
        for _ in 0..50 {
            if !pid_alive(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(!pid_alive(pid), "grandchild pid {pid} survived the group kill");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file-size bomb hits `RLIMIT_FSIZE` (SIGXFSZ) and dies fast — NOT the wall-clock timeout.
    /// FSIZE is reliably enforced on both macOS and Linux (unlike RLIMIT_AS on macOS), so this is the
    /// portable rlimit test.
    #[test]
    fn rlimit_fsize_caps_file_writes() {
        let dir = workdir("fsize");
        // Try to write 40 MiB with a 1 MiB file-size cap → SIGXFSZ → nonzero → red, well under timeout.
        let cmd = "dd if=/dev/zero of=big.bin bs=1048576 count=40 2>/dev/null";
        let start = std::time::Instant::now();
        let outcome = run(&dir, cmd, &[("HULL_CI_FSIZE_MB", "1"), ("HULL_CI_TIMEOUT", "60")]);
        assert!(matches!(outcome.status, CiStatus::Red), "fsize cap should fail the write: {}", outcome.summary);
        assert!(start.elapsed().as_secs() < 30, "should die on the cap, not the wall-clock");
        if let Ok(meta) = std::fs::metadata(dir.join("big.bin")) {
            assert!(meta.len() <= 2 * 1024 * 1024, "file grew past the fsize cap: {} bytes", meta.len());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A memory bomb hits `RLIMIT_AS` and dies rather than running to the wall-clock timeout. Linux
    /// only: macOS does not reliably enforce `RLIMIT_AS` (see `rlimit_fsize_caps_file_writes` for the
    /// portable cap test).
    #[cfg(target_os = "linux")]
    #[test]
    fn rlimit_as_caps_memory() {
        let dir = workdir("membomb");
        // `dd` allocates a single block buffer of `bs`; a 1 GiB buffer under a 256 MiB AS cap fails to
        // allocate → nonzero exit, fast (not the wall-clock).
        let cmd = "dd if=/dev/zero of=/dev/null bs=1073741824 count=1 2>/dev/null";
        let start = std::time::Instant::now();
        let outcome = run(&dir, cmd, &[("HULL_CI_MEM_MB", "256"), ("HULL_CI_TIMEOUT", "60")]);
        assert!(matches!(outcome.status, CiStatus::Red), "memory cap should fail: {}", outcome.summary);
        assert!(start.elapsed().as_secs() < 30, "should die on the cap, not the wall-clock");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Landlock denies reads OUTSIDE the checkout (a sibling secret dir, standing in for `~/.hull`),
    /// while reads INSIDE the checkout succeed. Linux only, and skipped if the running kernel lacks the
    /// Landlock LSM (so enforcement would be a no-op — e.g. some CI kernels). GitHub's ubuntu runners
    /// have it.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_denies_outside_the_checkout() {
        // Skip gracefully where Landlock can't be enforced.
        let lsm = std::fs::read_to_string("/sys/kernel/security/lsm").unwrap_or_default();
        if !lsm.split(',').any(|m| m == "landlock") {
            eprintln!("skipping landlock test: kernel LSM list = {lsm:?} (no landlock)");
            return;
        }

        let dir = workdir("landlock");
        // A secret file OUTSIDE the checkout + scratch (a sibling under the temp dir — NOT allow-listed).
        let secret_dir = std::env::temp_dir().join(format!("hull-ci-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&secret_dir);
        std::fs::create_dir_all(&secret_dir).unwrap();
        let secret = secret_dir.join("secret.txt");
        std::fs::write(&secret, "TOPSECRET-HULL").unwrap();
        // A file INSIDE the checkout.
        std::fs::write(dir.join("inside.txt"), "ok-inside").unwrap();

        let outside = run(&dir, &format!("cat {}", secret.display()), &[]);
        assert!(
            !matches!(outside.status, CiStatus::Green),
            "Landlock must DENY reading a file outside the checkout, got: {}",
            outside.summary
        );

        let inside = run(&dir, "cat inside.txt", &[]);
        assert!(
            matches!(inside.status, CiStatus::Green),
            "reading a file inside the checkout must succeed: {}",
            inside.summary
        );

        let _ = std::fs::remove_dir_all(&secret_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
