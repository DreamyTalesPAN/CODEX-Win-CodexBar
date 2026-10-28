//! Kilo provider implementation
//!
//! Fetches credit-block + Kilo Pass usage from Kilo's tRPC batch API using a
//! bearer API key sourced from `KILO_API_KEY`, the OS keyring, or
//! `~/.local/share/kilo/auth.json`.

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::core::{
    FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId, ProviderMetadata,
    RateWindow, SourceMode, UsageSnapshot,
};

const TRPC_BASE: &str = "https://app.kilo.ai/api/trpc";
const PROCEDURES: &str = "user.getCreditBlocks,kiloPass.getState,user.getAutoTopUpPaymentMethod";
const KILO_CREDENTIAL_TARGET: &str = "codexbar-kilo";

#[derive(Debug, Deserialize)]
struct CreditBlock {
    #[serde(default, rename = "amount_mUsd")]
    amount_m_usd: Option<f64>,
    #[serde(default, rename = "balance_mUsd")]
    balance_m_usd: Option<f64>,
}

pub struct KiloProvider {
    metadata: ProviderMetadata,
    client: Client,
}

impl KiloProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::Kilo,
                display_name: "Kilo",
                session_label: "Credits",
                weekly_label: "Pass",
                supports_opus: false,
                supports_credits: true,
                default_enabled: false,
                is_primary: false,
                dashboard_url: Some("https://app.kilo.ai/usage"),
                status_page_url: None,
            },
            client: crate::core::credentialed_http_client_builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn get_api_key(api_key: Option<&str>) -> Result<String, ProviderError> {
        if let Some(token) = direct_kilo_api_key(api_key) {
            return Ok(token);
        }

        if let Some(token) = keyring_kilo_api_key() {
            return Ok(token);
        }

        if let Some(token) = env_kilo_api_key() {
            return Ok(token);
        }

        if let Some(token) = file_kilo_api_key() {
            return Ok(token);
        }

        Err(ProviderError::NotInstalled(
            "Kilo API key not found. Set KILO_API_KEY, store in keychain, or run `kilo auth login`."
                .to_string(),
        ))
    }

    fn build_url() -> String {
        // tRPC batch GET — `input` maps each ordered procedure index to its input.
        let input = serde_json::json!({
            "0": { "json": null, "meta": { "values": ["undefined"] } },
            "1": { "json": null, "meta": { "values": ["undefined"] } },
            "2": { "json": null, "meta": { "values": ["undefined"] } }
        });
        let encoded = url_encode(&input.to_string());
        format!("{}/{}?batch=1&input={}", TRPC_BASE, PROCEDURES, encoded)
    }

    fn build_snapshot(
        credit_blocks_data: Option<&Value>,
        kilo_pass_data: Option<&Value>,
    ) -> Result<UsageSnapshot, ProviderError> {
        // A balance alone does not establish the original credit allowance.
        let blocks = credit_blocks_data.and_then(|payload| {
            payload
                .get("creditBlocks")
                .and_then(Value::as_array)
                .or_else(|| payload.as_array())
        });
        let totals = blocks
            .filter(|blocks| !blocks.is_empty())
            .and_then(|blocks| {
                blocks
                    .iter()
                    .try_fold((0.0, 0.0), |(total, remaining), block| {
                        let block = serde_json::from_value::<CreditBlock>(block.clone()).ok()?;
                        Some((
                            total + block.amount_m_usd?,
                            remaining + block.balance_m_usd?,
                        ))
                    })
            });
        let balance = credit_blocks_data
            .and_then(|payload| payload.get("totalBalance_mUsd"))
            .and_then(Value::as_f64)
            .filter(|balance| balance.is_finite());
        let primary = if let Some((total, remaining)) = totals.filter(|(total, _)| *total > 0.0) {
            let used = (total - remaining).max(0.0);
            RateWindow::with_details(
                (used / total * 100.0).clamp(0.0, 100.0),
                None,
                None,
                Some(format!(
                    "${:.2}/${:.2}",
                    used / 1_000_000.0,
                    total / 1_000_000.0
                )),
            )
        } else if let Some(balance) = balance {
            RateWindow::informational(format!("${:.2} balance", balance / 1_000_000.0))
        } else {
            RateWindow::informational("Credit usage unavailable")
        };

        let mut snap = UsageSnapshot::new(primary);

        // --- Kilo Pass (secondary window) ---
        if let Some(pass) = kilo_pass_data {
            let usage = pass.get("currentPeriodUsageUsd").and_then(Value::as_f64);
            let base = pass
                .get("currentPeriodBaseCreditsUsd")
                .and_then(Value::as_f64);
            let bonus = pass
                .get("currentPeriodBonusCreditsUsd")
                .and_then(Value::as_f64);
            if let (Some(usage), Some(base), Some(bonus)) = (usage, base, bonus)
                && base + bonus > 0.0
            {
                let pass_total = base + bonus;
                let pass_pct = ((usage / pass_total) * 100.0).clamp(0.0, 100.0);
                let mut secondary = RateWindow::new(pass_pct);
                secondary.reset_description = Some(format!("${:.2}/${:.2}", usage, pass_total));
                snap = snap.with_secondary(secondary);
            }

            if let Some(plan) = pass
                .get("planName")
                .or_else(|| pass.get("tier"))
                .or_else(|| pass.get("status"))
                .and_then(|v| v.as_str())
                && !plan.is_empty()
            {
                snap = snap.with_login_method(plan.to_string());
            }
        }

        if snap.primary.is_informational && snap.secondary.is_none() && balance.is_none() {
            return Err(ProviderError::Parse(
                "Kilo response contains no usable credit quota".into(),
            ));
        }
        Ok(snap)
    }

    fn extract_data(batch: &Value, index: usize) -> Option<&Value> {
        // tRPC batch entries nest the payload under `result.data.json`; some
        // Kilo procedures (e.g. `user.getCreditBlocks`) place fields directly
        // under `result.data`. Prefer `.json`, fall back to `data` itself.
        let data = batch.as_array()?.get(index)?.get("result")?.get("data")?;
        if let Some(json) = data.get("json")
            && !json.is_null()
        {
            return Some(json);
        }
        Some(data)
    }

    async fn fetch_with_key(&self, api_key: &str) -> Result<UsageSnapshot, ProviderError> {
        let url = Self::build_url();
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::AuthRequired);
        }
        if !status.is_success() {
            return Err(ProviderError::Other(format!(
                "Kilo tRPC API returned {}",
                status
            )));
        }

        let body = response.text().await?;
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| ProviderError::Parse(format!("Failed to parse Kilo response: {}", e)))?;

        let credit_blocks = Self::extract_data(&parsed, 0);
        let kilo_pass = Self::extract_data(&parsed, 1);

        Self::build_snapshot(credit_blocks, kilo_pass)
    }
}

fn direct_kilo_api_key(api_key: Option<&str>) -> Option<String> {
    api_key.filter(|key| !key.is_empty()).map(str::to_string)
}

fn keyring_kilo_api_key() -> Option<String> {
    keyring::Entry::new(KILO_CREDENTIAL_TARGET, "api_key")
        .ok()?
        .get_password()
        .ok()
        .filter(|token| !token.is_empty())
}

fn env_kilo_api_key() -> Option<String> {
    std::env::var("KILO_API_KEY")
        .ok()
        .filter(|token| !token.is_empty())
}

fn file_kilo_api_key() -> Option<String> {
    let path = dirs::home_dir()?.join(".local/share/kilo/auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let json = serde_json::from_str::<Value>(&text).ok()?;

    kilo_api_key_from_json(&json)
}

fn kilo_api_key_from_json(json: &Value) -> Option<String> {
    for key in ["apiKey", "api_key", "token"] {
        if let Some(value) = json.get(key).and_then(|v| v.as_str())
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }

    None
}

impl Default for KiloProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for KiloProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Kilo
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        tracing::debug!("Fetching Kilo usage");

        match ctx.source_mode {
            SourceMode::Auto | SourceMode::OAuth => {
                let api_key = Self::get_api_key(ctx.api_key.as_deref())?;
                let usage = self.fetch_with_key(&api_key).await?;
                Ok(ProviderFetchResult::new(usage, "api"))
            }
            SourceMode::Web | SourceMode::Cli => {
                Err(ProviderError::UnsupportedSource(ctx.source_mode))
            }
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![SourceMode::Auto, SourceMode::OAuth]
    }

    fn supports_web(&self) -> bool {
        false
    }

    fn supports_cli(&self) -> bool {
        false
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            _ => {
                for b in c.to_string().as_bytes() {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_credit_blocks_and_pass() {
        let credit_blocks = serde_json::json!({
            "creditBlocks": [
                { "amount_mUsd": 1_000_000.0, "balance_mUsd": 750_000.0 },
                { "amount_mUsd": 2_000_000.0, "balance_mUsd": 1_500_000.0 }
            ],
            "totalBalance_mUsd": 2_250_000.0,
            "autoTopUpEnabled": false
        });
        let kilo_pass = serde_json::json!({
            "currentPeriodUsageUsd": 5.0,
            "currentPeriodBaseCreditsUsd": 20.0,
            "currentPeriodBonusCreditsUsd": 5.0,
            "planName": "Kilo Pass"
        });
        let snap = KiloProvider::build_snapshot(Some(&credit_blocks), Some(&kilo_pass)).unwrap();
        // total $3, remaining $2.25, used $0.75 → 25%
        assert!((snap.primary.used_percent - 25.0).abs() < 0.001);
        let secondary = snap.secondary.expect("pass window");
        // 5/25 = 20%
        assert!((secondary.used_percent - 20.0).abs() < 0.001);
        assert_eq!(snap.login_method.as_deref(), Some("Kilo Pass"));
    }

    #[test]
    fn handles_missing_credit_blocks() {
        assert!(KiloProvider::build_snapshot(None, None).is_err());
    }

    #[test]
    fn review16_payg_balance_without_allowance_is_information() {
        // Pay-as-you-go account: empty creditBlocks, no Kilo Pass subscription.
        // Backs the "Remaining Credits" field on app.kilo.ai/profile (#355).
        let credit_blocks = serde_json::json!({
            "creditBlocks": [],
            "totalBalance_mUsd": 1_060_000.0,
            "autoTopUpEnabled": false
        });
        let kilo_pass = serde_json::json!({ "subscription": null });
        let snapshot =
            KiloProvider::build_snapshot(Some(&credit_blocks), Some(&kilo_pass)).unwrap();
        assert!(snapshot.primary.is_informational);
        assert_eq!(
            snapshot.primary.reset_description.as_deref(),
            Some("$1.06 balance")
        );
        assert_eq!(snapshot.primary.resets_at, None);
        assert!(snapshot.secondary.is_none());
    }
}

#[cfg(test)]
mod cx289a_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cx289a_missing_quota_is_error() {
        assert!(KiloProvider::build_snapshot(None, None).is_err());
        for value in [
            json!({"creditBlocks":[{"amount_mUsd":100}]}),
            json!({"creditBlocks":[{"balance_mUsd":100}]}),
        ] {
            assert!(
                KiloProvider::build_snapshot(Some(&value), None).is_err(),
                "{value}"
            );
        }
    }
    #[test]
    fn cx289a_pass_missing_counter_is_error() {
        assert!(
            KiloProvider::build_snapshot(
                None,
                Some(&json!({"currentPeriodBaseCreditsUsd":100,"currentPeriodBonusCreditsUsd":0}))
            )
            .is_err()
        );
    }
    #[test]
    fn cx289a_pass_only_keeps_unknown_primary() {
        let snap = KiloProvider::build_snapshot(None, Some(&json!({"currentPeriodUsageUsd":0,"currentPeriodBaseCreditsUsd":100,"currentPeriodBonusCreditsUsd":0}))).unwrap();
        assert!(snap.primary.is_informational);
        assert_eq!(snap.secondary.unwrap().used_percent, 0.0);
    }
    #[test]
    fn cx289a_real_zero_is_quota() {
        let snap = KiloProvider::build_snapshot(
            Some(&json!({"creditBlocks":[{"amount_mUsd":100,"balance_mUsd":100}]})),
            None,
        )
        .unwrap();
        assert!(!snap.primary.is_informational);
        assert_eq!(snap.primary.used_percent, 0.0);
    }
}
