//! Claude provider implementation

pub mod accounts;
mod admin_api;
pub mod claude_swap;
mod cli_reset;
mod oauth;
mod scoped_weekly;
mod web_api;

use async_trait::async_trait;
use chrono::Utc;
use regex_lite::Regex;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::{Command as StdCommand, Stdio};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::cli::tty_runner::{TtyCommandOptions, TtyCommandRunner};
use crate::core::{
    FetchContext, LastGoodFailurePolicy, Provider, ProviderError, ProviderFetchResult, ProviderId,
    ProviderMetadata, RateWindow, SourceMode, UsageSnapshot,
};

use admin_api::ClaudeAdminApiFetcher;
#[cfg(test)]
use cli_reset::parse_claude_reset_date_in_system_zone;
use cli_reset::{
    extract_cli_scoped_weekly_limits, normalized_for_label_search, parse_claude_reset_date,
    parse_percent_line, starts_next_usage_section,
};

// ── Upstream 0.50.1 #2516: CLI usage-result cache ────────────────────────────
//
// When token rotation revokes OAuth access, the auto path falls back to the
// CLI. To avoid hammering the CLI probe on every poll, cache the last
// successful CLI result for 15 minutes. The cache is only consulted when
// OAuth returned `OAuthRevoked` (revoked, not merely expired) or a rate
// limit, so normal refresh cycles are unaffected.
const CLI_RESULT_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

struct CachedCliResult {
    result: ProviderFetchResult,
    cached_at: Instant,
}

static CLI_RESULT_CACHE: LazyLock<Mutex<Option<CachedCliResult>>> =
    LazyLock::new(|| Mutex::new(None));

fn clear_account_caches(credential_path: &std::path::Path) {
    if let Ok(mut cache) = CLI_RESULT_CACHE.lock() {
        *cache = None;
    }
    if let Ok(mut last) = LAST_AUTO_READING.lock() {
        *last = LastAutoReading::default();
    }
    oauth::clear_account_cache(credential_path);
}

// After a CLI probe failed while OAuth was rate limited, skip further probes
// for this long so a throttled account is not probed on every poll. Short,
// because the probe also fails when it reads the screen before the limits
// are drawn, and the next try usually succeeds.
const RATE_LIMITED_CLI_PROBE_BACKOFF: Duration = Duration::from_secs(60);

static RATE_LIMITED_CLI_PROBE_FAILED_AT: LazyLock<Mutex<Option<Instant>>> =
    LazyLock::new(|| Mutex::new(None));

fn note_rate_limited_cli_probe(succeeded: bool) {
    if let Ok(mut guard) = RATE_LIMITED_CLI_PROBE_FAILED_AT.lock() {
        *guard = (!succeeded).then(Instant::now);
    }
}

fn rate_limited_cli_probe_failed_recently() -> bool {
    RATE_LIMITED_CLI_PROBE_FAILED_AT
        .lock()
        .ok()
        .and_then(|guard| *guard)
        .is_some_and(|failed_at| failed_at.elapsed() <= RATE_LIMITED_CLI_PROBE_BACKOFF)
}

// ── Stable percentages when Auto changes source ─────────────────────────────
//
// Auto answers from OAuth while the usage endpoint responds and from the CLI
// screen (probed once, then cached for 15 minutes) while it answers 429, so
// the source can change from one poll to the next. The two do not carry the
// same number: OAuth passes the endpoint's value through, the CLI screen
// prints a whole number (`Math.floor(utilization)`), and a cached screen can
// be minutes older than the OAuth reading before it.
//
// Within one usage window the used percentage only goes up. So when a reading
// comes from another source than the number on display and belongs to the
// same window, it replaces that number only if it is more than a rounding
// step higher. A lower one is an older reading, and one within a rounding
// step carries no news. Readings from the same source always pass through.
const SOURCE_CHANGE_HOLD_TTL: Duration = Duration::from_secs(15 * 60);
/// Largest gap between the two sources that rounding alone explains.
const SOURCE_ROUNDING_DISTANCE: f64 = 1.0;
/// Reset times further apart than this belong to different windows. The CLI
/// screen prints the reset to the minute or hour; a new window is hours away.
const SAME_WINDOW_RESET_TOLERANCE_SECS: i64 = 30 * 60;

const AUTO_SOURCE_OAUTH: &str = "oauth";
const AUTO_SOURCE_CLI: &str = "cli";

/// The number Auto last reported for one window, and the reading behind it.
struct ReportedWindow {
    window: RateWindow,
    source: &'static str,
    read_at: Instant,
}

#[derive(Default)]
struct LastAutoReading {
    primary: Option<ReportedWindow>,
    secondary: Option<ReportedWindow>,
    account_identity: Option<String>,
}

static LAST_AUTO_READING: LazyLock<Mutex<LastAutoReading>> =
    LazyLock::new(|| Mutex::new(LastAutoReading::default()));

/// Apply the source-change rule to a result Auto is about to return.
/// `read_at` is when the reading was taken, which for a cached CLI result is
/// when it was cached. Only `used_percent` is ever replaced.
fn steady_across_source_change(
    mut result: ProviderFetchResult,
    source: &'static str,
    read_at: Instant,
) -> ProviderFetchResult {
    if let Ok(mut last) = LAST_AUTO_READING.lock() {
        steady_percents(&mut result, &mut last, source, read_at, Instant::now());
    }
    result
}

fn steady_percents(
    result: &mut ProviderFetchResult,
    last: &mut LastAutoReading,
    source: &'static str,
    read_at: Instant,
    now: Instant,
) {
    // Both identities come from the credential; a mismatch means this reading
    // belongs to another account than the number on display.
    if let Some(identity) = &result.account_identity {
        if last
            .account_identity
            .as_ref()
            .is_some_and(|previous| previous != identity)
        {
            *last = LastAutoReading::default();
        }
        last.account_identity = Some(identity.clone());
    }
    steady_window(
        &mut result.usage.primary,
        &mut last.primary,
        source,
        read_at,
        now,
    );
    if let Some(secondary) = result.usage.secondary.as_mut() {
        steady_window(secondary, &mut last.secondary, source, read_at, now);
    }
}

fn steady_window(
    window: &mut RateWindow,
    last: &mut Option<ReportedWindow>,
    source: &'static str,
    read_at: Instant,
    now: Instant,
) {
    if let Some(previous) = last.as_ref()
        && previous.source != source
        && now.saturating_duration_since(previous.read_at) <= SOURCE_CHANGE_HOLD_TTL
        && is_same_usage_window(window, &previous.window)
        && window.used_percent <= previous.window.used_percent + SOURCE_ROUNDING_DISTANCE
    {
        // The number on display stays, and so does the reading behind it:
        // its age keeps counting, so the hold ends on its own.
        window.used_percent = previous.window.used_percent;
        return;
    }
    *last = Some(ReportedWindow {
        window: window.clone(),
        source,
        read_at,
    });
}

/// Whether two readings can be compared as the same usage window. Without a
/// reset time on both sides a new window cannot be told from an old one.
fn is_same_usage_window(new: &RateWindow, previous: &RateWindow) -> bool {
    if new.is_informational || previous.is_informational {
        return false;
    }
    // A reached limit is a state of its own and is never held or held back.
    if new.used_percent >= 100.0 || previous.used_percent >= 100.0 {
        return false;
    }
    match (new.resets_at, previous.resets_at) {
        (Some(new_reset), Some(previous_reset)) => {
            (new_reset - previous_reset).num_seconds().abs() <= SAME_WINDOW_RESET_TOLERANCE_SECS
        }
        _ => false,
    }
}

/// Store a successful CLI fetch result in the 15-minute cache.
fn cache_cli_result(result: ProviderFetchResult) {
    if let Ok(mut guard) = CLI_RESULT_CACHE.lock() {
        *guard = Some(CachedCliResult {
            result,
            cached_at: Instant::now(),
        });
    }
}

/// Return a cached CLI result if it is still within the TTL. Used when
/// revoked OAuth prevents a live fetch and the CLI should not be re-probed.
fn cached_cli_result() -> Option<ProviderFetchResult> {
    cached_cli_result_with_time().map(|(result, _)| result)
}

/// The cached CLI result together with the time it was read from the CLI.
fn cached_cli_result_with_time() -> Option<(ProviderFetchResult, Instant)> {
    let Ok(guard) = CLI_RESULT_CACHE.lock() else {
        return None;
    };
    guard
        .as_ref()
        .filter(|entry| entry.cached_at.elapsed() <= CLI_RESULT_CACHE_TTL)
        .map(|entry| {
            let mut result = entry.result.clone();
            // A retained payload is useful for display, but cannot prove that
            // the current fetch reached Claude CLI successfully.
            result.has_successful_claude_cli_quota = false;
            (result, entry.cached_at)
        })
}

/// The cached CLI result as Auto returns it, with the source-change rule applied.
fn cached_cli_result_for_auto() -> Option<ProviderFetchResult> {
    cached_cli_result_with_time()
        .map(|(result, read_at)| steady_across_source_change(result, AUTO_SOURCE_CLI, read_at))
}

/// Whether the OAuth source failed with a revocation (not just expiry).
/// Revoked tokens should reuse the working CLI fallback; expired/missing
/// tokens should NOT block the normal refresh path.
fn is_oauth_revoked_error(error: &ProviderError) -> bool {
    matches!(error, ProviderError::OAuthRevoked(_))
}
pub use oauth::ClaudeOAuthFetcher;
pub use web_api::ClaudeWebApiFetcher;

/// Recovery guidance for a Claude web request blocked by a Cloudflare challenge.
pub const CLOUDFLARE_CHALLENGE_MESSAGE: &str = concat!(
    "claude.ai is behind a Cloudflare challenge, often caused by VPN or datacenter networks. ",
    "Re-authenticating will not help. Switch Claude Usage source to OAuth in Settings ",
    "(Usage credits balance will be unavailable), or try a different network."
);

/// Stable marker appended to the Auto-mode failure summary when the OAuth
/// usage endpoint refused the request (rate limited) while no claude.ai
/// browser cookies were readable and the CLI probe also failed. Claude Code
/// is signed in in that situation; what is missing is a browser session.
/// The marker names the page that provides it, so companion apps match
/// `[<provider>:browser-sign-in-required <url>]` and open that page instead
/// of parsing the English summary or keeping their own sign-in table.
pub const CLAUDE_BROWSER_SIGN_IN_MARKER: &str =
    "[claude:browser-sign-in-required https://claude.ai/login]";

/// Whether the user explicitly consented to reading (and refreshing) Claude
/// Code's own credentials. Upstream #2634/#2745: without consent the
/// file/keyring sources stay closed and refreshed tokens are never rotated
/// into Claude Code's storage; Auto then falls back to labeled
/// reduced-fidelity CLI usage.
pub(crate) fn claude_code_consent() -> bool {
    crate::settings::Settings::load().claude_allow_reading_claude_code_credentials
}

/// Return the identity of the credential that can authorize a Claude CLI
/// resume. The OAuth module applies the same consent boundary as its fetcher.
pub fn auto_resume_identity() -> Option<String> {
    oauth::auto_resume_identity()
}

/// Claude provider implementation
pub struct ClaudeProvider {
    metadata: ProviderMetadata,
    web_fetcher: ClaudeWebApiFetcher,
    oauth_fetcher: ClaudeOAuthFetcher,
    admin_fetcher: ClaudeAdminApiFetcher,
}

impl ClaudeProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Claude,
                display_name: "Claude",
                session_label: "Session (5h)",
                weekly_label: "Weekly",
                supports_opus: true,
                supports_credits: true,
                default_enabled: true,
                is_primary: true,
                dashboard_url: Some("https://claude.ai/settings/usage"),
                status_page_url: Some("https://status.claude.com/"),
            },
            web_fetcher: ClaudeWebApiFetcher::new(),
            oauth_fetcher: ClaudeOAuthFetcher::new(),
            admin_fetcher: ClaudeAdminApiFetcher::new(),
        }
    }
}

impl Default for ClaudeProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn claude_plan_label(tier: &str) -> String {
    let normalized = tier.to_lowercase();
    if normalized.contains("claude_max_5x") || normalized.contains("claude_max_5") {
        "Claude Max 5x".to_string()
    } else if normalized.contains("claude_max_20x") || normalized.contains("claude_max_20") {
        "Claude Max 20x".to_string()
    } else {
        match normalized.as_str() {
            "free" => "Claude Free".to_string(),
            "pro" | "claude_pro" => "Claude Pro".to_string(),
            "max" => "Claude Max".to_string(),
            "team" => "Claude Team".to_string(),
            "enterprise" => "Claude Enterprise".to_string(),
            _ => format!("Claude ({})", tier),
        }
    }
}

const CLAUDE_PROBE_SESSION_ID_FILE: &str = ".codexbar-session-id";
const CLAUDE_PROBE_LOCK_FILE: &str = ".codexbar-probe.lock";
const CLAUDE_PROBE_CACHE_FILE: &str = ".codexbar-usage-cache.json";
/// How long a second codexbar process waits for a running probe to finish.
const CLAUDE_PROBE_LOCK_WAIT: Duration = Duration::from_secs(30);
/// Every codexbar process (the `serve` daemon, one-off `usage` calls from the
/// companion) launches its own Claude CLI for a probe. The interactive
/// `/usage` screen costs 6-10 s of CPU each time, so a recent successful
/// probe output is shared across processes for this long.
const CLAUDE_PROBE_CACHE_TTL: Duration = Duration::from_secs(45);

#[derive(serde::Serialize, serde::Deserialize)]
struct ClaudeProbeCache {
    captured_at_unix: u64,
    output: String,
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cached_probe_output(probe_dir: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(probe_dir.join(CLAUDE_PROBE_CACHE_FILE)).ok()?;
    let cache: ClaudeProbeCache = serde_json::from_str(&raw).ok()?;
    let age = unix_now_secs().saturating_sub(cache.captured_at_unix);
    if age > CLAUDE_PROBE_CACHE_TTL.as_secs() || cache.output.trim().is_empty() {
        return None;
    }
    tracing::debug!(age_secs = age, "Reusing recent Claude CLI probe output");
    Some(cache.output)
}

fn store_cached_probe_output(probe_dir: &std::path::Path, output: &str) {
    let cache = ClaudeProbeCache {
        captured_at_unix: unix_now_secs(),
        output: output.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&cache)
        && let Err(err) = std::fs::write(probe_dir.join(CLAUDE_PROBE_CACHE_FILE), json)
    {
        tracing::debug!(error = %err, "failed to persist Claude probe cache");
    }
}

/// Cross-process guard around the Claude PTY probe. Claude Code refuses to
/// start a session whose `--session-id` is already running ("Session ID …
/// is already in use"), so two codexbar processes (for example the
/// `serve` daemon and a one-off `usage` call) must not probe concurrently.
struct ClaudeProbeLock(std::fs::File);

impl ClaudeProbeLock {
    fn acquire(probe_dir: &std::path::Path) -> Option<Self> {
        let path = probe_dir.join(CLAUDE_PROBE_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        let deadline = Instant::now() + CLAUDE_PROBE_LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => {
                    tracing::debug!(error = %err, "Claude probe lock unavailable; continuing unlocked");
                    return None;
                }
            }
            if Instant::now() >= deadline {
                tracing::debug!("Claude probe lock wait expired; continuing unlocked");
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for ClaudeProbeLock {
    fn drop(&mut self) {
        // Best-effort unlock; closing the handle releases it anyway.
        let _unlocked = self.0.unlock();
    }
}

fn claude_usage_probe_dir() -> Result<std::path::PathBuf, ProviderError> {
    let base = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| {
            ProviderError::Other("Could not resolve a local data directory".to_string())
        })?;
    let dir = base.join("CodexBar").join("claude-usage-probe");
    std::fs::create_dir_all(&dir).map_err(|e| {
        ProviderError::Other(format!(
            "Failed to prepare Claude CLI probe directory: {}",
            e
        ))
    })?;
    Ok(dir)
}

/// Persist and reuse one probe session id so repeated `/usage` PTY launches do
/// not register a fresh empty Claude account session each refresh (upstream #2263).
fn load_or_create_probe_session_id(probe_dir: &std::path::Path) -> String {
    let path = probe_dir.join(CLAUDE_PROBE_SESSION_ID_FILE);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if uuid::Uuid::parse_str(trimmed).is_ok() {
            return trimmed.to_ascii_lowercase();
        }
    }
    let id = uuid::Uuid::new_v4().to_string().to_ascii_lowercase();
    if let Err(err) = std::fs::write(&path, &id) {
        tracing::debug!(error = %err, "failed to persist Claude probe session id");
    }
    id
}

/// Claude treats `--session-id` as create-only when a local transcript JSONL
/// already exists for that id. Clear probe-dir jsonl leftovers before reuse.
fn cleanup_probe_session_jsonl(probe_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(probe_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            // Best-effort cleanup: a locked or missing probe file just stays.
            let _removed = std::fs::remove_file(&path);
        }
    }
    cleanup_probe_transcript(probe_dir);
}

/// Claude stores the transcript for a working directory under
/// `~/.claude/projects/<sanitized cwd>/<session-id>.jsonl`, where the cwd is
/// sanitized by replacing every non-alphanumeric character with `-`
/// (`C:\\Users\\x` -> `C--Users-x`). Remove the probe session transcript there,
/// otherwise the fixed `--session-id` fails with "already in use" on the next run.
fn cleanup_probe_transcript(probe_dir: &std::path::Path) {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let project_dir = home
        .join(".claude")
        .join("projects")
        .join(claude_project_dir_name(probe_dir));
    let Ok(entries) = std::fs::read_dir(&project_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            // Best-effort cleanup: a locked transcript just stays.
            let _removed = std::fs::remove_file(&path);
        }
    }
}

fn claude_project_dir_name(dir: &std::path::Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn claude_probe_launch_args(session_id: &str) -> Vec<String> {
    vec![
        "--setting-sources".to_string(),
        "user".to_string(),
        "--allowed-tools".to_string(),
        String::new(),
        // `tui: default` keeps the probe on the classic renderer, whose output
        // the /usage parser reads, and stops Claude Code from offering the
        // fullscreen renderer in a dialog the probe cannot answer. Flag
        // settings apply to this launch only; the user's config is untouched.
        "--settings".to_string(),
        CLAUDE_PROBE_SETTINGS.to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
    ]
}

const CLAUDE_PROBE_SETTINGS: &str = r#"{"remoteControlAtStartup":false,"tui":"default"}"#;

/// Titles of Claude Code startup dialogs that wait for an answer before the
/// prompt accepts input. While one is open, the typed /usage goes nowhere.
const CLAUDE_STARTUP_DIALOG_TITLES: &[&str] = &[
    "Claude in Chrome extension detected",
    "Try the new fullscreen renderer?",
];

/// Returns the title of a startup dialog that is still waiting in the probe
/// output. Whitespace is ignored because ConPTY output can drop or move it.
fn claude_startup_dialog_title(clean: &str) -> Option<&'static str> {
    let squashed: String = clean
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    CLAUDE_STARTUP_DIALOG_TITLES.iter().copied().find(|title| {
        let needle: String = title
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        squashed.contains(&needle)
    })
}

struct ClaudePtyProbeOptions {
    script: &'static str,
    timeout_secs: f64,
    idle_timeout_secs: Option<f64>,
    initial_delay_secs: f64,
    script_char_delay_secs: f64,
    script_line_delay_secs: f64,
    send_on_substring: Option<(&'static str, &'static str)>,
    /// Re-type the script at these offsets while no done marker is visible.
    script_retry_delays_secs: &'static [f64],
    script_done_substrings: &'static [&'static str],
    script_echo_substrings: &'static [&'static str],
    /// Idle window after the done marker appeared (trailing output only).
    idle_timeout_after_done_secs: Option<f64>,
}

/// Offsets (seconds after launch) at which `/usage` is re-sent when Claude's
/// input widget was not ready for the first attempt. Claude Code needs roughly
/// 1-5 s to mount its prompt on Windows, and keystrokes before that are lost.
const CLAUDE_USAGE_RETRY_DELAYS_SECS: &[f64] = &[6.0, 9.5, 14.0];
/// Output markers that prove `/usage` opened (limits view or activity stats).
const CLAUDE_USAGE_DONE_MARKERS: &[&str] = &[
    "current session",
    "current week",
    "total duration",
    "favorite model:",
    "total tokens:",
];
/// The typed command as Claude echoes it into its prompt line. While this is
/// visible the first attempt is still being processed, so do not type again.
const CLAUDE_USAGE_ECHO_MARKERS: &[&str] = &["❯ /usage", "> /usage", "/usage show session cost"];

async fn run_claude_usage_pty_probe(
    claude_path: std::path::PathBuf,
    working_directory: std::path::PathBuf,
) -> Result<String, ProviderError> {
    run_claude_pty_probe(
        claude_path,
        working_directory,
        ClaudePtyProbeOptions {
            script: "/usage",
            timeout_secs: 24.0,
            idle_timeout_secs: Some(6.0),
            initial_delay_secs: 3.0,
            script_char_delay_secs: 0.04,
            script_line_delay_secs: 0.0,
            send_on_substring: None,
            script_retry_delays_secs: CLAUDE_USAGE_RETRY_DELAYS_SECS,
            script_done_substrings: CLAUDE_USAGE_DONE_MARKERS,
            script_echo_substrings: CLAUDE_USAGE_ECHO_MARKERS,
            idle_timeout_after_done_secs: Some(1.5),
        },
    )
    .await
}

async fn run_claude_trust_preflight(
    claude_path: std::path::PathBuf,
    working_directory: std::path::PathBuf,
    answer_keys: &'static str,
) -> Result<String, ProviderError> {
    run_claude_pty_probe(
        claude_path,
        working_directory,
        ClaudePtyProbeOptions {
            script: "",
            timeout_secs: 15.0,
            idle_timeout_secs: Some(4.0),
            initial_delay_secs: 0.6,
            script_char_delay_secs: 0.0,
            script_line_delay_secs: 0.0,
            send_on_substring: Some(("Enter", answer_keys)),
            script_retry_delays_secs: &[],
            script_done_substrings: &[],
            script_echo_substrings: &[],
            idle_timeout_after_done_secs: None,
        },
    )
    .await
}

fn resolve_claude_cli_path() -> Result<std::path::PathBuf, ProviderError> {
    locate_claude_binary().ok_or_else(|| {
        ProviderError::NotInstalled(
            "Claude CLI not found. Install from https://docs.claude.ai/claude-code".to_string(),
        )
    })
}

async fn fetch_claude_cli_usage_text(
    claude_path: std::path::PathBuf,
) -> Result<String, ProviderError> {
    let probe_dir = claude_usage_probe_dir()?;
    if let Some(cached) = load_cached_probe_output(&probe_dir) {
        return Ok(cached);
    }
    let combined = run_claude_usage_pty_probe(claude_path.clone(), probe_dir.clone()).await?;

    let combined =
        rerun_claude_usage_after_trust_prompt(claude_path, probe_dir.clone(), combined).await?;
    // Only a parseable limits screen is worth sharing; errors are retried live.
    if claude_cli_error_from_output(&combined).is_none()
        && ClaudeProvider::new().parse_cli_output(&combined).is_ok()
    {
        store_cached_probe_output(&probe_dir, &combined);
    }
    Ok(combined)
}

async fn rerun_claude_usage_after_trust_prompt(
    claude_path: std::path::PathBuf,
    probe_dir: std::path::PathBuf,
    combined: String,
) -> Result<String, ProviderError> {
    if !is_workspace_trust_prompt(&strip_ansi(&combined).to_lowercase()) {
        return Ok(combined);
    }

    let answer_keys = claude_trust_prompt_answer_keys(&strip_ansi(&combined).to_lowercase());
    let preflight =
        run_claude_trust_preflight(claude_path.clone(), probe_dir.clone(), answer_keys).await?;
    tracing::trace!(output = %strip_ansi(&preflight), "Claude trust preflight output");
    run_claude_usage_pty_probe(claude_path, probe_dir).await
}

/// Enter, then /exit once the folder is trusted.
const CLAUDE_TRUST_ACCEPT_FIRST_OPTION: &str = "\r/exit\r";
/// How long the trust dialog must be visible before the preflight answers.
const CLAUDE_DIALOG_ANSWER_DELAY_SECS: f64 = 1.0;
/// Arrow down to the second option, Enter, then /exit.
const CLAUDE_TRUST_ACCEPT_SECOND_OPTION: &str = "\x1b[B\r/exit\r";

/// Keys that pick "Yes, I trust this folder" in the trust dialog shown by the
/// first probe. The dialog focuses its first option, and since Claude Code
/// 2.1.29x that option is "No, exit": a plain Enter quits without saving the
/// trust, so the dialog came back on every probe.
fn claude_trust_prompt_answer_keys(lowered: &str) -> &'static str {
    match (
        lowered.find("no, exit"),
        lowered.find("yes, i trust this folder"),
    ) {
        (Some(no), Some(yes)) if no < yes => CLAUDE_TRUST_ACCEPT_SECOND_OPTION,
        _ => CLAUDE_TRUST_ACCEPT_FIRST_OPTION,
    }
}

fn claude_cli_error_from_output(output: &str) -> Option<ProviderError> {
    let lowered = output.to_lowercase();
    claude_cli_auth_error(&lowered).or_else(|| claude_cli_environment_error(&lowered))
}

fn claude_cli_auth_error(lowered: &str) -> Option<ProviderError> {
    if claude_output_requires_login(lowered) {
        return Some(ProviderError::AuthRequired);
    }
    if lowered.contains("token expired") || lowered.contains("token_expired") {
        return Some(ProviderError::OAuthExpired(
            "Token expired. Run `claude login` to refresh.".to_string(),
        ));
    }
    if lowered.contains("authentication_error") {
        return Some(ProviderError::OAuth(
            "Authentication error. Run `claude login`.".to_string(),
        ));
    }

    None
}

fn claude_output_requires_login(lowered: &str) -> bool {
    lowered.contains("not logged in") || lowered.contains("login required")
}

fn claude_cli_environment_error(lowered: &str) -> Option<ProviderError> {
    if lowered.contains("requires git-bash") {
        return Some(ProviderError::Other(
            "Claude CLI requires Git Bash on Windows. Install Git for Windows or set \
             CLAUDE_CODE_GIT_BASH_PATH to your bash.exe path."
                .to_string(),
        ));
    }
    if lowered.contains("running scripts is disabled") {
        return Some(ProviderError::Other(
            "Claude CLI could not start because PowerShell script execution is disabled. \
             Use claude.cmd or adjust the execution policy."
                .to_string(),
        ));
    }
    if lowered.contains("cannot run a document in the middle of a pipeline") {
        return Some(ProviderError::Other(
            "Claude CLI resolved to a Unix shell script on Windows. Reinstall Claude Code or \
             ensure claude.cmd is first on PATH."
                .to_string(),
        ));
    }

    None
}

/// Environment overrides for passive Claude CLI PTY probes.
fn claude_passive_probe_env(
    mut base: std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    // Passive status/usage probes must not mutate or update the user's Claude CLI installation.
    base.insert("NO_COLOR".to_string(), "1".to_string());
    base.insert("DISABLE_AUTOUPDATER".to_string(), "1".to_string());
    // Without this, Claude Code offers Claude in Chrome at startup whenever
    // the extension is installed, and that dialog swallows the typed /usage.
    base.insert("CLAUDE_CODE_ENABLE_CFC".to_string(), "0".to_string());
    base
}

async fn run_claude_pty_probe(
    claude_path: std::path::PathBuf,
    working_directory: std::path::PathBuf,
    probe: ClaudePtyProbeOptions,
) -> Result<String, ProviderError> {
    tokio::task::spawn_blocking(move || {
        // Keep ownership in the worker: cancelling the async refresh does not
        // stop spawn_blocking or its CLI process from rotating credentials.
        let _account_operation = accounts::CREDENTIAL_OPERATION.blocking_lock();
        let _probe_lock = ClaudeProbeLock::acquire(&working_directory);
        cleanup_probe_session_jsonl(&working_directory);
        let session_id = load_or_create_probe_session_id(&working_directory);
        let env = claude_passive_probe_env(TtyCommandRunner::enriched_environment());

        let mut options = TtyCommandOptions::new()
            .with_timeout(probe.timeout_secs)
            .with_initial_delay(probe.initial_delay_secs)
            .with_script_char_delay(probe.script_char_delay_secs)
            .with_script_line_delay(probe.script_line_delay_secs)
            .with_working_directory(working_directory)
            .with_extra_args(claude_probe_launch_args(&session_id));
        if let Some(idle) = probe.idle_timeout_secs {
            options = options.with_idle_timeout(idle);
        }
        if let Some(idle) = probe.idle_timeout_after_done_secs {
            options = options.with_idle_timeout_after_done(idle);
        }
        if let Some((trigger, keys)) = probe.send_on_substring {
            // Claude Code ignores keys that reach a dialog within 150-500 ms
            // of it opening and resets the dialog's focus, so answer only
            // once the dialog has been on screen for a moment.
            options = options
                .with_send_on_substring(trigger, keys)
                .with_send_trigger_delay(CLAUDE_DIALOG_ANSWER_DELAY_SECS);
        }
        if !probe.script_retry_delays_secs.is_empty() {
            options = options.with_script_retries(
                probe.script_retry_delays_secs.to_vec(),
                probe
                    .script_done_substrings
                    .iter()
                    .map(|marker| (*marker).to_string())
                    .collect(),
                probe
                    .script_echo_substrings
                    .iter()
                    .map(|marker| (*marker).to_string())
                    .collect(),
            );
        }
        options.env = env;

        TtyCommandRunner::new()
            .run(&claude_path.to_string_lossy(), probe.script, options)
            .map(|result| result.text)
    })
    .await
    .map_err(|e| ProviderError::Other(format!("Claude CLI probe failed: {}", e)))?
    .map_err(|e| match e {
        crate::cli::tty_runner::TtyCommandError::TimedOut => ProviderError::Timeout,
        other => ProviderError::Other(format!("Claude CLI failed: {}", other)),
    })
}

fn last_good_failure_policy_for_error(error: &str) -> LastGoodFailurePolicy {
    let lower = error.to_ascii_lowercase();
    if lower.contains("credentials not found")
        || (lower.contains("run") && lower.contains("claude") && lower.contains("authenticate"))
        || (lower.contains("not installed") && lower.contains("claude"))
        || (lower.contains("subscription") && lower.contains("unavailable"))
    {
        return LastGoodFailurePolicy::Replace;
    }
    if lower.contains(&CLOUDFLARE_CHALLENGE_MESSAGE.to_ascii_lowercase()) {
        return LastGoodFailurePolicy::PreserveOnceThenSurface;
    }
    if lower.contains("parse error")
        || lower.contains("empty output")
        || lower.contains("missing current session")
        || lower.contains("treated /usage as a normal prompt")
        || lower.contains("stopped at a startup question")
        || lower.contains("local activity stats")
        || lower.contains("could not parse")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("ratelimited")
        || error.eq_ignore_ascii_case("timeout")
        || lower.contains("timed out")
    {
        return LastGoodFailurePolicy::Preserve;
    }
    if lower.contains("unauthorized")
        || lower.contains("authentication required")
        || lower.contains("auth required")
    {
        return LastGoodFailurePolicy::PreserveOnce;
    }
    LastGoodFailurePolicy::Replace
}

#[async_trait]
impl Provider for ClaudeProvider {
    fn manual_cookie_precedes_token_account(&self) -> bool {
        true
    }

    fn automatic_metric_prioritizes_exhausted_window(&self) -> bool {
        false
    }

    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        match ctx.source_mode {
            SourceMode::Auto => self.fetch_via_auto(ctx).await,
            SourceMode::OAuth => self.fetch_via_oauth(ctx).await,
            SourceMode::Web => self.fetch_via_web(ctx).await,
            SourceMode::Cli => match self.fetch_via_cli(ctx).await {
                Ok(result) => Ok(result),
                Err(error) if should_fallback_from_claude_cli_error(&error) => {
                    tracing::debug!(
                        error = %error,
                        "Claude CLI usage probe failed with a fallback-safe error; trying OAuth"
                    );
                    self.fetch_via_oauth(ctx).await
                }
                Err(error) => Err(error),
            },
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![
            SourceMode::Auto,
            SourceMode::OAuth,
            SourceMode::Web,
            SourceMode::Cli,
        ]
    }

    fn supports_oauth(&self) -> bool {
        true
    }

    fn supports_web(&self) -> bool {
        true
    }

    fn supports_cli(&self) -> bool {
        true
    }

    fn owns_browser_cookie_resolution(&self) -> bool {
        true
    }

    fn last_good_failure_policy(&self, error: &str) -> LastGoodFailurePolicy {
        last_good_failure_policy_for_error(error)
    }

    fn detect_version(&self) -> Option<String> {
        detect_claude_version()
    }
    /// Claude's CLI-presence probe (`resolve_claude_cli_path`) raises
    /// `NotInstalled` when the `claude` binary itself is missing — an
    /// installation gap, not a credential problem — so it surfaces as an
    /// offline local runtime (matching the pre-backend classifier's
    /// treatment of CLI-presence failures). Message-scoped so any future
    /// credential-flavored `NotInstalled` keeps the default mapping.
    fn error_state_kind(&self, error: &ProviderError) -> crate::core::ProviderStateKind {
        match error {
            ProviderError::NotInstalled(msg) if msg.contains("CLI not found") => {
                crate::core::ProviderStateKind::LocalRuntimeOffline
            }
            _ => error.state_kind(),
        }
    }
}

impl ClaudeProvider {
    async fn fetch_via_auto(
        &self,
        ctx: &FetchContext,
    ) -> Result<ProviderFetchResult, ProviderError> {
        let mut failures = Vec::new();

        if let Some(result) = self.try_auto_admin_api(ctx, &mut failures).await {
            return Ok(result);
        }

        // Upstream 0.50.1 #2516: track whether OAuth failed with a revocation.
        let oauth_result = self.fetch_via_oauth(ctx).await;
        let oauth_revoked = oauth_result
            .as_ref()
            .err()
            .is_some_and(is_oauth_revoked_error);
        let oauth_rate_limited = oauth_result
            .as_ref()
            .err()
            .is_some_and(oauth::is_rate_limited_error);
        if let Some(result) = record_auto_source(&mut failures, "OAuth", oauth_result) {
            return Ok(steady_across_source_change(
                result,
                AUTO_SOURCE_OAUTH,
                Instant::now(),
            ));
        }

        // The browser can be signed in to another claude.ai account than
        // Claude Code (a second account in another browser), and its numbers
        // then belong to that account. So while Claude Code is signed in, its
        // login and its /usage screen answer first and the browser is only
        // the last live source. Without a Claude Code login the CLI cannot
        // answer either, so the browser keeps its place before it.
        let claude_code_signed_in = oauth_rate_limited
            || oauth_revoked
            || ctx.api_key.as_deref().is_some_and(|t| !t.trim().is_empty())
            || oauth::has_stored_credentials();
        if !claude_code_signed_in
            && let Some(result) =
                record_auto_source(&mut failures, "Web", self.fetch_via_web(ctx).await)
        {
            return Ok(result);
        }

        // When OAuth was revoked (not just expired), reuse a cached CLI result
        // if still within the 15-minute TTL to avoid re-probing the CLI.
        if oauth_revoked && let Some(cached) = cached_cli_result_for_auto() {
            tracing::debug!("Claude OAuth revoked; returning cached CLI result (15-min cache)");
            return Ok(cached);
        }

        // While the usage endpoint throttles this account, the CLI probe can
        // take ~24s, which pushes callers into a timeout when every poll runs
        // it. Probe the CLI once and reuse that result for the cache TTL: its
        // /usage screen still shows session and weekly, so a first run with
        // nothing to preserve gets values instead of an error. After a failed
        // probe, skip it for a while: the rate-limit error keeps the last
        // good values on screen (LastGoodFailurePolicy::Preserve).
        let skip_cli_probe = if oauth_rate_limited {
            if let Some(cached) = cached_cli_result_for_auto() {
                return Ok(cached);
            }
            rate_limited_cli_probe_failed_recently()
        } else {
            false
        };

        if !skip_cli_probe {
            let cli_result = self.fetch_via_cli(ctx).await;
            if oauth_rate_limited {
                note_rate_limited_cli_probe(cli_result.is_ok());
            }
            if let Some(mut result) = record_auto_source(&mut failures, "CLI", cli_result) {
                // Without consent for reading Claude Code credentials, label the
                // CLI fallback as reduced fidelity.
                if !claude_code_consent() {
                    result.source_label = "cli (reduced fidelity)".to_string();
                }
                // Cache the CLI result when OAuth was revoked or rate limited so polls
                // within the TTL reuse it without re-probing.
                if oauth_revoked || oauth_rate_limited {
                    cache_cli_result(result.clone());
                }
                return Ok(steady_across_source_change(
                    result,
                    AUTO_SOURCE_CLI,
                    Instant::now(),
                ));
            }
        }

        if claude_code_signed_in
            && let Some(result) =
                record_auto_source(&mut failures, "Web", self.fetch_via_web(ctx).await)
        {
            return Ok(result);
        }

        // Upstream 0.50.1 #2516: when all live sources fail, keep the
        // last-known quota visible (stale) instead of blanking the UI.
        if let Some(cached) = cached_cli_result_for_auto() {
            tracing::debug!("All Claude live sources failed; returning stale cached CLI result");
            return Ok(cached);
        }

        Err(claude_auto_fetch_error(failures))
    }

    async fn try_auto_admin_api(
        &self,
        ctx: &FetchContext,
        failures: &mut Vec<(&'static str, ProviderError)>,
    ) -> Option<ProviderFetchResult> {
        self.admin_fetcher
            .has_credentials(ctx)
            .then_some(async { self.fetch_via_admin_api(ctx).await })?
            .await
            .map_err(|error| failures.push(("Admin API", error)))
            .ok()
    }

    async fn fetch_via_oauth(
        &self,
        ctx: &FetchContext,
    ) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Attempting OAuth fetch for Claude");
        if let Some(token) = ctx
            .api_key
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        {
            return self.oauth_fetcher.fetch_with_access_token(token).await;
        }
        self.oauth_fetcher.fetch().await
    }

    async fn fetch_via_admin_api(
        &self,
        ctx: &FetchContext,
    ) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Attempting Admin API fetch for Claude");
        self.admin_fetcher.fetch(ctx).await
    }

    async fn fetch_via_web(
        &self,
        ctx: &FetchContext,
    ) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Attempting Web API fetch for Claude");

        // Check for manual cookie header first
        if let Some(ref cookie_header) = ctx.manual_cookie_header {
            tracing::debug!("Using manual cookie header");
            return self
                .web_fetcher
                .fetch_with_cookie_header(cookie_header)
                .await;
        }

        // Otherwise, try to extract cookies from browser
        self.web_fetcher.fetch_with_cookies().await
    }

    async fn fetch_via_cli(
        &self,
        _ctx: &FetchContext,
    ) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Attempting CLI probe for Claude");

        let claude_path = resolve_claude_cli_path()?;
        let combined = fetch_claude_cli_usage_text(claude_path).await?;
        tracing::trace!(output = %strip_ansi(&combined), "Claude CLI probe output");

        if let Some(error) = claude_cli_error_from_output(&combined) {
            return Err(error);
        }

        let mut result = self.parse_cli_output(&combined)?;
        if let Some(identity) = auto_resume_identity() {
            result = result.with_account_identity(identity);
        }
        Ok(mark_live_claude_cli_result(result))
    }

    /// Parse Claude CLI /usage output
    fn parse_cli_output(&self, output: &str) -> Result<ProviderFetchResult, ProviderError> {
        let clean = strip_ansi(output);
        let clean_lower = clean.to_lowercase();

        if clean.trim().is_empty() {
            return Err(ProviderError::Parse(
                "Empty output from Claude CLI".to_string(),
            ));
        }

        if is_non_interactive_slash_command_response(&clean_lower) {
            return Err(ProviderError::Other(
                "Claude CLI treated /usage as a normal prompt instead of opening the interactive usage screen. Use Auto, OAuth, or Web mode for Claude usage.".to_string(),
            ));
        }

        if !clean_lower.contains("current session")
            && !clean_lower.contains("current week")
            && let Some(title) = claude_startup_dialog_title(&clean)
        {
            return Err(ProviderError::Other(format!(
                "Claude CLI stopped at a startup question (\"{title}\") before /usage opened. Open Claude Code once in a terminal, answer the question, then refresh."
            )));
        }

        // Parse session percent: "X% used" or "X% left"
        let mut session_percent: Option<f64> = None;
        let mut weekly_percent: Option<f64> = None;

        // Look for "Current session" section
        if let Some(session_pct) = extract_percent_near_label(&clean, "current session") {
            session_percent = Some(session_pct);
        }

        // Look for "Current week" section
        if let Some(weekly_pct) = extract_percent_near_label(&clean, "current week (all models)")
            .or_else(|| extract_percent_near_label(&clean, "current week"))
        {
            weekly_percent = Some(weekly_pct);
        }

        // Newer Claude versions print local activity stats (cost, duration,
        // cache tokens) below the plan limits. Those sections carry their own
        // percentages, so only reject the output when no labelled limit
        // percentage was found; otherwise the stats are just extra noise.
        let activity_stats = is_cli_activity_stats_response(&clean_lower);
        if activity_stats && session_percent.is_none() && weekly_percent.is_none() {
            return Err(ProviderError::Other(
                "Claude CLI /usage opened, but this Claude version returned local activity stats instead of plan limit percentages. Use Auto, OAuth, or Web mode for Claude limits.".to_string(),
            ));
        }

        // Fallback: collect all percentages in order
        if session_percent.is_none() && !activity_stats {
            let all_percents = extract_all_percents(&clean);
            if !all_percents.is_empty() {
                session_percent = Some(all_percents[0]);
            }
            if all_percents.len() > 1 && weekly_percent.is_none() {
                weekly_percent = Some(all_percents[1]);
            }
        }

        if session_percent.is_none()
            && weekly_percent.is_none()
            && !is_exhausted_short_form(&clean_lower)
        {
            return Err(ProviderError::Parse(
                "Claude CLI did not return usage data".to_string(),
            ));
        }

        // Extract identity info
        let email = extract_email(&clean);
        let login_method = extract_login_method(&clean);

        // Extract reset times
        let session_reset = extract_reset_description(&clean, "current session");
        let weekly_reset = extract_reset_description(&clean, "current week (all models)")
            .or_else(|| extract_reset_description(&clean, "current week"));
        let short_form_reset = if is_exhausted_short_form(&clean_lower) {
            extract_inline_reset_description(&clean)
        } else {
            None
        };
        let session_reset = session_reset.or(short_form_reset);
        let now = Utc::now();
        let scoped_weekly_limits = extract_cli_scoped_weekly_limits(&clean, now);

        if session_percent.is_none() && is_exhausted_short_form(&clean_lower) {
            session_percent = Some(100.0);
        }

        // Build usage snapshot
        let session_used = session_percent.unwrap_or(0.0);
        let primary = RateWindow::with_details(
            session_used,
            Some(300), // 5 hour session window
            session_reset
                .as_deref()
                .and_then(|reset| parse_claude_reset_date(reset, now, Some(300))),
            session_reset,
        );

        let mut usage = UsageSnapshot::new(primary);

        if let Some(weekly_used) = weekly_percent {
            let secondary = RateWindow::with_details(
                weekly_used,
                Some(10080), // weekly (7 * 24 * 60)
                weekly_reset
                    .as_deref()
                    .and_then(|reset| parse_claude_reset_date(reset, now, Some(10080))),
                weekly_reset,
            );
            usage = usage.with_secondary(secondary);
        }

        for limit in scoped_weekly_limits {
            usage.extra_rate_windows.push(limit);
        }

        if let Some(method) = login_method {
            usage = usage.with_login_method(&method);
        } else {
            usage = usage.with_login_method("Claude (CLI)");
        }

        if let Some(email) = email {
            usage = usage.with_email(&email);
        }

        Ok(ProviderFetchResult::new(usage, "cli"))
    }
}

fn has_real_claude_quota_window(usage: &UsageSnapshot) -> bool {
    let is_real = |window: &RateWindow| !window.is_informational && window.used_percent.is_finite();
    is_real(&usage.primary) || usage.secondary.as_ref().is_some_and(is_real)
}

fn mark_live_claude_cli_result(mut result: ProviderFetchResult) -> ProviderFetchResult {
    if has_real_claude_quota_window(&result.usage) {
        result.has_successful_claude_cli_quota = true;
    }
    result
}

fn record_auto_source(
    failures: &mut Vec<(&'static str, ProviderError)>,
    source: &'static str,
    result: Result<ProviderFetchResult, ProviderError>,
) -> Option<ProviderFetchResult> {
    result.map_err(|error| failures.push((source, error))).ok()
}

fn claude_auto_fetch_error(failures: Vec<(&'static str, ProviderError)>) -> ProviderError {
    let oauth_rate_limited = failures
        .iter()
        .any(|(source, error)| *source == "OAuth" && oauth::is_rate_limited_error(error));
    let no_cookies = failures
        .iter()
        .any(|(source, error)| *source == "Web" && matches!(error, ProviderError::NoCookies));
    let summary = failures
        .into_iter()
        .map(|(source, error)| format!("{source}: {error}"))
        .collect::<Vec<_>>()
        .join("; ");

    let mut message = format!("Claude usage failed from all configured sources. {summary}");
    if oauth_rate_limited && no_cookies {
        message.push(' ');
        message.push_str(CLAUDE_BROWSER_SIGN_IN_MARKER);
    }
    ProviderError::Other(message)
}

fn should_fallback_from_claude_cli_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Parse(message) => {
            matches!(
                message.as_str(),
                "Claude CLI did not return usage data" | "Empty output from Claude CLI"
            )
        }
        ProviderError::Other(message) => {
            message.contains("returned local activity stats")
                || message.contains("treated /usage as a normal prompt")
                || message.contains("stopped at a startup question")
        }
        _ => false,
    }
}

/// Locate the Claude CLI for shell integrations that need to reopen a session.
pub fn locate_claude_binary() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CLAUDE_BINARY")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Some(path);
    }

    #[cfg(windows)]
    {
        let candidates = [
            // Direct install
            dirs::data_local_dir().map(|p| p.join("Programs").join("claude").join("claude.exe")),
            // npm global (AppData\Roaming\npm)
            dirs::data_local_dir().map(|p| p.join("npm").join("claude.cmd")),
            dirs::home_dir().map(|h| {
                h.join("AppData")
                    .join("Roaming")
                    .join("npm")
                    .join("claude.cmd")
            }),
            // npm global alternate (~\.npm-global)
            dirs::home_dir().map(|h| h.join(".npm-global").join("claude.cmd")),
            // Volta managed
            dirs::data_local_dir().map(|p| {
                p.join("Volta")
                    .join("tools")
                    .join("image")
                    .join("packages")
                    .join("@anthropic-ai")
                    .join("claude-code")
                    .join("bin")
                    .join("claude.cmd")
            }),
            // fnm managed (via shim)
            dirs::data_local_dir().map(|p| p.join("fnm_multishells").join("claude.cmd")),
            // PATH lookup
            find_windows_claude_in_path(),
        ];

        candidates.into_iter().flatten().find(|p| p.exists())
    }

    #[cfg(not(windows))]
    {
        which::which("claude").ok()
    }
}

#[cfg(windows)]
fn find_windows_claude_in_path() -> Option<std::path::PathBuf> {
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut command = StdCommand::new("where");
    command
        .arg("claude")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let output = command.output().ok()?;

    if !output.status.success() {
        return None;
    }

    let mut matches: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(std::path::PathBuf::from)
        .collect();

    matches.sort_by_key(|path| {
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("cmd") => 0,
            Some("bat") => 1,
            Some("exe") => 2,
            _ => 3,
        }
    });

    matches.into_iter().find(|path| path.exists())
}

/// Detect the version of the claude CLI
fn detect_claude_version() -> Option<String> {
    let claude_path = locate_claude_binary()?;

    #[cfg(windows)]
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let mut cmd = std::process::Command::new(claude_path);
    cmd.args(["--version"]);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output().ok()?;

    if output.status.success() {
        let version_str = String::from_utf8_lossy(&output.stdout);
        super::extract_semver(&version_str)
    } else {
        None
    }
}

/// Strip ANSI escape codes from text
fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1B' {
            // Skip CSI sequences: ESC[...letter
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut final_char = None;
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        final_char = Some(next);
                        break;
                    }
                }
                match final_char {
                    // Claude Code skips the blanks between words with a
                    // cursor-forward or an absolute column jump; both stand
                    // for whitespace on screen.
                    Some('C') | Some('G') => result.push(' '),
                    // ConPTY places each screen row with an absolute cursor
                    // position instead of a line break. Without the break the
                    // /usage rows run together and every label reads the
                    // first percentage in the screen.
                    Some('H') => result.push('\n'),
                    _ => {}
                }
            // Skip OSC sequences: ESC]...BEL
            } else if chars.peek() == Some(&']') {
                for next in chars.by_ref() {
                    if next == '\x07' || next == '\\' {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

fn is_non_interactive_slash_command_response(text: &str) -> bool {
    let mentions_usage_and_exit = text.contains("/usage") && text.contains("/exit");
    let says_entered_commands =
        text.contains("i see you've entered") || text.contains("you've entered two slash commands");
    let says_no_slash_command = text.contains("available custom slash commands")
        && text.contains("don't see these commands");
    let says_usage_is_cli_only = text
        .contains("token usage and statistics are typically displayed by the cli interface")
        || text.contains("i don't have direct access to those metrics");

    mentions_usage_and_exit
        && (says_entered_commands || says_no_slash_command || says_usage_is_cli_only)
}

fn is_workspace_trust_prompt(text: &str) -> bool {
    text.contains("quick safety check")
        && text.contains("trust this folder")
        && text.contains("yes, i trust this folder")
}

fn is_cli_activity_stats_response(text: &str) -> bool {
    let has_activity_overview = text.contains("favorite model:") || text.contains("total tokens:");
    let has_session_cost_summary =
        text.contains("total duration") && text.contains("usage:") && text.contains("cache read");

    has_activity_overview || has_session_cost_summary
}

/// Extract percentage near a label (e.g., "Current session")
/// Returns the percentage as "used" (not remaining)
fn extract_percent_near_label(text: &str, label: &str) -> Option<f64> {
    let label_normalized = normalized_for_label_search(label);
    let lines: Vec<&str> = text.lines().collect();

    // Find the line containing the label
    for (idx, line) in lines.iter().enumerate() {
        if normalized_for_label_search(line).contains(&label_normalized) {
            // Look in the next few lines for a percentage
            for (offset, next_line) in lines.iter().skip(idx).take(12).enumerate() {
                if offset > 0 && starts_next_usage_section(next_line, &label_normalized) {
                    break;
                }
                if let Some(pct) = parse_percent_line(next_line) {
                    return Some(pct);
                }
            }
        }
    }

    None
}

/// Extract all percentages from text in order
fn extract_all_percents(text: &str) -> Vec<f64> {
    let re = match Regex::new(
        r"(\d{1,3}(?:\.\d+)?)\s*%\s*(used|spent|consumed|left|remaining|available)",
    ) {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    let mut results = Vec::new();
    let lower = text.to_lowercase();

    for caps in re.captures_iter(&lower) {
        if let (Some(val_match), Some(kind_match)) = (caps.get(1), caps.get(2))
            && let Ok(val) = val_match.as_str().parse::<f64>()
        {
            let kind = kind_match.as_str();
            let used = if matches!(kind, "left" | "remaining" | "available") {
                (100.0 - val).max(0.0)
            } else {
                val.min(100.0)
            };
            results.push(used);
        }
    }

    results
}

fn is_exhausted_short_form(clean_lower: &str) -> bool {
    clean_lower.contains("out of extra usage") || clean_lower.contains("hit your limit")
}

/// Extract email address from text
fn extract_email(text: &str) -> Option<String> {
    // Try explicit patterns first
    let patterns = [
        r"Account:\s*([^\s@]+@[^\s@]+\.[^\s]+)",
        r"Email:\s*([^\s@]+@[^\s@]+\.[^\s]+)",
        r"([A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,})",
    ];

    for pattern in patterns {
        if let Ok(re) = Regex::new(pattern)
            && let Some(caps) = re.captures(text)
            && let Some(m) = caps.get(1)
        {
            return Some(m.as_str().trim().to_string());
        }
    }

    None
}

/// Extract login method / plan name from text
fn extract_login_method(text: &str) -> Option<String> {
    // Look for explicit "Login method:" line
    if let Ok(re) = Regex::new(r"(?i)login\s+method:\s*(.+)")
        && let Some(caps) = re.captures(text)
        && let Some(m) = caps.get(1)
    {
        let method = m.as_str().trim();
        if !method.is_empty() {
            return Some(clean_plan_name(method));
        }
    }

    // Look for "Claude <plan>" patterns
    if let Ok(re) = Regex::new(r"(?i)(claude\s+(?:max|pro|ultra|team|free)[a-z0-9\s._-]*)")
        && let Some(caps) = re.captures(text)
        && let Some(m) = caps.get(1)
    {
        let plan = m.as_str().trim();
        if !plan.to_lowercase().contains("code") {
            return Some(clean_plan_name(plan));
        }
    }

    None
}

/// Extract reset description near a label
fn extract_reset_description(text: &str, label: &str) -> Option<String> {
    let label_normalized = normalized_for_label_search(label);
    let lines: Vec<&str> = text.lines().collect();

    for (idx, line) in lines.iter().enumerate() {
        if normalized_for_label_search(line).contains(&label_normalized) {
            // Look in the next few lines for "Resets"
            for (offset, next_line) in lines.iter().skip(idx).take(14).enumerate() {
                if offset > 0 && starts_next_usage_section(next_line, &label_normalized) {
                    break;
                }
                let lower = next_line.to_lowercase();
                if lower.contains("resets") {
                    // Extract the reset info
                    if let Some(pos) = lower.find("resets") {
                        let reset_part = &next_line[pos..];
                        return Some(reset_part.trim().to_string());
                    }
                }
            }
        }
    }

    None
}

/// Extract a "resets ..." suffix from a short single-line status.
fn extract_inline_reset_description(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let pos = lower.find("resets")?;
    Some(text[pos..].trim().to_string())
}

/// Clean up a plan name by removing ANSI codes and extra whitespace
fn clean_plan_name(text: &str) -> String {
    let cleaned = strip_ansi(text);
    // Remove bracketed codes like [22m
    let re = Regex::new(r"\[\d+m").unwrap_or_else(|_| Regex::new(".^").unwrap());
    let result = re.replace_all(&cleaned, "");
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn probe_cache_roundtrip_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_cached_probe_output(dir.path()).is_none());
        store_cached_probe_output(dir.path(), "Current session 12% used");
        assert_eq!(
            load_cached_probe_output(dir.path()).as_deref(),
            Some("Current session 12% used")
        );
        let stale = ClaudeProbeCache {
            captured_at_unix: unix_now_secs() - CLAUDE_PROBE_CACHE_TTL.as_secs() - 5,
            output: "Current session 12% used".to_string(),
        };
        std::fs::write(
            dir.path().join(CLAUDE_PROBE_CACHE_FILE),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        assert!(load_cached_probe_output(dir.path()).is_none());
    }

    #[test]
    fn passive_probe_env_disables_autoupdater_and_color() {
        let env = claude_passive_probe_env(HashMap::new());
        assert_eq!(
            env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );
        assert_eq!(env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("CLAUDE_CODE_ENABLE_CFC").map(String::as_str),
            Some("0")
        );
    }

    #[test]
    fn startup_dialog_is_reported_by_name_instead_of_missing_usage() {
        // Claude Code 2.1.293 fullscreen offer as the probe saw it: the typed
        // /usage landed in the dialog and never reached the prompt.
        let output = "\u{1b}[1mTry the new fullscreen renderer?\u{1b}[0m\r\n\
            · Flicker-free output\r\n\
            · Mouse support — click to move your cursor or expand results\r\n\
            ❯ Yes, try it\r\n  Not now\r\n";
        let err = ClaudeProvider::new().parse_cli_output(output).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("stopped at a startup question"),
            "{message}"
        );
        assert!(
            message.contains("Try the new fullscreen renderer?"),
            "{message}"
        );
        assert!(should_fallback_from_claude_cli_error(&err));
    }

    #[test]
    fn startup_dialog_title_survives_squashed_conpty_spacing() {
        assert_eq!(
            claude_startup_dialog_title("ClaudeinChromeextension  detected\nYes, use my browser"),
            Some("Claude in Chrome extension detected")
        );
        assert_eq!(claude_startup_dialog_title("Current session 2% used"), None);
    }

    #[test]
    fn usage_screen_wins_over_a_leftover_dialog_title() {
        let output = "Try the new fullscreen renderer?\n\
            Current session\n██ 2% used\nResets 3pm (Europe/Berlin)\n\
            Current week (all models)\n████ 82% used\nResets Oct 12 (Europe/Berlin)\n";
        let result = ClaudeProvider::new().parse_cli_output(output).unwrap();
        assert_eq!(result.usage.primary.used_percent, 2.0);
    }

    #[test]
    fn probe_session_id_is_reused_from_probe_directory() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_probe_session_id(dir.path());
        let second = load_or_create_probe_session_id(dir.path());
        assert_eq!(first, second);
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        let args = claude_probe_launch_args(&first);
        assert_eq!(
            args,
            vec![
                "--setting-sources".to_string(),
                "user".to_string(),
                "--allowed-tools".to_string(),
                String::new(),
                "--settings".to_string(),
                r#"{"remoteControlAtStartup":false,"tui":"default"}"#.to_string(),
                "--session-id".to_string(),
                first,
            ]
        );
    }

    #[test]
    fn probe_session_jsonl_cleanup_removes_transcript_files() {
        assert_eq!(
            claude_project_dir_name(std::path::Path::new(
                r"C:\Users\erwin\AppData\Local\CodexBar\claude-usage-probe"
            )),
            "C--Users-erwin-AppData-Local-CodexBar-claude-usage-probe"
        );
        assert_eq!(
            claude_project_dir_name(std::path::Path::new(
                "/Users/me/Library/Application Support/x"
            )),
            "-Users-me-Library-Application-Support-x"
        );
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("session.jsonl");
        std::fs::write(&jsonl, "{}").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "x").unwrap();
        cleanup_probe_session_jsonl(dir.path());
        assert!(!jsonl.exists());
        assert!(dir.path().join("keep.txt").exists());
    }

    #[test]
    fn parses_current_cli_usage_screen() {
        let provider = ClaudeProvider::new();
        let output = r#"
Status   Config   Usage

  Current session
  ██████████████████████████████████████████████████ 100% used
  Resets 12pm (America/Bogota)

  Current week (all models)
  ████████████████████████▌                          49% used
  Resets Apr 3, 2pm (America/Bogota)

  Extra usage
  ██▍                                                4% used
  $3.31 / $70.00 spent · Resets Apr 1 (America/Bogota)
"#;

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.source_label, "cli");
        assert_eq!(result.usage.primary.used_percent, 100.0);
        assert_eq!(
            result.usage.primary.reset_description.as_deref(),
            Some("Resets 12pm (America/Bogota)")
        );

        let weekly = result
            .usage
            .secondary
            .expect("weekly usage should be present");
        assert_eq!(weekly.used_percent, 49.0);
        assert_eq!(
            weekly.reset_description.as_deref(),
            Some("Resets Apr 3, 2pm (America/Bogota)")
        );
    }

    #[test]
    fn parses_exhausted_short_form_as_full_session_usage() {
        let provider = ClaudeProvider::new();
        let output = "You're out of extra usage · resets 12pm (America/Bogota)";

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.usage.primary.used_percent, 100.0);
        assert_eq!(
            result.usage.primary.reset_description.as_deref(),
            Some("resets 12pm (America/Bogota)")
        );
    }

    #[test]
    fn parses_hit_limit_short_form_as_full_session_usage() {
        let provider = ClaudeProvider::new();
        let output = "You've hit your limit \u{00b7} resets 3:20pm (Asia/Shanghai)";

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.usage.primary.used_percent, 100.0);
        assert_eq!(
            result.usage.primary.reset_description.as_deref(),
            Some("resets 3:20pm (Asia/Shanghai)")
        );
    }

    #[test]
    fn parses_remaining_available_and_decimal_percentages() {
        let provider = ClaudeProvider::new();
        let output = r#"
Status   Config   Usage

  Current session
  12.5% remaining
  Resets 8pm

  Current week (all models)
  4% available
  Resets Apr 4, 2pm

  Current week (Sonnet only)
  1% consumed
"#;

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.usage.primary.used_percent, 87.5);
        assert_eq!(
            result.usage.primary.reset_description.as_deref(),
            Some("Resets 8pm")
        );

        let weekly = result
            .usage
            .secondary
            .expect("weekly usage should be present");
        assert_eq!(weekly.used_percent, 96.0);
        assert_eq!(
            weekly.reset_description.as_deref(),
            Some("Resets Apr 4, 2pm")
        );

        let sonnet = result
            .usage
            .extra_rate_windows
            .iter()
            .find(|window| window.id == "claude-weekly-scoped-sonnet")
            .expect("sonnet usage should be present");
        assert_eq!(sonnet.window.used_percent, 1.0);
    }

    #[test]
    fn parses_all_cli_model_scoped_weekly_limits() {
        let provider = ClaudeProvider::new();
        let output = r#"
Current session
10% used
Resets 12pm (America/Bogota)

Current week (all models)
20% used
Resets Apr 3, 2pm (America/Bogota)

Current week (Sonnet only)
30% used
Resets Apr 4, 2pm (America/Bogota)

Current week (Opus only)
40% used
Resets Apr 5, 2pm (America/Bogota)
"#;

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.usage.extra_rate_windows.len(), 2);
        assert_eq!(
            result.usage.extra_rate_windows[0].id,
            "claude-weekly-scoped-sonnet"
        );
        assert_eq!(result.usage.extra_rate_windows[0].title, "Sonnet only");
        assert_eq!(result.usage.extra_rate_windows[0].window.used_percent, 30.0);
        assert_eq!(
            result.usage.extra_rate_windows[1].id,
            "claude-weekly-scoped-opus"
        );
        assert!(result.usage.model_specific.is_none());
    }

    #[test]
    fn scoped_weekly_parser_handles_non_ascii_labels_and_reset_prefixes() {
        let now = "2026-04-02T18:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let limits = extract_cli_scoped_weekly_limits(
            "Current week (A€€)\n10% used\nİResets Apr 3 at 2pm (America/Bogota)",
            now,
        );

        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].title, "A€€");
        assert_eq!(
            limits[0].window.resets_at,
            Some("2026-04-03T19:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn resolves_cli_reset_occurrences_in_the_reported_timezone() {
        let now = "2026-04-02T18:00:00Z".parse::<DateTime<Utc>>().unwrap();

        assert_eq!(
            parse_claude_reset_date("Resets Apr 3, 2027, 2pm (America/Bogota)", now, None),
            Some("2027-04-03T19:00:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_claude_reset_date("Resets Apr 3, 2pm (America/Bogota)", now, None),
            Some("2026-04-03T19:00:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_claude_reset_date("Resets 12pm (America/Bogota)", now, None),
            Some("2026-04-03T17:00:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_claude_reset_date("ResetsApr3at2pm(America/Bogota)", now, None),
            Some("2026-04-03T19:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn timezone_less_resets_use_the_supplied_system_zone() {
        let now = "2026-03-07T18:00:00Z".parse::<DateTime<Utc>>().unwrap();

        assert_eq!(
            parse_claude_reset_date_in_system_zone(
                "Resets Mar 8 at 3:30am",
                now,
                None,
                "America/New_York".parse().unwrap(),
            ),
            Some("2026-03-08T07:30:00Z".parse().unwrap())
        );
        assert_eq!(
            parse_claude_reset_date_in_system_zone(
                "Resets Mar 8 at 3:30am (America/Los_Angeles)",
                now,
                None,
                "America/New_York".parse().unwrap(),
            ),
            Some("2026-03-08T10:30:00Z".parse().unwrap())
        );
    }

    #[test]
    fn parses_compact_usage_screen() {
        let provider = ClaudeProvider::new();
        let output = r#"
Settings:StatusConfigUsage(tabtocycle)
Loadingusagedata...
Currentsession
6%used
Resets4:29am(Asia/Calcutta)
Currentweek(allmodels)
4%used
ResetsFeb12at1:29pm(Asia/Calcutta)
Currentweek(Sonnetonly)
1%used
ResetsFeb12at1:29pm(Asia/Calcutta)
"#;

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.usage.primary.used_percent, 6.0);
        assert_eq!(
            result.usage.primary.reset_description.as_deref(),
            Some("Resets4:29am(Asia/Calcutta)")
        );
        assert_eq!(
            result
                .usage
                .secondary
                .expect("weekly usage should be present")
                .used_percent,
            4.0
        );
        let sonnet = result
            .usage
            .extra_rate_windows
            .iter()
            .find(|window| window.id == "claude-weekly-scoped-sonnet")
            .expect("sonnet usage should be present");
        assert_eq!(result.usage.extra_rate_windows.len(), 1);
        assert_eq!(sonnet.title, "Sonnet only");
        assert_eq!(sonnet.window.used_percent, 1.0);
    }

    #[test]
    fn does_not_promote_weekly_reset_to_session() {
        let provider = ClaudeProvider::new();
        let output = r#"
Current session
17% used
Current week (all models)
4% used
Resets Dec 24 at 3:59pm (Europe/Paris)
"#;

        let result = provider.parse_cli_output(output).expect("should parse");

        assert_eq!(result.usage.primary.used_percent, 17.0);
        assert_eq!(result.usage.primary.reset_description, None);
        assert_eq!(
            result
                .usage
                .secondary
                .expect("weekly usage should be present")
                .reset_description
                .as_deref(),
            Some("Resets Dec 24 at 3:59pm (Europe/Paris)")
        );
    }

    #[test]
    fn rejects_cli_output_without_usage_markers() {
        let provider = ClaudeProvider::new();
        let output = "Claude Code on Windows requires git-bash.";

        let err = provider
            .parse_cli_output(output)
            .expect_err("should reject non-usage output");

        assert!(matches!(err, ProviderError::Parse(_)));
        assert_eq!(
            err.to_string(),
            "Parse error: Claude CLI did not return usage data"
        );
    }

    #[test]
    fn cli_parse_usage_error_can_fallback_to_oauth() {
        let err = ProviderError::Parse("Claude CLI did not return usage data".to_string());

        assert!(should_fallback_from_claude_cli_error(&err));
    }

    #[test]
    fn cli_auth_error_does_not_fallback_to_oauth() {
        assert!(!should_fallback_from_claude_cli_error(
            &ProviderError::AuthRequired
        ));
    }

    #[test]
    fn auto_fetch_error_keeps_all_source_failures() {
        let err = claude_auto_fetch_error(vec![
            ("OAuth", ProviderError::OAuth("token expired".to_string())),
            ("Web", ProviderError::NoCookies),
            (
                "CLI",
                ProviderError::Parse("Empty output from Claude CLI".to_string()),
            ),
        ]);

        assert_eq!(
            err.to_string(),
            "Claude usage failed from all configured sources. OAuth: OAuth error: token expired; Web: No cookies available for web API; CLI: Parse error: Empty output from Claude CLI"
        );
    }

    #[test]
    fn auto_fetch_error_marks_browser_sign_in_when_oauth_refused_without_cookies() {
        let refused = ClaudeOAuthFetcher::rate_limited_error(Duration::from_secs(1));
        let err = claude_auto_fetch_error(vec![
            ("Web", ProviderError::NoCookies),
            ("OAuth", refused),
            (
                "CLI",
                ProviderError::Parse("Claude CLI did not return usage data".to_string()),
            ),
        ]);
        assert!(
            err.to_string().ends_with(CLAUDE_BROWSER_SIGN_IN_MARKER),
            "{err}"
        );

        // Rate limited alone (cookies were readable) is not a browser problem.
        let err = claude_auto_fetch_error(vec![(
            "OAuth",
            ClaudeOAuthFetcher::rate_limited_error(Duration::from_secs(1)),
        )]);
        assert!(!err.to_string().contains(CLAUDE_BROWSER_SIGN_IN_MARKER));
        // A real sign-out stays unmarked.
        let err = claude_auto_fetch_error(vec![
            ("Web", ProviderError::NoCookies),
            ("OAuth", ProviderError::OAuth("token expired".to_string())),
        ]);
        assert!(!err.to_string().contains(CLAUDE_BROWSER_SIGN_IN_MARKER));
    }

    #[test]
    fn trust_prompt_answer_moves_past_a_focused_no_exit_option() {
        // Claude Code 2.1.293 trust dialog as the probe captured it.
        let screen = "Accessing workspace:\r\n\
            C:\\Users\\x\\AppData\\Local\\CodexBar\\claude-usage-probe\r\n\
            Quick safety check: Is this a project you created or one you trust?\r\n\
            Security guide\r\n\
            \u{1b}[2G❯\u{1b}[4GNo,\u{1b}[8Gexit\r\n\
            \u{1b}[4GYes,\u{1b}[9GI\u{1b}[11Gtrust\u{1b}[17Gthis\u{1b}[22Gfolder\r\n\
            Enter to confirm · Esc to cancel\r\n";
        let lowered = strip_ansi(screen).to_lowercase();
        assert!(is_workspace_trust_prompt(&lowered), "{lowered}");
        assert_eq!(
            claude_trust_prompt_answer_keys(&lowered),
            CLAUDE_TRUST_ACCEPT_SECOND_OPTION
        );
    }

    #[test]
    fn trust_prompt_answer_keeps_enter_when_yes_is_listed_first() {
        let lowered = "quick safety check: is this a project you trust?\n\
            ❯ 1. yes, i trust this folder\n  2. no, exit\n\
            enter to confirm · esc to cancel";
        assert_eq!(
            claude_trust_prompt_answer_keys(lowered),
            CLAUDE_TRUST_ACCEPT_FIRST_OPTION
        );
    }

    #[test]
    fn rejects_claude_2_1_non_interactive_slash_response() {
        let provider = ClaudeProvider::new();
        let output = r#"
I see you've entered `/usage` and `/exit`.

**Usage**: Token usage and statistics are typically displayed by the CLI interface itself. I don't have direct access to those metrics through my available tools.

**Exit**: I'll end the session here. Goodbye!
"#;

        let err = provider
            .parse_cli_output(output)
            .expect_err("should reject non-interactive slash command response");

        assert!(matches!(err, ProviderError::Other(_)));
        assert_eq!(
            err.to_string(),
            "Claude CLI treated /usage as a normal prompt instead of opening the interactive usage screen. Use Auto, OAuth, or Web mode for Claude usage."
        );
    }

    #[test]
    fn rejects_legacy_non_interactive_slash_response() {
        let provider = ClaudeProvider::new();
        let output = r#"
I see you've entered two slash commands:

1. `/usage` - This appears to be a request to check usage information
2. `/exit` - This appears to be a request to exit

However, looking at the available custom slash commands, I don't see these commands defined.
"#;

        let err = provider
            .parse_cli_output(output)
            .expect_err("should reject non-interactive slash command response");

        assert!(matches!(err, ProviderError::Other(_)));
    }

    #[test]
    fn rejects_cli_activity_stats_without_plan_limits() {
        let provider = ClaudeProvider::new();
        let output = r#"
❯ /usage

Status   Config   Usage   Stats

Overview  Models

Favorite model: glm-4.6        Total tokens: 263.3k
Sessions: 6                    Longest session: 18s
Active days: 2/10              Longest streak: 1 day
"#;

        let err = provider
            .parse_cli_output(output)
            .expect_err("should reject local activity stats");

        assert!(matches!(err, ProviderError::Other(_)));
        assert_eq!(
            err.to_string(),
            "Claude CLI /usage opened, but this Claude version returned local activity stats instead of plan limit percentages. Use Auto, OAuth, or Web mode for Claude limits."
        );
    }

    #[test]
    fn rejects_ansi_spaced_cli_activity_stats_without_plan_limits() {
        let provider = ClaudeProvider::new();
        let output = "\x1b[2CTotal\x1b[1Ccost:\x1b[12C$0.0000\n\
                      \x1b[2CTotal\x1b[1Cduration\x1b[1C(API):\x1b[2C0s\n\
                      \x1b[2CUsage:\x1b[17C0\x1b[1Cinput,\x1b[1C0\x1b[1Coutput,\x1b[1C0\x1b[1Ccache\x1b[1Cread";

        let err = provider
            .parse_cli_output(output)
            .expect_err("should reject ANSI-spaced local activity stats");

        assert!(matches!(err, ProviderError::Other(_)));
    }

    #[test]
    fn accepts_plan_limits_followed_by_activity_stats() {
        // Claude Code 2.1.27x on Windows prints the exit summary (cost,
        // duration, cache tokens) after the /usage view when the probe ends.
        let provider = ClaudeProvider::new();
        let output = r#"
❯ /usage

Status   Config   Usage   Stats

Current session
███████░░░░░░░░░░░░░░░░░░░░░░ 19% used
Resets 3pm (Europe/Berlin)

Current week (all models)
█████████░░░░░░░░░░░░░░░░░░░░ 31% used
Resets Sep 19, 4pm (Europe/Berlin)

Total cost:            $0.0000
Total duration (API):  0s
Usage:                 0 input, 0 output, 0 cache read
"#;

        let result = provider
            .parse_cli_output(output)
            .expect("plan limits should win over trailing activity stats");

        assert_eq!(result.usage.primary.used_percent, 19.0);
        assert_eq!(
            result
                .usage
                .secondary
                .as_ref()
                .map(|window| window.used_percent),
            Some(31.0)
        );
    }

    #[test]
    fn parses_usage_rows_placed_by_cursor_position() {
        // Claude Code 2.1.289 through ConPTY: no line breaks, each row starts
        // with ESC[<row>;<col>H. Both windows used to read 18% with one shared
        // reset text and no reset time.
        let provider = ClaudeProvider::new();
        let output = "\x1b[17;3HUsage:\x1b[17C0\x1b[1Cinput,\x1b[1C0\x1b[1Coutput\
                      \x1b[19;3HCurrent\x1b[1Csession\
                      \x1b[20;3H█████████\x1b[42C18%\x1b[1Cused\
                      \x1b[21;3HResets\x1b[1C3:30am\x1b[1C(America/Los_Angeles)\
                      \x1b[23;3HCurrent\x1b[1Cweek\x1b[1C(all\x1b[1Cmodels)\
                      \x1b[24;3H██████████████████████████████████\x1b[17C68%\x1b[1Cused\
                      \x1b[25;3HResets\x1b[1COct\x1b[1C6,\x1b[1C4am\x1b[1C(America/Los_Angeles)\
                      \x1b[27;3HWhat's\x1b[1Ccontributing\x1b[1Cto\x1b[1Cyour\x1b[1Climits\x1b[1Cusage?";

        let result = provider.parse_cli_output(output).expect("should parse");

        let session = &result.usage.primary;
        assert_eq!(session.used_percent, 18.0);
        assert_eq!(
            session.reset_description.as_deref(),
            Some("Resets 3:30am (America/Los_Angeles)")
        );
        assert!(session.resets_at.is_some());

        let weekly = result
            .usage
            .secondary
            .expect("weekly usage should be present");
        assert_eq!(weekly.used_percent, 68.0);
        assert_eq!(
            weekly.reset_description.as_deref(),
            Some("Resets Oct 6, 4am (America/Los_Angeles)")
        );
        assert!(weekly.resets_at.is_some());
    }

    // ── Upstream 0.50.1 #2516: revoked vs missing OAuth ────────────────────────

    #[test]
    fn oauth_revoked_error_is_detected() {
        assert!(is_oauth_revoked_error(&ProviderError::OAuthRevoked(
            "revoked".to_string()
        )));
        assert!(!is_oauth_revoked_error(&ProviderError::OAuth(
            "expired".to_string()
        )));
        assert!(!is_oauth_revoked_error(&ProviderError::AuthRequired));
    }

    #[test]
    fn rate_limited_cli_probe_backs_off_only_after_a_failure() {
        note_rate_limited_cli_probe(true);
        assert!(!rate_limited_cli_probe_failed_recently());
        note_rate_limited_cli_probe(false);
        assert!(rate_limited_cli_probe_failed_recently());
        note_rate_limited_cli_probe(true);
        assert!(!rate_limited_cli_probe_failed_recently());
    }

    /// A reading with session and weekly windows. The CLI prints its reset
    /// times to the minute, so they sit a little off the OAuth ones.
    fn reading(source: &str, session: f64, weekly: f64) -> ProviderFetchResult {
        let base = chrono::DateTime::parse_from_rfc3339("2026-10-07T12:00:00Z")
            .expect("base time")
            .with_timezone(&Utc);
        let skew = if source == AUTO_SOURCE_CLI { 40 } else { 0 };
        let reset = |hours: i64| Some(base + chrono::Duration::seconds(hours * 3600 + skew));
        let usage =
            UsageSnapshot::new(RateWindow::with_details(session, Some(300), reset(2), None))
                .with_secondary(RateWindow::with_details(
                    weekly,
                    Some(10080),
                    reset(70),
                    None,
                ));
        ProviderFetchResult::new(usage, source)
    }

    fn percents(result: &ProviderFetchResult) -> (f64, f64) {
        (
            result.usage.primary.used_percent,
            result
                .usage
                .secondary
                .as_ref()
                .expect("weekly")
                .used_percent,
        )
    }

    /// Replays readings against one state, each `secs` after the start, and
    /// returns what Auto would report for them.
    fn replay(
        last: &mut LastAutoReading,
        start: Instant,
        readings: Vec<(u64, &'static str, ProviderFetchResult)>,
    ) -> Vec<(f64, f64)> {
        readings
            .into_iter()
            .map(|(secs, source, mut result)| {
                let at = start + Duration::from_secs(secs);
                steady_percents(&mut result, last, source, at, at);
                percents(&result)
            })
            .collect()
    }

    // Windows bench, 45 minutes with rising usage: OAuth 18, then throttled
    // polls answered from the cached CLI screen with 16, then OAuth 19.
    #[test]
    fn older_cached_cli_reading_does_not_take_the_percentage_back() {
        let oauth = steady_across_source_change(
            reading(AUTO_SOURCE_OAUTH, 18.0, 17.0),
            AUTO_SOURCE_OAUTH,
            Instant::now(),
        );
        assert_eq!(percents(&oauth), (18.0, 17.0));

        // The CLI screen was read before that OAuth poll and is replayed from
        // the cache on every throttled poll.
        let cached_at = Instant::now();
        for _ in 0..3 {
            let cached = steady_across_source_change(
                reading(AUTO_SOURCE_CLI, 16.0, 17.0),
                AUTO_SOURCE_CLI,
                cached_at,
            );
            assert_eq!(percents(&cached), (18.0, 17.0));
            assert_eq!(cached.source_label, "cli");
        }

        let oauth = steady_across_source_change(
            reading(AUTO_SOURCE_OAUTH, 19.0, 17.0),
            AUTO_SOURCE_OAUTH,
            Instant::now(),
        );
        assert_eq!(percents(&oauth), (19.0, 17.0));
    }

    // DreamyTalesPAN/CodexBar-Display#531: OAuth 7 / 9, a throttled poll
    // answered by the CLI screen with 8 / 10, then OAuth 7 / 9 again.
    #[test]
    fn source_change_within_a_rounding_step_keeps_the_percentage() {
        let mut last = LastAutoReading::default();
        let reported = replay(
            &mut last,
            Instant::now(),
            vec![
                (0, AUTO_SOURCE_OAUTH, reading(AUTO_SOURCE_OAUTH, 7.0, 9.0)),
                (30, AUTO_SOURCE_CLI, reading(AUTO_SOURCE_CLI, 8.0, 10.0)),
                (60, AUTO_SOURCE_OAUTH, reading(AUTO_SOURCE_OAUTH, 7.0, 9.0)),
                // Fractional OAuth values against the floored CLI screen.
                (90, AUTO_SOURCE_OAUTH, reading(AUTO_SOURCE_OAUTH, 7.6, 9.4)),
                (120, AUTO_SOURCE_CLI, reading(AUTO_SOURCE_CLI, 7.0, 9.0)),
            ],
        );
        assert_eq!(
            reported,
            vec![(7.0, 9.0), (7.0, 9.0), (7.0, 9.0), (7.6, 9.4), (7.6, 9.4)]
        );
    }

    #[test]
    fn higher_reading_from_another_source_passes_through() {
        let mut last = LastAutoReading::default();
        let reported = replay(
            &mut last,
            Instant::now(),
            vec![
                (0, AUTO_SOURCE_OAUTH, reading(AUTO_SOURCE_OAUTH, 17.0, 17.0)),
                // Each window is judged on its own: session moved, weekly did not.
                (30, AUTO_SOURCE_CLI, reading(AUTO_SOURCE_CLI, 19.0, 18.0)),
                // The CLI number is now on display; an OAuth reading below it
                // is held back, one above it passes.
                (
                    60,
                    AUTO_SOURCE_OAUTH,
                    reading(AUTO_SOURCE_OAUTH, 18.4, 17.0),
                ),
                (
                    90,
                    AUTO_SOURCE_OAUTH,
                    reading(AUTO_SOURCE_OAUTH, 21.0, 18.0),
                ),
            ],
        );
        assert_eq!(
            reported,
            vec![(17.0, 17.0), (19.0, 17.0), (19.0, 17.0), (21.0, 18.0)]
        );
    }

    #[test]
    fn same_source_reading_always_passes_through() {
        let mut last = LastAutoReading::default();
        let reported = replay(
            &mut last,
            Instant::now(),
            vec![
                (0, AUTO_SOURCE_OAUTH, reading(AUTO_SOURCE_OAUTH, 18.0, 17.0)),
                (
                    30,
                    AUTO_SOURCE_OAUTH,
                    reading(AUTO_SOURCE_OAUTH, 16.0, 17.5),
                ),
            ],
        );
        assert_eq!(reported, vec![(18.0, 17.0), (16.0, 17.5)]);
    }

    #[test]
    fn new_window_passes_through_at_once() {
        let mut last = LastAutoReading::default();
        let start = Instant::now();
        let mut oauth = reading(AUTO_SOURCE_OAUTH, 18.0, 17.0);
        steady_percents(&mut oauth, &mut last, AUTO_SOURCE_OAUTH, start, start);

        // The session rolled over: low number, reset time five hours later.
        let mut rolled = reading(AUTO_SOURCE_CLI, 2.0, 17.0);
        rolled.usage.primary.resets_at = rolled
            .usage
            .primary
            .resets_at
            .map(|at| at + chrono::Duration::hours(5));
        steady_percents(&mut rolled, &mut last, AUTO_SOURCE_CLI, start, start);
        assert_eq!(percents(&rolled), (2.0, 17.0));

        // The next OAuth reading for the new window is compared with that one.
        let mut next = reading(AUTO_SOURCE_OAUTH, 4.0, 17.0);
        next.usage.primary.resets_at = rolled.usage.primary.resets_at;
        steady_percents(&mut next, &mut last, AUTO_SOURCE_OAUTH, start, start);
        assert_eq!(percents(&next), (4.0, 17.0));
    }

    #[test]
    fn lower_reading_is_not_held_back_without_a_comparable_window() {
        let held_back = |mut previous: ProviderFetchResult, mut next: ProviderFetchResult| {
            let mut last = LastAutoReading::default();
            let start = Instant::now();
            steady_percents(&mut previous, &mut last, AUTO_SOURCE_OAUTH, start, start);
            steady_percents(&mut next, &mut last, AUTO_SOURCE_CLI, start, start);
            next.usage.primary.used_percent
        };
        let oauth = || reading(AUTO_SOURCE_OAUTH, 18.0, 17.0);
        let cli = || reading(AUTO_SOURCE_CLI, 16.0, 17.0);
        assert_eq!(held_back(oauth(), cli()), 18.0);

        // No reset time on the CLI screen: a new window cannot be ruled out.
        let mut no_reset = cli();
        no_reset.usage.primary.resets_at = None;
        assert_eq!(held_back(oauth(), no_reset), 16.0);

        // Another account.
        assert_eq!(
            held_back(
                oauth().with_account_identity("claude-account:a"),
                cli().with_account_identity("claude-account:b"),
            ),
            16.0
        );
        assert_eq!(
            held_back(
                oauth().with_account_identity("claude-account:a"),
                cli().with_account_identity("claude-account:a"),
            ),
            18.0
        );

        // A reached limit, on either side.
        assert_eq!(
            held_back(reading(AUTO_SOURCE_OAUTH, 100.0, 17.0), cli()),
            16.0
        );
        assert_eq!(
            held_back(reading(AUTO_SOURCE_OAUTH, 99.4, 17.0), {
                let mut full = cli();
                full.usage.primary.used_percent = 100.0;
                full
            }),
            100.0
        );

        // No active session on the OAuth side.
        let mut idle = oauth();
        idle.usage.primary = RateWindow::no_active_session();
        assert_eq!(held_back(idle, cli()), 16.0);
    }

    #[test]
    fn held_percentage_ends_with_the_age_of_the_reading_behind_it() {
        let mut last = LastAutoReading::default();
        let ttl = SOURCE_CHANGE_HOLD_TTL.as_secs();
        let reported = replay(
            &mut last,
            Instant::now(),
            vec![
                (0, AUTO_SOURCE_OAUTH, reading(AUTO_SOURCE_OAUTH, 18.0, 17.0)),
                // Held polls do not renew the reading behind the number.
                (300, AUTO_SOURCE_CLI, reading(AUTO_SOURCE_CLI, 16.0, 17.0)),
                (ttl, AUTO_SOURCE_CLI, reading(AUTO_SOURCE_CLI, 16.0, 17.0)),
                (
                    ttl + 1,
                    AUTO_SOURCE_CLI,
                    reading(AUTO_SOURCE_CLI, 16.0, 17.0),
                ),
            ],
        );
        assert_eq!(
            reported,
            vec![(18.0, 17.0), (18.0, 17.0), (18.0, 17.0), (16.0, 17.0)]
        );
    }

    #[test]
    fn cli_result_cache_round_trips() {
        let mut result = ProviderFetchResult::new(UsageSnapshot::new(RateWindow::new(42.0)), "cli");
        result.has_successful_claude_cli_quota = true;
        cache_cli_result(result.clone());
        let cached = cached_cli_result().expect("cached result within TTL");
        assert!((cached.usage.primary.used_percent - 42.0).abs() < 0.01);
        assert_eq!(cached.source_label, "cli");
        assert!(!cached.has_successful_claude_cli_quota);
    }

    #[test]
    fn cli_quota_without_credential_identity_cannot_prove_account_action() {
        let provider = ClaudeProvider::new();
        let result = provider
            .parse_cli_output("Current session\n25% used\nCurrent week (all models)\n40% used")
            .expect("CLI quota should parse");
        let result = mark_live_claude_cli_result(result);

        assert!(result.usage.account_email.is_none());
        assert!(result.has_successful_claude_cli_quota);
    }

    #[test]
    fn non_cli_fetch_result_does_not_prove_account_action() {
        let result = ProviderFetchResult::new(UsageSnapshot::new(RateWindow::new(42.0)), "oauth");

        assert!(!result.has_successful_claude_cli_quota);
    }
    #[test]
    fn cli_presence_maps_to_local_runtime_offline() {
        assert_eq!(
            ClaudeProvider::new().error_state_kind(&ProviderError::NotInstalled(
                "Claude CLI not found. Install from https://docs.claude.ai/claude-code".to_string(),
            )),
            crate::core::ProviderStateKind::LocalRuntimeOffline
        );
        // Other error kinds keep their default classification.
        assert_eq!(
            ClaudeProvider::new().error_state_kind(&ProviderError::AuthRequired),
            crate::core::ProviderStateKind::NeedsAuthentication
        );
    }
}
