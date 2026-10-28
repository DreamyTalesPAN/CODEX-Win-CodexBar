//! Manus provider implementation.
//!
//! Fetches credit balance using a Manus browser session token.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::core::{
    FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId, ProviderMetadata,
    RateWindow, SourceMode, UsageSnapshot,
};

const MANUS_CREDITS_URL: &str = "https://api.manus.im/user.v1.UserService/GetAvailableCredits";

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ManusCreditsResponse {
    #[serde(default)]
    total_credits: Option<f64>,
    #[serde(default)]
    free_credits: f64,
    #[serde(default)]
    periodic_credits: Option<f64>,
    #[serde(default)]
    addon_credits: Option<f64>,
    #[serde(default)]
    refresh_credits: Option<f64>,
    #[serde(default)]
    max_refresh_credits: Option<f64>,
    #[serde(default)]
    pro_monthly_credits: Option<f64>,
    #[serde(default)]
    event_credits: Option<f64>,
    #[serde(default)]
    next_refresh_time: Option<DateTime<Utc>>,
    #[serde(default)]
    refresh_interval: Option<String>,
}

pub struct ManusProvider {
    metadata: ProviderMetadata,
    client: Client,
}

impl ManusProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Manus,
                display_name: "Manus",
                session_label: "Credits",
                weekly_label: "Refresh",
                supports_opus: false,
                supports_credits: true,
                default_enabled: false,
                is_primary: false,
                dashboard_url: Some("https://manus.im"),
                status_page_url: None,
            },
            client: crate::core::credentialed_http_client_builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    async fn fetch_web(&self, raw_cookie: &str) -> Result<UsageSnapshot, ProviderError> {
        let token = session_token(raw_cookie).ok_or(ProviderError::NoCookies)?;
        let response = self
            .client
            .post(MANUS_CREDITS_URL)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("Origin", "https://manus.im")
            .header("Referer", "https://manus.im/")
            .header("Connect-Protocol-Version", "1")
            .body("{}")
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(ProviderError::AuthRequired);
        }
        if !response.status().is_success() {
            return Err(ProviderError::Other(format!(
                "Manus API returned status {}",
                response.status()
            )));
        }

        let body: Value = response
            .json()
            .await
            .map_err(|e| ProviderError::Parse(format!("Failed to parse Manus response: {e}")))?;
        let credits = parse_credits(body)?;
        snapshot_from_credits(&credits)
    }
}

fn session_token(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains('=') && !trimmed.contains(';') {
        return Some(trimmed.to_string());
    }
    for chunk in trimmed.split(';') {
        let (name, value) = chunk.trim().split_once('=')?;
        if name.trim().eq_ignore_ascii_case("session_id") && !value.trim().is_empty() {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn parse_credits(body: Value) -> Result<ManusCreditsResponse, ProviderError> {
    for key in ["data", "result", "response", "availableCredits"] {
        if let Some(value) = body.get(key)
            && value.is_object()
        {
            return serde_json::from_value(value.clone())
                .map_err(|e| ProviderError::Parse(format!("Failed to parse Manus credits: {e}")));
        }
    }
    if !body.is_object() {
        return Err(ProviderError::Parse(
            "Manus response was not an object".into(),
        ));
    }
    serde_json::from_value(body)
        .map_err(|e| ProviderError::Parse(format!("Failed to parse Manus credits: {e}")))
}

fn snapshot_from_credits(credits: &ManusCreditsResponse) -> Result<UsageSnapshot, ProviderError> {
    let primary = if let (Some(total), Some(remaining)) = (
        credits.pro_monthly_credits.filter(|total| *total > 0.0),
        credits.periodic_credits,
    ) {
        let used = (total - remaining).max(0.0);
        let percent = used / total * 100.0;
        RateWindow::with_details(
            percent,
            None,
            None,
            Some(credits.total_credits.map_or_else(
                || format!("{remaining:.0}/{total:.0} monthly credits"),
                |balance| {
                    format!(
                        "{balance:.0} total credits ({:.0} free)",
                        credits.free_credits
                    )
                },
            )),
        )
    } else if let Some(balance) = credits.total_credits {
        RateWindow::informational(format!("{balance:.0} total credits"))
    } else {
        RateWindow::informational("Monthly credit usage unavailable")
    };

    let mut snapshot = UsageSnapshot::new(primary);
    if let Some(balance) = credits.total_credits {
        snapshot = snapshot.with_login_method(format!("Balance: {:.0} credits", balance.round()));
    }

    if let (Some(total), Some(remaining)) = (
        credits.max_refresh_credits.filter(|total| *total > 0.0),
        credits.refresh_credits,
    ) {
        let used = (total - remaining).max(0.0);
        let secondary = RateWindow::with_details(
            used / total * 100.0,
            None,
            credits.next_refresh_time,
            Some(format!(
                "{:.0}/{:.0} refresh credits{}",
                remaining,
                total,
                credits
                    .refresh_interval
                    .as_deref()
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default()
            )),
        );
        snapshot = snapshot.with_secondary(secondary);
    }

    if let Some(balance) = credits.addon_credits {
        let addon = RateWindow::informational(format!("{balance:.0} add-on credits"));
        snapshot = snapshot.with_extra_rate_window("addon", "Add-on credits", addon);
    }
    if let Some(balance) = credits.event_credits {
        let event = RateWindow::informational(format!("{balance:.0} event credits"));
        snapshot = snapshot.with_extra_rate_window("event", "Event credits", event);
    }
    if snapshot.primary.is_informational
        && snapshot.secondary.is_none()
        && credits.total_credits.is_none()
        && snapshot.extra_rate_windows.is_empty()
    {
        return Err(ProviderError::Parse(
            "Manus response contains no usable credit quota or balance".into(),
        ));
    }
    Ok(snapshot)
}

impl Default for ManusProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ManusProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Manus
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        match ctx.source_mode {
            SourceMode::Auto | SourceMode::Web => {
                let cookie = match ctx.manual_cookie_header.as_deref() {
                    Some(cookie) => cookie.to_string(),
                    None => crate::providers::browser_cookie_header(&["manus.im"])?,
                };
                Ok(ProviderFetchResult::new(
                    self.fetch_web(&cookie).await?,
                    "web",
                ))
            }
            SourceMode::OAuth | SourceMode::Cli => {
                Err(ProviderError::UnsupportedSource(ctx.source_mode))
            }
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![SourceMode::Auto, SourceMode::Web]
    }

    fn supports_web(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manus_extracts_session_id_from_cookie_header() {
        assert_eq!(
            session_token("foo=bar; session_id=abc123; other=1").as_deref(),
            Some("abc123")
        );
    }
}

#[cfg(test)]
mod cx289a_tests {
    use super::*;
    use serde_json::json;
    fn build(value: Value) -> Result<UsageSnapshot, ProviderError> {
        snapshot_from_credits(&parse_credits(value)?)
    }
    #[test]
    fn review16_quota_less_response_preserves_all_balances() {
        let snapshot =
            build(json!({"totalCredits":47,"addonCredits":40,"eventCredits":7})).unwrap();
        assert!(snapshot.primary.is_informational);
        assert_eq!(
            snapshot.primary.reset_description.as_deref(),
            Some("47 total credits")
        );
        assert_eq!(
            snapshot.login_method.as_deref(),
            Some("Balance: 47 credits")
        );
        for (id, description) in [("addon", "40 add-on credits"), ("event", "7 event credits")] {
            let window = &snapshot
                .extra_rate_windows
                .iter()
                .find(|w| w.id == id)
                .unwrap()
                .window;
            assert!(window.is_informational);
            assert_eq!(window.reset_description.as_deref(), Some(description));
        }
    }

    #[test]
    fn review16_explicit_zero_balance_is_not_missing() {
        let snapshot = build(json!({"totalCredits":0})).unwrap();
        assert!(snapshot.primary.is_informational);
        assert_eq!(
            snapshot.primary.reset_description.as_deref(),
            Some("0 total credits")
        );
        assert!(build(json!({})).is_err());
    }

    #[test]
    fn review16_addon_only_does_not_invent_total_balance() {
        let snapshot = build(json!({"addonCredits":40})).unwrap();
        assert!(snapshot.primary.is_informational);
        assert_eq!(
            snapshot.primary.reset_description.as_deref(),
            Some("Monthly credit usage unavailable")
        );
        assert!(snapshot.login_method.is_none());
        assert_eq!(
            snapshot.extra_rate_windows[0]
                .window
                .reset_description
                .as_deref(),
            Some("40 add-on credits")
        );
    }

    #[test]
    fn cx289a_missing_quota_is_error() {
        for value in [
            json!({}),
            json!({"proMonthlyCredits":100}),
            json!({"periodicCredits":100}),
            json!({"maxRefreshCredits":100}),
        ] {
            assert!(build(value.clone()).is_err(), "{value}");
        }
    }
    #[test]
    fn cx289a_refresh_only_keeps_unknown_primary() {
        let snap = build(json!({"maxRefreshCredits":100,"refreshCredits":100})).unwrap();
        assert!(snap.primary.is_informational);
        assert_eq!(snap.secondary.unwrap().used_percent, 0.0);
    }
    #[test]
    fn addon_and_event_balances_are_information_not_zero_percent() {
        let snap = build(json!({
            "proMonthlyCredits":100,"periodicCredits":100,
            "addonCredits":40,"eventCredits":7
        }))
        .unwrap();
        for id in ["addon", "event"] {
            let window = &snap
                .extra_rate_windows
                .iter()
                .find(|w| w.id == id)
                .unwrap_or_else(|| panic!("{id} window"))
                .window;
            assert!(window.is_informational, "{id}");
        }
    }
    #[test]
    fn cx289a_real_zero_is_quota() {
        let snap = build(json!({"proMonthlyCredits":100,"periodicCredits":100})).unwrap();
        assert!(!snap.primary.is_informational);
        assert_eq!(snap.primary.used_percent, 0.0);
    }
}
