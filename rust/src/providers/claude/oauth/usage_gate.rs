//! One gate in front of the Claude OAuth usage endpoint, shared by every
//! codexbar process of this user and kept across restarts.
//!
//! The endpoint (`/api/oauth/usage`) is undocumented and throttles per
//! account. A 30-second poll from `serve`, plus one-off `usage` processes for
//! provider checks that each kept their own in-memory backoff, kept accounts
//! in a 429 lockout that outlived app restarts (anthropics/claude-code#30930).
//! So: at most one live request per `MIN_INTERVAL`, answered from the last
//! response in between, and after a 429 no request until the backoff has
//! passed, doubling with every consecutive 429.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;

/// Shortest time between two live usage requests for one sign-in.
pub(super) const MIN_INTERVAL: Duration = Duration::from_secs(3 * 60);
/// Backoff after the first 429 when the endpoint asks for less (it often
/// answers `Retry-After: 0` while it keeps refusing).
const FIRST_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// The doubling stops here; a longer `Retry-After` is still respected.
const MAX_ESCALATED_BACKOFF: Duration = Duration::from_secs(30 * 60);
const GATE_FILE: &str = "claude-oauth-usage-gate.json";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct GateState {
    /// Fingerprint of the access token `last_ok_body` was read with; another
    /// token never gets that answer. The block below is the account's, so a
    /// token refresh does not lift it.
    token: String,
    last_ok_at: Option<DateTime<Utc>>,
    last_ok_body: Option<String>,
    pub(super) blocked_until: Option<DateTime<Utc>>,
    strikes: u32,
}

#[derive(Debug, PartialEq)]
pub(super) enum Decision {
    /// Answer from this recent response without a request.
    Cached(String),
    /// The endpoint refused recently; no request before this time.
    Blocked(DateTime<Utc>),
    Fetch,
}

pub(super) fn token_fingerprint(access_token: &str) -> String {
    crate::core::sha256_hex(access_token.as_bytes())[..16].to_string()
}

pub(super) fn decide(state: &GateState, token: &str, now: DateTime<Utc>) -> Decision {
    if let Some(until) = state.blocked_until.filter(|until| *until > now) {
        return Decision::Blocked(until);
    }
    let recent = state
        .last_ok_at
        .is_some_and(|at| (now - at).to_std().is_ok_and(|age| age < MIN_INTERVAL));
    match &state.last_ok_body {
        Some(body) if recent && state.token == token => Decision::Cached(body.clone()),
        _ => Decision::Fetch,
    }
}

pub(super) fn after_success(token: &str, body: String, now: DateTime<Utc>) -> GateState {
    GateState {
        token: token.to_string(),
        last_ok_at: Some(now),
        last_ok_body: Some(body),
        blocked_until: None,
        strikes: 0,
    }
}

pub(super) fn after_rate_limit(
    state: &GateState,
    retry_after: Duration,
    now: DateTime<Utc>,
) -> GateState {
    let strikes = state.strikes.saturating_add(1);
    let escalated = FIRST_BACKOFF
        .saturating_mul(1 << (strikes - 1).min(8))
        .min(MAX_ESCALATED_BACKOFF);
    let wait = retry_after.max(escalated);
    GateState {
        blocked_until: Some(now + chrono::Duration::from_std(wait).unwrap_or_default()),
        strikes,
        ..state.clone()
    }
}

// The file is the shared copy; the in-memory one keeps the gate working in
// this process when the file cannot be read or written.
static MEMORY: Mutex<Option<GateState>> = Mutex::new(None);

fn gate_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    let base = dirs::data_local_dir().or_else(dirs::home_dir)?;
    Some(base.join("CodexBar").join(GATE_FILE))
}

pub(super) fn load() -> GateState {
    gate_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .or_else(|| MEMORY.lock().ok().and_then(|guard| guard.clone()))
        .unwrap_or_default()
}

pub(super) fn store(state: &GateState) {
    if let Ok(mut guard) = MEMORY.lock() {
        *guard = Some(state.clone());
    }
    let Some(path) = gate_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _created = std::fs::create_dir_all(parent);
    }
    let written = serde_json::to_vec(state)
        .map_err(anyhow::Error::from)
        .and_then(|json| crate::atomic_file::write_atomic(&path, &json));
    if let Err(err) = written {
        tracing::debug!(error = %err, "failed to persist the Claude OAuth usage gate");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn answers_from_the_last_response_within_the_interval() {
        let state = after_success("a", "{}".to_string(), at(0));
        assert_eq!(
            decide(&state, "a", at(179)),
            Decision::Cached("{}".to_string())
        );
        assert_eq!(decide(&state, "a", at(180)), Decision::Fetch);
        // Another sign-in never gets this account's answer.
        assert_eq!(decide(&state, "b", at(10)), Decision::Fetch);
    }

    #[test]
    fn a_429_blocks_every_token_and_doubles_until_the_cap() {
        let ok = after_success("a", "{}".to_string(), at(0));
        let first = after_rate_limit(&ok, Duration::ZERO, at(200));
        assert_eq!(decide(&first, "a", at(201)), Decision::Blocked(at(500)));
        assert_eq!(
            decide(&first, "refreshed", at(201)),
            Decision::Blocked(at(500))
        );
        assert_eq!(decide(&first, "a", at(500)), Decision::Fetch);

        let second = after_rate_limit(&first, Duration::ZERO, at(500));
        assert_eq!(second.blocked_until, Some(at(500 + 600)));
        let mut state = second;
        for _ in 0..10 {
            state = after_rate_limit(&state, Duration::ZERO, at(0));
        }
        assert_eq!(state.blocked_until, Some(at(30 * 60)));
        // A longer Retry-After than the escalation wins.
        let long = after_rate_limit(&ok, Duration::from_secs(3600), at(0));
        assert_eq!(long.blocked_until, Some(at(3600)));
    }

    #[test]
    fn a_success_clears_the_block_and_the_count() {
        let blocked = after_rate_limit(&GateState::default(), Duration::ZERO, at(0));
        assert_eq!(blocked.strikes, 1);
        let ok = after_success("a", "{}".to_string(), at(400));
        assert_eq!(ok.strikes, 0);
        assert_eq!(ok.blocked_until, None);
    }
}
