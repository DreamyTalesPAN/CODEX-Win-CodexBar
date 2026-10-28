use async_trait::async_trait;
use reqwest::{Client, Url};
use serde_json::Value;

use crate::core::{
    CostSnapshot, FetchContext, Provider, ProviderError, ProviderFetchResult, ProviderId,
    ProviderMetadata, RateWindow, SourceMode, UsageSnapshot,
};

const CREDENTIAL_TARGET: &str = "codexbar-litellm";

pub struct LiteLLMProvider {
    metadata: ProviderMetadata,
    client: Client,
}

impl LiteLLMProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                id: ProviderId::LiteLLM,
                display_name: "LiteLLM",
                session_label: "Budget",
                weekly_label: "Spend",
                supports_opus: false,
                supports_credits: true,
                default_enabled: false,
                is_primary: false,
                dashboard_url: None,
                status_page_url: None,
            },
            client: crate::core::credentialed_http_client_builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }
}

impl Default for LiteLLMProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for LiteLLMProvider {
    fn id(&self) -> ProviderId {
        ProviderId::LiteLLM
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn fetch_usage(&self, ctx: &FetchContext) -> Result<ProviderFetchResult, ProviderError> {
        match ctx.source_mode {
            SourceMode::Auto | SourceMode::OAuth => {
                let (base, key) = resolve_base_and_key(ctx)?;
                let response = self
                    .client
                    .get(management_url(&base, "key/info")?)
                    .bearer_auth(key)
                    .header("Accept", "application/json")
                    .send()
                    .await?;
                if response.status() == reqwest::StatusCode::UNAUTHORIZED
                    || response.status() == reqwest::StatusCode::FORBIDDEN
                {
                    return Err(ProviderError::AuthRequired);
                }
                if !response.status().is_success() {
                    return Err(ProviderError::Other(format!(
                        "LiteLLM key/info returned status {}",
                        response.status()
                    )));
                }
                let value: Value = response.json().await.map_err(|e| {
                    ProviderError::Parse(format!("Failed to parse LiteLLM key/info: {e}"))
                })?;
                result_from_key_info(&value)
            }
            SourceMode::Web | SourceMode::Cli => {
                Err(ProviderError::UnsupportedSource(ctx.source_mode))
            }
        }
    }

    fn available_sources(&self) -> Vec<SourceMode> {
        vec![SourceMode::Auto, SourceMode::OAuth]
    }
}

fn resolve_base_and_key(ctx: &FetchContext) -> Result<(String, String), ProviderError> {
    if let Some(base) = ctx
        .workspace_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let key = ctx.api_key.as_deref().ok_or(ProviderError::AuthRequired)?;
        return Ok((base.to_string(), key.to_string()));
    }

    let key = crate::providers::resolve_api_key(
        ctx.api_key.as_deref(),
        CREDENTIAL_TARGET,
        &["LITELLM_API_KEY"],
    )?;
    let base = std::env::var("LITELLM_BASE_URL")
        .ok()
        .or_else(|| std::env::var("LITELLM_API_BASE").ok())
        .ok_or_else(|| {
            ProviderError::NotInstalled(
                "LiteLLM base URL not found. Set it in provider extras or LITELLM_BASE_URL.".into(),
            )
        })?;
    Ok((base, key))
}

fn management_url(base: &str, path: &str) -> Result<Url, ProviderError> {
    let mut url = crate::providers::validated_https_url(base, "LiteLLM base")?;
    if url.path().trim_end_matches('/').ends_with("/v1") {
        let stripped = url
            .path()
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string();
        url.set_path(&stripped);
    }
    url.join(path)
        .map_err(|e| ProviderError::Other(format!("Invalid LiteLLM URL: {e}")))
}

fn result_from_key_info(value: &Value) -> Result<ProviderFetchResult, ProviderError> {
    let root = value
        .get("info")
        .or_else(|| value.get("key"))
        .unwrap_or(value);
    let spend = number(root, &["spend", "spend_usd", "spendUSD"]);
    let limit = number(root, &["max_budget", "maxBudget", "budget", "limit"]);
    let primary = match (spend, limit.filter(|v| *v > 0.0)) {
        (Some(spend), Some(limit)) => RateWindow::with_details(
            spend / limit * 100.0,
            None,
            None,
            Some(budget_detail(spend, limit)),
        ),
        _ => RateWindow::informational("Key budget usage unavailable"),
    };
    let mut snapshot = UsageSnapshot::new(primary);
    if let Some(spend) = spend {
        snapshot = snapshot.with_login_method(format!("Spend ${spend:.2}"));
    }
    if let Some(team) = root.get("team_info").or_else(|| root.get("teamInfo"))
        && let Some(team_spend) = number(team, &["spend", "team_spend", "teamSpend"])
    {
        let team_limit = number(team, &["max_budget", "budget", "limit"]);
        let team_window = if let Some(team_limit) = team_limit.filter(|value| *value > 0.0) {
            let alias = string(team, &["team_alias", "teamAlias", "alias"])
                .map(|value| format!("Team {value}: "))
                .unwrap_or_default();
            RateWindow::with_details(
                team_spend / team_limit * 100.0,
                None,
                None,
                Some(format!("{alias}{}", budget_detail(team_spend, team_limit))),
            )
        } else {
            RateWindow::informational(format!("Team spend ${team_spend:.2}"))
        };
        snapshot = snapshot.with_extra_rate_window("team", "Team budget", team_window);
    }
    if spend.is_none() && snapshot.extra_rate_windows.is_empty() {
        return Err(ProviderError::Parse(
            "LiteLLM response contains no usage data".into(),
        ));
    }
    let mut result = ProviderFetchResult::new(snapshot, "api");
    if let Some(spend) = spend {
        let mut cost = CostSnapshot::new(spend, "USD", "Spend");
        if let Some(limit) = limit {
            cost = cost.with_limit(limit);
        }
        result = result.with_cost(cost);
    }
    Ok(result)
}

fn number(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_f64))
}

fn string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn budget_detail(spend: f64, budget: f64) -> String {
    format!("${spend:.2} / ${budget:.2}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spend_budget() {
        let result =
            result_from_key_info(&serde_json::json!({"info":{"spend":25.0,"max_budget":100.0}}))
                .unwrap();
        assert_eq!(result.usage.primary.used_percent, 25.0);
        assert_eq!(
            result.usage.primary.reset_description.as_deref(),
            Some("$25.00 / $100.00")
        );
    }

    #[test]
    fn preserves_team_budget_detail_with_alias() {
        let result = result_from_key_info(&serde_json::json!({
            "info": {
                "team_info": {
                    "team_alias": "Platform",
                    "spend": 70.0,
                    "max_budget": 1000.0
                }
            }
        }))
        .unwrap();
        assert_eq!(result.usage.extra_rate_windows.len(), 1);
        assert_eq!(
            result.usage.extra_rate_windows[0]
                .window
                .reset_description
                .as_deref(),
            Some("Team Platform: $70.00 / $1000.00")
        );
    }

    #[test]
    fn saved_base_url_uses_only_app_saved_key() {
        let mut ctx = FetchContext {
            workspace_id: Some("https://litellm.example.com".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            resolve_base_and_key(&ctx),
            Err(ProviderError::AuthRequired)
        ));

        ctx.api_key = Some("sk-app".to_string());
        let (base, key) = resolve_base_and_key(&ctx).unwrap();
        assert_eq!(base, "https://litellm.example.com");
        assert_eq!(key, "sk-app");
    }
}

#[cfg(test)]
mod cx289a_tests {
    use super::*;
    use serde_json::json;
    fn build(value: Value) -> Result<ProviderFetchResult, ProviderError> {
        result_from_key_info(&value)
    }
    #[test]
    fn cx289a_missing_spend_is_error() {
        for value in [json!({}), json!({"max_budget":100})] {
            assert!(build(value.clone()).is_err(), "{value}");
        }
    }
    #[test]
    fn cx289a_spend_only_is_informational() {
        let result = build(json!({"spend":12})).unwrap();
        assert!(result.usage.primary.is_informational);
        assert_eq!(result.cost.unwrap().used, 12.0);
    }
    #[test]
    fn cx289a_team_only_keeps_unknown_primary() {
        let result = build(json!({"team_info":{"spend":0,"max_budget":100}})).unwrap();
        assert!(result.usage.primary.is_informational);
        assert!(!result.usage.extra_rate_windows[0].window.is_informational);
    }
    #[test]
    fn cx289a_real_zero_is_quota() {
        let result = build(json!({"spend":0,"max_budget":100})).unwrap();
        assert!(!result.usage.primary.is_informational);
        assert_eq!(result.usage.primary.used_percent, 0.0);
    }
}
