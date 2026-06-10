//! HTTP route handlers for all Bullarchy commands.

use axum::{Json, response::IntoResponse};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Response type ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct CommandResult {
    pub ok:     bool,
    pub output: String,
}

fn ok(output: String) -> Json<CommandResult> {
    Json(CommandResult { ok: true, output })
}

// ── Output capture ────────────────────────────────────────────────────────────
//
// Redirects fd 1 & 2 to a pipe for the duration of the closure, then reads
// everything back.  Must be called from a blocking thread (spawn_blocking).

fn capture<F: FnOnce() + Send + 'static>(f: F) -> String {
    let (reader, writer) = os_pipe::pipe().unwrap();
    let writer2 = writer.try_clone().unwrap();

    let old_stdout = redirect_fd(1, &writer);
    let old_stderr = redirect_fd(2, &writer2);
    drop(writer);
    drop(writer2);

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok();

    restore_fd(1, old_stdout);
    restore_fd(2, old_stderr);

    let mut buf = String::new();
    use std::io::Read;
    let mut r = reader;
    let _ = r.read_to_string(&mut buf);
    buf
}

#[cfg(unix)]
fn redirect_fd(fd: i32, writer: &os_pipe::PipeWriter) -> i32 {
    use std::os::unix::io::AsRawFd;
    unsafe {
        let old = libc::dup(fd);
        libc::dup2(writer.as_raw_fd(), fd);
        old
    }
}

#[cfg(unix)]
fn restore_fd(fd: i32, old: i32) {
    unsafe { libc::dup2(old, fd); libc::close(old); }
}

#[cfg(windows)]
fn redirect_fd(_fd: i32, _writer: &os_pipe::PipeWriter) -> i32 { -1 }
#[cfg(windows)]
fn restore_fd(_fd: i32, _old: i32) {}

// ── /api/init ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct InitRequest {
    pub name:      String,
    pub depth:     Option<u8>,
    pub lang:      Option<String>,
    pub libs:      Option<Vec<String>>,
    pub blueprint: Option<String>,
    pub path:      Option<String>,
}

pub async fn handle_init(Json(req): Json<InitRequest>) -> impl IntoResponse {
    let name      = req.name.clone();
    let depth     = req.depth.unwrap_or(2);
    let lang      = req.lang.clone();
    let libs      = req.libs.unwrap_or_default();
    let blueprint = req.blueprint.map(PathBuf::from);
    let path      = req.path.map(PathBuf::from);

    let output = tokio::task::spawn_blocking(move || {
        capture(move || { crate::cmd::cmd_init(name, depth, blueprint, lang, libs, path); })
    }).await.unwrap_or_default();

    ok(output)
}

// ── /api/convert ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ConvertRequest {
    pub target: Option<String>,
    pub second: Option<String>,
}

pub async fn handle_convert(Json(req): Json<ConvertRequest>) -> impl IntoResponse {
    let target = req.target.map(PathBuf::from);
    let second = req.second.clone();

    let output = tokio::task::spawn_blocking(move || {
        capture(move || { crate::cmd::cmd_convert(target, second); })
    }).await.unwrap_or_default();

    ok(output)
}

// ── /api/fmt ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct FmtRequest {
    pub folder:  Option<String>,
    pub dry_run: Option<bool>,
}

pub async fn handle_fmt(Json(req): Json<FmtRequest>) -> impl IntoResponse {
    let folder  = req.folder.map(PathBuf::from);
    let dry_run = req.dry_run.unwrap_or(false);

    let output = tokio::task::spawn_blocking(move || {
        capture(move || { crate::cmd::cmd_fmt(folder, dry_run); })
    }).await.unwrap_or_default();

    ok(output)
}

// ── /api/check ────────────────────────────────────────────────────────────────

pub async fn handle_check() -> impl IntoResponse {
    let output = tokio::task::spawn_blocking(|| {
        capture(|| { crate::cmd::cmd_check(); })
    }).await.unwrap_or_default();

    ok(output)
}

// ── /api/editor-setup ─────────────────────────────────────────────────────────

pub async fn handle_editor_setup() -> impl IntoResponse {
    let output = tokio::task::spawn_blocking(|| {
        capture(|| { crate::cmd::cmd_editor_setup(); })
    }).await.unwrap_or_default();

    ok(output)
}

// ── /api/update ───────────────────────────────────────────────────────────────
//
// `cargo install` is a long-running subprocess — we cannot capture it via
// the fd-redirect trick because the pipe would fill and block before the
// process finishes.  Instead we run it with stdout/stderr piped directly
// and collect the output after it exits.

pub async fn handle_update() -> impl IntoResponse {
    let repo = crate::cmd::cmd_update::DEFAULT_REPO;

    let output = tokio::task::spawn_blocking(move || {
        // 1. Check remote hash
        let remote = match remote_head(repo, "main") {
            Some(h) => h,
            None => return "Could not reach repository. Check your internet connection.".to_string(),
        };

        // 2. Check installed hash
        let installed = installed_hash("bullarchy-gui", repo, "main");
        if installed.map_or(false, |h| remote.starts_with(&h)) {
            return format!("Already up to date (commit: {}).", &remote[..8]);
        }

        // 3. Run cargo install, collecting combined output
        let result = std::process::Command::new("cargo")
            .args(["install", "--git", repo, "--branch", "main", "--force", "bullarchy-gui"])
            .output();

        match result {
            Ok(out) => {
                let mut buf = String::new();
                buf.push_str(&String::from_utf8_lossy(&out.stdout));
                buf.push_str(&String::from_utf8_lossy(&out.stderr));
                if out.status.success() {
                    buf.push_str("\nUpdate complete.");
                } else {
                    buf.push_str(&format!("\ncargo install exited with {}.", out.status));
                }
                buf
            }
            Err(e) => format!("Failed to run cargo: {}.", e),
        }
    }).await.unwrap_or_else(|_| "Internal error running update.".to_string());

    ok(output)
}

fn remote_head(repo: &str, branch: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["ls-remote", repo, &format!("refs/heads/{}", branch)])
        .output().ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    let hash = stdout.split_whitespace().next()?;
    if hash.len() == 40 { Some(hash.to_string()) } else { None }
}

fn installed_hash(package: &str, repo: &str, branch: &str) -> Option<String> {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cargo")
        });

    let crates2 = std::fs::read_to_string(cargo_home.join(".crates2.json")).ok()?;
    let repo_fragment = repo.trim_end_matches(".git");
    let branch_tag = format!("branch={}", branch);

    for line in crates2.lines() {
        if line.contains(package) && line.contains(repo_fragment) && line.contains(&branch_tag) {
            if let Some(hash_start) = line.rfind('#') {
                let rest = &line[hash_start + 1..];
                if let Some(hash_end) = rest.find('"') {
                    return Some(rest[..hash_end].to_string());
                }
            }
        }
    }
    None
}

// ── /api/blueprint/save ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BlueprintSaveRequest {
    pub path:    String,
    pub content: String,
}

#[derive(Serialize)]
pub struct SaveResult {
    pub ok:    bool,
    pub error: Option<String>,
}

pub async fn handle_blueprint_save(Json(req): Json<BlueprintSaveRequest>) -> impl IntoResponse {
    let path = PathBuf::from(&req.path);

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Json(SaveResult { ok: false, error: Some(e.to_string()) });
        }
    }

    match std::fs::write(&path, &req.content) {
        Ok(_)  => Json(SaveResult { ok: true, error: None }),
        Err(e) => Json(SaveResult { ok: false, error: Some(e.to_string()) }),
    }
}
