use anyhow::Result;
use flawrence_chat::{ContextPricingTier, ModelPricing};
use serde::Deserialize;
use std::collections::HashMap;

const MODELS_API_URL: &str = "https://models.dev/api.json";

#[derive(Deserialize)]
struct Provider {
    #[serde(default)]
    models: HashMap<String, ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    #[serde(default)]
    cost: Option<CostEntry>,
}

#[derive(Deserialize)]
struct CostEntry {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
    #[serde(default)]
    tiers: Vec<CostTierEntry>,
}

#[derive(Deserialize)]
struct CostTierEntry {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    tier: TierCondition,
}

#[derive(Deserialize)]
struct TierCondition {
    #[serde(rename = "type")]
    kind: String,
    size: u64,
}

pub async fn fetch_pricing() -> Result<HashMap<String, ModelPricing>> {
    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let providers: HashMap<String, Provider> = client
        .get(MODELS_API_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    flatten_pricing(providers)
}

fn flatten_pricing(providers: HashMap<String, Provider>) -> Result<HashMap<String, ModelPricing>> {
    let mut pricing = HashMap::new();
    for (provider_id, provider) in providers {
        for (model_id, model) in provider.models {
            if let Some(cost) = model.cost {
                let base_input = cost.input;
                let base_output = cost.output;
                let base_cache_read = cost.cache_read;
                let base_cache_write = cost.cache_write;
                for (name, value) in [
                    ("input", base_input),
                    ("output", base_output),
                    ("cache_read", base_cache_read),
                    ("cache_write", base_cache_write),
                ] {
                    validate_price(&provider_id, &model_id, name, value)?;
                }
                let context_tiers = cost
                    .tiers
                    .into_iter()
                    .filter(|tier| tier.tier.kind == "context")
                    .map(|tier| {
                        let pricing = ContextPricingTier {
                            min_input_tokens: tier.tier.size,
                            input: tier.input.unwrap_or(base_input),
                            cache_write: tier.cache_write.unwrap_or(base_cache_write),
                            cache_read: tier.cache_read.unwrap_or(base_cache_read),
                            output: tier.output.unwrap_or(base_output),
                        };
                        for (name, value) in [
                            ("tier.input", pricing.input),
                            ("tier.output", pricing.output),
                            ("tier.cache_read", pricing.cache_read),
                            ("tier.cache_write", pricing.cache_write),
                        ] {
                            validate_price(&provider_id, &model_id, name, value)?;
                        }
                        anyhow::ensure!(
                            pricing.min_input_tokens > 0,
                            "invalid zero context tier for {provider_id}/{model_id}"
                        );
                        Ok(pricing)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let model_pricing = ModelPricing {
                    input: base_input,
                    cache_write: base_cache_write,
                    cache_read: base_cache_read,
                    output: base_output,
                    context_tiers,
                };
                let key = format!("{provider_id}/{model_id}");
                pricing.insert(key, model_pricing.clone());
                if provider_id == "cloudflare-workers-ai" {
                    pricing.insert(format!("workers-ai/{model_id}"), model_pricing);
                }
            }
        }
    }
    Ok(pricing)
}

fn validate_price(provider: &str, model: &str, name: &str, value: f64) -> Result<()> {
    anyhow::ensure!(
        value.is_finite() && value >= 0.0,
        "invalid {name} price for {provider}/{model}: {value}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_context_tiers_and_workers_alias() {
        let providers = serde_json::from_value(json!({
            "openai": {
                "models": {
                    "gpt-5.6": {
                        "cost": {
                            "input": 5.0,
                            "output": 30.0,
                            "tiers": [{
                                "input": 10.0,
                                "output": 45.0,
                                "tier": { "type": "context", "size": 272000 }
                            }]
                        }
                    }
                }
            },
            "cloudflare-workers-ai": {
                "models": {
                    "@cf/model": { "cost": { "input": 1.0, "output": 2.0 } }
                }
            }
        }))
        .unwrap();

        let pricing = flatten_pricing(providers).unwrap();
        assert_eq!(pricing["openai/gpt-5.6"].context_tiers[0].input, 10.0);
        assert_eq!(pricing["workers-ai/@cf/model"].input, 1.0);
    }

    #[test]
    fn rejects_negative_or_non_finite_prices() {
        let providers = serde_json::from_value(json!({
            "openai": {
                "models": {
                    "bad": { "cost": { "input": -1.0, "output": 2.0 } }
                }
            }
        }))
        .unwrap();
        assert!(flatten_pricing(providers).is_err());
        assert!(validate_price("p", "m", "input", f64::NAN).is_err());
    }
}
