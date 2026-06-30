use eframe::egui;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, USER_AGENT};
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use crate::RusplorerApp;

const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/jeremythille/Rusplorer/releases/latest";
const GITHUB_REPO_API_URL: &str = "https://api.github.com/repos/jeremythille/Rusplorer";
const GITHUB_REPO_WEB_URL: &str = "https://github.com/jeremythille/Rusplorer";

#[derive(Clone, Debug)]
pub(crate) struct UpdateRelease {
    pub(crate) version_display: String,
    pub(crate) exe_url: String,
    pub(crate) checksum_url: Option<String>,
    pub(crate) expected_sha256: Option<String>,
    pub(crate) html_url: String,
}

enum ReleaseCheckError {
    NotFound,
    Other(String),
}

#[derive(Clone, Debug)]
pub(crate) enum UpdateUiState {
    Idle,
    Checking,
    UpToDate { latest: String },
    Available(UpdateRelease),
    Downloading { version_display: String },
    Applying { version_display: String },
    Error(String),
}

pub(crate) enum UpdateCheckResult {
    UpToDate(String),
    Available(UpdateRelease),
    Error(String),
}

pub(crate) enum UpdateApplyResult {
    ReadyToRestart { script_path: PathBuf, version_display: String },
    Error(String),
}

impl RusplorerApp {
    pub(crate) fn start_update_check(&mut self) {
        if self.update_check_receiver.is_some() {
            return;
        }
        self.update_state = UpdateUiState::Checking;
        self.update_last_check = std::time::Instant::now();

        let (tx, rx) = channel::<UpdateCheckResult>();
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        std::thread::spawn(move || {
            let result = check_for_update(&current_version)
                .unwrap_or_else(UpdateCheckResult::Error);
            let _ = tx.send(result);
        });
        self.update_check_receiver = Some(rx);
    }

    pub(crate) fn start_apply_update(&mut self) {
        if self.update_apply_receiver.is_some() {
            return;
        }

        let release = match self.update_state.clone() {
            UpdateUiState::Available(r) => r,
            _ => return,
        };

        self.update_state = UpdateUiState::Downloading {
            version_display: release.version_display.clone(),
        };

        let (tx, rx) = channel::<UpdateApplyResult>();
        std::thread::spawn(move || {
            let result = apply_update(&release).unwrap_or_else(UpdateApplyResult::Error);
            let _ = tx.send(result);
        });
        self.update_apply_receiver = Some(rx);
    }

    pub(crate) fn poll_update_tasks(&mut self, ctx: &egui::Context) {
        let check_msg = self
            .update_check_receiver
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        if let Some(msg) = check_msg {
            self.update_check_receiver = None;
            match msg {
                UpdateCheckResult::UpToDate(v) => {
                    self.update_state = UpdateUiState::UpToDate { latest: v };
                }
                UpdateCheckResult::Available(release) => {
                    self.update_state = UpdateUiState::Available(release);
                }
                UpdateCheckResult::Error(err) => {
                    self.update_state = UpdateUiState::Error(err);
                }
            }
        }

        let apply_msg = self
            .update_apply_receiver
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        if let Some(msg) = apply_msg {
            self.update_apply_receiver = None;
            match msg {
                UpdateApplyResult::ReadyToRestart {
                    script_path,
                    version_display,
                } => {
                    self.update_state = UpdateUiState::Applying {
                        version_display: version_display.clone(),
                    };
                    match spawn_update_script(&script_path) {
                        Ok(()) => {
                            self.delete_feedback_msg = Some(format!(
                                "Applying update to {version_display}. Rusplorer will restart shortly..."
                            ));
                            self.delete_feedback_until = None;
                            self.delete_feedback_is_error = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => {
                            self.update_state = UpdateUiState::Error(format!(
                                "Failed to launch updater helper: {e}"
                            ));
                        }
                    }
                }
                UpdateApplyResult::Error(err) => {
                    self.update_state = UpdateUiState::Error(err);
                }
            }
        }
    }

    pub(crate) fn render_update_button(&mut self, ui: &mut egui::Ui) {
        match &self.update_state {
            UpdateUiState::Available(release) => {
                let label = egui::RichText::new("● Update")
                    .color(egui::Color32::from_rgb(255, 165, 0))
                    .strong();
                let tip = format!(
                    "Current version: {}\nAvailable version: {}\n{}",
                    env!("BUILD_TIMESTAMP"),
                    release.version_display,
                    release.html_url
                );
                if ui.button(label).on_hover_text(tip).clicked() {
                    self.start_apply_update();
                }
            }
            UpdateUiState::Checking => {
                ui.add_enabled(false, egui::Button::new("Checking update..."))
                    .on_hover_text(format!(
                        "Current version: {}\nAvailable version: checking...",
                        env!("BUILD_TIMESTAMP")
                    ));
            }
            UpdateUiState::Downloading { version_display } => {
                ui.add_enabled(false, egui::Button::new(format!("Downloading {version_display}...")))
                    .on_hover_text(format!(
                        "Current version: {}\nAvailable version: {}",
                        env!("BUILD_TIMESTAMP"),
                        version_display
                    ));
            }
            UpdateUiState::Applying { version_display } => {
                ui.add_enabled(false, egui::Button::new(format!("Applying {version_display}...")))
                    .on_hover_text(format!(
                        "Current version: {}\nAvailable version: {}",
                        env!("BUILD_TIMESTAMP"),
                        version_display
                    ));
            }
            UpdateUiState::Error(err) => {
                let retry = egui::RichText::new("⚠ Update").color(egui::Color32::from_rgb(255, 140, 0));
                if ui
                    .button(retry)
                    .on_hover_text(format!("Last update error: {err}\nClick to retry check"))
                    .clicked()
                {
                    self.start_update_check();
                }
            }
            UpdateUiState::UpToDate { latest } => {
                if ui
                    .small_button("Check updates")
                    .on_hover_text(format!(
                        "Current version: {}\nAvailable version: {}",
                        env!("BUILD_TIMESTAMP"),
                        latest
                    ))
                    .clicked()
                {
                    self.start_update_check();
                }
            }
            UpdateUiState::Idle => {
                if ui
                    .small_button("Check updates")
                    .on_hover_text(format!(
                        "Current version: {}\nAvailable version: not checked yet",
                        env!("BUILD_TIMESTAMP")
                    ))
                    .clicked()
                {
                    self.start_update_check();
                }
            }
        }
    }
}

fn check_for_update(current_version: &str) -> Result<UpdateCheckResult, String> {
    let client = http_client()?;
    match check_release_update(&client, current_version) {
        Ok(result) => Ok(result),
        Err(ReleaseCheckError::NotFound) => check_repo_exe_update(&client),
        Err(ReleaseCheckError::Other(err)) => Err(err),
    }
}

fn check_release_update(client: &Client, current_version: &str) -> Result<UpdateCheckResult, ReleaseCheckError> {
    let response = client
        .get(GITHUB_LATEST_RELEASE_URL)
        .header(USER_AGENT, "Rusplorer-Updater")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .map_err(|e| ReleaseCheckError::Other(format!("release query failed: {e}")))?;

    if response.status() == StatusCode::NOT_FOUND {
        return Err(ReleaseCheckError::NotFound);
    }

    let release_json = response
        .error_for_status()
        .map_err(|e| ReleaseCheckError::Other(format!("release query failed: {e}")))?
        .json::<Value>()
        .map_err(|e| ReleaseCheckError::Other(format!("invalid release payload: {e}")))?;

    let tag_name = release_json
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| ReleaseCheckError::Other("release payload missing tag_name".to_string()))?
        .to_string();

    let html_url = release_json
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("https://github.com/jeremythille/Rusplorer/releases")
        .to_string();

    let version_display = release_json
        .get("published_at")
        .and_then(Value::as_str)
        .map(format_github_datetime)
        .unwrap_or_else(|| tag_name.clone());

    let assets = release_json
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| ReleaseCheckError::Other("release payload missing assets".to_string()))?;

    let exe_url = assets
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str).map(|n| n.eq_ignore_ascii_case("rusplorer.exe")).unwrap_or(false))
        .and_then(|a| a.get("browser_download_url").and_then(Value::as_str))
        .ok_or_else(|| ReleaseCheckError::Other("latest release does not contain rusplorer.exe asset".to_string()))?
        .to_string();

    let checksum_url = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(Value::as_str)
                .map(|n| {
                    let lower = n.to_ascii_lowercase();
                    lower == "rusplorer.exe.sha256" || lower.ends_with(".sha256")
                })
                .unwrap_or(false)
        })
        .and_then(|a| a.get("browser_download_url").and_then(Value::as_str))
        .map(ToString::to_string);

    if checksum_url.is_none() {
        return Err(ReleaseCheckError::Other(
            "latest release is missing checksum asset (expected rusplorer.exe.sha256)"
                .to_string(),
        ));
    }

    let local_build_ts = env!("BUILD_TIMESTAMP");
    let is_newer = is_remote_newer(current_version, &tag_name)
        && is_remote_timestamp_newer(local_build_ts, &version_display);
    if is_newer {
        Ok(UpdateCheckResult::Available(UpdateRelease {
            version_display,
            exe_url,
            checksum_url,
            expected_sha256: None,
            html_url,
        }))
    } else {
        Ok(UpdateCheckResult::UpToDate(version_display))
    }
}

fn check_repo_exe_update(client: &Client) -> Result<UpdateCheckResult, String> {
    let repo_json = client
        .get(GITHUB_REPO_API_URL)
        .header(USER_AGENT, "Rusplorer-Updater")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("repo query failed: {e}"))?
        .json::<Value>()
        .map_err(|e| format!("invalid repo payload: {e}"))?;

    let default_branch = repo_json
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or("main");

    let contents_url = format!(
        "{}/contents/rusplorer.exe?ref={}",
        GITHUB_REPO_API_URL, default_branch
    );

    let exe_json = client
        .get(&contents_url)
        .header(USER_AGENT, "Rusplorer-Updater")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("repo executable query failed: {e}"))?
        .json::<Value>()
        .map_err(|e| format!("invalid executable payload: {e}"))?;

    let exe_url = exe_json
        .get("download_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "repository rusplorer.exe has no download_url".to_string())?
        .to_string();

    let remote_sha_tag = exe_json
        .get("sha")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| "repo".to_string());

    let remote_sha256 = sha256_url(client, &exe_url)?;
    let local_exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let local_sha256 = sha256_file(&local_exe)?;

    let version_label = if remote_sha_tag.len() >= 7 {
        format!("repo-{}", &remote_sha_tag[..7])
    } else {
        "repo-latest".to_string()
    };
    let version_display = fetch_repo_file_timestamp(client, default_branch)
        .unwrap_or_else(|| version_label.clone());

    let local_build_ts = env!("BUILD_TIMESTAMP");
    if !is_remote_timestamp_newer(local_build_ts, &version_display) {
        return Ok(UpdateCheckResult::UpToDate(version_display));
    }

    if remote_sha256.eq_ignore_ascii_case(&local_sha256) {
        Ok(UpdateCheckResult::UpToDate(version_display))
    } else {
        Ok(UpdateCheckResult::Available(UpdateRelease {
            version_display,
            exe_url,
            checksum_url: None,
            expected_sha256: Some(remote_sha256),
            html_url: format!("{}/blob/{}/rusplorer.exe", GITHUB_REPO_WEB_URL, default_branch),
        }))
    }
}

fn apply_update(release: &UpdateRelease) -> Result<UpdateApplyResult, String> {
    let exe_path = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| "failed to resolve executable directory".to_string())?
        .to_path_buf();

    let staged_exe = exe_dir.join("rusplorer.update.new.exe");
    download_file(&release.exe_url, &staged_exe)?;

    let expected_sha = if let Some(expected) = &release.expected_sha256 {
        Some(expected.clone())
    } else if let Some(checksum_url) = &release.checksum_url {
        Some(download_checksum(checksum_url)?)
    } else {
        None
    };

    if let Some(expected) = expected_sha {
        let got = sha256_file(&staged_exe)?;
        if !expected.eq_ignore_ascii_case(&got) {
            let _ = std::fs::remove_file(&staged_exe);
            return Err(format!(
                "checksum mismatch for downloaded update (expected {expected}, got {got})"
            ));
        }
    }

    let script_path = exe_dir.join("rusplorer.update.apply.cmd");
    write_update_script(&script_path, &exe_path, &staged_exe)?;

    Ok(UpdateApplyResult::ReadyToRestart {
        script_path,
        version_display: release.version_display.clone(),
    })
}

fn fetch_repo_file_timestamp(client: &Client, branch: &str) -> Option<String> {
    let commits_url = format!(
        "{}/commits?path=rusplorer.exe&sha={}&per_page=1",
        GITHUB_REPO_API_URL, branch
    );

    let commits = client
        .get(commits_url)
        .header(USER_AGENT, "Rusplorer-Updater")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .ok()?;

    let date = commits
        .as_array()?
        .first()?
        .get("commit")?
        .get("committer")?
        .get("date")?
        .as_str()?;

    Some(format_github_datetime(date))
}

fn format_github_datetime(raw: &str) -> String {
    if raw.len() >= 16 {
        raw[..16].replace('T', " ")
    } else {
        raw.to_string()
    }
}

fn spawn_update_script(script_path: &Path) -> Result<(), String> {
    std::process::Command::new("cmd")
        .arg("/C")
        .arg(script_path)
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    Ok(())
}

fn write_update_script(script_path: &Path, target_exe: &Path, staged_exe: &Path) -> Result<(), String> {
    let target = target_exe.to_string_lossy();
    let staged = staged_exe.to_string_lossy();

    let script = format!(
        "@echo off\r\nsetlocal\r\nset \"TARGET={target}\"\r\nset \"STAGED={staged}\"\r\n\r\nif not exist \"%STAGED%\" goto :fail\r\n\r\nrem Wait until TARGET is no longer locked (all running instances exited).\r\nfor /L %%I in (1,1,180) do (\r\n  powershell -NoProfile -Command \"try {{ $fs=[System.IO.File]::Open($env:TARGET,'Open','ReadWrite','None'); $fs.Close(); exit 0 }} catch {{ exit 1 }}\" >nul 2>nul\r\n  if errorlevel 1 (\r\n    timeout /t 1 /nobreak >nul\r\n  ) else (\r\n    goto :replace\r\n  )\r\n)\r\n\r\ngoto :fail\r\n\r\n:replace\r\ndel /F /Q \"%TARGET%\" >nul 2>nul\r\ncopy /Y \"%STAGED%\" \"%TARGET%\" >nul\r\nif errorlevel 1 goto :fail\r\n\r\nstart \"\" \"%TARGET%\"\r\ndel /F /Q \"%STAGED%\" >nul 2>nul\r\ndel /F /Q \"%~f0\" >nul 2>nul\r\nexit /b 0\r\n\r\n:fail\r\nrem Best-effort rollback: if original executable still exists, start it.\r\nif exist \"%TARGET%\" start \"\" \"%TARGET%\"\r\nexit /b 1\r\n"
    );

    std::fs::write(script_path, script).map_err(|e| format!("failed to write helper script: {e}"))
}

fn download_checksum(url: &str) -> Result<String, String> {
    let client = http_client()?;
    let body = client
        .get(url)
        .header(USER_AGENT, "Rusplorer-Updater")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("checksum download failed: {e}"))?
        .text()
        .map_err(|e| format!("checksum text parse failed: {e}"))?;

    let token = body
        .split_whitespace()
        .find(|part| part.len() == 64 && part.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| "checksum file does not contain a SHA-256 hex value".to_string())?;
    Ok(token.to_ascii_lowercase())
}

fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let client = http_client()?;
    let mut response = client
        .get(url)
        .header(USER_AGENT, "Rusplorer-Updater")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("download failed: {e}"))?;

    let mut file = File::create(dest).map_err(|e| format!("failed to create {}: {e}", dest.display()))?;
    std::io::copy(&mut response, &mut file)
        .map_err(|e| format!("failed to write {}: {e}", dest.display()))?;
    Ok(())
}

fn sha256_url(client: &Client, url: &str) -> Result<String, String> {
    let mut response = client
        .get(url)
        .header(USER_AGENT, "Rusplorer-Updater")
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("download failed: {e}"))?;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("http client init failed: {e}"))
}

fn is_remote_newer(current_version: &str, remote_tag: &str) -> bool {
    let current = strip_tag_prefix(current_version);
    let remote = strip_tag_prefix(remote_tag);

    if current.eq_ignore_ascii_case(remote) {
        return false;
    }

    match (parse_simple_version(current), parse_simple_version(remote)) {
        (Some(c), Some(r)) => compare_simple_version(&r, &c) == Ordering::Greater,
        _ => true,
    }
}

fn strip_tag_prefix(s: &str) -> &str {
    s.strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s)
}

fn parse_simple_version(s: &str) -> Option<Vec<u64>> {
    let numeric = s
        .split('-')
        .next()
        .unwrap_or(s)
        .split('+')
        .next()
        .unwrap_or(s);
    let mut parts = Vec::new();
    for part in numeric.split('.') {
        if part.is_empty() {
            return None;
        }
        let n = part.parse::<u64>().ok()?;
        parts.push(n);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

fn compare_simple_version(a: &[u64], b: &[u64]) -> Ordering {
    let max_len = a.len().max(b.len());
    for i in 0..max_len {
        let av = *a.get(i).unwrap_or(&0);
        let bv = *b.get(i).unwrap_or(&0);
        match av.cmp(&bv) {
            Ordering::Equal => continue,
            non_eq => return non_eq,
        }
    }
    Ordering::Equal
}

fn is_remote_timestamp_newer(local_ts: &str, remote_ts: &str) -> bool {
    compare_timestamp_ymdhm(remote_ts, local_ts)
        .map(|o| o == Ordering::Greater)
        .unwrap_or(true)
}

fn compare_timestamp_ymdhm(a: &str, b: &str) -> Option<Ordering> {
    let na = normalize_timestamp_ymdhm(a)?;
    let nb = normalize_timestamp_ymdhm(b)?;
    Some(na.cmp(&nb))
}

fn normalize_timestamp_ymdhm(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() < 16 {
        return None;
    }
    let core = &s[..16];
    let bytes = core.as_bytes();
    if bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b' ')
        || bytes.get(13) != Some(&b':')
    {
        return None;
    }
    let mut out = String::with_capacity(12);
    for (i, ch) in core.chars().enumerate() {
        if i == 4 || i == 7 || i == 10 || i == 13 {
            continue;
        }
        if !ch.is_ascii_digit() {
            return None;
        }
        out.push(ch);
    }
    Some(out)
}
