//! Abacus AI provider implementation
//!
//! Fetches compute-point usage and billing info via apps.abacus.ai web APIs.
//! Uses browser cookies for authentication.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

use crate::core::{
    FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId, ProviderMetadata,
    RateWindow, SourceMode, UsageSnapshot,
};

const COMPUTE_URL: &str = "https://apps.abacus.ai/api/_getOrganizationComputePoints";
const BILLING_URL: &str = "https://apps.abacus.ai/api/_getBillingInfo";
const CREDITS_LABEL: &str = "Credits";
const FALLBACK_MONTHLY_WINDOW_MINUTES: u32 = 30 * 24 * 60;

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    #[serde(default)]
    success: bool,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComputePoints {
    total_compute_points: f64,
    compute_points_left: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingInfo {
    #[serde(default)]
    next_billing_date: Option<String>,
    #[serde(default)]
    current_tier: Option<String>,
}

fn parse_compute_response(body: &str) -> Result<ComputePoints, ProviderError> {
    let env: ApiEnvelope<ComputePoints> = serde_json::from_str(body)
        .map_err(|_| ProviderError::Parse("Abacus compute response is invalid".into()))?;

    if !env.success {
        return Err(ProviderError::AuthRequired);
    }
    env.result.ok_or_else(|| {
        ProviderError::Parse("Abacus response contains no compute points result".into())
    })
}

pub struct AbacusProvider {
    metadata: ProviderMetadata,
    client: Client,
}

impl AbacusProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Abacus,
                display_name: "Abacus AI",
                session_label: CREDITS_LABEL,
                weekly_label: "",
                supports_opus: false,
                supports_credits: true,
                default_enabled: false,
                is_primary: false,
                dashboard_url: Some("https://apps.abacus.ai/app/billing"),
                status_page_url: None,
            },
            client: crate::core::credentialed_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn build_snapshot(
        compute: ComputePoints,
        billing: Option<BillingInfo>,
    ) -> Result<UsageSnapshot, ProviderError> {
        let total = compute.total_compute_points.max(0.0);
        let left = compute.compute_points_left.max(0.0);
        let used = (total - left).max(0.0);
        let percent = if total > 0.0 {
            ((used / total) * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };

        let resets_at = billing
            .as_ref()
            .and_then(|b| b.next_billing_date.as_deref())
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let primary = if total <= 0.0 {
            RateWindow::informational(format_credit_detail(used, total))
        } else {
            RateWindow::with_details(
                percent,
                RateWindow::monthly_window_minutes(resets_at)
                    .or(Some(FALLBACK_MONTHLY_WINDOW_MINUTES)),
                resets_at,
                Some(format_credit_detail(used, total)),
            )
        };

        let mut snapshot = UsageSnapshot::new(primary).with_primary_label(CREDITS_LABEL);
        if let Some(b) = billing
            && let Some(tier) = b.current_tier
            && !tier.is_empty()
        {
            snapshot = snapshot.with_login_method(tier);
        }
        Ok(snapshot)
    }

    async fn fetch_compute(&self, cookie_header: &str) -> Result<ComputePoints, ProviderError> {
        let resp = self
            .client
            .get(COMPUTE_URL)
            .header("Cookie", cookie_header)
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::AuthRequired);
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!(
                "Abacus compute API returned {}",
                status
            )));
        }

        let body = resp.text().await?;
        parse_compute_response(&body)
    }

    async fn fetch_billing(&self, cookie_header: &str) -> Option<BillingInfo> {
        let resp = self
            .client
            .post(BILLING_URL)
            .header("Cookie", cookie_header)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body("{}")
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        let body = resp.text().await.ok()?;
        let env: ApiEnvelope<BillingInfo> = serde_json::from_str(&body).ok()?;
        env.result
    }

    async fn fetch_with_cookies(
        &self,
        cookie_header: &str,
    ) -> Result<UsageSnapshot, ProviderError> {
        let compute = self.fetch_compute(cookie_header).await?;
        let billing = self.fetch_billing(cookie_header).await;
        Self::build_snapshot(compute, billing)
    }
}

fn format_credit_detail(used: f64, total: f64) -> String {
    format!(
        "{} / {} credits",
        format_credit_value(used),
        format_credit_value(total)
    )
}

fn format_credit_value(value: f64) -> String {
    let value = if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    };
    let formatted = if value >= 1000.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    };

    let (integer, fraction) = match formatted.split_once('.') {
        Some(parts) => parts,
        None => (formatted.as_str(), ""),
    };
    let grouped = group_credit_digits(integer);
    if fraction.is_empty() {
        grouped
    } else {
        format!("{grouped}.{fraction}")
    }
}

fn group_credit_digits(value: &str) -> String {
    let mut reversed = String::with_capacity(value.len() + value.len() / 3);
    for (index, digit) in value.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(digit);
    }
    reversed.chars().rev().collect()
}

impl Default for AbacusProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AbacusProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Abacus
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Fetching Abacus AI usage");

        match ctx.source_mode {
            SourceMode::Auto | SourceMode::Web => {
                if let Some(ref cookie_header) = ctx.manual_cookie_header {
                    let usage = self.fetch_with_cookies(cookie_header).await?;
                    return Ok(ProviderFetchResult::new(usage, "web"));
                }

                match crate::providers::browser_cookie_header(&["apps.abacus.ai"]) {
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
    fn parses_compute_points_and_tier() {
        let compute = ComputePoints {
            total_compute_points: 1000.0,
            compute_points_left: 750.0,
        };
        let billing = BillingInfo {
            next_billing_date: Some("2025-03-01T00:00:00Z".into()),
            current_tier: Some("Pro".into()),
        };
        let snap = AbacusProvider::build_snapshot(compute, Some(billing)).unwrap();
        assert!((snap.primary.used_percent - 25.0).abs() < 0.001);
        assert_eq!(
            snap.primary.reset_description.as_deref(),
            Some("250 / 1,000 credits")
        );
        assert_eq!(
            snap.primary.resets_at,
            Some(
                DateTime::parse_from_rfc3339("2025-03-01T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        assert_eq!(snap.primary.window_minutes, Some(28 * 24 * 60));
        assert_eq!(snap.primary_label.as_deref(), Some(CREDITS_LABEL));
        assert_eq!(snap.login_method.as_deref(), Some("Pro"));
        assert!(snap.account_email.is_none());
        assert!(snap.account_organization.is_none());
    }

    #[test]
    fn handles_missing_billing() {
        let compute = ComputePoints {
            total_compute_points: 500.0,
            compute_points_left: 500.0,
        };
        let snap = AbacusProvider::build_snapshot(compute, None).unwrap();
        assert!((snap.primary.used_percent - 0.0).abs() < f64::EPSILON);
        assert_eq!(
            snap.primary.reset_description.as_deref(),
            Some("0 / 500 credits")
        );
        assert_eq!(
            snap.primary.window_minutes,
            Some(FALLBACK_MONTHLY_WINDOW_MINUTES)
        );
        assert!(snap.primary.resets_at.is_none());
        assert_eq!(snap.primary_label.as_deref(), Some(CREDITS_LABEL));
        assert!(snap.login_method.is_none());
    }

    #[test]
    fn formats_credit_details_with_grouping_and_fraction() {
        assert_eq!(
            format_credit_detail(12_345.0, 50_000.0),
            "12,345 / 50,000 credits"
        );
        assert_eq!(format_credit_detail(42.5, 100.0), "42.5 / 100 credits");
    }
}

#[cfg(test)]
mod cx289a_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cx289a_missing_compute_fields_are_rejected() {
        for payload in [
            json!({}),
            json!({"totalComputePoints":100}),
            json!({"computePointsLeft":100}),
        ] {
            let result = serde_json::from_value::<ComputePoints>(payload)
                .map_err(|e| ProviderError::Parse(e.to_string()))
                .and_then(|compute| AbacusProvider::build_snapshot(compute, None));
            assert!(result.is_err());
        }
    }
    #[test]
    fn cx289a_real_zero_is_preserved() {
        let compute =
            serde_json::from_value(json!({"totalComputePoints":100,"computePointsLeft":100}))
                .unwrap();
        let snapshot = AbacusProvider::build_snapshot(compute, None).unwrap();
        assert_eq!(snapshot.primary.used_percent, 0.0);
        assert!(!snapshot.primary.is_informational);
    }
}

#[cfg(test)]
mod p1_11_zero_denominator_is_information_tests {
    use super::*;
    #[test]
    fn review16_p1_11_zero_denominator_is_information() {
        let compute = serde_json::from_value(
            serde_json::json!({"totalComputePoints":0,"computePointsLeft":0}),
        )
        .unwrap();
        let snapshot = AbacusProvider::build_snapshot(compute, None).unwrap();
        assert!(snapshot.primary.is_informational);
        assert!(snapshot.primary.window_minutes.is_none());
        assert!(snapshot.primary.resets_at.is_none());
    }
}

#[cfg(test)]
mod p2_02_incomplete_compute_is_a_data_error_tests {
    use super::*;
    #[test]
    fn review16_p2_02_incomplete_compute_is_a_data_error() {
        for body in [
            r#"{"success":true,"result":{"totalComputePoints":100}}"#,
            r#"{"success":true}"#,
        ] {
            let error = parse_compute_response(body).unwrap_err();
            assert!(matches!(error, ProviderError::Parse(_)));
            let text = error.to_string().to_lowercase();
            for marker in [
                "missing",
                "not found",
                "required",
                "auth",
                "token",
                "cookie",
                "login",
            ] {
                assert!(!text.contains(marker), "{text}");
            }
        }
    }
}
