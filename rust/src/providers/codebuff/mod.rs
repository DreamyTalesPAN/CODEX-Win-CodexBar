//! Codebuff provider implementation.
//!
//! Fetches credit usage from Codebuff's REST API using an API key from
//! Preferences, Windows Credential Manager, `CODEBUFF_API_KEY`, or the
//! Manicode/Codebuff credentials file.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde_json::{Value, json};

use crate::core::{
    FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId, ProviderMetadata,
    RateWindow, SourceMode, UsageSnapshot,
};

const DEFAULT_API_BASE: &str = "https://www.codebuff.com";
const CODEBUFF_CREDENTIAL_TARGET: &str = "codexbar-codebuff";

fn clean_api_key(token: &str) -> Option<String> {
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

pub struct CodebuffProvider {
    metadata: ProviderMetadata,
    client: Client,
}

impl CodebuffProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Codebuff,
                display_name: "Codebuff",
                session_label: "Credits",
                weekly_label: "Weekly",
                supports_opus: false,
                supports_credits: true,
                default_enabled: false,
                is_primary: false,
                dashboard_url: Some("https://www.codebuff.com/usage"),
                status_page_url: None,
            },
            client: crate::core::credentialed_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn api_base() -> Result<Url, ProviderError> {
        let raw = std::env::var("CODEBUFF_API_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API_BASE.to_string());
        crate::providers::validated_https_url(&raw, "Codebuff API")
    }

    fn get_api_key(api_key: Option<&str>) -> Result<String, ProviderError> {
        Self::api_key_from_argument(api_key)
            .or_else(Self::api_key_from_keyring)
            .or_else(Self::api_key_from_env)
            .or_else(Self::api_key_from_credentials_file)
            .ok_or_else(|| {
                ProviderError::NotInstalled(
                    "Codebuff API key not found. Set it in Preferences → Providers, CODEBUFF_API_KEY, or sign in with Codebuff/Manicode."
                        .to_string(),
                )
            })
    }

    fn api_key_from_argument(api_key: Option<&str>) -> Option<String> {
        api_key.and_then(clean_api_key)
    }

    fn api_key_from_keyring() -> Option<String> {
        let entry = keyring::Entry::new(CODEBUFF_CREDENTIAL_TARGET, "api_key").ok()?;
        clean_api_key(&entry.get_password().ok()?)
    }

    fn api_key_from_env() -> Option<String> {
        clean_api_key(&std::env::var("CODEBUFF_API_KEY").ok()?)
    }

    fn api_key_from_credentials_file() -> Option<String> {
        let path = dirs::home_dir()?
            .join(".config")
            .join("manicode")
            .join("credentials.json");
        let text = std::fs::read_to_string(&path).ok()?;
        let json = serde_json::from_str::<Value>(&text).ok()?;

        ["apiKey", "api_key", "token", "accessToken", "access_token"]
            .iter()
            .find_map(|key| json.get(*key).and_then(|v| v.as_str()))
            .and_then(clean_api_key)
    }

    async fn fetch_usage_api(&self, ctx: &FetchContext) -> Result<UsageSnapshot, ProviderError> {
        let api_key = Self::get_api_key(ctx.api_key.as_deref())?;
        let base = Self::api_base()?;
        let usage_url = base
            .join("api/v1/usage")
            .map_err(|e| ProviderError::Other(format!("Invalid Codebuff API URL: {e}")))?;

        let usage_resp = self
            .client
            .post(usage_url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .json(&json!({ "fingerprintId": "codexbar-usage" }))
            .send()
            .await?;

        if usage_resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::AuthRequired);
        }
        if !usage_resp.status().is_success() {
            return Err(ProviderError::Other(format!(
                "Codebuff API returned status {}",
                usage_resp.status()
            )));
        }

        let usage: Value = usage_resp.json().await.map_err(|e| {
            ProviderError::Parse(format!("Failed to parse Codebuff usage response: {e}"))
        })?;

        let subscription = self.fetch_subscription(&base, &api_key).await;
        Self::snapshot_from_values(&usage, subscription.as_ref())
    }

    async fn fetch_subscription(&self, base: &Url, api_key: &str) -> Option<Value> {
        let client = crate::core::credentialed_http_client_builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .ok()?;
        let url = base.join("api/user/subscription").ok()?;
        let resp = client
            .get(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()
    }

    fn snapshot_from_values(
        usage: &Value,
        subscription: Option<&Value>,
    ) -> Result<UsageSnapshot, ProviderError> {
        let usage = data_payload(usage);
        let subscription = subscription.map(data_payload);
        let used = number_at(usage, &["usage", "used"]);
        let remaining = number_at(usage, &["remainingBalance", "remaining"]);
        let total = number_at(usage, &["creditsTotal", "quota", "limit"]).or_else(|| {
            used.zip(remaining)
                .map(|(used, remaining)| used + remaining)
        });
        let used = used.or_else(|| {
            total
                .zip(remaining)
                .map(|(total, remaining)| total - remaining)
        });
        let weekly = subscription
            .and_then(|v| v.get("rateLimit").or_else(|| v.get("subscription")))
            .and_then(weekly_window);
        let primary = match used
            .zip(total)
            .filter(|(used, total)| used.is_finite() && total.is_finite() && *total > 0.0)
        {
            Some((used, total)) => {
                let mut window = RateWindow::new((used / total * 100.0).clamp(0.0, 100.0));
                window.resets_at = string_at(usage, &["next_quota_reset", "nextQuotaReset"])
                    .and_then(parse_datetime);
                window.reset_description = Some(match remaining {
                    Some(remaining) => {
                        format!("{used:.0}/{total:.0} credits ({remaining:.0} remaining)")
                    }
                    None => format!("{used:.0}/{total:.0} credits"),
                });
                window
            }
            None if weekly.is_some() => {
                RateWindow::informational("Codebuff credit usage unavailable")
            }
            None => {
                return Err(ProviderError::Parse(
                    "Codebuff response contains no usable quota".into(),
                ));
            }
        };
        let mut snapshot = UsageSnapshot::new(primary);
        if let Some(weekly) = weekly {
            snapshot = snapshot.with_secondary(weekly);
        }

        let mut method_parts = Vec::new();
        if let Some(sub) = subscription.and_then(|v| v.get("subscription").or(Some(v))) {
            if let Some(tier) = string_at(
                sub,
                &[
                    "displayName",
                    "display_name",
                    "scheduledTier",
                    "scheduled_tier",
                    "tier",
                ],
            ) {
                method_parts.push(tier.to_string());
            }
            if let Some(email) = string_at(sub, &["email", "userEmail", "user_email"]) {
                snapshot = snapshot.with_email(email.to_string());
            }
            if let Some(status) = string_at(sub, &["status"])
                && method_parts.is_empty()
            {
                method_parts.push(status.to_string());
            }
        }
        if let Some(remaining) = remaining {
            method_parts.push(format!("{remaining:.0} remaining"));
        }
        if bool_at(usage, &["autoTopupEnabled", "auto_topup_enabled"]) == Some(true) {
            method_parts.push("auto top-up".to_string());
        }
        if !method_parts.is_empty() {
            snapshot = snapshot.with_login_method(method_parts.join(" · "));
        }

        Ok(snapshot)
    }
}

impl Default for CodebuffProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CodebuffProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codebuff
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        match ctx.source_mode {
            SourceMode::Auto | SourceMode::OAuth => Ok(ProviderFetchResult::new(
                self.fetch_usage_api(ctx).await?,
                "api",
            )),
            SourceMode::Web | SourceMode::Cli => {
                Err(ProviderError::UnsupportedSource(ctx.source_mode))
            }
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![SourceMode::Auto, SourceMode::OAuth]
    }
}

fn number_at(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_i64().map(|n| n as f64))
                .or_else(|| v.as_str()?.parse::<f64>().ok())
        })
    })
}

fn data_payload(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

fn bool_at(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| {
        value.get(*key).and_then(|v| {
            v.as_bool()
                .or_else(|| v.as_str().and_then(|s| s.parse::<bool>().ok()))
        })
    })
}

fn string_at<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| value.get(*key)?.as_str())
}

fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
}

fn weekly_window(value: &Value) -> Option<RateWindow> {
    let used = number_at(value, &["weeklyUsed", "used"])?;
    let limit = number_at(value, &["weeklyLimit", "limit"])?;
    if limit <= 0.0 {
        return None;
    }
    let mut window = RateWindow::new((used / limit * 100.0).clamp(0.0, 100.0));
    if let Some(reset) =
        string_at(value, &["weeklyResetsAt", "resetsAt", "resetAt"]).and_then(parse_datetime)
    {
        window.resets_at = Some(reset);
    }
    window.reset_description = Some(format!("{used:.0}/{limit:.0} weekly credits"));
    Some(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_snapshot_from_usage_and_subscription() {
        let usage = json!({
            "usage": 25,
            "quota": 100,
            "remainingBalance": 75,
            "autoTopupEnabled": true
        });
        let subscription = json!({
            "subscription": {
                "displayName": "Pro",
                "email": "dev@example.com"
            },
            "rateLimit": {
                "weeklyUsed": 10,
                "weeklyLimit": 50
            }
        });

        let snapshot = CodebuffProvider::snapshot_from_values(&usage, Some(&subscription)).unwrap();

        assert_eq!(snapshot.primary.used_percent, 25.0);
        assert_eq!(
            snapshot.secondary.as_ref().map(|w| w.used_percent),
            Some(20.0)
        );
        assert_eq!(snapshot.account_email.as_deref(), Some("dev@example.com"));
        assert_eq!(
            snapshot.login_method.as_deref(),
            Some("Pro · 75 remaining · auto top-up")
        );
    }

    #[test]
    fn builds_snapshot_from_data_wrapped_payload() {
        let usage = json!({
            "data": {
                "used": "40",
                "limit": 100,
                "remaining": 60
            }
        });
        let subscription = json!({
            "data": {
                "subscription": {
                    "tier": "Team"
                }
            }
        });

        let snapshot = CodebuffProvider::snapshot_from_values(&usage, Some(&subscription)).unwrap();

        assert_eq!(snapshot.primary.used_percent, 40.0);
        assert_eq!(
            snapshot.login_method.as_deref(),
            Some("Team · 60 remaining")
        );
    }
}

#[cfg(test)]
mod cx289a_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cx289a_incomplete_quotas_are_rejected() {
        for payload in [
            json!({}),
            json!({"creditsTotal":100}),
            json!({"remaining":100}),
            json!({"used":0}),
        ] {
            assert!(CodebuffProvider::snapshot_from_values(&payload, None).is_err());
        }
    }
    #[test]
    fn cx289a_remaining_and_total_determine_used() {
        let snapshot = CodebuffProvider::snapshot_from_values(
            &json!({"creditsTotal":100,"remaining":75}),
            None,
        )
        .unwrap();
        assert_eq!(snapshot.primary.used_percent, 25.0);
    }

    #[test]
    fn cx289a_weekly_only_keeps_primary_unknown() {
        let snapshot = CodebuffProvider::snapshot_from_values(
            &json!({"creditsTotal":100}),
            Some(&json!({"rateLimit":{"weeklyUsed":0,"weeklyLimit":100}})),
        )
        .unwrap();
        assert!(snapshot.primary.is_informational);
        let weekly = snapshot.secondary.unwrap();
        assert_eq!(weekly.used_percent, 0.0);
        assert!(!weekly.is_informational);
    }
    #[test]
    fn cx289a_real_zero_is_preserved() {
        let snapshot =
            CodebuffProvider::snapshot_from_values(&json!({"creditsTotal":100,"used":0}), None)
                .unwrap();
        assert_eq!(snapshot.primary.used_percent, 0.0);
        assert!(!snapshot.primary.is_informational);
    }
}
