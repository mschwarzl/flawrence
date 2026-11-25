use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Tracks token usage for a single model
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input: u64,
    pub cache_write_input: u64,
    pub cache_read_input: u64,
    pub output: u64,
    pub cost_usd: f64,
}

impl TokenUsage {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.input + self.cache_write_input + self.cache_read_input + self.output
    }

    /// Add token counts. For non-caching models, pass 0 for cache fields.
    pub fn add(&mut self, input: u64, cache_write: u64, cache_read: u64, output: u64) {
        self.input = self.input.saturating_add(input);
        self.cache_write_input = self.cache_write_input.saturating_add(cache_write);
        self.cache_read_input = self.cache_read_input.saturating_add(cache_read);
        self.output = self.output.saturating_add(output);
    }
}

/// Tracks token usage per model for accurate cost calculation
#[derive(Debug, Clone, Default)]
pub struct UsageByModel {
    pub by_model: HashMap<String, TokenUsage>,
}

/// Thread-safe shared usage tracker.
/// Used for both per-task usage (read by log formatter) and global cross-session totals.
pub type SharedUsage = Arc<Mutex<UsageByModel>>;

impl UsageByModel {
    /// Add token counts for a specific model
    pub fn add(&mut self, model: &str, input: u32, cache_write: u32, cache_read: u32, output: u32) {
        let input = u64::from(input);
        let cache_write = u64::from(cache_write);
        let cache_read = u64::from(cache_read);
        let output = u64::from(output);
        let usage = self.by_model.entry(model.to_string()).or_default();
        usage.add(input, cache_write, cache_read, output);
        if let Some(pricing) = pricing_for_context(model, input + cache_write + cache_read) {
            usage.cost_usd += pricing.cost(input, cache_write, cache_read, output);
        }
    }

    /// Merge another UsageByModel into this one (for subagent merging)
    pub fn merge(&mut self, other: &UsageByModel) {
        for (model, usage) in &other.by_model {
            let target = self.by_model.entry(model.clone()).or_default();
            target.add(
                usage.input,
                usage.cache_write_input,
                usage.cache_read_input,
                usage.output,
            );
            target.cost_usd += usage.cost_usd;
        }
    }

    /// Create a new shared usage tracker (zero-initialized).
    pub fn new_shared() -> SharedUsage {
        Arc::new(Mutex::new(UsageByModel::default()))
    }

    /// Get total tokens across all models
    #[must_use]
    pub fn total(&self) -> u64 {
        self.by_model.values().map(|u| u.total()).sum()
    }

    /// Compatibility alias for the `u64` total used by budget enforcement.
    #[must_use]
    pub fn total_u64(&self) -> u64 {
        self.total()
    }

    /// Render this usage as a JSON object keyed by model name. Single
    /// canonical shape consumed by both the public `JobView` and the
    /// event stream.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let map: serde_json::Map<String, serde_json::Value> = self
            .by_model
            .iter()
            .map(|(model, u)| {
                (
                    model.clone(),
                    serde_json::json!({
                        "input_tokens": u.input,
                        "output_tokens": u.output,
                        "cache_read_tokens": u.cache_read_input,
                        "cache_write_tokens": u.cache_write_input,
                    }),
                )
            })
            .collect();
        serde_json::Value::Object(map)
    }

    /// Calculate total cost across all models with per-model pricing
    #[must_use]
    pub fn total_cost(&self) -> f64 {
        self.by_model.values().map(|usage| usage.cost_usd).sum()
    }
}

/// Output from running a task
#[derive(Debug, Clone, Default)]
pub struct TaskOutput {
    pub response: String,
    /// Complete billed usage for this root run.
    pub usage: UsageByModel,
    /// Usage returned through completed task paths. The difference from
    /// `usage` is failed or otherwise unreturned child work.
    pub attributed_usage: UsageByModel,
    pub elapsed_secs: f64,
}

impl TaskOutput {
    /// A zero-usage result for a subagent that was cancelled before it could
    /// return, so the parent sees a plain message rather than an error and can
    /// move on to consolidating what the finished agents produced.
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            response: message.into(),
            ..Self::default()
        }
    }
}

/// Pricing: (input, cache_write, cache_read, output) in $/MTok
///
/// Populated at startup from <https://models.dev/api.json>.
/// Falls back to zeros if not initialized (e.g. in tests).
#[derive(Debug, Clone, Default)]
pub struct ModelPricing {
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
    pub context_tiers: Vec<ContextPricingTier>,
}

#[derive(Debug, Clone)]
pub struct ContextPricingTier {
    pub min_input_tokens: u64,
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
}

impl ModelPricing {
    fn tuple(&self) -> (f64, f64, f64, f64) {
        (self.input, self.cache_write, self.cache_read, self.output)
    }

    fn cost(&self, input: u64, cache_write: u64, cache_read: u64, output: u64) -> f64 {
        (input as f64 / 1_000_000.0) * self.input
            + (cache_write as f64 / 1_000_000.0) * self.cache_write
            + (cache_read as f64 / 1_000_000.0) * self.cache_read
            + (output as f64 / 1_000_000.0) * self.output
    }

    fn for_context(&self, input_tokens: u64) -> Self {
        let mut selected = self.clone();
        if let Some(tier) = self
            .context_tiers
            .iter()
            .filter(|tier| input_tokens > tier.min_input_tokens)
            .max_by_key(|tier| tier.min_input_tokens)
        {
            selected.input = tier.input;
            selected.cache_write = tier.cache_write;
            selected.cache_read = tier.cache_read;
            selected.output = tier.output;
        }
        selected
    }
}

static MODEL_PRICING: OnceLock<HashMap<String, ModelPricing>> = OnceLock::new();
static DEFAULT_MODEL_PRICING: OnceLock<ModelPricing> = OnceLock::new();

/// Initialize the global pricing table. Call once at startup after fetching
/// pricing data. Subsequent calls are ignored (first write wins).
pub fn init_pricing(pricing: HashMap<String, ModelPricing>) {
    let _ = MODEL_PRICING.set(pricing);
}

/// Initialize a pricing override applied to every model. Subsequent calls are ignored.
pub fn init_default_pricing(pricing: ModelPricing) {
    let _ = DEFAULT_MODEL_PRICING.set(pricing);
}

/// Strip trailing date suffix (e.g. "-20250514") from a model string.
fn strip_date_suffix(model: &str) -> &str {
    if let Some((prefix, suffix)) = model.rsplit_once('-')
        && suffix.len() == 8
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        return prefix;
    }
    model
}

/// Get pricing for a model ($/MTok): (input, cache_write, cache_read, output)
pub fn get_model_pricing(model: &str) -> (f64, f64, f64, f64) {
    model_pricing(model)
        .map(ModelPricing::tuple)
        .unwrap_or((0.0, 0.0, 0.0, 0.0))
}

/// Return configured pricing for a model, preserving unknown-vs-free.
#[must_use]
pub fn model_pricing(model: &str) -> Option<&'static ModelPricing> {
    if let Some(pricing) = DEFAULT_MODEL_PRICING.get() {
        return Some(pricing);
    }
    let table = MODEL_PRICING.get()?;
    lookup_model_pricing(table, model)
}

/// Return the models.dev key selected through best-effort provider aliasing.
#[must_use]
pub fn model_pricing_alias(model: &str) -> Option<&'static str> {
    let table = MODEL_PRICING.get()?;
    lookup_model_pricing_alias(table, model)
}

fn lookup_model_pricing_alias<'a>(
    table: &'a HashMap<String, ModelPricing>,
    model: &str,
) -> Option<&'a str> {
    if table.get(model).is_some() || table.get(strip_date_suffix(model)).is_some() {
        return None;
    }
    let lookup_model = model.split_once('/').map(|(_, rest)| rest).unwrap_or(model);
    if table.get(lookup_model).is_some() || table.get(strip_date_suffix(lookup_model)).is_some() {
        return None;
    }
    best_provider_alias(table, model)
        .or_else(|| best_provider_alias(table, lookup_model))
        .map(|(key, _)| key.as_str())
}

fn lookup_model_pricing<'a>(
    table: &'a HashMap<String, ModelPricing>,
    model: &str,
) -> Option<&'a ModelPricing> {
    lookup_direct_model_pricing(table, model).or_else(|| {
        let lookup_model = model.split_once('/').map(|(_, rest)| rest).unwrap_or(model);
        best_provider_alias(table, model)
            .or_else(|| best_provider_alias(table, lookup_model))
            .map(|(_, pricing)| pricing)
    })
}

fn lookup_direct_model_pricing<'a>(
    table: &'a HashMap<String, ModelPricing>,
    model: &str,
) -> Option<&'a ModelPricing> {
    if let Some(pricing) = table.get(model) {
        return Some(pricing);
    }
    if let Some(pricing) = table.get(strip_date_suffix(model)) {
        return Some(pricing);
    }
    if !model.contains('/') {
        let workers_ai_model = format!("workers-ai/{model}");
        if let Some(pricing) = table.get(&workers_ai_model) {
            return Some(pricing);
        }
        return table.get(strip_date_suffix(&workers_ai_model));
    }

    // Custom OpenAI-compatible hosts use their local provider name as the
    // routing prefix. Remove only that prefix: the remainder may itself
    // identify a public provider such as minmax/minmax-m3.
    let (_, model_id) = model.split_once('/')?;
    table
        .get(model_id)
        .or_else(|| table.get(strip_date_suffix(model_id)))
}

fn best_provider_alias<'a>(
    table: &'a HashMap<String, ModelPricing>,
    model: &str,
) -> Option<(&'a String, &'a ModelPricing)> {
    let (provider, model_id) = model.split_once('/')?;
    table
        .iter()
        .filter_map(|(key, pricing)| {
            let (candidate_provider, candidate_model_id) = key.split_once('/')?;
            let overlap = provider_prefix_overlap(provider, candidate_provider)?;
            (!candidate_provider.eq_ignore_ascii_case(provider)
                && candidate_model_id.eq_ignore_ascii_case(model_id))
            .then_some((key, pricing, overlap, candidate_provider.len()))
        })
        .max_by(|left, right| {
            left.2
                .cmp(&right.2)
                .then_with(|| right.3.cmp(&left.3))
                .then_with(|| right.0.cmp(left.0))
        })
        .map(|(key, pricing, _, _)| (key, pricing))
}

fn provider_prefix_overlap(first: &str, second: &str) -> Option<usize> {
    let first = first.to_ascii_lowercase();
    let second = second.to_ascii_lowercase();
    (first.starts_with(&second) || second.starts_with(&first))
        .then_some(first.len().min(second.len()))
}

fn pricing_for_context(model: &str, input_tokens: u64) -> Option<ModelPricing> {
    Some(model_pricing(model)?.for_context(input_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_u64_does_not_overflow_across_models() {
        let mut usage = UsageByModel::default();
        usage.add("model-a", u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        usage.add("model-a", u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        usage.add("model-b", u32::MAX, 0, 0, 0);

        let expected = 8 * u64::from(u32::MAX) + u64::from(u32::MAX);
        assert_eq!(usage.total_u64(), expected);
        assert!(usage.total_u64() > u64::from(u32::MAX));
    }

    #[test]
    fn total_u64_matches_total_for_small_counts() {
        let mut usage = UsageByModel::default();
        usage.add("m", 10, 20, 30, 40);
        assert_eq!(usage.total_u64(), usage.total());
        assert_eq!(usage.total_u64(), 100);
    }

    #[test]
    fn context_pricing_tier_applies_only_above_threshold() {
        let pricing = ModelPricing {
            input: 5.0,
            output: 30.0,
            context_tiers: vec![ContextPricingTier {
                min_input_tokens: 272_000,
                input: 10.0,
                cache_write: 12.5,
                cache_read: 1.0,
                output: 45.0,
            }],
            ..ModelPricing::default()
        };

        assert_eq!(pricing.for_context(272_000).input, 5.0);
        assert_eq!(pricing.for_context(272_001).input, 10.0);
    }

    #[test]
    fn bare_workers_ai_model_uses_prefixed_pricing() {
        let mut table = HashMap::new();
        table.insert(
            "workers-ai/test-model".to_string(),
            ModelPricing {
                input: 1.0,
                ..ModelPricing::default()
            },
        );

        assert_eq!(
            lookup_model_pricing(&table, "test-model").unwrap().input,
            1.0
        );
    }

    #[test]
    fn custom_host_strips_only_its_routing_prefix_for_pricing() {
        let mut table = HashMap::new();
        table.insert(
            "minmax/minmax-m3".to_string(),
            ModelPricing {
                input: 5.0,
                ..ModelPricing::default()
            },
        );

        assert_eq!(
            lookup_model_pricing(&table, "myhost/minmax/minmax-m3")
                .unwrap()
                .input,
            5.0
        );
    }

    #[test]
    fn provider_alias_uses_the_longest_matching_prefix() {
        let mut table = HashMap::new();
        table.insert("mini/MiniMax-M3".to_string(), ModelPricing::default());
        table.insert(
            "minimax-cn/MiniMax-M3".to_string(),
            ModelPricing {
                input: 5.0,
                ..ModelPricing::default()
            },
        );

        let (key, pricing) = best_provider_alias(&table, "minimax/minimax-m3").unwrap();
        assert_eq!(key, "minimax-cn/MiniMax-M3");
        assert_eq!(pricing.input, 5.0);
    }

    #[test]
    fn provider_alias_prefers_the_shortest_tied_prefix() {
        let mut table = HashMap::new();
        table.insert(
            "minimax-cn/MiniMax-M3".to_string(),
            ModelPricing {
                input: 0.3,
                ..ModelPricing::default()
            },
        );
        table.insert(
            "minimax-cn-coding-plan/MiniMax-M3".to_string(),
            ModelPricing::default(),
        );

        let (key, pricing) = best_provider_alias(&table, "minimax/MiniMax-M3").unwrap();
        assert_eq!(key, "minimax-cn/MiniMax-M3");
        assert_eq!(pricing.input, 0.3);
    }

    #[test]
    fn custom_host_uses_a_provider_alias_for_case_normalized_model_ids() {
        let mut table = HashMap::new();
        table.insert(
            "minimax/MiniMax-M3".to_string(),
            ModelPricing {
                input: 0.2,
                ..ModelPricing::default()
            },
        );
        table.insert(
            "minimax-cn/MiniMax-M3".to_string(),
            ModelPricing {
                input: 0.3,
                ..ModelPricing::default()
            },
        );
        table.insert(
            "minimax-cn-coding-plan/MiniMax-M3".to_string(),
            ModelPricing::default(),
        );

        assert!(lookup_direct_model_pricing(&table, "lyceum/minimax/minimax-m3").is_none());
        assert_eq!(
            lookup_model_pricing(&table, "lyceum/minimax/minimax-m3")
                .unwrap()
                .input,
            0.3
        );
    }

    #[test]
    fn custom_host_pricing_lookup_reports_the_selected_key() {
        let mut table = HashMap::new();
        table.insert("minimax-cn/MiniMax-M3".to_string(), ModelPricing::default());

        assert_eq!(
            lookup_model_pricing_alias(&table, "lyceum/minimax/minimax-m3"),
            Some("minimax-cn/MiniMax-M3")
        );
    }
}
