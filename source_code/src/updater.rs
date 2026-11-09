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
use crate::theme;

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

pub(crate) enum UpdateCheckEvent {
    Log(String),
    Done(UpdateCheckResult),
}

pub(crate) enum UpdateApplyResult {
    ReadyToRestart { script_path: PathBuf, version_display: String },
    Error(String),
}

pub(crate) enum UpdateApplyEvent {
    Log(String),
    Done(UpdateApplyResult),
}

impl RusplorerApp {
    pub(crate) fn push_update_log(&mut self, line: impl Into<String>) {
        self.update_log_lines.push(line.into());
        if self.update_log_lines.len() > 300 {
            let drop_count = self.update_log_lines.len().saturating_sub(300);
            self.update_log_lines.drain(0..drop_count);
        }
    }

    pub(crate) fn open_updates_page(&mut self) {
        self.show_updates_page = true;
        self.show_drives_page = false;
    }

    pub(crate) fn start_update_check(&mut self) {
        self.start_update_check_with_mode(true);
    }

    pub(crate) fn start_update_check_background(&mut self) {
        self.start_update_check_with_mode(false);
    }

    fn start_update_check_with_mode(&mut self, open_page: bool) {
        if self.update_check_receiver.is_some() {
            self.push_update_log("Update check already running...");
            return;
        }

        if open_page {
            self.open_updates_page();
        }
        self.update_state = UpdateUiState::Checking;
        self.update_last_check = std::time::Instant::now();
        self.push_update_log(format!(
            "Checking updates... (you have {})",
            env!("BUILD_TIMESTAMP")
        ));

        let (tx, rx) = channel::<UpdateCheckEvent>();
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        std::thread::spawn(move || {
            let result = check_for_update_with_logs(&current_version, |line| {
                let _ = tx.send(UpdateCheckEvent::Log(line));
            })
                .unwrap_or_else(UpdateCheckResult::Error);
            let _ = tx.send(UpdateCheckEvent::Done(result));
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

        self.open_updates_page();
        self.update_state = UpdateUiState::Downloading {
            version_display: release.version_display.clone(),
        };
        self.push_update_log(format!(
            "Starting update to {}...",
            release.version_display
        ));

        let (tx, rx) = channel::<UpdateApplyEvent>();
        std::thread::spawn(move || {
            let result = apply_update_with_logs(&release, |line| {
                let _ = tx.send(UpdateApplyEvent::Log(line));
            })
            .unwrap_or_else(UpdateApplyResult::Error);
            let _ = tx.send(UpdateApplyEvent::Done(result));
        });
        self.update_apply_receiver = Some(rx);
    }

    pub(crate) fn poll_update_tasks(&mut self, ctx: &egui::Context) {
        let mut check_events: Vec<UpdateCheckEvent> = Vec::new();
        if let Some(rx) = self.update_check_receiver.as_ref() {
            while let Ok(event) = rx.try_recv() {
                check_events.push(event);
            }
        }

        let mut clear_check_rx = false;
        for event in check_events {
            match event {
                UpdateCheckEvent::Log(line) => {
                    self.push_update_log(line);
                }
                UpdateCheckEvent::Done(msg) => {
                    clear_check_rx = true;
                    match msg {
                        UpdateCheckResult::UpToDate(v) => {
                            self.push_update_log(format!(
                                "Already up to date. Latest build: {} (you have {}).",
                                v,
                                env!("BUILD_TIMESTAMP")
                            ));
                            self.update_state = UpdateUiState::UpToDate { latest: v };
                        }
                        UpdateCheckResult::Available(release) => {
                            self.push_update_log(format!(
                                "New version found! {} (you have {}).",
                                release.version_display,
                                env!("BUILD_TIMESTAMP")
                            ));
                            self.push_update_log("Click 'Update now' below to install it.".to_string());
                            self.update_state = UpdateUiState::Available(release);
                        }
                        UpdateCheckResult::Error(err) => {
                            self.push_update_log(format!("Update check failed: {err}"));
                            self.update_state = UpdateUiState::Error(err);
                        }
                    }
                }
            }
        }
        if clear_check_rx {
            self.update_check_receiver = None;
        }

        let mut apply_events: Vec<UpdateApplyEvent> = Vec::new();
        if let Some(rx) = self.update_apply_receiver.as_ref() {
            while let Ok(event) = rx.try_recv() {
                apply_events.push(event);
            }
        }

        let mut clear_apply_rx = false;
        for event in apply_events {
            match event {
                UpdateApplyEvent::Log(line) => {
                    self.push_update_log(line);
                }
                UpdateApplyEvent::Done(msg) => {
                    clear_apply_rx = true;
                    match msg {
                        UpdateApplyResult::ReadyToRestart {
                            script_path,
                            version_display,
                        } => {
                            self.update_state = UpdateUiState::Applying {
                                version_display: version_display.clone(),
                            };
                            self.push_update_log("Launching updater helper...".to_string());
                            match spawn_update_script(&script_path) {
                                Ok(()) => {
                                    self.push_update_log(format!(
                                        "Applying update to {}. Rusplorer will restart now.",
                                        version_display
                                    ));
                                    self.delete_feedback_msg = Some(format!(
                                        "Applying update to {version_display}. Rusplorer will restart shortly..."
                                    ));
                                    self.delete_feedback_until = None;
                                    self.delete_feedback_is_error = false;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                Err(e) => {
                                    self.push_update_log(format!(
                                        "Failed to launch updater helper: {}",
                                        e
                                    ));
                                    self.update_state = UpdateUiState::Error(format!(
                                        "Failed to launch updater helper: {e}"
                                    ));
                                }
                            }
                        }
                        UpdateApplyResult::Error(err) => {
                            self.push_update_log(format!("Update apply failed: {err}"));
                            self.update_state = UpdateUiState::Error(err);
                        }
                    }
                }
            }
        }
        if clear_apply_rx {
            self.update_apply_receiver = None;
        }
    }

    pub(crate) fn render_update_button(&mut self, ui: &mut egui::Ui) {
        let label = match self.update_state {
            UpdateUiState::Available(_) => "● Updates",
            UpdateUiState::Checking => "Updates...",
            UpdateUiState::Downloading { .. } => "Downloading...",
            UpdateUiState::Applying { .. } => "Applying...",
            UpdateUiState::Error(_) => "⚠ Updates",
            UpdateUiState::UpToDate { .. } | UpdateUiState::Idle => "Check updates",
        };

        if ui.add(egui::Button::new(label).fill(theme::ACCENT).stroke(egui::Stroke::NONE)).clicked() {
            self.open_updates_page();
            if matches!(
                self.update_state,
                UpdateUiState::Idle | UpdateUiState::UpToDate { .. } | UpdateUiState::Error(_)
            ) {
                self.start_update_check();
            }
        }
    }

    pub(crate) fn render_updates_page(&mut self, ui: &mut egui::Ui) {
        let current_build = env!("BUILD_TIMESTAMP");

        ui.heading("Updates");
        ui.add_space(4.0);
        ui.label(format!("Installed build: {current_build}"));

        match &self.update_state {
            UpdateUiState::Available(release) => {
                ui.colored_label(
                    egui::Color32::from_rgb(255, 165, 0),
                    format!("New version found: {}", release.version_display),
                );
            }
            UpdateUiState::UpToDate { latest } => {
                ui.colored_label(
                    egui::Color32::from_rgb(120, 200, 120),
                    format!("Up to date. Latest build: {latest}"),
                );
            }
            UpdateUiState::Checking => {
                ui.label("Checking for updates...");
            }
            UpdateUiState::Downloading { version_display } => {
                ui.label(format!("Downloading update {version_display}..."));
            }
            UpdateUiState::Applying { version_display } => {
                ui.label(format!("Applying update {version_display}..."));
            }
            UpdateUiState::Error(err) => {
                ui.colored_label(egui::Color32::from_rgb(240, 120, 90), format!("Error: {err}"));
            }
            UpdateUiState::Idle => {
                ui.label("No check has been run in this session yet.");
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_check = self.update_check_receiver.is_none() && self.update_apply_receiver.is_none();
            if ui
                .add_enabled(can_check, egui::Button::new("Check updates now"))
                .clicked()
            {
                self.start_update_check();
            }

            let release = match &self.update_state {
                UpdateUiState::Available(release) => Some(release.clone()),
                _ => None,
            };

            if let Some(release) = release {
                if ui
                    .button(format!("Update now -> {}", release.version_display))
                    .clicked()
                {
                    self.start_apply_update();
                }
                ui.hyperlink_to("Release notes", &release.html_url);
            }
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Live log").strong());

        egui::ScrollArea::vertical()
            .id_source("updates_live_log")
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.update_log_lines {
                    ui.label(line);
                }
            });

        if self.update_check_receiver.is_some() || self.update_apply_receiver.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

}

fn current_artifact_name() -> Result<&'static str, String> {
    let current_exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let file_name = current_exe
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if file_name.eq_ignore_ascii_case("rusplorer_compatibility.exe") {
        Ok("rusplorer_compatibility.exe")
    } else {
        Ok("rusplorer.exe")
    }
}

fn check_for_update_with_logs<F: FnMut(String)>(
    current_version: &str,
    mut log: F,
) -> Result<UpdateCheckResult, String> {
    let artifact_name = current_artifact_name()?;
    log("Contacting update service...".to_string());
    let client = http_client()?;
    log("Checking GitHub latest release...".to_string());
    match check_release_update(&client, current_version, &mut log, artifact_name) {
        Ok(result) => Ok(result),
        Err(ReleaseCheckError::NotFound) => {
            log("No release endpoint found, falling back to repository executable...".to_string());
            check_repo_exe_update(&client, &mut log, artifact_name)
        }
        Err(ReleaseCheckError::Other(err)) => Err(err),
    }
}

fn check_release_update<F: FnMut(String)>(
    client: &Client,
    current_version: &str,
    log: &mut F,
    artifact_name: &str,
) -> Result<UpdateCheckResult, ReleaseCheckError> {
    log("Fetching latest release metadata...".to_string());
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
    log(format!("Latest release candidate: {version_display}"));

    let assets = release_json
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| ReleaseCheckError::Other("release payload missing assets".to_string()))?;

    let exe_url = assets
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str).map(|n| n.eq_ignore_ascii_case(artifact_name)).unwrap_or(false))
        .and_then(|a| a.get("browser_download_url").and_then(Value::as_str))
        .ok_or_else(|| ReleaseCheckError::Other(format!("latest release does not contain {artifact_name} asset")))?
        .to_string();

    let checksum_name = format!("{artifact_name}.sha256");
    let checksum_url = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(Value::as_str)
                .map(|n| n.eq_ignore_ascii_case(&checksum_name))
                .unwrap_or(false)
        })
        .and_then(|a| a.get("browser_download_url").and_then(Value::as_str))
        .map(ToString::to_string);

    if checksum_url.is_none() {
        return Err(ReleaseCheckError::Other(
            format!("latest release is missing checksum asset (expected {checksum_name})")
                .to_string(),
        ));
    }

    let local_build_ts = env!("BUILD_TIMESTAMP");
    let is_newer = is_remote_newer(current_version, &tag_name)
        && is_remote_timestamp_newer(local_build_ts, &version_display);
    if is_newer {
        log("A newer release is available.".to_string());
        Ok(UpdateCheckResult::Available(UpdateRelease {
            version_display,
            exe_url,
            checksum_url,
            expected_sha256: None,
            html_url,
        }))
    } else {
        log("You already have the latest release build.".to_string());
        Ok(UpdateCheckResult::UpToDate(version_display))
    }
}

fn check_repo_exe_update<F: FnMut(String)>(
    client: &Client,
    log: &mut F,
    artifact_name: &str,
) -> Result<UpdateCheckResult, String> {
    log("Fetching repository metadata...".to_string());
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
    log(format!("Default branch detected: {default_branch}"));

    let contents_url = format!(
        "{}/contents/{}?ref={}",
        GITHUB_REPO_API_URL, artifact_name, default_branch
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
        .ok_or_else(|| format!("repository {artifact_name} has no download_url"))?
        .to_string();
    log("Downloading remote checksum for comparison...".to_string());

    let remote_sha_tag = exe_json
        .get("sha")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| "repo".to_string());

    let remote_sha256 = sha256_url(client, &exe_url)?;
    let local_exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    log("Computing local executable checksum...".to_string());
    let local_sha256 = sha256_file(&local_exe)?;

    let version_label = if remote_sha_tag.len() >= 7 {
        format!("repo-{}", &remote_sha_tag[..7])
    } else {
        "repo-latest".to_string()
    };
    let version_display = fetch_repo_file_timestamp(client, default_branch, artifact_name)
        .unwrap_or_else(|| version_label.clone());
    log(format!("Repository executable timestamp: {version_display}"));

    let local_build_ts = env!("BUILD_TIMESTAMP");
    if !is_remote_timestamp_newer(local_build_ts, &version_display) {
        return Ok(UpdateCheckResult::UpToDate(version_display));
    }

    if remote_sha256.eq_ignore_ascii_case(&local_sha256) {
        log("Local executable checksum matches repository build.".to_string());
        Ok(UpdateCheckResult::UpToDate(version_display))
    } else {
        log("Repository executable differs from local build.".to_string());
        Ok(UpdateCheckResult::Available(UpdateRelease {
            version_display,
            exe_url,
            checksum_url: None,
            expected_sha256: Some(remote_sha256),
            html_url: format!("{}/blob/{}/{}", GITHUB_REPO_WEB_URL, default_branch, artifact_name),
        }))
    }
}

fn apply_update_with_logs<F: FnMut(String)>(
    release: &UpdateRelease,
    mut log: F,
) -> Result<UpdateApplyResult, String> {
    log("Preparing update staging files...".to_string());
    let exe_path = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| "failed to resolve executable directory".to_string())?
        .to_path_buf();

    let staged_exe = exe_dir.join("rusplorer.update.new.exe");
    log("Downloading update executable...".to_string());
    download_file(&release.exe_url, &staged_exe)?;
    log("Download complete.".to_string());

    let expected_sha = if let Some(expected) = &release.expected_sha256 {
        log("Using provided remote SHA-256 checksum.".to_string());
        Some(expected.clone())
    } else if let Some(checksum_url) = &release.checksum_url {
        log("Downloading checksum file...".to_string());
        Some(download_checksum(checksum_url)?)
    } else {
        None
    };

    if let Some(expected) = expected_sha {
        log("Verifying downloaded executable integrity...".to_string());
        let got = sha256_file(&staged_exe)?;
        if !expected.eq_ignore_ascii_case(&got) {
            let _ = std::fs::remove_file(&staged_exe);
            return Err(format!(
                "checksum mismatch for downloaded update (expected {expected}, got {got})"
            ));
        }
        log("Checksum OK.".to_string());
    }

    let script_path = exe_dir.join("rusplorer.update.apply.cmd");
    log("Writing updater helper script...".to_string());
    write_update_script(&script_path, &exe_path, &staged_exe)?;
    log("Update package staged. Ready to restart.".to_string());

    Ok(UpdateApplyResult::ReadyToRestart {
        script_path,
        version_display: release.version_display.clone(),
    })
}

fn fetch_repo_file_timestamp(client: &Client, branch: &str, artifact_name: &str) -> Option<String> {
    let commits_url = format!(
        "{}/commits?path={}&sha={}&per_page=1",
        GITHUB_REPO_API_URL, artifact_name, branch
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
