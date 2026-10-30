//! Perplexity provider implementation
//!
//! Fetches credit/usage data from Perplexity's REST billing endpoint.
//! Uses browser cookies (`__Secure-next-auth.session-token`) for authentication.

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use reqwest::Client;
use serde::Deserialize;

use crate::core::{
    FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId, ProviderMetadata,
    RateWindow, SourceMode, UsageSnapshot,
};

const CREDITS_URL: &str =
    "https://www.perplexity.ai/rest/billing/credits?version=2.18&source=default";
const ORIGIN: &str = "https://www.perplexity.ai";
const REFERER: &str = "https://www.perplexity.ai/account/usage";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

#[derive(Debug, Deserialize)]
struct CreditsResponse {
    #[serde(default)]
    balance_cents: f64,
    #[serde(default)]
    renewal_date_ts: Option<f64>,
    #[serde(default)]
    current_period_purchased_cents: f64,
    #[serde(default)]
    credit_grants: Vec<CreditGrant>,
    #[serde(default)]
    total_usage_cents: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct CreditGrant {
    #[serde(default, rename = "type")]
    grant_type: String,
    #[serde(default)]
    amount_cents: Option<f64>,
    #[serde(default)]
    expires_at_ts: Option<f64>,
}

pub struct PerplexityProvider {
    metadata: ProviderMetadata,
    client: Client,
}

impl PerplexityProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Perplexity,
                display_name: "Perplexity",
                session_label: "Credits",
                weekly_label: "Bonus credits",
                supports_opus: false,
                supports_credits: true,
                default_enabled: false,
                is_primary: false,
                dashboard_url: Some("https://www.perplexity.ai/account/usage"),
                status_page_url: None,
            },
            client: crate::core::credentialed_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn ts_to_datetime(ts: f64) -> Option<DateTime<Utc>> {
        // Grant expiry epochs are whole-second unix timestamps, far below i64::MAX.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "whole-second epoch fits i64"
        )]
        let secs = ts as i64;
        Utc.timestamp_opt(secs, 0).single()
    }

    fn parse_response(resp: CreditsResponse) -> Result<UsageSnapshot, ProviderError> {
        // Bucket grants by type.
        let mut recurring_total = 0.0_f64;
        let mut bonus_total = 0.0_f64;
        let mut bonus_expiry: Option<DateTime<Utc>> = None;
        let mut purchased_total = 0.0_f64;

        for grant in &resp.credit_grants {
            let amount = grant.amount_cents.ok_or_else(|| {
                ProviderError::Parse("Perplexity response contains no credit grant amount".into())
            })?;
            match grant.grant_type.as_str() {
                "recurring" => recurring_total += amount,
                "promotional" | "bonus" => {
                    bonus_total += amount;
                    if let Some(ts) = grant.expires_at_ts.and_then(Self::ts_to_datetime)
                        && bonus_expiry.is_none_or(|cur| ts < cur)
                    {
                        bonus_expiry = Some(ts);
                    }
                }
                "purchased" => purchased_total += amount,
                _ => {}
            }
        }

        if purchased_total <= 0.0 && resp.current_period_purchased_cents > 0.0 {
            purchased_total = resp.current_period_purchased_cents;
        }

        // Waterfall attribution: usage burns recurring first, then bonus, then purchased.
        let mut remaining_usage = resp
            .total_usage_cents
            .ok_or_else(|| {
                ProviderError::Parse("Perplexity response contains no usage counter".into())
            })?
            .max(0.0);
        if recurring_total <= 0.0 && bonus_total <= 0.0 && purchased_total <= 0.0 {
            return Err(ProviderError::Parse(
                "Perplexity response contains no credit quota".into(),
            ));
        }
        let recurring_used = remaining_usage.min(recurring_total);
        remaining_usage -= recurring_used;
        let bonus_used = remaining_usage.min(bonus_total);
        remaining_usage -= bonus_used;
        let purchased_used = remaining_usage.min(purchased_total);

        let pct = |used: f64, total: f64| -> f64 {
            if total <= 0.0 {
                0.0
            } else {
                ((used / total) * 100.0).clamp(0.0, 100.0)
            }
        };

        let renewal = resp.renewal_date_ts.and_then(Self::ts_to_datetime);

        let primary = if recurring_total > 0.0 {
            RateWindow::with_details(
                pct(recurring_used, recurring_total),
                None,
                renewal,
                Some(format!(
                    "${:.2}/${:.2}",
                    recurring_used / 100.0,
                    recurring_total / 100.0
                )),
            )
        } else {
            RateWindow::informational("No recurring credit quota")
        };

        let mut snapshot = UsageSnapshot::new(primary);

        if bonus_total > 0.0 {
            let mut secondary = RateWindow::new(pct(bonus_used, bonus_total));
            secondary.resets_at = bonus_expiry;
            let mut bonus_description =
                format!("${:.2}/${:.2}", bonus_used / 100.0, bonus_total / 100.0);
            if let Some(expiry) = bonus_expiry {
                bonus_description.push_str(&format!(" · exp. {}", expiry.format("%Y-%m-%d")));
            }
            secondary.reset_description = Some(bonus_description);
            snapshot = snapshot.with_secondary(secondary);
        }

        if purchased_total > 0.0 {
            let mut tertiary = RateWindow::new(pct(purchased_used, purchased_total));
            tertiary.reset_description = Some(format!(
                "${:.2}/${:.2}",
                purchased_used / 100.0,
                purchased_total / 100.0
            ));
            snapshot = snapshot.with_tertiary(tertiary);
        }

        let plan = if recurring_total <= 0.0 {
            None
        } else if recurring_total < 5000.0 {
            Some("Pro")
        } else {
            Some("Max")
        };
        if let Some(p) = plan {
            snapshot = snapshot.with_login_method(p);
        }

        // Avoid unused-warning for balance_cents on some build configs.
        let _ = resp.balance_cents;

        Ok(snapshot)
    }

    async fn fetch_with_cookies(
        &self,
        cookie_header: &str,
    ) -> Result<UsageSnapshot, ProviderError> {
        let response = self
            .client
            .get(CREDITS_URL)
            .header("Cookie", cookie_header)
            .header("Origin", ORIGIN)
            .header("Referer", REFERER)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/json, text/plain, */*")
            .send()
            .await?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::AuthRequired);
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!(
                "Perplexity API returned {}",
                status
            )));
        }

        let body = response.text().await?;
        let parsed: CreditsResponse = serde_json::from_str(&body)
            .map_err(|e| ProviderError::Parse(format!("Failed to parse credits: {}", e)))?;

        Self::parse_response(parsed)
    }
}

impl Default for PerplexityProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for PerplexityProvider {
    fn automatic_metric_prioritizes_exhausted_window(&self) -> bool {
        false
    }

    fn id(&self) -> ProviderId {
        ProviderId::Perplexity
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Fetching Perplexity usage");

        match ctx.source_mode {
            SourceMode::Auto | SourceMode::Web => {
                if let Some(ref cookie_header) = ctx.manual_cookie_header {
                    let usage = self.fetch_with_cookies(cookie_header).await?;
                    return Ok(ProviderFetchResult::new(usage, "web"));
                }

                match crate::providers::browser_cookie_header(&["perplexity.ai"]) {
                    Ok(cookie_header) => match self.fetch_with_cookies(&cookie_header).await {
                        Ok(usage) => return Ok(ProviderFetchResult::new(usage, "web")),
                        Err(ProviderError::AuthRequired) => {}
                        Err(e) => return Err(e),
                    },
                    Err(ProviderError::NoCookies) => {}
                    Err(e) => return Err(e),
                }

                Err(ProviderError::AuthRequired)
            }
            SourceMode::Cli => Err(ProviderError::UnsupportedSource(SourceMode::Cli)),
            SourceMode::OAuth => Err(ProviderError::UnsupportedSource(SourceMode::OAuth)),
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![SourceMode::Auto, SourceMode::Web]
    }

    fn supports_web(&self) -> bool {
        true
    }

    fn supports_cli(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_credits_with_waterfall_attribution() {
        let resp: CreditsResponse = serde_json::from_value(serde_json::json!({
            "balance_cents": 450.0,
            "renewal_date_ts": 1_750_000_000.0,
            "current_period_purchased_cents": 0.0,
            "credit_grants": [
                { "type": "recurring", "amount_cents": 500.0, "expires_at_ts": null },
                { "type": "promotional", "amount_cents": 100.0, "expires_at_ts": 1_750_000_000.0 },
                { "type": "purchased", "amount_cents": 0.0, "expires_at_ts": null }
            ],
            "total_usage_cents": 150.0
        }))
        .unwrap();

        let snap = PerplexityProvider::parse_response(resp).unwrap();
        // 150 cents of usage all attributed to recurring (500 cap).
        assert!((snap.primary.used_percent - 30.0).abs() < 0.001);
        let bonus = snap.secondary.expect("bonus window");
        assert!((bonus.used_percent - 0.0).abs() < 0.001);
        assert!(
            bonus
                .reset_description
                .as_deref()
                .is_some_and(|description| description.contains("exp. 2025-06-15"))
        );
        assert_eq!(snap.login_method.as_deref(), Some("Pro"));
    }

    #[test]
    fn classifies_max_plan_when_recurring_high() {
        let resp: CreditsResponse = serde_json::from_value(serde_json::json!({
            "balance_cents": 5000.0,
            "renewal_date_ts": null,
            "current_period_purchased_cents": 0.0,
            "credit_grants": [
                { "type": "recurring", "amount_cents": 6000.0, "expires_at_ts": null }
            ],
            "total_usage_cents": 0.0
        }))
        .unwrap();
        let snap = PerplexityProvider::parse_response(resp).unwrap();
        assert_eq!(snap.login_method.as_deref(), Some("Max"));
    }
}

#[cfg(test)]
mod cx289a_parent_tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> Result<UsageSnapshot, ProviderError> {
        PerplexityProvider::parse_response(serde_json::from_value(value).unwrap())
    }
    #[test]
    fn missing_usage_is_an_error() {
        for value in [
            json!({}),
            json!({"credit_grants":[{"type":"recurring","amount_cents":100}]}),
            json!({"total_usage_cents":0}),
            json!({"total_usage_cents":0,"credit_grants":[{"type":"recurring"}]}),
        ] {
            assert!(parse(value.clone()).is_err(), "{value}");
        }
    }
    #[test]
    fn real_zero_usage_stays_a_quota() {
        let snapshot = parse(json!({"total_usage_cents":0,"credit_grants":[{"type":"recurring","amount_cents":100}]})).unwrap();
        assert_eq!(snapshot.primary.used_percent, 0.0);
        assert!(!snapshot.primary.is_informational);
    }
    #[test]
    fn bonus_only_keeps_recurring_unavailable() {
        let snapshot = parse(
            json!({"total_usage_cents":0,"credit_grants":[{"type":"bonus","amount_cents":100}]}),
        )
        .unwrap();
        assert!(snapshot.primary.is_informational);
        assert_eq!(snapshot.secondary.unwrap().used_percent, 0.0);
    }
}
