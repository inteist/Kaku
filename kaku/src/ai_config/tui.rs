use crate::assistant_config;
use crate::utils::{is_jsonc_path, parse_json_or_jsonc, write_atomic};
use anyhow::Context;
use chrono::{DateTime, Utc};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

mod ui;

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    KakuAssistant,
    ClaudeCode,
    Codex,
    Kimi,
    Gemini,
    Copilot,
    FactoryDroid,
    OpenClaw,
}

impl Tool {
    fn label(&self) -> &'static str {
        match self {
            Tool::KakuAssistant => "Kaku Assistant",
            Tool::ClaudeCode => "Claude Code",
            Tool::Codex => "Codex",
            Tool::Kimi => "Kimi Code",
            Tool::Gemini => "Gemini CLI",
            Tool::Copilot => "Copilot CLI",
            Tool::FactoryDroid => "Factory Droid",
            Tool::OpenClaw => "OpenClaw",
        }
    }

    fn config_path(&self) -> PathBuf {
        let home = config::HOME_DIR.clone();
        match self {
            Tool::KakuAssistant => assistant_config::assistant_toml_path().unwrap_or_else(|_| {
                config::HOME_DIR
                    .join(".config")
                    .join("kaku")
                    .join("assistant.toml")
            }),
            Tool::ClaudeCode => home.join(".claude").join("settings.json"),
            Tool::Codex => home.join(".codex").join("config.toml"),
            Tool::Kimi => home.join(".kimi").join("config.toml"),
            Tool::Gemini => home.join(".gemini").join("settings.json"),
            Tool::Copilot => home.join(".copilot").join("config.json"),
            Tool::FactoryDroid => home.join(".factory").join("settings.json"),
            Tool::OpenClaw => {
                let new_path = home.join(".openclaw").join("openclaw.json");
                if new_path.exists() {
                    return new_path;
                }
                let legacy = home.join(".clawdbot").join("clawdbot.json");
                if legacy.exists() {
                    return legacy;
                }
                new_path
            }
        }
    }
}

const ALL_TOOLS: [Tool; 8] = [
    Tool::KakuAssistant,
    Tool::ClaudeCode,
    Tool::Codex,
    Tool::Kimi,
    Tool::Gemini,
    Tool::Copilot,
    Tool::FactoryDroid,
    Tool::OpenClaw,
];

const FACTORY_DROID_DEFAULT_MODEL: &str = "opus";
const FACTORY_DROID_DEFAULT_REASONING: &str = "off";
const FACTORY_DROID_DEFAULT_AUTONOMY: &str = "normal";
const FACTORY_DROID_REASONING_OPTIONS: [&str; 5] = ["off", "none", "low", "medium", "high"];
const FACTORY_DROID_AUTONOMY_OPTIONS: [&str; 5] =
    ["normal", "spec", "auto-low", "auto-medium", "auto-high"];
const USAGE_CACHE_TTL: Duration = Duration::from_secs(120);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const UI_STATUS_TTL: Duration = Duration::from_secs(3);
const UI_ERROR_TTL: Duration = Duration::from_secs(5);
const CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const KIMI_OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_OAUTH_TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
const KIMI_DEFAULT_BASE_URL: &str = "https://api.kimi.com/coding/v1";

static UI_ERRORS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn ui_errors() -> &'static Mutex<Vec<String>> {
    UI_ERRORS.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_ui_error(message: impl Into<String>) {
    let message = message.into();
    let mut guard = match ui_errors().lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if guard.last().is_some_and(|last| last == &message) {
        return;
    }
    if guard.len() >= 8 {
        guard.remove(0);
    }
    guard.push(message);
}

fn pop_ui_error() -> Option<String> {
    let mut guard = ui_errors().lock().ok()?;
    if guard.is_empty() {
        None
    } else {
        Some(guard.remove(0))
    }
}

struct FieldEntry {
    key: String,
    value: String,
    options: Vec<String>,
    editable: bool,
}

impl Default for FieldEntry {
    fn default() -> Self {
        Self {
            key: String::new(),
            value: String::new(),
            options: Vec::new(),
            editable: true,
        }
    }
}

struct ToolState {
    tool: Tool,
    installed: bool,
    fields: Vec<FieldEntry>,
    summary: Option<String>,
}

impl ToolState {
    fn load_without_remote_usage(tool: Tool) -> Self {
        Self::load_with_usage(tool, false)
    }

    fn load_with_usage(tool: Tool, eager_remote_usage: bool) -> Self {
        let path = if tool == Tool::KakuAssistant {
            match assistant_config::ensure_assistant_toml_exists() {
                Ok(path) => path,
                Err(err) => {
                    return ToolState {
                        tool,
                        installed: true,
                        fields: vec![FieldEntry {
                            key: "error".into(),
                            value: err.to_string(),
                            options: vec![],
                            editable: false,
                        }],
                        summary: Some("Setup failed".into()),
                    };
                }
            }
        } else {
            tool.config_path()
        };

        let extra_exists = match tool {
            Tool::Codex => config::HOME_DIR.join(".codex").join("auth.json").exists(),
            Tool::Kimi => config::HOME_DIR
                .join(".kimi")
                .join("credentials")
                .join("kimi-code.json")
                .exists(),
            _ => false,
        };

        if tool != Tool::KakuAssistant && !path.exists() && !extra_exists {
            return ToolState {
                tool,
                installed: false,
                fields: Vec::new(),
                summary: None,
            };
        }

        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound && extra_exists => String::new(),
            Err(e) => {
                log::warn!("failed to read config for {}: {}", tool.label(), e);
                return ToolState {
                    tool,
                    installed: false,
                    fields: vec![FieldEntry {
                        key: "error".into(),
                        value: format!("failed to read config: {}", e),
                        options: vec![],
                        ..Default::default()
                    }],
                    summary: Some("Config unreadable".into()),
                };
            }
        };

        let (fields, usage_summary) = match tool {
            Tool::KakuAssistant => (extract_kaku_assistant_fields(&raw), None),
            Tool::ClaudeCode => {
                let parsed = parse_json_or_jsonc_with_debug(&raw, tool.label());
                (
                    extract_claude_code_fields(&parsed),
                    eager_remote_usage
                        .then(|| load_claude_usage_snapshot().and_then(|snapshot| snapshot.summary))
                        .flatten(),
                )
            }
            Tool::Codex => (
                extract_codex_fields(&raw),
                eager_remote_usage
                    .then(|| load_codex_usage_snapshot().and_then(|snapshot| snapshot.summary))
                    .flatten(),
            ),
            Tool::Kimi => (
                extract_kimi_fields(&raw),
                eager_remote_usage
                    .then(|| load_kimi_usage_snapshot().and_then(|snapshot| snapshot.summary))
                    .flatten(),
            ),
            Tool::Gemini => {
                let parsed = parse_json_with_debug(&raw, tool.label());
                (
                    extract_gemini_fields(&parsed),
                    gemini_quota_summary(&parsed),
                )
            }
            Tool::Copilot => {
                let parsed = parse_json_with_debug(&raw, tool.label());
                (
                    extract_copilot_fields(&parsed),
                    eager_remote_usage
                        .then(|| {
                            load_copilot_usage_snapshot().and_then(|snapshot| snapshot.summary)
                        })
                        .flatten(),
                )
            }
            Tool::FactoryDroid => {
                let parsed = parse_json_with_debug(&raw, tool.label());
                (extract_factory_droid_fields(&parsed), None)
            }
            Tool::OpenClaw => {
                let parsed = parse_json_or_jsonc_with_debug(&raw, tool.label());
                (extract_openclaw_fields(&parsed), None)
            }
        };

        let summary = if !eager_remote_usage && supports_remote_usage(tool) {
            Some("Loading usage...".into())
        } else {
            summarize_tool_fields(tool, true, &fields, usage_summary.as_deref())
        };

        ToolState {
            tool,
            installed: true,
            fields,
            summary,
        }
    }
}

fn field_value<'a>(fields: &'a [FieldEntry], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|field| field.key == key)
        .map(|field| field.value.as_str())
}

fn compact_summary_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value == "—" {
        return None;
    }

    let value = value
        .strip_prefix("✓ ")
        .or_else(|| value.strip_prefix("✗ "))
        .unwrap_or(value)
        .trim();
    let value = value.strip_suffix(" (default)").unwrap_or(value).trim();

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn summarize_tool_fields(
    tool: Tool,
    installed: bool,
    fields: &[FieldEntry],
    usage_summary: Option<&str>,
) -> Option<String> {
    if !installed {
        return None;
    }

    if tool == Tool::KakuAssistant {
        let model = field_value(fields, "Model").and_then(compact_summary_value)?;
        let has_api_key = field_value(fields, "API Key").is_some_and(|value| value != "—");
        if has_api_key {
            return Some(format!("Ready · {model}"));
        }
        return Some(format!("Setup required · {model}"));
    }

    if matches!(
        tool,
        Tool::Codex | Tool::ClaudeCode | Tool::Kimi | Tool::Copilot
    ) {
        return usage_summary
            .map(str::to_string)
            .or_else(|| Some("Quota unavailable".into()));
    }

    if tool == Tool::Gemini {
        if let Some(summary) = usage_summary {
            return Some(summary.to_string());
        }
    }

    let mut parts = Vec::new();
    if let Some(account) = field_value(fields, "Auth").and_then(compact_summary_value) {
        parts.push(account);
    }

    let model = field_value(fields, "Model")
        .or_else(|| field_value(fields, "Primary Model"))
        .and_then(compact_summary_value);
    if let Some(model) = model {
        parts.push(model);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn should_collapse_kaku_assistant(fields: &[FieldEntry]) -> bool {
    field_value(fields, "API Key").is_some_and(|value| value != "—")
}

fn kaku_assistant_visible() -> bool {
    assistant_config::read_enabled().unwrap_or(true)
}

struct CodexUsageSnapshot {
    summary: Option<String>,
}

struct ClaudeUsageSnapshot {
    summary: Option<String>,
}

struct CopilotUsageSnapshot {
    summary: Option<String>,
}

struct KimiUsageSnapshot {
    summary: Option<String>,
}

#[derive(Clone)]
struct UsageSummaryUpdate {
    tool: Tool,
    summary: Option<String>,
}

fn codex_usage_cache_path() -> PathBuf {
    config::HOME_DIR
        .join(".cache")
        .join("kaku")
        .join("codex_usage.json")
}

fn claude_usage_cache_path() -> PathBuf {
    config::HOME_DIR
        .join(".cache")
        .join("kaku")
        .join("claude_usage.json")
}

fn copilot_usage_cache_path() -> PathBuf {
    config::HOME_DIR
        .join(".cache")
        .join("kaku")
        .join("copilot_usage.json")
}

fn kimi_usage_cache_path() -> PathBuf {
    config::HOME_DIR
        .join(".cache")
        .join("kaku")
        .join("kimi_usage.json")
}

fn read_codex_auth_info() -> Option<(String, String)> {
    let auth_path = config::HOME_DIR.join(".codex").join("auth.json");
    let auth_json = read_json_file_with_debug(&auth_path, "codex auth status")?;

    let access_token = auth_json
        .get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(|value| value.as_str())
        .or_else(|| {
            auth_json
                .get("access_token")
                .and_then(|value| value.as_str())
        })?
        .to_string();

    let account_id = auth_json
        .get("tokens")
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(|value| value.as_str())
        .or_else(|| auth_json.get("account_id").and_then(|value| value.as_str()))
        .map(|value| value.to_string())
        .or_else(|| {
            decode_jwt_payload_with_debug(&access_token, "codex auth status").and_then(|payload| {
                payload
                    .get("chatgpt_account_id")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            })
        })?;

    Some((access_token, account_id))
}

fn format_duration_short(total_seconds: i64) -> Option<String> {
    if total_seconds <= 0 {
        return None;
    }

    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3_600;
    let minutes = (total_seconds % 3_600) / 60;

    if days > 0 {
        Some(format!("{days}d{hours}h"))
    } else if hours > 0 {
        Some(format!("{hours}h{minutes}m"))
    } else {
        Some(format!("{minutes}m"))
    }
}

fn format_reset_time_from_epoch(reset_at: i64) -> Option<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    format_duration_short(reset_at - now)
}

fn format_reset_time_from_iso(reset_at: &str) -> Option<String> {
    let reset_at = DateTime::parse_from_rfc3339(reset_at).ok()?;
    let reset_at = reset_at.with_timezone(&Utc);
    format_duration_short((reset_at - Utc::now()).num_seconds())
}

fn supports_remote_usage(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::ClaudeCode | Tool::Codex | Tool::Kimi | Tool::Copilot
    )
}

fn load_usage_summary(tool: Tool) -> Option<String> {
    match tool {
        Tool::ClaudeCode => load_claude_usage_snapshot().and_then(|snapshot| snapshot.summary),
        Tool::Codex => load_codex_usage_snapshot().and_then(|snapshot| snapshot.summary),
        Tool::Kimi => load_kimi_usage_snapshot().and_then(|snapshot| snapshot.summary),
        Tool::Copilot => load_copilot_usage_snapshot().and_then(|snapshot| snapshot.summary),
        _ => None,
    }
}

fn format_percent_value(percent: f64) -> String {
    if (percent.fract()).abs() < 0.05 {
        format!("{percent:.0}%")
    } else {
        format!("{percent:.1}%")
    }
}

fn format_remaining_percent_value(used_percent: f64) -> String {
    format_percent_value((100.0 - used_percent).clamp(0.0, 100.0))
}

fn format_remaining_window_value(
    label: &str,
    used_percent: f64,
    reset_in: Option<String>,
) -> String {
    let mut value = format!(
        "{label} remain {}",
        format_remaining_percent_value(used_percent)
    );
    if let Some(reset_in) = reset_in {
        value.push_str(" · reset ");
        value.push_str(&reset_in);
    }
    value
}

fn format_remaining_count_value(value: f64) -> String {
    if (value.fract()).abs() < 0.05 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn format_codex_usage_value(label: &str, window: &serde_json::Value) -> Option<String> {
    let used_percent = window.get("used_percent")?.as_f64()?;
    let reset_in = window
        .get("reset_at")
        .and_then(|value| value.as_i64())
        .and_then(format_reset_time_from_epoch);
    Some(format_remaining_window_value(label, used_percent, reset_in))
}

fn parse_codex_usage_snapshot(data: &serde_json::Value) -> Option<CodexUsageSnapshot> {
    let rate_limit = data.get("rate_limit")?;
    let current_value = rate_limit
        .get("primary_window")
        .and_then(|window| format_codex_usage_value("5h", window));
    let weekly_value = rate_limit
        .get("secondary_window")
        .and_then(|window| format_codex_usage_value("7d", window));

    let summary = match (current_value, weekly_value) {
        (Some(current), Some(weekly)) => Some(format!("{current}  |  {weekly}")),
        (Some(current), None) => Some(current),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    };

    summary.as_ref()?;
    Some(CodexUsageSnapshot { summary })
}

fn usage_cache_is_fresh(path: &Path) -> bool {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed < USAGE_CACHE_TTL)
}

fn write_json_cache(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        if let Err(err) = config::create_user_owned_dirs(parent) {
            log::debug!("failed to create cache dir {}: {}", parent.display(), err);
            return;
        }
    }

    match serde_json::to_vec(value) {
        Ok(bytes) => {
            if let Err(err) = write_atomic(path, &bytes) {
                log::debug!("failed to write {}: {}", path.display(), err);
            }
        }
        Err(err) => log::debug!("failed to serialize {}: {}", path.display(), err),
    }
}

fn run_curl(args: &[&str]) -> Option<serde_json::Value> {
    // Request headers are passed via argv for portability, which means short-lived
    // tokens may be visible to local process inspectors such as `ps` while curl runs.
    let output = std::process::Command::new(OsStr::new("/usr/bin/curl"))
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "curl failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    let raw = String::from_utf8(output.stdout)
        .map_err(|err| log::debug!("curl returned non-utf8 stdout: {}", err))
        .ok()?;
    serde_json::from_str(&raw)
        .map_err(|err| log::debug!("failed to parse curl json: {}", err))
        .ok()
}

fn fetch_codex_usage_json() -> Option<serde_json::Value> {
    let cache_path = codex_usage_cache_path();
    if cache_path.exists() && usage_cache_is_fresh(&cache_path) {
        return load_usage_json_from_cache(&cache_path, "codex usage cache");
    }

    let (access_token, account_id) = read_codex_auth_info()?;
    let live = run_curl(&[
        "-sS",
        "--max-time",
        "3",
        "-H",
        &format!("Authorization: Bearer {access_token}"),
        "-H",
        &format!("ChatGPT-Account-Id: {account_id}"),
        "-H",
        "Accept: application/json",
        "https://chatgpt.com/backend-api/wham/usage",
    ]);

    if let Some(value) = live {
        write_json_cache(&cache_path, &value);
        return Some(value);
    }

    load_usage_json_from_cache(&cache_path, "codex usage cache")
}

fn load_codex_usage_snapshot() -> Option<CodexUsageSnapshot> {
    let data = fetch_codex_usage_json()?;
    parse_codex_usage_snapshot(&data)
}

fn load_usage_json_from_cache(path: &Path, context: &str) -> Option<serde_json::Value> {
    read_json_file_with_debug(path, context)
}

fn fetch_usage_json_with_cache<F>(
    path: PathBuf,
    context: &str,
    fetcher: F,
) -> Option<serde_json::Value>
where
    F: FnOnce() -> Option<serde_json::Value>,
{
    if path.exists() && usage_cache_is_fresh(&path) {
        return load_usage_json_from_cache(&path, context);
    }

    if let Some(value) = fetcher() {
        write_json_cache(&path, &value);
        return Some(value);
    }

    load_usage_json_from_cache(&path, context)
}

fn read_claude_oauth_credentials() -> Option<serde_json::Value> {
    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "claude keychain probe failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    let raw = String::from_utf8(output.stdout)
        .map_err(|err| log::debug!("claude keychain probe returned non-utf8 stdout: {}", err))
        .ok()?;
    serde_json::from_str::<serde_json::Value>(raw.trim())
        .map_err(|err| log::debug!("failed to parse claude keychain json: {}", err))
        .ok()
}

fn read_claude_oauth_access_token() -> Option<String> {
    let parsed = read_claude_oauth_credentials()?;

    parsed
        .get("claudeAiOauth")
        .and_then(|value| value.get("accessToken"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn read_claude_oauth_refresh_token() -> Option<String> {
    let parsed = read_claude_oauth_credentials()?;
    parsed
        .get("claudeAiOauth")
        .and_then(|value| value.get("refreshToken"))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn parse_claude_keychain_account(raw: &str) -> Option<String> {
    let marker = "\"acct\"<blob>=\"";
    raw.lines().find_map(|line| {
        let line = line.trim();
        let start = line.find(marker)? + marker.len();
        let end = line[start..].find('"')?;
        Some(line[start..start + end].to_string())
    })
}

fn read_claude_keychain_account() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", "Claude Code-credentials"])
        .output()
        .ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "claude keychain account probe failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    let raw = String::from_utf8(output.stdout)
        .map_err(|err| log::debug!("claude keychain account probe returned non-utf8: {}", err))
        .ok()?;
    parse_claude_keychain_account(&raw)
}

fn write_claude_oauth_credentials(credentials: &serde_json::Value) -> Option<()> {
    let account = read_claude_keychain_account()?;
    let secret = serde_json::to_string(credentials)
        .map_err(|err| log::debug!("failed to serialize claude keychain json: {}", err))
        .ok()?;

    let output = std::process::Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            &account,
            "-s",
            "Claude Code-credentials",
            "-w",
            &secret,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::warn!(
            "failed to update claude keychain credentials: status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    Some(())
}

fn refresh_claude_oauth_access_token() -> Option<String> {
    let current_credentials = read_claude_oauth_credentials()?;
    let refresh_token = read_claude_oauth_refresh_token()?;
    let refreshed = run_curl(&[
        "-sS",
        "--max-time",
        "5",
        "-X",
        "POST",
        CLAUDE_OAUTH_TOKEN_URL,
        "-H",
        "Content-Type: application/x-www-form-urlencoded",
        "--data-urlencode",
        "grant_type=refresh_token",
        "--data-urlencode",
        &format!("refresh_token={refresh_token}"),
        "--data-urlencode",
        &format!("client_id={CLAUDE_OAUTH_CLIENT_ID}"),
    ])?;

    let access_token = refreshed
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())?;
    let rotated_refresh_token = refreshed
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .unwrap_or(&refresh_token)
        .to_string();

    let mut updated_credentials = current_credentials;
    let oauth = updated_credentials
        .get_mut("claudeAiOauth")
        .and_then(|value| value.as_object_mut())?;
    oauth.insert(
        "accessToken".into(),
        serde_json::Value::String(access_token.clone()),
    );
    oauth.insert(
        "refreshToken".into(),
        serde_json::Value::String(rotated_refresh_token),
    );

    if let Some(expires_in) = refreshed.get("expires_in").and_then(|value| value.as_i64()) {
        let expires_at_ms = Utc::now().timestamp_millis() + expires_in * 1000;
        oauth.insert(
            "expiresAt".into(),
            serde_json::Value::Number(expires_at_ms.into()),
        );
    }

    if let Some(scope) = refreshed.get("scope").and_then(|value| value.as_str()) {
        let scopes = scope
            .split_whitespace()
            .map(|item| serde_json::Value::String(item.to_string()))
            .collect::<Vec<_>>();
        oauth.insert("scopes".into(), serde_json::Value::Array(scopes));
    }

    let _ = write_claude_oauth_credentials(&updated_credentials);
    Some(access_token)
}

fn fetch_claude_usage_with_access_token(access_token: &str) -> Option<serde_json::Value> {
    run_curl(&[
        "-sS",
        "--max-time",
        "3",
        "-H",
        &format!("Authorization: Bearer {access_token}"),
        "-H",
        "anthropic-beta: oauth-2025-04-20",
        "-H",
        "Accept: application/json",
        "-H",
        "Content-Type: application/json",
        "-H",
        "User-Agent: claude-code/2.0.27",
        "https://api.anthropic.com/api/oauth/usage",
    ])
}

fn fetch_claude_usage_json() -> Option<serde_json::Value> {
    let cache_path = claude_usage_cache_path();
    if cache_path.exists() && usage_cache_is_fresh(&cache_path) {
        if let Some(cached) = load_usage_json_from_cache(&cache_path, "claude usage cache") {
            if parse_claude_usage_error(&cached).is_none() {
                return Some(cached);
            }
        }
    }

    let access_token = read_claude_oauth_access_token()?;
    let live = fetch_claude_usage_with_access_token(&access_token)
        .filter(|value| parse_claude_usage_error(value).is_none())
        .or_else(|| {
            let refreshed = refresh_claude_oauth_access_token()?;
            fetch_claude_usage_with_access_token(&refreshed)
        });

    if let Some(value) = live {
        if parse_claude_usage_error(&value).is_none() {
            write_json_cache(&cache_path, &value);
        }
        return Some(value);
    }

    load_usage_json_from_cache(&cache_path, "claude usage cache")
}

fn parse_claude_usage_error(data: &serde_json::Value) -> Option<String> {
    let error = data.get("error")?;
    let error_type = error.get("type").and_then(|value| value.as_str());
    let error_code = error
        .get("details")
        .and_then(|value| value.get("error_code"))
        .and_then(|value| value.as_str());

    if matches!(error_type, Some("authentication_error"))
        || matches!(error_code, Some("token_expired" | "invalid_token"))
    {
        return Some("Re-auth required".into());
    }

    None
}

fn parse_claude_usage_snapshot(data: &serde_json::Value) -> Option<ClaudeUsageSnapshot> {
    if let Some(summary) = parse_claude_usage_error(data) {
        return Some(ClaudeUsageSnapshot {
            summary: Some(summary),
        });
    }

    let current_value = data.get("five_hour").and_then(|window| {
        let used_percent = window.get("utilization")?.as_f64()?;
        let reset_in = window
            .get("resets_at")
            .and_then(|value| value.as_str())
            .and_then(format_reset_time_from_iso);
        Some(format_remaining_window_value("5h", used_percent, reset_in))
    });
    let weekly_value = data.get("seven_day").and_then(|window| {
        let used_percent = window.get("utilization")?.as_f64()?;
        let reset_in = window
            .get("resets_at")
            .and_then(|value| value.as_str())
            .and_then(format_reset_time_from_iso);
        Some(format_remaining_window_value("7d", used_percent, reset_in))
    });

    let summary = match (current_value, weekly_value) {
        (Some(current), Some(weekly)) => Some(format!("{current}  |  {weekly}")),
        (Some(current), None) => Some(current),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    };

    summary.as_ref()?;
    Some(ClaudeUsageSnapshot { summary })
}

fn load_claude_usage_snapshot() -> Option<ClaudeUsageSnapshot> {
    let data = fetch_claude_usage_json()?;
    parse_claude_usage_snapshot(&data)
}

fn read_kimi_oauth_credentials() -> Option<serde_json::Value> {
    read_json_file_with_debug(&kimi_credentials_path(), "kimi credentials")
}

fn write_kimi_oauth_credentials(credentials: &serde_json::Value) -> Option<()> {
    let path = kimi_credentials_path();
    let bytes = serde_json::to_vec(credentials)
        .map_err(|err| log::debug!("failed to serialize kimi credentials: {}", err))
        .ok()?;
    write_atomic(&path, &bytes)
        .map_err(|err| log::debug!("failed to write kimi credentials: {}", err))
        .ok()?;
    Some(())
}

fn read_kimi_access_token() -> Option<String> {
    read_kimi_oauth_credentials()?
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn read_kimi_refresh_token() -> Option<String> {
    read_kimi_oauth_credentials()?
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
}

fn kimi_access_token_needs_refresh() -> bool {
    let Some(credentials) = read_kimi_oauth_credentials() else {
        return false;
    };
    let expires_at = credentials
        .get("expires_at")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    expires_at <= now + 300.0
}

fn refresh_kimi_access_token() -> Option<String> {
    let current_credentials = read_kimi_oauth_credentials()?;
    let refresh_token = read_kimi_refresh_token()?;
    let refreshed = run_curl(&[
        "-sS",
        "--max-time",
        "5",
        "-X",
        "POST",
        KIMI_OAUTH_TOKEN_URL,
        "--data-urlencode",
        &format!("client_id={KIMI_OAUTH_CLIENT_ID}"),
        "--data-urlencode",
        "grant_type=refresh_token",
        "--data-urlencode",
        &format!("refresh_token={refresh_token}"),
    ])?;

    let access_token = refreshed
        .get("access_token")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())?;
    let rotated_refresh_token = refreshed
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .unwrap_or(&refresh_token)
        .to_string();

    let mut updated_credentials = current_credentials;
    let object = updated_credentials.as_object_mut()?;
    object.insert(
        "access_token".into(),
        serde_json::Value::String(access_token.clone()),
    );
    object.insert(
        "refresh_token".into(),
        serde_json::Value::String(rotated_refresh_token),
    );
    if let Some(expires_in) = refreshed.get("expires_in").and_then(|value| value.as_f64()) {
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs_f64() + expires_in)?;
        object.insert("expires_at".into(), serde_json::json!(expires_at));
    }
    if let Some(scope) = refreshed.get("scope").and_then(|value| value.as_str()) {
        object.insert("scope".into(), serde_json::Value::String(scope.to_string()));
    }
    if let Some(token_type) = refreshed.get("token_type").and_then(|value| value.as_str()) {
        object.insert(
            "token_type".into(),
            serde_json::Value::String(token_type.to_string()),
        );
    }

    let _ = write_kimi_oauth_credentials(&updated_credentials);
    Some(access_token)
}

fn read_kimi_base_url() -> String {
    let raw = std::fs::read_to_string(Tool::Kimi.config_path()).ok();
    raw.and_then(|raw| raw.parse::<toml::Value>().ok())
        .and_then(|parsed| {
            parsed
                .get("providers")
                .and_then(|value| value.get("managed:kimi-code"))
                .and_then(|value| value.get("base_url"))
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        })
        .unwrap_or_else(|| KIMI_DEFAULT_BASE_URL.to_string())
}

fn fetch_kimi_usage_with_access_token(access_token: &str) -> Option<serde_json::Value> {
    let usage_url = format!("{}/usages", read_kimi_base_url().trim_end_matches('/'));
    run_curl(&[
        "-sS",
        "--max-time",
        "3",
        "-H",
        &format!("Authorization: Bearer {access_token}"),
        usage_url.as_str(),
    ])
}

fn fetch_kimi_usage_json() -> Option<serde_json::Value> {
    let cache_path = kimi_usage_cache_path();
    if cache_path.exists() && usage_cache_is_fresh(&cache_path) {
        if let Some(cached) = load_usage_json_from_cache(&cache_path, "kimi usage cache") {
            if cached.get("usage").is_some() {
                return Some(cached);
            }
        }
    }

    let mut access_token = read_kimi_access_token()?;
    let mut refreshed = false;
    if kimi_access_token_needs_refresh() {
        access_token = refresh_kimi_access_token()?;
        refreshed = true;
    }

    let live = if let Some(value) = fetch_kimi_usage_with_access_token(&access_token)
        .and_then(|value| value.get("usage").is_some().then_some(value))
    {
        Some(value)
    } else if refreshed {
        None
    } else {
        let retried = {
            let refreshed = refresh_kimi_access_token()?;
            fetch_kimi_usage_with_access_token(&refreshed)
        };
        retried.and_then(|value| value.get("usage").is_some().then_some(value))
    };

    if let Some(value) = live {
        write_json_cache(&cache_path, &value);
        return Some(value);
    }

    load_usage_json_from_cache(&cache_path, "kimi usage cache")
}

fn kimi_limit_label(item: &serde_json::Value, idx: usize) -> String {
    let duration = item
        .get("window")
        .and_then(|value| value.get("duration"))
        .and_then(|value| value.as_i64())
        .or_else(|| item.get("duration").and_then(|value| value.as_i64()));
    let time_unit = item
        .get("window")
        .and_then(|value| value.get("timeUnit"))
        .and_then(|value| value.as_str())
        .or_else(|| item.get("timeUnit").and_then(|value| value.as_str()))
        .unwrap_or("");

    match (duration, time_unit) {
        (Some(duration), unit) if unit.contains("MINUTE") => {
            if duration >= 60 && duration % 60 == 0 {
                format!("{}h", duration / 60)
            } else {
                format!("{duration}m")
            }
        }
        (Some(duration), unit) if unit.contains("HOUR") => format!("{duration}h"),
        (Some(duration), unit) if unit.contains("DAY") => format!("{duration}d"),
        _ => format!("Limit #{}", idx + 1),
    }
}

fn format_kimi_usage_value(label: &str, detail: &serde_json::Value) -> Option<String> {
    let limit = detail
        .get("limit")
        .and_then(|value| value.as_str().and_then(|value| value.parse::<f64>().ok()))
        .or_else(|| detail.get("limit").and_then(|value| value.as_f64()))?;
    let used = detail
        .get("used")
        .and_then(|value| value.as_str().and_then(|value| value.parse::<f64>().ok()))
        .or_else(|| detail.get("used").and_then(|value| value.as_f64()))
        .or_else(|| {
            let remaining = detail
                .get("remaining")
                .and_then(|value| value.as_str().and_then(|value| value.parse::<f64>().ok()))
                .or_else(|| detail.get("remaining").and_then(|value| value.as_f64()))?;
            Some(limit - remaining)
        })?;
    let reset_in = detail
        .get("resetTime")
        .or_else(|| detail.get("reset_at"))
        .or_else(|| detail.get("resetAt"))
        .and_then(|value| value.as_str())
        .and_then(format_reset_time_from_iso);
    let used_percent = if limit > 0.0 {
        (used / limit) * 100.0
    } else {
        100.0
    };
    Some(format_remaining_window_value(label, used_percent, reset_in))
}

fn parse_kimi_usage_snapshot(data: &serde_json::Value) -> Option<KimiUsageSnapshot> {
    let weekly_value = data
        .get("usage")
        .and_then(|usage| format_kimi_usage_value("7d", usage));
    let current_value = data
        .get("limits")
        .and_then(|value| value.as_array())
        .and_then(|limits| {
            limits.iter().enumerate().find_map(|(idx, item)| {
                let label = kimi_limit_label(item, idx);
                let detail = item.get("detail").unwrap_or(item);
                if label == "5h" || label == "300m" {
                    format_kimi_usage_value("5h", detail)
                } else {
                    format_kimi_usage_value(&label, detail)
                }
            })
        });

    let summary = match (current_value, weekly_value) {
        (Some(current), Some(weekly)) => Some(format!("{current}  |  {weekly}")),
        (Some(current), None) => Some(current),
        (None, Some(weekly)) => Some(weekly),
        (None, None) => None,
    };

    summary.as_ref()?;
    Some(KimiUsageSnapshot { summary })
}

fn load_kimi_usage_snapshot() -> Option<KimiUsageSnapshot> {
    let data = fetch_kimi_usage_json()?;
    parse_kimi_usage_snapshot(&data)
}

fn read_gh_auth_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "gh auth token probe failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    String::from_utf8(output.stdout)
        .map_err(|err| log::debug!("gh auth token probe returned non-utf8 stdout: {}", err))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn fetch_copilot_usage_json() -> Option<serde_json::Value> {
    let cache_path = copilot_usage_cache_path();
    fetch_usage_json_with_cache(cache_path, "copilot usage cache", || {
        let token = read_gh_auth_token()?;
        run_curl(&[
            "-sS",
            "--max-time",
            "3",
            "-H",
            &format!("Authorization: token {token}"),
            "-H",
            "Accept: application/json",
            "-H",
            "Editor-Version: vscode/1.96.2",
            "-H",
            "Editor-Plugin-Version: copilot-chat/0.26.7",
            "-H",
            "User-Agent: GitHubCopilotChat/0.26.7",
            "-H",
            "X-Github-Api-Version: 2025-04-01",
            "https://api.github.com/copilot_internal/user",
        ])
    })
}

fn parse_copilot_usage_snapshot(data: &serde_json::Value) -> Option<CopilotUsageSnapshot> {
    let premium = data
        .get("quota_snapshots")
        .and_then(|value| value.get("premium_interactions"))?;
    let remaining = premium
        .get("remaining")
        .and_then(|value| value.as_f64())
        .or_else(|| {
            premium
                .get("remaining")
                .and_then(|value| value.as_str()?.parse::<f64>().ok())
        })
        .or_else(|| {
            premium
                .get("quota_remaining")
                .and_then(|value| value.as_f64())
        })?;
    let reset_in = data
        .get("quota_reset_date_utc")
        .and_then(|value| value.as_str())
        .and_then(format_reset_time_from_iso);

    let mut summary = format!(
        "{} left this month",
        format_remaining_count_value(remaining)
    );
    if let Some(reset_in) = reset_in {
        summary.push_str(" · reset ");
        summary.push_str(&reset_in);
    }

    Some(CopilotUsageSnapshot {
        summary: Some(summary),
    })
}

fn load_copilot_usage_snapshot() -> Option<CopilotUsageSnapshot> {
    let data = fetch_copilot_usage_json()?;
    parse_copilot_usage_snapshot(&data)
}

fn gemini_quota_summary(data: &serde_json::Value) -> Option<String> {
    let auth_type = data
        .get("security")
        .and_then(|security| security.get("auth"))
        .and_then(|auth| auth.get("selectedType"))
        .and_then(|value| value.as_str())?;

    // Gemini CLI doesn't expose a stable local "remaining quota" endpoint yet,
    // so these are approximate plan limits used for quick at-a-glance guidance.
    match auth_type {
        "oauth-personal" => Some("Quota 1000/day · 60/min".into()),
        "gemini-api-key" | "api-key" => Some("Quota 250/day · 10/min".into()),
        "workspace-standard" => Some("Quota 1500/day · 120/min".into()),
        "workspace-enterprise" => Some("Quota 2000/day · 120/min".into()),
        _ if auth_type.contains("workspace") => Some("Quota via workspace plan".into()),
        _ if auth_type.contains("vertex") => Some("Quota via Vertex AI".into()),
        _ => None,
    }
}

fn parse_json_with_debug(raw: &str, tool_label: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|e| {
        log::warn!("failed to parse {} config json: {}", tool_label, e);
        push_ui_error(format!("{tool_label} config is invalid JSON"));
        serde_json::Value::Null
    })
}

fn parse_json_or_jsonc_with_debug(raw: &str, tool_label: &str) -> serde_json::Value {
    parse_json_or_jsonc(raw).unwrap_or_else(|e| {
        log::warn!("failed to parse {} config json/jsonc: {}", tool_label, e);
        push_ui_error(format!("{tool_label} config is malformed"));
        serde_json::Value::Null
    })
}

fn read_text_with_debug(path: &Path, context: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            log::debug!("{context}: {} not found", path.display());
            None
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidData
            ) =>
        {
            log::warn!("{context}: failed to read {}: {}", path.display(), e);
            push_ui_error(format!(
                "{context}: cannot read {}. Check file permission or encoding.",
                path.display()
            ));
            None
        }
        Err(e) => {
            log::debug!("{context}: failed to read {}: {}", path.display(), e);
            None
        }
    }
}

fn parse_json_value_with_debug(raw: &str, context: &str) -> Option<serde_json::Value> {
    serde_json::from_str(raw)
        .map_err(|e| {
            log::warn!("{context}: failed to parse json: {}", e);
            push_ui_error(format!("{context}: invalid JSON format"));
        })
        .ok()
}

fn read_json_file_with_debug(path: &Path, context: &str) -> Option<serde_json::Value> {
    let raw = read_text_with_debug(path, context)?;
    parse_json_value_with_debug(&raw, context)
}

fn decode_jwt_payload_with_debug(token: &str, context: &str) -> Option<serde_json::Value> {
    // JWT format: header.payload.signature
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        log::debug!("{context}: invalid jwt format");
        push_ui_error(format!("{context}: invalid token format"));
        return None;
    }

    let mut payload = parts[1].to_string();
    while payload.len() % 4 != 0 {
        payload.push('=');
    }

    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(&payload)
        .map_err(|e| {
            log::debug!("{context}: failed to decode jwt payload: {}", e);
            push_ui_error(format!("{context}: token payload decode failed"));
        })
        .ok()?;
    serde_json::from_slice(&decoded)
        .map_err(|e| {
            log::debug!("{context}: failed to parse jwt payload json: {}", e);
            push_ui_error(format!("{context}: token payload JSON is invalid"));
        })
        .ok()
}

fn json_str(val: &serde_json::Value, key: &str) -> String {
    val.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn json_nested_or_top_str(
    val: &serde_json::Value,
    parent_key: &str,
    nested_key: &str,
    legacy_top_key: &str,
) -> String {
    val.get(parent_key)
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(nested_key))
        .and_then(|v| v.as_str())
        .or_else(|| val.get(legacy_top_key).and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

fn string_options(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

fn mask_key(val: &str) -> String {
    if val.is_empty() {
        return "—".into();
    }
    if val.len() <= 12 {
        return "****".into();
    }
    // Show first 12 chars and last 4 chars
    format!("{}...{}", &val[..12], &val[val.len() - 4..])
}

fn normalize_custom_header(value: &str) -> Option<String> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    let (name, header_value) = raw.split_once(':')?;
    let name = name.trim();
    let header_value = header_value.trim();
    if name.is_empty() || header_value.is_empty() {
        return None;
    }
    if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("content-type") {
        return None;
    }
    Some(format!("{name}: {header_value}"))
}

fn normalize_custom_headers(values: Vec<String>) -> Vec<String> {
    let mut dedup = HashSet::new();
    values
        .into_iter()
        .filter_map(|item| normalize_custom_header(&item))
        .filter(|header| {
            let key = header
                .split_once(':')
                .map(|(name, _)| name)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            dedup.insert(key)
        })
        .collect()
}

fn parse_custom_headers_toml(value: Option<&toml::Value>) -> Vec<String> {
    match value {
        Some(toml::Value::Array(items)) => normalize_custom_headers(
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect(),
        ),
        Some(toml::Value::String(raw)) => normalize_custom_headers(
            raw.split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect(),
        ),
        _ => vec![],
    }
}

/// Configuration for the Kaku built-in AI assistant.
///
/// This struct holds the configuration for Kaku's AI-powered command analysis
/// feature. It ensures that model and base_url always have valid values
/// by falling back to defaults when empty strings are provided.
#[derive(Debug, Clone)]
struct KakuAssistantConfig {
    /// Whether the AI assistant is enabled
    enabled: bool,
    /// API key for the AI service (may be empty if not configured)
    api_key: String,
    /// Model identifier (never empty, falls back to default)
    model: String,
    /// Base URL for the API endpoint (never empty, falls back to default)
    base_url: String,
    /// Optional extra request headers as `Name: Value`
    custom_headers: Vec<String>,
}

impl KakuAssistantConfig {
    /// Creates a new configuration with the given values.
    ///
    /// # Arguments
    /// * `enabled` - Whether the assistant is enabled
    /// * `api_key` - API key (empty string if not set)
    /// * `model` - Model identifier (empty strings will be replaced with default)
    /// * `base_url` - Base URL (empty strings will be replaced with default)
    fn new(
        enabled: bool,
        api_key: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let base_url = base_url.into();
        Self {
            enabled,
            api_key: api_key.into(),
            model: if model.trim().is_empty() {
                assistant_config::DEFAULT_MODEL.to_string()
            } else {
                model
            },
            base_url: if base_url.trim().is_empty() {
                assistant_config::DEFAULT_BASE_URL.to_string()
            } else {
                base_url
            },
            custom_headers: vec![],
        }
    }

    fn with_custom_headers(mut self, custom_headers: Vec<String>) -> Self {
        self.custom_headers = normalize_custom_headers(custom_headers);
        self
    }

    /// Returns whether the assistant is enabled.
    fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the API key (may be empty).
    fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Returns the model identifier (never empty).
    fn model(&self) -> &str {
        &self.model
    }

    /// Returns the base URL (never empty).
    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn custom_headers(&self) -> &[String] {
        &self.custom_headers
    }
}

impl Default for KakuAssistantConfig {
    fn default() -> Self {
        Self::new(true, String::new(), String::new(), String::new())
    }
}

/// Parses a KakuAssistantConfig from TOML content.
///
/// This function gracefully handles malformed TOML by using default values
/// for any missing or invalid fields.
fn parse_kaku_assistant_config(raw: &str) -> KakuAssistantConfig {
    let parsed = raw.parse::<toml::Value>().unwrap_or_else(|e| {
        log::warn!("failed to parse assistant.toml: {}", e);
        push_ui_error("Kaku Assistant config TOML is malformed");
        toml::Value::Table(Default::default())
    });

    let enabled = parsed
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let api_key = parsed.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
    let model = parsed.get("model").and_then(|v| v.as_str()).unwrap_or("");
    let base_url = parsed
        .get("base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let custom_headers = parse_custom_headers_toml(parsed.get("custom_headers"));

    KakuAssistantConfig::new(enabled, api_key, model, base_url).with_custom_headers(custom_headers)
}

fn get_kaku_assistant_api_key() -> Option<String> {
    let path = assistant_config::ensure_assistant_toml_exists()
        .map_err(|e| log::debug!("assistant config not available: {}", e))
        .ok()?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| log::debug!("failed to read assistant config: {}", e))
        .ok()?;
    let cfg = parse_kaku_assistant_config(&raw);
    if cfg.api_key().trim().is_empty() {
        log::debug!("assistant config has no api_key set");
        None
    } else {
        Some(cfg.api_key().to_string())
    }
}

fn extract_kaku_assistant_fields(raw: &str) -> Vec<FieldEntry> {
    let cfg = parse_kaku_assistant_config(raw);
    vec![
        FieldEntry {
            key: "Model".into(),
            value: cfg.model().to_string(),
            options: vec![],
            editable: true,
        },
        FieldEntry {
            key: "Base URL".into(),
            value: cfg.base_url().to_string(),
            options: vec![],
            editable: true,
        },
        FieldEntry {
            key: "API Key".into(),
            value: mask_key(cfg.api_key()),
            options: vec![],
            editable: true,
        },
    ]
}

fn render_toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn write_kaku_assistant_config(path: &Path, cfg: &KakuAssistantConfig) -> anyhow::Result<()> {
    let mut out = String::new();
    out.push_str("# Kaku Assistant configuration\n");
    out.push_str(
        "# enabled: true enables command analysis suggestions; false disables requests.\n",
    );
    out.push_str("# api_key: provider API key, example: \"sk-xxxx\".\n");
    out.push_str("# model: model id, example: \"DeepSeek-V3.2\" or \"gpt-5-mini\".\n");
    out.push_str("# base_url: chat-completions API root URL.\n");
    out.push_str(
        "# custom_headers: optional extra HTTP headers for enterprise proxies or API gateways.\n",
    );
    out.push_str("#                 format: [\"Header-Name: value\", \"Another-Header: value\"]\n");
    out.push_str("#                 note: Authorization and Content-Type are reserved and cannot be overridden.\n\n");
    out.push_str(if cfg.is_enabled() {
        "enabled = true\n"
    } else {
        "enabled = false\n"
    });
    if cfg.api_key().trim().is_empty() {
        out.push_str("# api_key = \"<your_api_key>\"\n");
    } else {
        out.push_str(&format!(
            "api_key = {}\n",
            render_toml_string(cfg.api_key().trim())
        ));
    }
    out.push_str(&format!(
        "model = {}\n",
        render_toml_string(cfg.model().trim())
    ));
    out.push_str(&format!(
        "base_url = {}\n",
        render_toml_string(cfg.base_url().trim())
    ));
    if cfg.custom_headers().is_empty() {
        out.push_str("# custom_headers = [\"X-Customer-ID: your-customer-id\"]\n");
    } else {
        let arr = toml::Value::Array(
            cfg.custom_headers()
                .iter()
                .map(|item| toml::Value::String(item.clone()))
                .collect(),
        );
        out.push_str(&format!("custom_headers = {}\n", arr));
    }
    write_atomic(path, out.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn save_kaku_assistant_field(field_key: &str, new_val: &str) -> anyhow::Result<()> {
    let path = assistant_config::ensure_assistant_toml_exists()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            log::debug!(
                "assistant config missing when saving; recreating {}",
                path.display()
            );
            String::new()
        }
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::InvalidData
            ) =>
        {
            log::warn!("failed to read assistant config {}: {}", path.display(), e);
            push_ui_error(format!(
                "cannot read {}. Check file permission or encoding.",
                path.display()
            ));
            String::new()
        }
        Err(e) => {
            log::debug!("failed to read assistant config {}: {}", path.display(), e);
            String::new()
        }
    };
    let cfg = parse_kaku_assistant_config(&raw);

    // Build updated config based on which field changed
    let updated = match field_key {
        "Enabled" => {
            let enabled = matches!(new_val.trim(), "On" | "on" | "true" | "1");
            KakuAssistantConfig::new(enabled, cfg.api_key(), cfg.model(), cfg.base_url())
                .with_custom_headers(cfg.custom_headers().to_vec())
        }
        "Model" => {
            let model = if new_val.trim().is_empty() || new_val == "—" {
                assistant_config::DEFAULT_MODEL
            } else {
                new_val.trim()
            };
            KakuAssistantConfig::new(cfg.is_enabled(), cfg.api_key(), model, cfg.base_url())
                .with_custom_headers(cfg.custom_headers().to_vec())
        }
        "Base URL" => {
            let base_url = if new_val.trim().is_empty() || new_val == "—" {
                assistant_config::DEFAULT_BASE_URL
            } else {
                new_val.trim()
            };
            KakuAssistantConfig::new(cfg.is_enabled(), cfg.api_key(), cfg.model(), base_url)
                .with_custom_headers(cfg.custom_headers().to_vec())
        }
        "API Key" => KakuAssistantConfig::new(
            cfg.is_enabled(),
            new_val.trim(),
            cfg.model(),
            cfg.base_url(),
        )
        .with_custom_headers(cfg.custom_headers().to_vec()),
        _ => return Ok(()),
    };

    write_kaku_assistant_config(&path, &updated)
}

/// Get Gemini account email from google_accounts.json
fn get_gemini_account() -> Option<String> {
    let path = config::HOME_DIR
        .join(".gemini")
        .join("google_accounts.json");

    let parsed = read_json_file_with_debug(&path, "gemini account")?;

    // Extract "active" field
    parsed.get("active")?.as_str().map(|s| s.to_string())
}

/// Get Codex account email from JWT token in auth.json
fn get_codex_account() -> Option<String> {
    let auth_path = config::HOME_DIR.join(".codex").join("auth.json");
    let auth_json = read_json_file_with_debug(&auth_path, "codex account")?;

    // Extract access_token from tokens object
    let token = auth_json.get("tokens")?.get("access_token")?.as_str()?;

    let jwt_data = decode_jwt_payload_with_debug(token, "codex account")?;

    // OpenAI JWT payload contains email in custom claim
    jwt_data
        .get("https://api.openai.com/profile")?
        .get("email")?
        .as_str()
        .map(|s| s.to_string())
}

/// Get GitHub Copilot username from gh CLI
fn get_copilot_account() -> Option<String> {
    let output = match std::process::Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("gh account probe failed to launch: {}", e);
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "gh account probe failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    String::from_utf8(output.stdout)
        .map_err(|e| log::debug!("gh account probe returned non-utf8 stdout: {}", e))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Get Claude Code account email from claude auth status
fn get_claude_code_account() -> Option<String> {
    let output = match std::process::Command::new("claude")
        .args(["auth", "status"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("claude auth status probe failed to launch: {}", e);
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "claude auth status probe failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    let json_str = String::from_utf8(output.stdout)
        .map_err(|e| log::debug!("claude auth status probe returned non-utf8 stdout: {}", e))
        .ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| log::debug!("failed to parse claude auth status json: {}", e))
        .ok()?;

    // Extract email from auth status JSON
    parsed.get("email")?.as_str().map(|s| s.to_string())
}

fn kimi_credentials_path() -> PathBuf {
    config::HOME_DIR
        .join(".kimi")
        .join("credentials")
        .join("kimi-code.json")
}

fn kimi_config_model_options(parsed: &toml::Value) -> Vec<String> {
    parsed
        .get("models")
        .and_then(|value| value.as_table())
        .map(|table| table.keys().cloned().collect::<Vec<_>>())
        .filter(|models| !models.is_empty())
        .unwrap_or_else(|| vec!["kimi-code/kimi-for-coding".into()])
}

fn read_kimi_auth_status() -> String {
    let Some(auth) = read_json_file_with_debug(&kimi_credentials_path(), "kimi credentials") else {
        return "✗ not signed in".into();
    };

    let has_token = auth
        .get("access_token")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.is_empty());
    let expires_at = auth.get("expires_at").and_then(|value| value.as_f64());
    let has_refresh_token = auth
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.is_empty());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);

    if has_refresh_token || (has_token && expires_at.is_some_and(|expires_at| expires_at > now)) {
        "✓ oauth".into()
    } else {
        "✗ login required".into()
    }
}

/// Format auth status, with account fallback to auth method
fn format_auth_status(account: Option<String>, fallback_method: &str) -> String {
    match account {
        Some(acc) if !acc.is_empty() => format!("✓ {}", acc),
        _ => format!("✓ {}", fallback_method),
    }
}

/// Fetch models.dev data, cached to ~/.cache/kaku/models.json.
/// No TTL — use `r` key in TUI to force refresh.
fn load_models_dev_json() -> Option<serde_json::Value> {
    let cache_dir = config::HOME_DIR.join(".cache").join("kaku");
    let cache_path = cache_dir.join("models.json");

    // Use cache if exists
    if let Some(raw) = read_text_with_debug(&cache_path, "models.dev cache") {
        if let Some(v) = parse_json_value_with_debug(&raw, "models.dev cache") {
            return Some(v);
        }
    }

    // Fetch from API via curl (macOS built-in)
    fetch_models_dev_json()
}

/// Force fetch from models.dev and update cache.
fn fetch_models_dev_json() -> Option<serde_json::Value> {
    let cache_dir = config::HOME_DIR.join(".cache").join("kaku");
    let cache_path = cache_dir.join("models.json");

    let output = match std::process::Command::new("curl")
        .args(["-sS", "--max-time", "10", "https://models.dev/api.json"])
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            log::debug!("models.dev fetch failed to launch curl: {}", e);
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "models.dev fetch failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return None;
    }

    let raw = String::from_utf8(output.stdout)
        .map_err(|e| log::debug!("models.dev fetch returned non-utf8 stdout: {}", e))
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| log::debug!("failed to parse models.dev json: {}", e))
        .ok()?;
    if let Err(e) = config::create_user_owned_dirs(&cache_dir) {
        log::debug!("Failed to create cache directory: {}", e);
    }
    if let Err(e) = std::fs::write(&cache_path, &raw) {
        log::debug!("Failed to write models cache: {}", e);
    }
    Some(v)
}

/// Read model IDs from models.dev for a given provider.
/// Returns latest models sorted by release_date (newest first), deduped to
/// only keep the short alias (e.g. "claude-sonnet-4-5") and skip dated
/// variants (e.g. "claude-sonnet-4-5-20250929").
fn read_models_dev(provider_id: &str) -> Vec<String> {
    let parsed = match load_models_dev_json() {
        Some(v) => v,
        None => return Vec::new(),
    };
    let models = match parsed
        .get(provider_id)
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_object())
    {
        Some(m) => m,
        None => return Vec::new(),
    };

    // Collect (id, release_date) pairs, skip embedding/tts/image-only models
    let mut items: Vec<(&str, &str)> = models
        .iter()
        .filter_map(|(id, m)| {
            // Skip non-chat models
            let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.contains("Embedding")
                || name.contains("TTS")
                || name.contains("Image")
                || name.contains("Live")
            {
                return None;
            }
            // Skip dated variants (e.g. "claude-opus-4-5-20251101", "gemini-2.5-flash-preview-06-17")
            if id.chars().rev().take(8).all(|c| c.is_ascii_digit()) {
                return None;
            }
            // Skip dated preview variants with "xx-xx" suffix (e.g. "09-2025", "06-17")
            // Require both segments ≥ 2 chars to avoid filtering version numbers like "4-5"
            if let Some(last_dash) = id.rfind('-') {
                let suffix = &id[last_dash + 1..];
                if let Some(second_dash) = id[..last_dash].rfind('-') {
                    let prev = &id[second_dash + 1..last_dash];
                    if prev.len() >= 2
                        && prev.len() <= 4
                        && suffix.len() >= 2
                        && suffix.len() <= 4
                        && prev.chars().all(|c| c.is_ascii_digit())
                        && suffix.chars().all(|c| c.is_ascii_digit())
                    {
                        return None;
                    }
                }
            }
            // Skip "-latest" aliases (e.g. "gemini-flash-latest")
            if id.ends_with("-latest") {
                return None;
            }
            let rd = m.get("release_date").and_then(|v| v.as_str()).unwrap_or("");
            Some((id.as_str(), rd))
        })
        .collect();

    items.sort_by(|a, b| b.1.cmp(a.1));
    items
        .into_iter()
        .take(4)
        .map(|(id, _)| id.to_string())
        .collect()
}

fn extract_claude_code_fields(val: &serde_json::Value) -> Vec<FieldEntry> {
    let model = json_str(val, "model");

    let model_options = read_models_dev("anthropic");

    let display_value = if model.is_empty() {
        // Show the latest model name as default hint
        model_options
            .first()
            .cloned()
            .unwrap_or_else(|| "default".into())
    } else {
        model
    };

    let mut fields = vec![FieldEntry {
        key: "Model".into(),
        value: display_value,
        options: model_options,
        ..Default::default()
    }];

    // Auth status: Claude Code uses OAuth; statsig dir indicates active session
    let statsig_dir = config::HOME_DIR.join(".claude").join("statsig");
    if statsig_dir.exists() {
        let account = get_claude_code_account();
        fields.push(FieldEntry {
            key: "Auth".into(),
            value: format_auth_status(account, "oauth"),
            options: vec![],
            editable: false,
        });
    }

    // Show env-based provider config if present (e.g. OpenRouter, Pipe AI, Kimi)
    if let Some(env) = val.get("env").and_then(|e| e.as_object()) {
        let base_url = env
            .get("ANTHROPIC_BASE_URL")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let auth_token = env
            .get("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| env.get("ANTHROPIC_API_KEY"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !base_url.is_empty() {
            fields.push(FieldEntry {
                key: "Base URL".into(),
                value: base_url,
                options: vec![],
                ..Default::default()
            });
        }
        if !auth_token.is_empty() {
            fields.push(FieldEntry {
                key: "API Key".into(),
                value: mask_key(&auth_token),
                options: vec![],
                ..Default::default()
            });
        }
    }

    fields
}

fn extract_codex_fields(raw: &str) -> Vec<FieldEntry> {
    let mut fields = Vec::new();
    let mut has_model = false;

    // Read available models from Codex model cache
    let model_options = read_codex_model_options();

    // Parse TOML manually for the fields we care about
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim().trim_matches('"');
            let val = val.trim().trim_matches('"');
            match key {
                "model" => {
                    has_model = true;
                    fields.push(FieldEntry {
                        key: "Model".into(),
                        value: val.to_string(),
                        options: model_options.clone(),
                        ..Default::default()
                    });
                }
                _ => {}
            }
        }
    }

    if !has_model {
        let default_model = model_options
            .first()
            .cloned()
            .unwrap_or_else(|| "default".into());
        fields.insert(
            0,
            FieldEntry {
                key: "Model".into(),
                value: default_model,
                options: model_options.clone(),
                ..Default::default()
            },
        );
    }

    // Check auth status from auth.json
    let auth_path = config::HOME_DIR.join(".codex").join("auth.json");
    if let Some(auth) = read_json_file_with_debug(&auth_path, "codex auth status") {
        let auth_mode = auth.get("auth_mode").and_then(|v| v.as_str()).unwrap_or("");
        if !auth_mode.is_empty() {
            let account = get_codex_account();
            fields.push(FieldEntry {
                key: "Auth".into(),
                value: format_auth_status(account, auth_mode),
                options: vec![],
                editable: false,
            });
        }
    }

    fields
}

fn extract_kimi_fields(raw: &str) -> Vec<FieldEntry> {
    let parsed = raw.parse::<toml::Value>().ok();
    let model_options = parsed
        .as_ref()
        .map(kimi_config_model_options)
        .unwrap_or_else(|| vec!["kimi-code/kimi-for-coding".into()]);
    let model = parsed
        .as_ref()
        .and_then(|parsed| parsed.get("default_model"))
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();

    let display_value = if model.is_empty() {
        model_options
            .first()
            .cloned()
            .unwrap_or_else(|| "kimi-code/kimi-for-coding".into())
    } else {
        model
    };

    vec![
        FieldEntry {
            key: "Model".into(),
            value: display_value,
            options: model_options,
            ..Default::default()
        },
        FieldEntry {
            key: "Auth".into(),
            value: read_kimi_auth_status(),
            options: vec![],
            editable: false,
        },
    ]
}

/// Read model slugs from Codex's own cache, or from models.dev.
fn read_codex_model_options() -> Vec<String> {
    let cache_path = config::HOME_DIR.join(".codex").join("models_cache.json");
    if let Some(parsed) = read_json_file_with_debug(&cache_path, "codex model cache") {
        let mut models: Vec<(String, usize)> = parsed
            .get("models")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|m| {
                        m.get("visibility")
                            .and_then(|v| v.as_str())
                            .map(|v| v == "list")
                            .unwrap_or(false)
                    })
                    .filter_map(|m| {
                        let slug = m.get("slug").and_then(|v| v.as_str())?;
                        let priority =
                            m.get("priority").and_then(|v| v.as_u64()).unwrap_or(999) as usize;
                        Some((slug.to_string(), priority))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !models.is_empty() {
            models.sort_by_key(|(_, p)| *p);
            return models.into_iter().map(|(s, _)| s).collect();
        }
    }

    read_models_dev("openai")
}

fn extract_gemini_fields(val: &serde_json::Value) -> Vec<FieldEntry> {
    let mut fields = Vec::new();

    let model = val
        .get("model")
        .and_then(|m| {
            m.get("name")
                .and_then(|n| n.as_str())
                .or_else(|| m.as_str())
        })
        .unwrap_or("")
        .to_string();
    let model_options = read_models_dev("google");

    let display_value = if model.is_empty() {
        model_options
            .first()
            .cloned()
            .unwrap_or_else(|| "default".into())
    } else {
        model
    };

    fields.push(FieldEntry {
        key: "Model".into(),
        value: display_value,
        options: model_options,
        ..Default::default()
    });

    let auth_type = val
        .get("security")
        .and_then(|s| s.get("auth"))
        .and_then(|a| a.get("selectedType"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !auth_type.is_empty() {
        let account = get_gemini_account();
        fields.push(FieldEntry {
            key: "Auth".into(),
            value: format_auth_status(account, auth_type),
            options: vec![],
            editable: false,
        });
    }

    fields
}

/// Read model choices from `copilot --help` output, fallback to models.dev.
fn read_copilot_model_options() -> Vec<String> {
    let output = match std::process::Command::new("copilot").arg("--help").output() {
        Ok(output) => output,
        Err(e) => {
            log::debug!("copilot --help probe failed to launch: {}", e);
            return read_models_dev("anthropic");
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::debug!(
            "copilot --help probe failed with status {}: {}",
            output.status,
            stderr.trim()
        );
        return read_models_dev("anthropic");
    }

    let text = String::from_utf8_lossy(&output.stdout);
    // Find "--model" first, then parse the choices after it
    if let Some(model_pos) = text.find("--model") {
        let after_model = &text[model_pos..];
        if let Some(choices_pos) = after_model.find("choices:") {
            let rest = &after_model[choices_pos + "choices:".len()..];
            if let Some(end) = rest.find(')') {
                let choices_str = &rest[..end];
                let models: Vec<String> = choices_str
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !models.is_empty() {
                    return models;
                }
            }
        }
    }

    log::debug!("copilot --help output did not expose model choices");
    read_models_dev("anthropic")
}

fn extract_copilot_fields(val: &serde_json::Value) -> Vec<FieldEntry> {
    let model = json_str(val, "model");

    let model_options = read_copilot_model_options();

    let mut fields = vec![FieldEntry {
        key: "Model".into(),
        value: if model.is_empty() {
            "default".into()
        } else {
            model
        },
        options: model_options,
        ..Default::default()
    }];

    // Copilot authenticates via GitHub OAuth; session files indicate auth
    let session_dir = config::HOME_DIR.join(".copilot").join("session-state");
    if session_dir.exists() {
        let account = get_copilot_account();
        fields.push(FieldEntry {
            key: "Auth".into(),
            value: format_auth_status(account, "github"),
            options: vec![],
            editable: false,
        });
    }

    fields
}

fn extract_factory_droid_fields(val: &serde_json::Value) -> Vec<FieldEntry> {
    let session_defaults = val
        .get("sessionDefaultSettings")
        .and_then(|v| v.as_object());
    let model = json_nested_or_top_str(val, "sessionDefaultSettings", "model", "model");
    let reasoning = json_nested_or_top_str(
        val,
        "sessionDefaultSettings",
        "reasoningEffort",
        "reasoningEffort",
    );
    let autonomy = session_defaults
        .and_then(|s| s.get("autonomyMode").or_else(|| s.get("autonomyLevel")))
        .and_then(|v| v.as_str())
        .or_else(|| {
            val.get("autonomyMode")
                .or_else(|| val.get("autonomyLevel"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string();

    vec![
        FieldEntry {
            key: "Model".into(),
            value: if model.is_empty() {
                FACTORY_DROID_DEFAULT_MODEL.into()
            } else {
                model
            },
            options: vec![],
            editable: true,
        },
        FieldEntry {
            key: "Reasoning Effort".into(),
            value: if reasoning.is_empty() {
                FACTORY_DROID_DEFAULT_REASONING.into()
            } else {
                reasoning
            },
            options: string_options(&FACTORY_DROID_REASONING_OPTIONS),
            editable: true,
        },
        FieldEntry {
            key: "Autonomy Level".into(),
            value: if autonomy.is_empty() {
                FACTORY_DROID_DEFAULT_AUTONOMY.into()
            } else {
                autonomy
            },
            options: string_options(&FACTORY_DROID_AUTONOMY_OPTIONS),
            editable: true,
        },
    ]
}

fn factory_droid_session_key(field_key: &str) -> Option<&'static str> {
    match field_key {
        "Model" => Some("model"),
        "Reasoning Effort" => Some("reasoningEffort"),
        // Normalize writes to `autonomyMode` while still accepting legacy `autonomyLevel`.
        "Autonomy Level" => Some("autonomyMode"),
        _ => None,
    }
}

fn save_factory_droid_field(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    field_key: &str,
    new_val: &str,
) -> anyhow::Result<()> {
    let Some(target_key) = factory_droid_session_key(field_key) else {
        return Ok(());
    };

    let session_defaults = obj
        .entry("sessionDefaultSettings")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("sessionDefaultSettings is not an object")?;

    if new_val == "—" || new_val.is_empty() {
        session_defaults.remove(target_key);
    } else {
        session_defaults.insert(
            target_key.to_string(),
            serde_json::Value::String(new_val.to_string()),
        );
    }

    Ok(())
}

fn extract_openclaw_fields(val: &serde_json::Value) -> Vec<FieldEntry> {
    let agents = val.get("agents").unwrap_or(&serde_json::Value::Null);
    let defaults = agents.get("defaults").unwrap_or(&serde_json::Value::Null);
    let model = defaults.get("model").unwrap_or(&serde_json::Value::Null);

    let primary = model
        .get("primary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Collect all model IDs from all providers for the Primary Model selector
    let mut all_model_ids: Vec<String> = Vec::new();

    let mut provider_fields: Vec<FieldEntry> = Vec::new();

    // Dynamically enumerate all providers
    if let Some(providers) = val
        .get("models")
        .and_then(|m| m.get("providers"))
        .and_then(|p| p.as_object())
    {
        for (name, prov) in providers {
            let url = prov
                .get("baseUrl")
                .or_else(|| prov.get("baseURL"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let key = prov
                .get("apiKey")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Collect models from this provider
            let model_ids: Vec<String> = prov
                .get("models")
                .and_then(|m| m.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                        .map(|id| format!("{}/{}", name, id))
                        .collect()
                })
                .unwrap_or_default();

            // Also check agents.defaults.models for any registered models with this provider
            if let Some(agent_models) = defaults.get("models").and_then(|m| m.as_object()) {
                for model_key in agent_models.keys() {
                    if model_key.starts_with(&format!("{}/", name))
                        && !all_model_ids.contains(model_key)
                    {
                        all_model_ids.push(model_key.clone());
                    }
                }
            }

            for mid in &model_ids {
                if !all_model_ids.contains(mid) {
                    all_model_ids.push(mid.clone());
                }
            }

            let models_display = if model_ids.is_empty() {
                // Show from agent defaults if provider models array is empty
                all_model_ids
                    .iter()
                    .filter(|m| m.starts_with(&format!("{}/", name)))
                    .map(|m| m.split('/').last().unwrap_or(m).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                model_ids
                    .iter()
                    .map(|m| m.split('/').last().unwrap_or(m).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };

            // Provider header
            let api_type = prov
                .get("api")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            provider_fields.push(FieldEntry {
                key: name.clone(),
                value: if api_type.is_empty() {
                    "provider".into()
                } else {
                    api_type
                },
                options: vec![],
                editable: false,
            });

            provider_fields.push(FieldEntry {
                key: format!("{} ▸ Base URL", name),
                value: if url.is_empty() { "—".into() } else { url },
                options: vec![],
                ..Default::default()
            });
            provider_fields.push(FieldEntry {
                key: format!("{} ▸ API Key", name),
                value: mask_key(&key),
                options: vec![],
                ..Default::default()
            });
            if !models_display.is_empty() {
                provider_fields.push(FieldEntry {
                    key: format!("{} ▸ Models", name),
                    value: models_display,
                    options: vec![],
                    editable: false,
                });
            }
        }
    }

    // Fallback: if no providers defined, collect models from agents.defaults.models
    if all_model_ids.is_empty() {
        if let Some(agent_models) = defaults.get("models").and_then(|m| m.as_object()) {
            for model_key in agent_models.keys() {
                all_model_ids.push(model_key.clone());
            }
        }
    }

    let mut fields = vec![FieldEntry {
        key: "Primary Model".into(),
        value: if primary.is_empty() {
            "—".into()
        } else {
            primary
        },
        options: all_model_ids,
        ..Default::default()
    }];

    fields.extend(provider_fields);

    let plugins = val
        .get("plugins")
        .and_then(|p| p.get("entries"))
        .and_then(|e| e.as_object())
        .map(|obj| {
            obj.keys()
                .filter(|k| {
                    obj.get(*k)
                        .and_then(|v| v.get("enabled"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true)
                })
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    fields.push(FieldEntry {
        key: "Plugins".into(),
        value: if plugins.is_empty() {
            "—".into()
        } else {
            plugins
        },
        options: vec![],
        editable: false,
    });

    fields
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    ToolList,
    Editor,
}

#[derive(Clone, PartialEq, Eq)]
enum AppMode {
    Browsing,
    Editing {
        field_idx: usize,
        buffer: String,
        cursor: usize,
    },
    Selecting {
        field_idx: usize,
        options: Vec<String>,
        selected: usize,
    },
}

struct App {
    tools: Vec<ToolState>,
    tool_index: usize,
    field_index: usize,
    assistant_collapsed: bool,
    usage_update_rx: Option<Receiver<UsageSummaryUpdate>>,
    usage_update_tx: Option<Sender<UsageSummaryUpdate>>,
    focus: Focus,
    mode: AppMode,
    status_msg: Option<String>,
    status_expire: Option<Instant>,
    last_error: Option<String>,
    error_expire: Option<Instant>,
    should_quit: bool,
}

fn status_value_for_display(field_key: &str, new_val: &str) -> String {
    if field_key.contains("API Key") {
        return if new_val.trim().is_empty() {
            "—".into()
        } else {
            mask_key(new_val.trim())
        };
    }
    new_val.to_string()
}

impl App {
    fn new() -> Self {
        let show_assistant = kaku_assistant_visible();
        let tools = Self::load_tools(show_assistant);
        let assistant_collapsed = tools
            .iter()
            .find(|tool| tool.tool == Tool::KakuAssistant)
            .map(|tool| should_collapse_kaku_assistant(&tool.fields))
            .unwrap_or(false);
        let first = tools.iter().position(|t| !t.fields.is_empty()).unwrap_or(0);
        let mut app = App {
            tools,
            tool_index: first,
            field_index: 0,
            assistant_collapsed,
            usage_update_rx: None,
            usage_update_tx: None,
            focus: Focus::ToolList,
            mode: AppMode::Browsing,
            status_msg: None,
            status_expire: None,
            last_error: None,
            error_expire: None,
            should_quit: false,
        };
        app.restart_usage_loading();
        app.sync_transient_errors();
        app
    }

    fn load_tools(show_assistant: bool) -> Vec<ToolState> {
        ALL_TOOLS
            .iter()
            .map(|t| ToolState::load_without_remote_usage(*t))
            .filter(|t| {
                (t.tool != Tool::KakuAssistant || show_assistant)
                    && (matches!(t.tool, Tool::KakuAssistant | Tool::Codex) || t.installed)
            })
            .collect()
    }

    fn tool_row_count(&self, tool_index: usize) -> usize {
        let Some(tool) = self.tools.get(tool_index) else {
            return 0;
        };
        if tool.tool == Tool::KakuAssistant {
            return 1 + if self.assistant_collapsed {
                0
            } else {
                tool.fields.len()
            };
        }
        tool.fields.len()
    }

    fn current_tool(&self) -> Option<&ToolState> {
        self.tools.get(self.tool_index)
    }

    fn current_tool_mut(&mut self) -> Option<&mut ToolState> {
        self.tools.get_mut(self.tool_index)
    }

    fn tool_is_collapsed(&self, tool_index: usize) -> bool {
        self.tools
            .get(tool_index)
            .is_some_and(|tool| tool.tool == Tool::KakuAssistant && self.assistant_collapsed)
    }

    fn selected_field_index(&self) -> Option<usize> {
        let tool = self.current_tool()?;
        if tool.fields.is_empty() {
            return None;
        }

        if tool.tool == Tool::KakuAssistant {
            self.field_index.checked_sub(1)
        } else {
            Some(self.field_index)
        }
    }

    fn display_index_for_field(&self, tool: Tool, field_index: usize) -> usize {
        if tool == Tool::KakuAssistant {
            field_index + 1
        } else {
            field_index
        }
    }

    fn toggle_kaku_assistant_collapsed(&mut self) {
        if !self
            .tools
            .iter()
            .any(|tool| tool.tool == Tool::KakuAssistant && !tool.fields.is_empty())
        {
            return;
        }

        self.assistant_collapsed = !self.assistant_collapsed;
        if self
            .current_tool()
            .is_some_and(|tool| tool.tool == Tool::KakuAssistant)
        {
            self.field_index = 0;
        }
        self.set_status(if self.assistant_collapsed {
            "Kaku Assistant hidden"
        } else {
            "Kaku Assistant expanded"
        });
    }

    fn total_rows(&self) -> usize {
        (0..self.tools.len())
            .map(|idx| self.tool_row_count(idx))
            .sum()
    }

    fn rendered_tool_row_count(&self) -> usize {
        self.tools
            .iter()
            .map(|tool| {
                1 + if tool.tool == Tool::KakuAssistant && self.assistant_collapsed {
                    1
                } else {
                    tool.fields.len() + 1
                }
            })
            .sum()
    }

    fn flatten_index(&self) -> usize {
        let mut idx = 0;
        for (ti, _) in self.tools.iter().enumerate() {
            let count = self.tool_row_count(ti);
            if ti == self.tool_index {
                return idx + self.field_index.min(count.saturating_sub(1));
            }
            idx += count;
        }
        idx
    }

    fn is_editing(&self) -> bool {
        matches!(self.mode, AppMode::Editing { .. })
    }

    fn is_selecting(&self) -> bool {
        matches!(self.mode, AppMode::Selecting { .. })
    }

    fn editing_view(&self) -> Option<(usize, &str, usize)> {
        match &self.mode {
            AppMode::Editing {
                field_idx,
                buffer,
                cursor,
            } => Some((*field_idx, buffer.as_str(), *cursor)),
            _ => None,
        }
    }

    fn editing_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match &mut self.mode {
            AppMode::Editing { buffer, cursor, .. } => Some((buffer, cursor)),
            _ => None,
        }
    }

    fn selecting_view(&self) -> Option<(usize, &[String], usize)> {
        match &self.mode {
            AppMode::Selecting {
                field_idx,
                options,
                selected,
            } => Some((*field_idx, options.as_slice(), *selected)),
            _ => None,
        }
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.last_error = Some(message.into());
        self.error_expire = Some(Instant::now() + UI_ERROR_TTL);
    }

    fn set_status(&mut self, message: impl Into<String>) {
        self.status_msg = Some(message.into());
        self.status_expire = Some(Instant::now() + UI_STATUS_TTL);
    }

    fn sync_transient_errors(&mut self) {
        self.drain_usage_updates();

        while let Some(error) = pop_ui_error() {
            self.set_error(error);
        }

        if self
            .status_expire
            .is_some_and(|expire_at| Instant::now() >= expire_at)
        {
            self.status_msg = None;
            self.status_expire = None;
        }

        if self
            .error_expire
            .is_some_and(|expire_at| Instant::now() >= expire_at)
        {
            self.last_error = None;
            self.error_expire = None;
        }
    }

    fn restart_usage_loading(&mut self) {
        let (tx, rx) = mpsc::channel();
        let mut spawned = false;

        for tool in self
            .tools
            .iter()
            .filter(|tool| tool.installed && supports_remote_usage(tool.tool))
        {
            spawned = true;
            let tx = tx.clone();
            let tool_kind = tool.tool;
            std::thread::spawn(move || {
                let _ = tx.send(UsageSummaryUpdate {
                    tool: tool_kind,
                    summary: load_usage_summary(tool_kind),
                });
            });
        }

        if spawned {
            self.usage_update_rx = Some(rx);
            self.usage_update_tx = Some(tx);
        } else {
            self.usage_update_rx = None;
            self.usage_update_tx = None;
        }
    }

    fn schedule_usage_reload(&self, tool: Tool) {
        if !supports_remote_usage(tool) {
            return;
        }
        if !self
            .tools
            .iter()
            .any(|entry| entry.tool == tool && entry.installed)
        {
            return;
        }
        let Some(tx) = self.usage_update_tx.clone() else {
            return;
        };
        std::thread::spawn(move || {
            let _ = tx.send(UsageSummaryUpdate {
                tool,
                summary: load_usage_summary(tool),
            });
        });
    }

    fn drain_usage_updates(&mut self) {
        let Some(rx) = &self.usage_update_rx else {
            return;
        };

        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }

        for update in updates {
            if let Some(index) = self.tools.iter().position(|tool| tool.tool == update.tool) {
                let installed = self.tools[index].installed;
                let summary = summarize_tool_fields(
                    update.tool,
                    installed,
                    &self.tools[index].fields,
                    update.summary.as_deref(),
                );
                self.tools[index].summary = summary;
            }
        }
    }

    fn set_from_flat(&mut self, flat: usize) {
        let mut remaining = flat;
        for (ti, _) in self.tools.iter().enumerate() {
            let count = self.tool_row_count(ti);
            if count == 0 {
                continue;
            }
            if remaining < count {
                self.tool_index = ti;
                self.field_index = remaining;
                return;
            }
            remaining -= count;
        }
    }

    fn move_up(&mut self) {
        let flat = self.flatten_index();
        if flat > 0 {
            self.set_from_flat(flat - 1);
        }
    }

    fn move_down(&mut self) {
        let flat = self.flatten_index();
        if flat + 1 < self.total_rows() {
            self.set_from_flat(flat + 1);
        }
    }

    fn move_select_up(&mut self) {
        if let AppMode::Selecting { selected, .. } = &mut self.mode {
            if *selected > 0 {
                *selected -= 1;
            }
        }
    }

    fn move_select_down(&mut self) {
        if let AppMode::Selecting {
            selected, options, ..
        } = &mut self.mode
        {
            if *selected + 1 < options.len() {
                *selected += 1;
            }
        }
    }

    fn start_edit(&mut self) {
        if self
            .current_tool()
            .is_some_and(|tool| tool.tool == Tool::KakuAssistant && self.field_index == 0)
        {
            self.toggle_kaku_assistant_collapsed();
            return;
        }

        let Some(tool) = self.current_tool() else {
            return;
        };
        if !tool.installed || tool.fields.is_empty() {
            return;
        }
        let Some(selected_field_idx) = self.selected_field_index() else {
            return;
        };
        if selected_field_idx >= tool.fields.len() {
            return;
        }
        let field = &tool.fields[selected_field_idx];

        // Show OAuth re-authentication command for non-editable auth fields
        if !field.editable {
            if field.key == "Auth" || (field.value.starts_with('✓') && !field.key.contains(" ▸ "))
            {
                let cmd = match tool.tool {
                    Tool::KakuAssistant => None,
                    Tool::Gemini => Some("gemini auth login"),
                    Tool::Codex => Some("codex auth login"),
                    Tool::Kimi => Some("kimi login"),
                    Tool::Copilot => Some("gh auth login"),
                    Tool::FactoryDroid => Some("droid"),
                    Tool::ClaudeCode => Some("claude auth login"),
                    Tool::OpenClaw => None,
                };

                if let Some(auth_cmd) = cmd {
                    self.open_in_terminal(auth_cmd);
                } else {
                    self.set_status("OpenClaw uses API keys, check config file");
                }
            }
            return;
        }

        if !field.options.is_empty() {
            self.mode = AppMode::Selecting {
                field_idx: selected_field_idx,
                options: field.options.clone(),
                selected: field
                    .options
                    .iter()
                    .position(|o| *o == field.value)
                    .unwrap_or(0),
            };
            self.focus = Focus::Editor;
            return;
        }

        let edit_buf = if field.value == "—" {
            // Empty placeholder
            String::new()
        } else if tool.tool == Tool::KakuAssistant && field.key == "API Key" {
            get_kaku_assistant_api_key().unwrap_or_else(String::new)
        } else {
            field.value.clone()
        };
        let edit_cursor = edit_buf.len(); // Start cursor at end (always a valid byte boundary)
        self.mode = AppMode::Editing {
            field_idx: selected_field_idx,
            buffer: edit_buf,
            cursor: edit_cursor,
        };
        self.focus = Focus::Editor;
    }

    fn confirm_select(&mut self) {
        let AppMode::Selecting {
            field_idx,
            options,
            selected,
        } = std::mem::replace(&mut self.mode, AppMode::Browsing)
        else {
            return;
        };
        self.focus = Focus::ToolList;

        if selected >= options.len() {
            return;
        }

        let Some((tool_kind, field_key, old_val)) = self.current_tool().and_then(|tool| {
            tool.fields
                .get(field_idx)
                .map(|field| (tool.tool, field.key.clone(), field.value.clone()))
        }) else {
            return;
        };
        self.field_index = self.display_index_for_field(tool_kind, field_idx);
        let new_val = options[selected].clone();

        if new_val == old_val {
            return;
        }

        if let Some(tool) = self.current_tool_mut() {
            tool.fields[field_idx].value = new_val.clone();
        }
        let status_val = status_value_for_display(&field_key, &new_val);
        match save_field(tool_kind, &field_key, &new_val) {
            Ok(()) => self.set_status(format!("Saved {} → {}", field_key, status_val)),
            Err(e) => {
                self.set_status(format!("Save failed: {}", e));
                self.set_error(format!("Save failed: {}", e));
            }
        }
        self.reload_current_tool();
    }

    fn cancel_select(&mut self) {
        self.mode = AppMode::Browsing;
        self.focus = Focus::ToolList;
    }

    fn confirm_edit(&mut self) {
        let AppMode::Editing {
            field_idx,
            buffer,
            cursor: _,
        } = std::mem::replace(&mut self.mode, AppMode::Browsing)
        else {
            return;
        };
        self.focus = Focus::ToolList;

        let Some((tool_kind, field_key, old_val)) = self.current_tool().and_then(|tool| {
            tool.fields
                .get(field_idx)
                .map(|field| (tool.tool, field.key.clone(), field.value.clone()))
        }) else {
            return;
        };
        self.field_index = if tool_kind == Tool::KakuAssistant {
            field_idx + 1
        } else {
            field_idx
        };
        let new_val = buffer.trim().to_string();

        // Empty input on API Key fields means cancel, not clear
        if new_val.is_empty() && field_key.contains("API Key") {
            return;
        }

        if new_val == old_val || (new_val.is_empty() && old_val == "—") {
            return;
        }

        if let Some(tool) = self.current_tool_mut() {
            tool.fields[field_idx].value = new_val.clone();
        }

        let status_val = status_value_for_display(&field_key, &new_val);
        match save_field(tool_kind, &field_key, &new_val) {
            Ok(()) => self.set_status(format!("Saved {} → {}", field_key, status_val)),
            Err(e) => {
                self.set_status(format!("Save failed: {}", e));
                self.set_error(format!("Save failed: {}", e));
            }
        }
        self.reload_current_tool();
    }

    fn reload_current_tool(&mut self) {
        let Some(tool_type) = self.current_tool().map(|tool| tool.tool) else {
            return;
        };
        let refreshed = ToolState::load_without_remote_usage(tool_type);
        if let Some(tool) = self.current_tool_mut() {
            *tool = refreshed;
        }
        if tool_type == Tool::KakuAssistant {
            if let Some(tool) = self.current_tool() {
                self.assistant_collapsed = should_collapse_kaku_assistant(&tool.fields);
            }
            self.field_index = 0;
        }
        self.schedule_usage_reload(tool_type);
    }

    fn cancel_edit(&mut self) {
        self.mode = AppMode::Browsing;
        self.focus = Focus::ToolList;
    }

    fn open_config(&mut self) {
        let Some(tool) = self.current_tool() else {
            return;
        };
        let path = tool.tool.config_path();
        if !path.exists() {
            return;
        }
        match std::process::Command::new("/usr/bin/open")
            .arg(&path)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(_) => {
                log::debug!("open command returned non-zero status");
                self.set_status("Failed to open config file");
            }
            Err(e) => {
                log::debug!("Failed to open config file: {}", e);
                self.set_status(format!("Failed to open: {}", e));
            }
        }
    }

    fn refresh_models(&mut self) {
        let codex_usage_cache = codex_usage_cache_path();
        if let Err(e) = std::fs::remove_file(&codex_usage_cache) {
            log::trace!("Could not remove codex usage cache: {}", e);
        }
        let claude_usage_cache = claude_usage_cache_path();
        if let Err(e) = std::fs::remove_file(&claude_usage_cache) {
            log::trace!("Could not remove claude usage cache: {}", e);
        }
        let copilot_usage_cache = copilot_usage_cache_path();
        if let Err(e) = std::fs::remove_file(&copilot_usage_cache) {
            log::trace!("Could not remove copilot usage cache: {}", e);
        }
        let kimi_usage_cache = kimi_usage_cache_path();
        if let Err(e) = std::fs::remove_file(&kimi_usage_cache) {
            log::trace!("Could not remove kimi usage cache: {}", e);
        }

        let models_refreshed = fetch_models_dev_json().is_some();
        let show_assistant = kaku_assistant_visible();
        self.tools = Self::load_tools(show_assistant);
        self.tool_index = self
            .tools
            .iter()
            .position(|tool| !tool.fields.is_empty())
            .unwrap_or(0);
        self.field_index = 0;
        self.restart_usage_loading();
        if models_refreshed {
            self.set_status("AI settings refreshed");
        } else {
            self.set_status("Usage refreshed");
            self.set_error("Models refresh failed. Kept local model cache.");
        }
        self.sync_transient_errors();
    }

    /// Open a shell command in a new Kaku tab (preferred) or fall back to Terminal.app.
    fn open_in_terminal(&mut self, cmd: &str) {
        // Prefer a new tab in the current Kaku window.
        // kaku cli spawn reads WEZTERM_PANE from the environment to target the right window.
        // Append `exec $SHELL` so the pane stays alive after the command finishes.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let shell_cmd = format!("{}; exec \"{}\"", cmd, shell);
        let kaku_status = std::process::Command::new("kaku")
            .args(["cli", "spawn", "--", &shell, "-c", &shell_cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if kaku_status.as_ref().is_ok_and(|status| status.success()) {
            self.set_status("Opening in new Kaku tab...");
            return;
        }

        // Fallback: open in macOS Terminal.app via osascript.
        let script = format!("tell application \"Terminal\" to do script \"{}\"", cmd);
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .spawn()
        {
            Ok(_) => self.set_status("Opening in new terminal window..."),
            Err(_) => self.set_status(format!("Failed to open terminal. Run '{}' manually", cmd)),
        }
    }
}

fn save_field(tool: Tool, field_key: &str, new_val: &str) -> anyhow::Result<()> {
    if tool == Tool::KakuAssistant {
        return save_kaku_assistant_field(field_key, new_val);
    }

    // Codex uses TOML; delegate immediately before any JSON parsing attempt.
    if tool == Tool::Codex {
        return save_codex_field(field_key, new_val);
    }
    if tool == Tool::Kimi {
        return save_kimi_field(field_key, new_val);
    }

    let path = tool.config_path();
    if !path.exists() {
        anyhow::bail!("config file not found: {}", path.display());
    }

    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut parsed: serde_json::Value =
        parse_json_or_jsonc(&raw).with_context(|| format!("parse {}", path.display()))?;

    match tool {
        Tool::KakuAssistant => unreachable!("Kaku Assistant is handled before JSON parsing"),
        Tool::Gemini => {
            if field_key == "Model" {
                if let Some(obj) = parsed.as_object_mut() {
                    if new_val == "—" || new_val.is_empty() {
                        obj.remove("model");
                    } else {
                        let keep_string_shape = obj.get("model").is_some_and(|m| m.is_string());
                        if keep_string_shape {
                            obj.insert(
                                "model".to_string(),
                                serde_json::Value::String(new_val.to_string()),
                            );
                        } else {
                            obj.insert("model".to_string(), serde_json::json!({"name": new_val}));
                        }
                    }
                }
            } else {
                return Ok(());
            }
        }
        Tool::Copilot => {
            if field_key == "Model" {
                if let Some(obj) = parsed.as_object_mut() {
                    if new_val == "—" || new_val.is_empty() {
                        obj.remove("model");
                    } else {
                        obj.insert(
                            "model".to_string(),
                            serde_json::Value::String(new_val.to_string()),
                        );
                    }
                }
            } else {
                return Ok(());
            }
        }
        Tool::FactoryDroid => {
            let obj = parsed.as_object_mut().context("root is not object")?;
            save_factory_droid_field(obj, field_key, new_val)?;
        }
        Tool::ClaudeCode => {
            let env_key = match field_key {
                "Base URL" => Some("ANTHROPIC_BASE_URL"),
                "API Key" => Some("ANTHROPIC_AUTH_TOKEN"),
                _ => None,
            };
            let top_key = match field_key {
                "Model" => Some("model"),
                _ => None,
            };

            if let Some(ek) = env_key {
                let obj = parsed.as_object_mut().context("root is not object")?;
                let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
                if let Some(env_obj) = env.as_object_mut() {
                    if new_val == "—" || new_val.is_empty() {
                        env_obj.remove(ek);
                    } else {
                        env_obj.insert(
                            ek.to_string(),
                            serde_json::Value::String(new_val.to_string()),
                        );
                    }
                }
            } else if let Some(tk) = top_key {
                if let Some(obj) = parsed.as_object_mut() {
                    if new_val == "—" || new_val.is_empty() {
                        obj.remove(tk);
                    } else {
                        obj.insert(
                            tk.to_string(),
                            serde_json::Value::String(new_val.to_string()),
                        );
                    }
                }
            } else {
                return Ok(());
            }
        }
        Tool::OpenClaw => {
            // Parse "provider_name ▸ Base URL" or "provider_name ▸ API Key"
            if let Some(sep_pos) = field_key.find(" ▸ ") {
                let provider_name = &field_key[..sep_pos];
                let sub_field = &field_key[sep_pos + " ▸ ".len()..];

                if sub_field == "Models" {
                    // Rename model key in agents.defaults.models
                    // old_display: "claude-opus-4-5-20251101" → full key: "provider/claude-opus-4-5-20251101"
                    // new_val:     "claude-opus-4-6"          → full key: "provider/claude-opus-4-6"
                    let defaults = parsed
                        .as_object_mut()
                        .context("root not object")?
                        .entry("agents")
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .context("agents not object")?
                        .entry("defaults")
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .context("defaults not object")?;

                    // Collect old→new key mappings first
                    let prefix = format!("{}/", provider_name);
                    let mut renames: Vec<(String, String)> = Vec::new();

                    if let Some(models_obj) =
                        defaults.get_mut("models").and_then(|m| m.as_object_mut())
                    {
                        let old_keys: Vec<String> = models_obj
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned()
                            .collect();

                        for old_key in old_keys {
                            if let Some(val) = models_obj.remove(&old_key) {
                                let new_full = format!("{}/{}", provider_name, new_val);
                                models_obj.insert(new_full.clone(), val);
                                renames.push((old_key, new_full));
                            }
                        }
                    }

                    // Sync model.primary if it pointed to an old key
                    if let Some(model_obj) =
                        defaults.get_mut("model").and_then(|m| m.as_object_mut())
                    {
                        for (old_key, new_full) in &renames {
                            if model_obj.get("primary").and_then(|v| v.as_str()) == Some(old_key) {
                                model_obj.insert(
                                    "primary".to_string(),
                                    serde_json::Value::String(new_full.clone()),
                                );
                            }
                        }
                    }
                } else {
                    let json_field = match sub_field {
                        "Base URL" => "baseUrl",
                        "API Key" => "apiKey",
                        _ => return Ok(()),
                    };

                    let providers = parsed
                        .as_object_mut()
                        .context("root not object")?
                        .entry("models")
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .context("models not object")?
                        .entry("providers")
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .context("providers not object")?;

                    let prov = providers
                        .entry(provider_name)
                        .or_insert_with(|| serde_json::json!({}));

                    if let Some(obj) = prov.as_object_mut() {
                        if new_val == "—" || new_val.is_empty() {
                            obj.remove(json_field);
                        } else {
                            obj.insert(
                                json_field.to_string(),
                                serde_json::Value::String(new_val.to_string()),
                            );
                        }
                    }
                }
            } else if field_key == "Primary Model" {
                let model = parsed
                    .as_object_mut()
                    .context("root not object")?
                    .entry("agents")
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .context("agents not object")?
                    .entry("defaults")
                    .or_insert_with(|| serde_json::json!({}))
                    .as_object_mut()
                    .context("defaults not object")?
                    .entry("model")
                    .or_insert_with(|| serde_json::json!({}));

                if let Some(obj) = model.as_object_mut() {
                    if new_val == "—" || new_val.is_empty() {
                        obj.remove("primary");
                    } else {
                        obj.insert(
                            "primary".to_string(),
                            serde_json::Value::String(new_val.to_string()),
                        );
                    }
                }
            } else {
                return Ok(());
            }
        }
        Tool::Codex | Tool::Kimi => {
            unreachable!("TOML-backed tools are handled before JSON parsing")
        }
    }

    let output = serde_json::to_string_pretty(&parsed).context("serialize config")?;
    if is_jsonc_path(&path) {
        log::info!(
            "Note: {} comments will be removed when Kaku rewrites this file.",
            path.display()
        );
    }
    write_atomic(&path, output.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Save a field to Codex TOML config (~/.codex/config.toml)
fn save_codex_field(field_key: &str, new_val: &str) -> anyhow::Result<()> {
    let path = Tool::Codex.config_path();
    save_codex_field_at(&path, field_key, new_val)
}

fn save_kimi_field(field_key: &str, new_val: &str) -> anyhow::Result<()> {
    let path = Tool::Kimi.config_path();
    save_kimi_field_at(&path, field_key, new_val)
}

fn save_codex_field_at(path: &Path, field_key: &str, new_val: &str) -> anyhow::Result<()> {
    let toml_key = match field_key {
        "Model" => "model",
        "Reasoning Effort" => "model_reasoning_effort",
        _ => return Ok(()),
    };

    let raw = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };

    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    let target = format!("{} = ", toml_key);
    let new_line = format!("{} = \"{}\"", toml_key, new_val);

    let mut found = false;
    let mut in_top_level = true;
    for line in &mut lines {
        let trimmed = line.trim_start();
        // Entering a table section: [section] or [[array-of-tables]]
        if trimmed.starts_with('[') {
            in_top_level = false;
        }
        if in_top_level && trimmed.starts_with(&target) {
            if new_val == "—" || new_val.is_empty() {
                *line = String::new();
            } else {
                *line = new_line.clone();
            }
            found = true;
            break;
        }
    }

    if !found && !new_val.is_empty() && new_val != "—" {
        // Insert before the first [section] or at the end
        let insert_pos = lines
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .unwrap_or(lines.len());
        lines.insert(insert_pos, new_line);
    }

    // Remove empty lines that resulted from deletion
    let output: Vec<&str> = lines.iter().map(|l| l.as_str()).collect();
    let result = output.join("\n");
    write_atomic(path, result.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn save_kimi_field_at(path: &Path, field_key: &str, new_val: &str) -> anyhow::Result<()> {
    let toml_key = match field_key {
        "Model" => "default_model",
        _ => return Ok(()),
    };

    let raw = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };

    let mut lines: Vec<String> = raw.lines().map(|line| line.to_string()).collect();
    let target = format!("{toml_key} = ");
    let new_line = format!("{toml_key} = \"{new_val}\"");

    let mut found = false;
    let mut in_top_level = true;
    for line in &mut lines {
        let trimmed = line.trim_start();
        // Entering a table section: [section] or [[array-of-tables]]
        if trimmed.starts_with('[') {
            in_top_level = false;
        }
        if in_top_level && trimmed.starts_with(&target) {
            if new_val == "—" || new_val.is_empty() {
                *line = String::new();
            } else {
                *line = new_line.clone();
            }
            found = true;
            break;
        }
    }

    if !found && !new_val.is_empty() && new_val != "—" {
        let insert_pos = lines
            .iter()
            .position(|line| line.trim_start().starts_with('['))
            .unwrap_or(lines.len());
        lines.insert(insert_pos, new_line);
    }

    let output: Vec<&str> = lines.iter().map(|line| line.as_str()).collect();
    let result = output.join("\n");
    write_atomic(path, result.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    struct TerminalGuard {
        raw_mode: bool,
        bracketed_paste: bool,
        alternate_screen: bool,
    }

    impl TerminalGuard {
        fn new() -> Self {
            Self {
                raw_mode: false,
                bracketed_paste: false,
                alternate_screen: false,
            }
        }
    }

    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            if self.raw_mode {
                let _ = disable_raw_mode();
            }

            let mut stdout = io::stdout();
            if self.bracketed_paste {
                let _ = stdout.execute(DisableBracketedPaste);
            }
            if self.alternate_screen {
                let _ = stdout.execute(LeaveAlternateScreen);
            }
        }
    }

    let mut guard = TerminalGuard::new();
    enable_raw_mode().context("enable raw mode")?;
    guard.raw_mode = true;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnableBracketedPaste).context("enable bracketed paste")?;
    guard.bracketed_paste = true;
    stdout
        .execute(EnterAlternateScreen)
        .context("enter alternate screen")?;
    guard.alternate_screen = true;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal
        .draw(ui::loading_ui)
        .context("draw loading screen")?;

    let mut app = App::new();
    let result = run_loop(&mut terminal, &mut app);

    terminal.show_cursor().context("show cursor")?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    loop {
        app.sync_transient_errors();
        terminal.draw(|frame| ui::ui(frame, app))?;

        if !event::poll(EVENT_POLL_INTERVAL).context("poll event")? {
            continue;
        }

        match event::read().context("read event")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                app.status_msg = None;
                app.status_expire = None;
                if app.is_selecting() {
                    match key.code {
                        KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => {
                            app.confirm_select()
                        }
                        KeyCode::Esc => app.cancel_select(),
                        KeyCode::Up | KeyCode::Char('k') => app.move_select_up(),
                        KeyCode::Down | KeyCode::Char('j') => app.move_select_down(),
                        _ => {}
                    }
                    continue;
                }

                if app.is_editing() {
                    match key.code {
                        KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => {
                            app.confirm_edit()
                        }
                        KeyCode::Esc => app.cancel_edit(),
                        KeyCode::Left => {
                            if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                if *edit_cursor > 0 {
                                    *edit_cursor = prev_char_boundary(edit_buf, *edit_cursor);
                                }
                            }
                        }
                        KeyCode::Right => {
                            if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                if *edit_cursor < edit_buf.len() {
                                    *edit_cursor = next_char_boundary(edit_buf, *edit_cursor);
                                }
                            }
                        }
                        KeyCode::Home => {
                            if let Some((_, edit_cursor)) = app.editing_mut() {
                                *edit_cursor = 0;
                            }
                        }
                        KeyCode::End => {
                            if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                *edit_cursor = edit_buf.len();
                            }
                        }
                        KeyCode::Backspace => {
                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                || key.modifiers.contains(KeyModifiers::SUPER)
                            {
                                // Cmd+Backspace (macOS) or Ctrl+Backspace - clear all
                                if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                    edit_buf.clear();
                                    *edit_cursor = 0;
                                }
                            } else if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                if *edit_cursor > 0 {
                                    edit_backspace(edit_buf, edit_cursor);
                                }
                            }
                        }
                        KeyCode::Delete => {
                            if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                if *edit_cursor < edit_buf.len() {
                                    edit_delete(edit_buf, *edit_cursor);
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            // Handle Ctrl+U (clear line) - macOS Cmd+Backspace may also send this
                            if (key.modifiers.contains(KeyModifiers::CONTROL)
                                || key.modifiers.contains(KeyModifiers::SUPER))
                                && c == 'u'
                            {
                                if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                    edit_buf.clear();
                                    *edit_cursor = 0;
                                }
                            }
                            // Ignore other control characters
                            else if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !key.modifiers.contains(KeyModifiers::SUPER)
                            {
                                if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                                    edit_insert_char(edit_buf, edit_cursor, c);
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Esc => app.should_quit = true,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.should_quit = true
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.move_down(),
                    KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => app.start_edit(),
                    KeyCode::Char('o') => app.open_config(),
                    KeyCode::Char('r') => app.refresh_models(),
                    _ => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => app.move_up(),
                MouseEventKind::ScrollDown => app.move_down(),
                _ => {}
            },
            Event::Paste(text) => {
                if !app.is_editing() || text.is_empty() {
                    continue;
                }

                let ends_with_newline = text.ends_with('\n') || text.ends_with('\r');

                // Clipboard paste may include a trailing newline from terminal copy.
                // Strip line breaks so paste doesn't break single-line inputs.
                let cleaned: String = text.chars().filter(|c| *c != '\r' && *c != '\n').collect();
                if !cleaned.is_empty() {
                    if let Some((edit_buf, edit_cursor)) = app.editing_mut() {
                        for c in cleaned.chars() {
                            edit_insert_char(edit_buf, edit_cursor, c);
                        }
                    }
                }

                // If the pasted text (often from voice typing tools) ends with a newline, auto-submit
                if ends_with_newline {
                    app.confirm_edit();
                }
            }
            _ => {}
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn prev_char_boundary(buf: &str, cursor: usize) -> usize {
    let cursor = cursor.min(buf.len());
    if cursor == 0 {
        return 0;
    }
    buf[..cursor]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(buf: &str, cursor: usize) -> usize {
    let cursor = cursor.min(buf.len());
    if cursor >= buf.len() {
        return buf.len();
    }
    cursor + buf[cursor..].chars().next().map_or(0, |c| c.len_utf8())
}

fn edit_backspace(buf: &mut String, cursor: &mut usize) {
    if *cursor == 0 || buf.is_empty() {
        return;
    }
    let prev = prev_char_boundary(buf, *cursor);
    buf.remove(prev);
    *cursor = prev;
}

fn edit_delete(buf: &mut String, cursor: usize) {
    if cursor >= buf.len() || buf.is_empty() {
        return;
    }
    buf.remove(cursor);
}

fn edit_insert_char(buf: &mut String, cursor: &mut usize, c: char) {
    let at = (*cursor).min(buf.len());
    buf.insert(at, c);
    *cursor = at + c.len_utf8();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn codex_save_round_trip_for_model_and_reasoning_effort() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        std::fs::write(
            &path,
            "model = \"old\"\nmodel_reasoning_effort = \"low\"\n\n[projects.\"/tmp\"]\ntrust_level = \"trusted\"\n",
        )
        .expect("seed config");

        save_codex_field_at(&path, "Model", "gpt-5").expect("update model");
        save_codex_field_at(&path, "Reasoning Effort", "high").expect("update effort");
        let saved = std::fs::read_to_string(&path).expect("read config");
        assert!(saved.contains("model = \"gpt-5\""));
        assert!(saved.contains("model_reasoning_effort = \"high\""));
        assert!(saved.contains("[projects.\"/tmp\"]"));

        save_codex_field_at(&path, "Model", "").expect("remove model");
        let saved = std::fs::read_to_string(&path).expect("read config");
        assert!(!saved.contains("model = \"gpt-5\""));
        assert!(saved.contains("model_reasoning_effort = \"high\""));
        assert!(saved.contains("[projects.\"/tmp\"]"));
    }

    #[test]
    fn codex_save_creates_new_top_level_entry_before_sections() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[profiles.default]\nfoo = \"bar\"\n").expect("seed config");

        save_codex_field_at(&path, "Model", "gpt-5").expect("insert model");
        let saved = std::fs::read_to_string(&path).expect("read config");
        let model_pos = saved.find("model = \"gpt-5\"").expect("model line");
        let section_pos = saved.find("[profiles.default]").expect("section");
        assert!(model_pos < section_pos);
    }

    #[test]
    fn unicode_edit_helpers_keep_cursor_on_char_boundaries() {
        let mut buf = "你a好".to_string();
        let mut cursor = buf.len();

        cursor = prev_char_boundary(&buf, cursor);
        assert_eq!(cursor, "你a".len());
        cursor = prev_char_boundary(&buf, cursor);
        assert_eq!(cursor, "你".len());
        cursor = prev_char_boundary(&buf, cursor);
        assert_eq!(cursor, 0);

        cursor = next_char_boundary(&buf, cursor);
        assert_eq!(cursor, "你".len());
        cursor = next_char_boundary(&buf, cursor);
        assert_eq!(cursor, "你a".len());

        edit_insert_char(&mut buf, &mut cursor, '界');
        assert_eq!(buf, "你a界好");
        assert_eq!(cursor, "你a界".len());

        edit_backspace(&mut buf, &mut cursor);
        assert_eq!(buf, "你a好");
        assert_eq!(cursor, "你a".len());

        cursor = prev_char_boundary(&buf, cursor);
        assert_eq!(cursor, "你".len());
        edit_delete(&mut buf, cursor);
        assert_eq!(buf, "你好");
        assert_eq!(cursor, "你".len());
    }

    #[test]
    fn factory_droid_extract_prefers_session_defaults() {
        let parsed = serde_json::json!({
            "model": "legacy-model",
            "reasoningEffort": "low",
            "autonomyMode": "spec",
            "sessionDefaultSettings": {
                "model": "fd-model",
                "reasoningEffort": "medium",
                "autonomyLevel": "auto-low"
            }
        });

        let fields = extract_factory_droid_fields(&parsed);
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].key, "Model");
        assert_eq!(fields[0].value, "fd-model");
        assert!(fields[0].options.is_empty());

        assert_eq!(fields[1].key, "Reasoning Effort");
        assert_eq!(fields[1].value, "medium");
        assert_eq!(
            fields[1].options,
            string_options(&FACTORY_DROID_REASONING_OPTIONS)
        );

        assert_eq!(fields[2].key, "Autonomy Level");
        assert_eq!(fields[2].value, "auto-low");
        assert_eq!(
            fields[2].options,
            string_options(&FACTORY_DROID_AUTONOMY_OPTIONS)
        );
    }

    #[test]
    fn factory_droid_extract_falls_back_to_top_level_and_defaults() {
        let parsed = serde_json::json!({
            "model": "legacy-model",
            "reasoningEffort": "high",
            "autonomyLevel": "spec"
        });

        let fields = extract_factory_droid_fields(&parsed);
        assert_eq!(fields[0].value, "legacy-model");
        assert_eq!(fields[1].value, "high");
        assert_eq!(fields[2].value, "spec");

        let empty_fields = extract_factory_droid_fields(&serde_json::json!({}));
        assert_eq!(empty_fields[0].value, FACTORY_DROID_DEFAULT_MODEL);
        assert_eq!(empty_fields[1].value, FACTORY_DROID_DEFAULT_REASONING);
        assert_eq!(empty_fields[2].value, FACTORY_DROID_DEFAULT_AUTONOMY);
    }

    #[test]
    fn factory_droid_save_writes_session_defaults_and_normalizes_autonomy_key() {
        let mut obj = serde_json::json!({
            "autonomyLevel": "spec"
        })
        .as_object()
        .cloned()
        .expect("object");

        save_factory_droid_field(&mut obj, "Autonomy Level", "auto-high").expect("save autonomy");
        save_factory_droid_field(&mut obj, "Model", "gpt-5.1-codex").expect("save model");
        save_factory_droid_field(&mut obj, "Reasoning Effort", "").expect("clear reasoning effort");

        let session_defaults = obj
            .get("sessionDefaultSettings")
            .and_then(|v| v.as_object())
            .expect("sessionDefaultSettings object");

        assert_eq!(
            session_defaults
                .get("autonomyMode")
                .and_then(|v| v.as_str()),
            Some("auto-high")
        );
        assert_eq!(session_defaults.get("autonomyLevel"), None);
        assert_eq!(
            session_defaults.get("model").and_then(|v| v.as_str()),
            Some("gpt-5.1-codex")
        );
        assert_eq!(session_defaults.get("reasoningEffort"), None);
    }

    #[test]
    fn kaku_assistant_fields_do_not_include_enabled_toggle() {
        let fields = extract_kaku_assistant_fields("enabled = true\n");
        assert!(fields.iter().all(|field| field.key != "Enabled"));
    }

    #[test]
    fn summarize_kaku_assistant_requires_api_key() {
        let fields = extract_kaku_assistant_fields("enabled = false\n");
        assert_eq!(
            summarize_tool_fields(Tool::KakuAssistant, true, &fields, None),
            Some(format!(
                "Setup required · {}",
                assistant_config::DEFAULT_MODEL
            ))
        );
    }

    #[test]
    fn summarize_kaku_assistant_prefers_status_and_model() {
        let fields = extract_kaku_assistant_fields(
            "enabled = true\napi_key = \"sk-test\"\nmodel = \"gpt-5-mini\"\n",
        );
        assert_eq!(
            summarize_tool_fields(Tool::KakuAssistant, true, &fields, None),
            Some("Ready · gpt-5-mini".into())
        );
    }

    #[test]
    fn summarize_non_usage_tool_uses_auth_and_model() {
        let fields = vec![
            FieldEntry {
                key: "Model".into(),
                value: "gpt-5".into(),
                options: vec![],
                editable: true,
            },
            FieldEntry {
                key: "Auth".into(),
                value: "✓ user@example.com".into(),
                options: vec![],
                editable: false,
            },
        ];

        assert_eq!(
            summarize_tool_fields(Tool::FactoryDroid, true, &fields, None),
            Some("user@example.com · gpt-5".into())
        );
    }

    #[test]
    fn summarize_codex_prefers_usage_only() {
        assert_eq!(
            summarize_tool_fields(
                Tool::Codex,
                true,
                &[FieldEntry {
                    key: "Auth".into(),
                    value: "✓ user@example.com".into(),
                    options: vec![],
                    editable: false,
                }],
                Some("5h remain 75% · reset 4h0m  |  7d remain 93% · reset 6d0h"),
            ),
            Some("5h remain 75% · reset 4h0m  |  7d remain 93% · reset 6d0h".into())
        );
    }

    #[test]
    fn summarize_claude_prefers_usage_only() {
        assert_eq!(
            summarize_tool_fields(
                Tool::ClaudeCode,
                true,
                &[],
                Some("5h remain 94% · reset 2h0m")
            ),
            Some("5h remain 94% · reset 2h0m".into())
        );
    }

    #[test]
    fn summarize_kimi_prefers_usage_only() {
        assert_eq!(
            summarize_tool_fields(
                Tool::Kimi,
                true,
                &[],
                Some("5h remain 94% · reset 14m  |  7d remain 67% · reset 4d11h")
            ),
            Some("5h remain 94% · reset 14m  |  7d remain 67% · reset 4d11h".into())
        );
    }

    #[test]
    fn summarize_gemini_falls_back_to_auth_and_model_when_quota_missing() {
        let fields = vec![
            FieldEntry {
                key: "Model".into(),
                value: "gemini-3.1-flash-lite-preview".into(),
                options: vec![],
                editable: true,
            },
            FieldEntry {
                key: "Auth".into(),
                value: "✓ user@example.com".into(),
                options: vec![],
                editable: false,
            },
        ];

        assert_eq!(
            summarize_tool_fields(Tool::Gemini, true, &fields, None),
            Some("user@example.com · gemini-3.1-flash-lite-preview".into())
        );
    }

    #[test]
    fn claude_usage_snapshot_shows_reauth_required_for_invalid_refresh_state() {
        let parsed = serde_json::json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "details": {
                    "error_code": "token_expired"
                }
            }
        });

        let snapshot = parse_claude_usage_snapshot(&parsed).expect("snapshot");
        assert_eq!(snapshot.summary, Some("Re-auth required".into()));
    }

    #[test]
    fn parse_claude_usage_error_detects_auth_failure() {
        let parsed = serde_json::json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "details": {
                    "error_code": "token_expired"
                }
            }
        });

        assert_eq!(
            parse_claude_usage_error(&parsed).as_deref(),
            Some("Re-auth required")
        );
    }

    #[test]
    fn parse_claude_keychain_account_extracts_account_name() {
        let raw = r#"
keychain: "/Users/tw93/Library/Keychains/login.keychain-db"
version: 512
class: "genp"
attributes:
    "acct"<blob>="tw93"
    "svce"<blob>="Claude Code-credentials"
"#;

        assert_eq!(parse_claude_keychain_account(raw), Some("tw93".into()));
    }

    #[test]
    fn gemini_quota_summary_uses_auth_type() {
        let parsed = serde_json::json!({
            "security": {
                "auth": {
                    "selectedType": "oauth-personal"
                }
            }
        });
        assert_eq!(
            gemini_quota_summary(&parsed),
            Some("Quota 1000/day · 60/min".into())
        );
    }

    #[test]
    fn copilot_usage_snapshot_prefers_remaining_count() {
        let parsed = serde_json::json!({
            "quota_reset_date_utc": "2100-04-01T00:00:00.000Z",
            "quota_snapshots": {
                "premium_interactions": {
                    "remaining": 298,
                    "quota_remaining": 298.34
                }
            }
        });
        let snapshot = parse_copilot_usage_snapshot(&parsed).expect("snapshot");
        let summary = snapshot.summary.expect("summary");
        assert!(summary.starts_with("298 left this month · reset "));
    }

    #[test]
    fn codex_extract_adds_default_model_when_missing() {
        let fields = extract_codex_fields("");
        let model = fields
            .iter()
            .find(|field| field.key == "Model")
            .expect("model field");
        assert!(!model.value.is_empty());
        assert!(!model.value.ends_with(" (default)"));
    }

    #[test]
    fn kimi_extract_reads_model_and_auth_field() {
        let fields = extract_kimi_fields(
            r#"
default_model = "kimi-code/kimi-for-coding"

[models."kimi-code/kimi-for-coding"]
provider = "managed:kimi-code"
"#,
        );
        assert_eq!(fields[0].key, "Model");
        assert_eq!(fields[0].value, "kimi-code/kimi-for-coding");
        assert_eq!(fields[1].key, "Auth");
    }

    #[test]
    fn kimi_save_round_trip_for_default_model() {
        let path = std::env::temp_dir().join(format!(
            "kaku-kimi-{}.toml",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("unix epoch")
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "default_model = \"kimi-code/kimi-for-coding\"\n[models.\"kimi-code/kimi-for-coding\"]\nprovider = \"managed:kimi-code\"\n",
        )
        .expect("write temp config");

        save_kimi_field_at(&path, "Model", "kimi-code/new-model").expect("save model");
        let updated = std::fs::read_to_string(&path).expect("read temp config");
        assert!(updated.contains("default_model = \"kimi-code/new-model\""));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kimi_usage_snapshot_formats_current_and_weekly_windows() {
        let parsed = serde_json::json!({
            "usage": {
                "limit": "100",
                "used": "33",
                "remaining": "67",
                "resetTime": "2100-03-12T14:43:16.853067Z"
            },
            "limits": [
                {
                    "window": {
                        "duration": 300,
                        "timeUnit": "TIME_UNIT_MINUTE"
                    },
                    "detail": {
                        "limit": "100",
                        "used": "6",
                        "remaining": "94",
                        "resetTime": "2100-03-08T03:43:16.853067Z"
                    }
                }
            ]
        });

        let snapshot = parse_kimi_usage_snapshot(&parsed).expect("snapshot");
        let summary = snapshot.summary.expect("summary");
        assert!(summary.starts_with("5h remain 94% · reset "));
        assert!(summary.contains("  |  7d remain 67% · reset "));
    }

    #[test]
    fn kimi_auth_status_accepts_refreshable_session() {
        let auth = serde_json::json!({
            "access_token": "",
            "refresh_token": "refresh-token",
            "expires_at": 0.0
        });
        let path = kimi_credentials_path();
        let parent = path.parent().expect("credentials dir");
        std::fs::create_dir_all(parent).expect("create credentials dir");
        let previous = std::fs::read_to_string(&path).ok();
        std::fs::write(&path, serde_json::to_vec(&auth).expect("serialize auth"))
            .expect("write credentials");

        assert_eq!(read_kimi_auth_status(), "✓ oauth");

        if let Some(previous) = previous {
            std::fs::write(&path, previous).expect("restore credentials");
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn kaku_assistant_collapses_only_when_ready() {
        let ready = extract_kaku_assistant_fields(
            "enabled = true\napi_key = \"sk-test\"\nmodel = \"gpt-5-mini\"\n",
        );
        assert!(should_collapse_kaku_assistant(&ready));

        let not_ready = extract_kaku_assistant_fields("enabled = true\n");
        assert!(!should_collapse_kaku_assistant(&not_ready));
    }

    #[test]
    fn codex_usage_snapshot_prefers_current_window() {
        let parsed = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 62.7,
                    "reset_at": 4_102_444_800i64
                },
                "secondary_window": {
                    "used_percent": 18.0,
                    "reset_at": 4_102_704_000i64
                }
            }
        });

        let snapshot = parse_codex_usage_snapshot(&parsed).expect("snapshot");
        let summary = snapshot.summary.expect("summary");
        assert!(summary.starts_with("5h remain 37.3% · reset "));
        assert!(summary.contains("  |  7d remain 82% · reset "));
    }
}
