use anyhow::Result;
use flawrence_chat::Tool;
use flawrence_chat::UsageByModel;
use schemars::JsonSchema;
use schemars::schema_for;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;
use tracing::debug;

use crate::context::ToolContext;

/// Trait that co-locates a tool's name, description, and handler.
pub trait ToolDef: DeserializeOwned + JsonSchema + Send + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    fn execute(
        ctx: ToolContext,
        args: Self,
    ) -> impl Future<Output = Result<(String, Option<UsageByModel>)>> + Send;
}

/// Rewrite a `schemars`-generated schema into the shape tool-calling models
/// handle most reliably, and that provider "strict" modes can enforce:
///
/// * verbose bookkeeping (`$schema`, `title`) is dropped,
/// * `$ref`/`$defs` indirection is inlined — a unit enum behind a `$ref`
///   becomes a flat `enum` array on the property itself, because weaker models
///   routinely omit a required field whose allowed values live in `$defs`,
/// * string length bounds and numeric range bounds (`minimum`/`maximum`) move
///   into the description, and `schemars`' numeric `format` hints (`uint`,
///   `int64`, …) and the exclusive/`multipleOf` bounds are dropped, since none
///   is part of the schema subset providers accept in strict mode -- an
///   unsigned field's unbidden `minimum: 0` would otherwise 400 the provider,
/// * `anyOf: [X, {"type": "null"}]` -- how `schemars` renders an `Option` whose
///   inner type is itself a schema -- collapses to `X` with `null` folded into
///   its type, matching the flat form every other property already uses,
/// * every property is listed in `required` (optional ones are already
///   nullable), and `additionalProperties: false` is preserved.
fn prepare_schema(value: &mut Value) {
    let definitions = take_definitions(value);
    strip_verbose_fields(value);
    inline_references(value, &definitions);
    flatten_nullable_unions(value);
    fold_constraints(value);
    require_all_properties(value);
}

/// Remove and return the schema's definition table (`$defs`, or `definitions`
/// on older drafts) so references can be resolved against it.
fn take_definitions(value: &mut Value) -> Map<String, Value> {
    let Value::Object(object) = value else {
        return Map::new();
    };
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(definitions)) = object.remove(key) {
            return definitions;
        }
    }
    Map::new()
}

/// Visit every *schema* node, and only schema nodes.
///
/// A JSON Schema object and a `properties` map are both JSON objects, but the
/// keys of the second are argument names chosen by the tool author. Walking
/// values indiscriminately deletes a `title` argument as though it were the
/// `title` keyword — which silently dropped `report_finding`'s required
/// `title` from its schema and made the tool impossible to call.
fn walk_schema_nodes(value: &mut Value, visit: &mut impl FnMut(&mut Map<String, Value>)) {
    let Value::Object(object) = value else {
        return;
    };
    visit(object);

    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for property in properties.values_mut() {
            walk_schema_nodes(property, visit);
        }
    }
    for key in ["items", "additionalProperties", "not", "if", "then", "else"] {
        if let Some(child) = object.get_mut(key)
            && child.is_object()
        {
            walk_schema_nodes(child, visit);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(children)) = object.get_mut(key) {
            for child in children {
                walk_schema_nodes(child, visit);
            }
        }
    }
}

fn strip_verbose_fields(value: &mut Value) {
    walk_schema_nodes(value, &mut |object| {
        object.remove("$schema");
        object.remove("title");
    });
}

/// Replace `{"$ref": "#/$defs/X"}` with the definition itself, collapsing a
/// `oneOf` of string constants into a flat `enum` and folding each variant's
/// doc comment into the property description.
fn inline_references(value: &mut Value, definitions: &Map<String, Value>) {
    let Value::Object(object) = value else {
        return;
    };

    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or_default().to_string();
        if let Some(target) = definitions.get(&name) {
            let mut resolved = target.clone();
            inline_references(&mut resolved, definitions);
            object.remove("$ref");
            merge_resolved(object, resolved);
        }
    }

    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for property in properties.values_mut() {
            inline_references(property, definitions);
        }
    }
    for key in ["items", "additionalProperties", "not", "if", "then", "else"] {
        if let Some(child) = object.get_mut(key)
            && child.is_object()
        {
            inline_references(child, definitions);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(children)) = object.get_mut(key) {
            for child in children {
                inline_references(child, definitions);
            }
        }
    }

    flatten_constant_variants(object);
}

/// Collapse `anyOf: [X, {"type": "null"}]` into `X` with `null` folded into its
/// own type.
///
/// `schemars` renders `Option<T>` this way whenever `T` is itself a schema -- an
/// enum, a struct -- rather than the flat `type: ["string", "null"]` it emits
/// for `Option<String>`. This module exists because weaker models follow the
/// flat form more reliably, so a lone `anyOf` in an otherwise flat tool surface
/// is exactly the shape that gets a provider to hand back an empty body instead
/// of a tool call.
fn flatten_nullable_unions(value: &mut Value) {
    walk_schema_nodes(value, &mut |object| {
        let Some(Value::Array(branches)) = object.get("anyOf") else {
            return;
        };
        if branches.len() != 2 {
            return;
        }
        let Some(null_at) = branches.iter().position(is_null_schema) else {
            return;
        };
        let Some(inner) = branches
            .get(1 - null_at)
            .and_then(Value::as_object)
            .cloned()
        else {
            return;
        };

        object.remove("anyOf");
        for (key, inner_value) in inner {
            let inner_value = if key == "type" {
                permit_null(inner_value)
            } else {
                inner_value
            };
            object.entry(key).or_insert(inner_value);
        }

        // `enum` is a whitelist: a property whose type admits null but whose
        // enum omits it has no way to express "absent", which is the whole
        // point of the Option it came from.
        if let Some(Value::Array(values)) = object.get_mut("enum")
            && !values.iter().any(Value::is_null)
        {
            values.push(Value::Null);
        }
    });
}

/// A bare `{"type": "null"}`, the null half of a `schemars` Option union.
fn is_null_schema(schema: &Value) -> bool {
    schema
        .as_object()
        .is_some_and(|object| object.len() == 1 && object.get("type") == Some(&Value::from("null")))
}

fn permit_null(declared: Value) -> Value {
    match declared {
        Value::String(name) => Value::Array(vec![Value::String(name), Value::from("null")]),
        Value::Array(mut names) => {
            if !names.iter().any(|name| name == "null") {
                names.push(Value::from("null"));
            }
            Value::Array(names)
        }
        other => other,
    }
}

/// Merge a resolved definition into the referring schema. Keys already present
/// on the referrer win, since a property's own schema is the more specific
/// one — except for `description`, where both halves are worth keeping: the
/// property says what the field is for, the definition documents its values.
fn merge_resolved(object: &mut Map<String, Value>, resolved: Value) {
    let Value::Object(resolved) = resolved else {
        return;
    };
    for (key, value) in resolved {
        if key == "description"
            && let Value::String(description) = &value
            && object.contains_key("description")
        {
            append_description(object, description);
            continue;
        }
        object.entry(key).or_insert(value);
    }
}

/// `oneOf: [{const: "add", description: "..."}, ...]` becomes
/// `type: "string", enum: ["add", ...]` with the per-variant docs appended to
/// the description, which is the form models follow most reliably.
fn flatten_constant_variants(object: &mut Map<String, Value>) {
    let Some(Value::Array(variants)) = object.get("oneOf") else {
        return;
    };
    let mut values = Vec::with_capacity(variants.len());
    let mut documentation = Vec::new();
    for variant in variants {
        let Some(variant) = variant.as_object() else {
            return;
        };
        let Some(Value::String(constant)) = variant.get("const") else {
            return;
        };
        if let Some(Value::String(description)) = variant.get("description") {
            documentation.push(format!("{constant}: {description}"));
        }
        values.push(Value::String(constant.clone()));
    }
    if values.is_empty() {
        return;
    }
    object.remove("oneOf");
    object.insert("type".into(), Value::String("string".into()));
    object.insert("enum".into(), Value::Array(values));
    if !documentation.is_empty() {
        append_description(object, &documentation.join(" "));
    }
}

/// Move string length bounds and defaults into the description, and drop
/// `schemars`' numeric `format` hints. None of the three are part of the schema
/// subset providers accept in strict mode, and the first two carry information
/// a model can still use once it is phrased in prose.
fn fold_constraints(value: &mut Value) {
    walk_schema_nodes(value, &mut |object| {
        let max_length = object.remove("maxLength").and_then(|v| v.as_u64());
        let min_length = object.remove("minLength").and_then(|v| v.as_u64());
        match (min_length, max_length) {
            (Some(min), Some(max)) => {
                append_description(object, &format!("({min}-{max} characters.)"))
            }
            (None, Some(max)) => {
                append_description(object, &format!("(At most {max} characters.)"))
            }
            (Some(min), None) => {
                append_description(object, &format!("(At least {min} characters.)"))
            }
            (None, None) => {}
        }
        if is_numeric(object) {
            object.remove("format");
        }
        // Numeric range bounds, folded like the string-length bounds above.
        // Strict tool mode rejects `minimum`/`maximum` on integers, and an
        // unsigned Rust field (`usize`/`u32`) carries a `minimum: 0` it never
        // asked for, so leaving them in drops the tool out of strict mode (or
        // 400s the provider). The exclusive and `multipleOf` variants are
        // equally rejected and rare, so drop them rather than fold.
        let maximum = object.remove("maximum").filter(Value::is_number);
        let minimum = object.remove("minimum").filter(Value::is_number);
        match (&minimum, &maximum) {
            (Some(min), Some(max)) => append_description(object, &format!("({min}-{max}.)")),
            (None, Some(max)) => append_description(object, &format!("(At most {max}.)")),
            (Some(min), None) => append_description(object, &format!("(At least {min}.)")),
            (None, None) => {}
        }
        object.remove("exclusiveMinimum");
        object.remove("exclusiveMaximum");
        object.remove("multipleOf");
        if let Some(default) = object.remove("default") {
            append_description(object, &format!("(Defaults to {default}.)"));
        }
    });
}

fn is_numeric(object: &Map<String, Value>) -> bool {
    match object.get("type") {
        Some(Value::String(name)) => name == "integer" || name == "number",
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| name == "integer" || name == "number"),
        _ => false,
    }
}

fn append_description(object: &mut Map<String, Value>, extra: &str) {
    let description = match object.get("description").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{existing} {extra}"),
        _ => extra.to_string(),
    };
    object.insert("description".into(), Value::String(description));
}

/// List every property in `required`. Optional arguments are already emitted
/// as nullable (`"type": ["string", "null"]`), so this costs an explicit
/// `null` rather than any expressiveness — and it is what strict modes demand.
fn require_all_properties(value: &mut Value) {
    walk_schema_nodes(value, &mut |object| {
        if let Some(Value::Object(properties)) = object.get("properties") {
            let names: Vec<Value> = properties.keys().cloned().map(Value::String).collect();
            object.insert("required".into(), Value::Array(names));
            object
                .entry("additionalProperties")
                .or_insert(Value::Bool(false));
        }
    });
}

/// Top-level argument names, sorted.
fn property_names(schema: &Value) -> Vec<String> {
    let mut names: Vec<String> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// A one-line call signature used when a model sends arguments that do not
/// deserialize. The raw serde message ("missing field `operation`") says what
/// is wrong but never what the tool actually accepts.
fn usage_hint(name: &str, schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return format!("{name}()");
    };
    let rendered: Vec<String> = properties
        .iter()
        .map(|(field, spec)| format!("{field}: {}", type_hint(spec)))
        .collect();
    format!("{name}({})", rendered.join(", "))
}

fn type_hint(spec: &Value) -> String {
    if let Some(Value::Array(values)) = spec.get("enum") {
        let rendered: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(|value| format!("\"{value}\""))
            .collect();
        if !rendered.is_empty() {
            return rendered.join("|");
        }
    }
    match spec.get("type") {
        Some(Value::String(name)) => name.clone(),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ => "value".to_string(),
    }
}

/// Handler type for tool execution (returns result string and optional token usage)
type ToolHandler = Box<
    dyn Fn(
            &ToolContext,
            Value,
        ) -> Pin<Box<dyn Future<Output = Result<(String, Option<UsageByModel>)>> + Send>>
        + Send
        + Sync,
>;

/// Registry holding all tools and their handlers
pub struct ToolRegistry {
    tools: Vec<Tool>,
    handlers: HashMap<String, ToolHandler>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            tools: Vec::new(),
            handlers: HashMap::new(),
        };
        registry.register_tool::<crate::agent::SpawnTaskArgs>();
        registry.register_tool::<crate::code_index::CodeIndexArgs>();
        registry.register_tool::<crate::bash::BashArgs>();
        registry.register_tool::<crate::filesystem::GrepArgs>();
        registry.register_tool::<crate::filesystem::ReadFileArgs>();
        registry.register_tool::<crate::filesystem::ListDirectoryArgs>();
        registry.register_tool::<crate::filesystem::WriteFileArgs>();
        registry.register_tool::<crate::filesystem::ApplyPatchArgs>();
        registry.register_tool::<crate::filesystem::GlobFilesArgs>();
        registry.register_tool::<crate::skill::LoadSkillArgs>();
        registry.register_tool::<crate::submit::SubmitResultArgs>();
        registry.register_tool::<crate::artifact::ArtifactArgs>();
        registry.register_tool::<crate::report_finding::ReportFindingArgs>();
        registry.register_tool::<crate::todo::TodoArgs>();
        registry.register_tool::<crate::git_history::GitHistoryArgs>();
        registry.register_tool::<crate::commitraider::CommitRaiderArgs>();
        registry.register_tool::<crate::advisory::WriteAdvisoryArgs>();
        registry.register_tool::<crate::dedup::DedupCheckArgs>();
        registry.register_tool::<crate::submit_finding::SubmitFindingArgs>();
        registry.register_tool::<crate::trailmark::TrailmarkArgs>();
        registry.register_tool::<crate::github::GitHubCreateAdvisoryArgs>();
        registry.register_tool::<crate::github::GitHubCreateAdvisoryForkArgs>();
        registry.register_tool::<crate::github::GitHubReportVulnerabilityArgs>();
        registry
    }

    /// Register a tool via its ToolDef implementation.
    pub fn register_tool<T: ToolDef>(&mut self) {
        self.register::<T, _, _>(T::NAME, T::DESCRIPTION, T::execute);
    }

    /// Register a tool with typed Args that derive Deserialize + JsonSchema
    fn register<Args, F, Fut>(&mut self, name: &str, description: &str, handler: F)
    where
        Args: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(ToolContext, Args) -> Fut + Send + Sync + Clone + 'static,
        Fut: Future<Output = Result<(String, Option<UsageByModel>)>> + Send + 'static,
    {
        // Generate JSON schema from Args type
        let schema = schema_for!(Args);
        let mut parameters: Value = serde_json::to_value(&schema).unwrap_or_default();
        let declared = property_names(&parameters);
        prepare_schema(&mut parameters);
        // Preparation may rewrite a property, never remove one: a dropped
        // argument produces a schema the model cannot satisfy while serde still
        // demands the field, which is an unbreakable retry loop at runtime.
        // Checked for every registered tool whenever tests run.
        debug_assert_eq!(
            declared,
            property_names(&parameters),
            "preparing the schema for `{name}` changed its argument list"
        );
        let hint = usage_hint(name, &parameters);

        // `strict` is a request, not a guarantee: each client downgrades it for
        // schemas its provider cannot enforce.
        let mut tool = Tool::new(name)
            .with_description(description)
            .with_schema(parameters);
        if strict_tools_enabled() {
            tool = tool.with_strict(true);
        }

        let handler_clone = handler.clone();
        let tool_name = name.to_string();
        let argument_schema = tool.schema.clone().unwrap_or_default();
        let boxed_handler: ToolHandler = Box::new(move |ctx, mut args| {
            let handler = handler_clone.clone();
            let ctx = ctx.clone();
            let hint = hint.clone();
            let tool_name = tool_name.clone();
            let argument_schema = argument_schema.clone();
            Box::pin(async move {
                normalize_arguments(&mut args, &argument_schema);
                let received = summarize_arguments(&args);
                let parsed_args: Args = serde_json::from_value(args).map_err(|e| {
                    invalid_arguments_error(&tool_name, &e.to_string(), &hint, &received)
                })?;
                handler(ctx, parsed_args).await
            })
        });

        self.tools.push(tool);
        self.handlers.insert(name.to_string(), boxed_handler);
    }

    pub fn tools_filtered(&self, allowed: &[String]) -> Vec<Tool> {
        if allowed.is_empty() {
            return self.tools.clone();
        }
        self.tools
            .iter()
            .filter(|t| allowed.contains(&t.name))
            .cloned()
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<(String, Option<UsageByModel>)> {
        let handler = self
            .handlers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {}", name))?;

        let start = Instant::now();
        let result = handler(ctx, args).await;
        let duration_ms = start.elapsed().as_millis();

        match &result {
            Ok((output, usage)) => {
                let usage_str = usage
                    .as_ref()
                    .map(|u| format!(" [{}t]", u.total()))
                    .unwrap_or_default();
                debug!(tool = %name, status = "ok", chars = output.len(), duration_ms = %duration_ms, usage = %usage_str, "Tool completed");
            }
            Err(e) => {
                debug!(tool = %name, status = "error", error = %e, duration_ms = %duration_ms, "Tool completed")
            }
        }

        result
    }

    /// Whether a tool with this name exists at all, regardless of whether the
    /// calling agent may use it.
    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Repair the argument shapes models get wrong in ways the schema makes
/// unambiguous, before deserialisation turns them into an error the model has
/// to burn a turn recovering from. Three cases, all observed in practice:
///
/// * the whole argument object arrives as a JSON *string* (double-encoded),
/// * a scalar arrives as its string spelling — `"limit": "80"`, `"stats": "true"`,
/// * an inapplicable optional arrives as `""` rather than `null`.
///
/// Every repair is driven by the declared type, so nothing is guessed: an
/// argument the schema types as a string keeps whatever string it was given.
fn normalize_arguments(args: &mut Value, schema: &Value) {
    if let Value::String(text) = args
        && schema.get("properties").is_some()
        && let Ok(decoded @ Value::Object(_)) = serde_json::from_str::<Value>(text)
    {
        *args = decoded;
    }
    coerce_blank_optionals(args, schema);
}

/// Blank out empty strings for arguments the schema marks nullable.
///
/// Every argument is required (strict modes demand it), so a model that has
/// nothing to say for an optional one reaches for `""` about as often as
/// `null` — `{"operation":"analyze","target":""}` is a real example. Treating
/// that as a value pushes the failure deeper, where it surfaces as a confusing
/// "no symbol matches ''" instead of the argument simply being absent.
///
/// Only nullable properties are touched, so an argument whose schema is a
/// plain `string` keeps its empty value: `artifact` documents `""` as the way
/// to say "not applicable" for `key` and `content`.
fn coerce_blank_optionals(args: &mut Value, schema: &Value) {
    match args {
        Value::Object(fields) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return;
            };
            for (name, value) in fields.iter_mut() {
                let Some(property) = properties.get(name) else {
                    continue;
                };
                if is_nullable(property) && means_absent(value) {
                    *value = Value::Null;
                    continue;
                }
                coerce_stringified_scalar(value, property);
                coerce_blank_optionals(value, property);
            }
        }
        Value::Array(items) => {
            let Some(element) = schema.get("items") else {
                return;
            };
            for item in items {
                coerce_blank_optionals(item, element);
            }
        }
        _ => {}
    }
}

/// `"80"` where the schema says integer, `"true"` where it says boolean. The
/// spelling is unambiguous, so accept it rather than bouncing the call.
fn coerce_stringified_scalar(value: &mut Value, schema: &Value) {
    let Some(text) = value.as_str().map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    if accepts(schema, "string") {
        return;
    }
    if accepts(schema, "integer")
        && let Ok(number) = text.parse::<i64>()
    {
        *value = Value::from(number);
    } else if accepts(schema, "number")
        && let Ok(number) = text.parse::<f64>()
        && let Some(number) = serde_json::Number::from_f64(number)
    {
        *value = Value::Number(number);
    } else if accepts(schema, "boolean") {
        match text.to_ascii_lowercase().as_str() {
            "true" => *value = Value::Bool(true),
            "false" => *value = Value::Bool(false),
            _ => {}
        }
    }
}

/// Whether the schema's declared type includes `name`.
fn accepts(schema: &Value, name: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(declared)) => return declared == name,
        Some(Value::Array(declared)) => {
            return declared.iter().any(|entry| entry == name);
        }
        _ => {}
    }
    // An `enum` is its own value domain: a whitelist carrying `null` admits
    // null whether or not a `type` keyword survived beside it. Reading `type`
    // alone would make nullability depend on a keyword a provider-compatibility
    // pass may legitimately drop, silently disabling the repair below.
    if name == "null"
        && let Some(Value::Array(values)) = schema.get("enum")
        && values.iter().any(Value::is_null)
    {
        return true;
    }

    // An optional whose inner type is itself a schema -- an enum, a struct --
    // is emitted as `anyOf: [{"$ref": ...}, {"type": "null"}]` with no type of
    // its own. Reading only the top level would call such an argument
    // non-nullable, and the `"null"`-as-a-word repair below would stop firing
    // for exactly the arguments whose docs most invite the model to write it.
    ["anyOf", "oneOf"]
        .iter()
        .filter_map(|key| schema.get(key))
        .filter_map(Value::as_array)
        .flatten()
        .any(|branch| accepts(branch, name))
}

/// Whether a string value is the model's way of writing "nothing here".
///
/// Blank is the common one. The literal `"null"` shows up because every
/// argument is required and the docs say null means absent — so the model
/// writes the word. Only these two: `"none"`, `"nil"` and `"undefined"` are
/// real identifiers in Python, Ruby and JavaScript, and a symbol search for
/// them must not be silently turned into "no symbol".
fn means_absent(value: &Value) -> bool {
    value
        .as_str()
        .map(str::trim)
        .is_some_and(|text| text.is_empty() || text.eq_ignore_ascii_case("null"))
}

fn is_nullable(schema: &Value) -> bool {
    accepts(schema, "null")
}

/// Constrained tool arguments are on by default. `FLAWRENCE_STRICT_TOOLS=0`
/// turns the request off for a provider that rejects strict schemas outright,
/// without needing a rebuild; the improved schema shape stays either way.
fn strict_tools_enabled() -> bool {
    !matches!(
        std::env::var("FLAWRENCE_STRICT_TOOLS")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// The raw serde message ("missing field `operation`") names the problem but
/// not the remedy. Pair it with the call signature and with what the model
/// actually sent, so the retry has everything it needs.
fn invalid_arguments_error(tool: &str, error: &str, hint: &str, received: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Invalid arguments for {tool}: {error}. Expected {hint}. Received {received}. \
         Re-issue the call with every argument present, using null for the ones that do not apply."
    )
}

/// Echo back what the model actually sent, so it can see the difference
/// between what it produced and what the tool expects.
fn summarize_arguments(args: &Value) -> String {
    const MAX_ARGUMENT_CHARS: usize = 200;
    let rendered = match args {
        Value::Object(object) if object.is_empty() => "{} (no arguments)".to_string(),
        other => other.to_string(),
    };
    if rendered.chars().count() <= MAX_ARGUMENT_CHARS {
        return rendered;
    }
    let truncated: String = rendered.chars().take(MAX_ARGUMENT_CHARS).collect();
    format!("{truncated}... [truncated]")
}

// Global registry
static REGISTRY: std::sync::OnceLock<ToolRegistry> = std::sync::OnceLock::new();

pub fn registry() -> &'static ToolRegistry {
    REGISTRY.get_or_init(ToolRegistry::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_of(tool: &str) -> Value {
        let tools = ToolRegistry::new().tools_filtered(&[tool.to_string()]);
        tools[0].schema.clone().expect("tool has a schema")
    }

    /// Every tool argument must reach the model in the flat form the rest of the
    /// surface uses. `Option<SymbolKind>` on `code_index` briefly shipped as
    /// `anyOf: [{enum}, {type: null}]` -- the only union in the whole registry --
    /// and the run that followed produced empty provider responses instead of
    /// tool calls.
    #[test]
    fn no_tool_argument_reaches_the_model_as_a_union() {
        for tool in ToolRegistry::new().tools_filtered(&[]) {
            let Some(schema) = &tool.schema else { continue };
            let rendered = schema.to_string();
            assert!(
                !rendered.contains("anyOf"),
                "{} emits anyOf: {rendered}",
                tool.name
            );
            assert!(
                !rendered.contains("oneOf"),
                "{} emits oneOf: {rendered}",
                tool.name
            );
        }
    }

    /// Flattening must not quietly drop the null half: an omittable enum has to
    /// keep a way to say "absent", in both `type` and the `enum` whitelist.
    #[test]
    fn a_flattened_optional_enum_still_admits_null() {
        let kind = &schema_of("code_index")["properties"]["kind"];

        assert_eq!(
            kind["type"],
            serde_json::json!(["string", "null"]),
            "{kind}"
        );
        let values = kind["enum"].as_array().expect("an enum whitelist");
        assert!(
            values.iter().any(Value::is_null),
            "null must be sayable: {kind}"
        );
        assert!(
            values.iter().any(|value| value == "class"),
            "the real kinds must survive: {kind}"
        );
        assert!(is_nullable(kind), "{kind}");
    }

    /// `code_index`'s `kind` filter reached the model as free-form text, and it was duly
    /// filled with "null", "outline", "find" and "source" -- none of which are kinds. An
    /// unparseable kind silently meant "no filter", so a guess and a correct omission were
    /// indistinguishable. The allowed values must reach the model as a closed set.
    #[test]
    fn the_code_index_kind_filter_offers_a_closed_set_of_values() {
        let schema = schema_of("code_index");
        let kind = &schema["properties"]["kind"];
        let rendered = kind.to_string();

        for value in [
            "function",
            "method",
            "class",
            "module",
            "constant",
            "interface",
        ] {
            assert!(
                rendered.contains(value),
                "kind must offer `{value}`: {rendered}"
            );
        }
        assert!(
            !rendered.contains("$ref"),
            "values must be inline: {rendered}"
        );
        assert!(
            kind.get("type").and_then(Value::as_str) != Some("string"),
            "kind must not be plain free-form text: {rendered}"
        );
        assert!(is_nullable(kind), "kind must stay omittable: {rendered}");
    }

    #[test]
    fn enum_arguments_are_inlined_with_their_documentation() {
        let schema = schema_of("todo");
        let rendered = schema.to_string();
        assert!(!rendered.contains("$ref"), "{rendered}");
        assert!(!rendered.contains("$defs"), "{rendered}");

        let operation = &schema["properties"]["operation"];
        assert_eq!(operation["type"], "string");
        assert_eq!(
            operation["enum"],
            serde_json::json!(["add", "update", "delete", "complete", "list"])
        );

        // The property's own doc and each variant's doc both survive.
        let description = operation["description"].as_str().unwrap();
        assert!(
            description.contains("The operation to perform."),
            "{description}"
        );
        assert!(
            description.contains("add: Add a new todo item."),
            "{description}"
        );
    }

    #[test]
    fn arguments_named_like_schema_keywords_survive() {
        // `report_finding` takes a `title`, and schema preparation used to
        // delete it as though it were the `title` keyword — leaving a tool the
        // model could never call successfully, because serde still required it.
        let schema = schema_of("report_finding");
        let properties = schema["properties"].as_object().unwrap();

        for name in [
            "title",
            "description",
            "finding_key",
            "severity",
            "file",
            "line",
        ] {
            assert!(
                properties.contains_key(name),
                "{name} missing from {schema}"
            );
        }
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"title"), "{required:?}");
        assert!(usage_hint("report_finding", &schema).contains("title: string"));
    }

    #[test]
    fn keyword_named_arguments_are_untouched_across_every_pass() {
        // One synthetic tool exercising each pass against a property whose name
        // collides with the keyword that pass looks for.
        let mut schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Args",
            "type": "object",
            "properties": {
                "title":       { "type": "string" },
                "$schema":     { "type": "string" },
                "format":      { "type": "string" },
                "default":     { "type": "string" },
                "maxLength":   { "type": "string" },
                "items":       { "type": "string" },
                "oneOf":       { "type": "string" },
                "properties":  { "type": "string" },
                "required":    { "type": "string" }
            },
            "required": ["title"]
        });
        prepare_schema(&mut schema);

        let properties = schema["properties"].as_object().unwrap();
        for name in [
            "title",
            "$schema",
            "format",
            "default",
            "maxLength",
            "items",
            "oneOf",
            "properties",
            "required",
        ] {
            assert!(properties.contains_key(name), "{name} was eaten: {schema}");
        }
        // The real keywords on the schema node itself are still stripped.
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("title").is_none());
    }

    #[test]
    fn every_argument_is_required_and_extras_are_rejected() {
        let schema = schema_of("todo");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();

        // Optional arguments stay nullable, so "required" costs an explicit
        // null rather than forcing a value.
        for name in ["operation", "content", "id"] {
            assert!(required.contains(&name), "{name} missing from {required:?}");
        }
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        assert_eq!(
            schema["properties"]["content"]["type"],
            serde_json::json!(["string", "null"])
        );
    }

    #[test]
    fn string_bounds_defaults_and_numeric_formats_become_prose() {
        let todo = schema_of("todo");
        let content = &todo["properties"]["content"];
        assert!(content.get("maxLength").is_none());
        assert!(
            content["description"]
                .as_str()
                .unwrap()
                .contains("At most 500 characters"),
            "{content}"
        );

        let history = schema_of("git_history");
        let limit = &history["properties"]["limit"];
        assert!(limit.get("default").is_none());
        assert!(
            limit["description"]
                .as_str()
                .unwrap()
                .contains("Defaults to 20"),
            "{limit}"
        );

        // `schemars` emits `"format": "uint"`, which no provider understands.
        let read_file = schema_of("read_file");
        assert!(read_file["properties"]["offset"].get("format").is_none());
    }

    #[test]
    fn tools_ask_for_strict_argument_validation() {
        let tools = ToolRegistry::new().tools_filtered(&["todo".to_string()]);
        assert_eq!(tools[0].strict, Some(true));
    }

    #[test]
    fn strict_requests_can_be_switched_off_by_environment() {
        // Serialised against other env-mutating tests by running in one test.
        for value in ["0", "false", "off", "no"] {
            unsafe { std::env::set_var("FLAWRENCE_STRICT_TOOLS", value) };
            assert!(
                !strict_tools_enabled(),
                "{value} should disable strict mode"
            );
        }
        for value in ["1", "true", ""] {
            unsafe { std::env::set_var("FLAWRENCE_STRICT_TOOLS", value) };
            assert!(strict_tools_enabled(), "{value} should keep strict mode");
        }
        unsafe { std::env::remove_var("FLAWRENCE_STRICT_TOOLS") };
        assert!(strict_tools_enabled());
    }

    #[test]
    fn usage_hint_lists_arguments_and_their_allowed_values() {
        let hint = usage_hint("todo", &schema_of("todo"));
        assert_eq!(
            hint,
            "todo(content: string|null, id: string|null, operation: \"add\"|\"update\"|\"delete\"|\"complete\"|\"list\")"
        );
    }

    #[test]
    fn invalid_arguments_error_names_the_signature_and_the_input() {
        let hint = usage_hint("todo", &schema_of("todo"));
        let received = summarize_arguments(&serde_json::json!({"content": "explore"}));
        let error = invalid_arguments_error("todo", "missing field `operation`", &hint, &received)
            .to_string();

        assert!(error.contains("missing field `operation`"), "{error}");
        assert!(error.contains("\"add\"|\"update\""), "{error}");
        assert!(
            error.contains(r#"Received {"content":"explore"}"#),
            "{error}"
        );
        assert!(error.contains("using null"), "{error}");
    }

    #[test]
    fn oversized_arguments_are_truncated_in_the_error() {
        let received = summarize_arguments(&serde_json::json!({"content": "x".repeat(5_000)}));
        assert!(received.ends_with("... [truncated]"));
        assert!(received.chars().count() < 300);
    }

    #[test]
    fn blank_optional_arguments_are_treated_as_absent() {
        // Observed in a live run: every argument is required now, so the model
        // filled the inapplicable one with "" instead of null.
        let schema = schema_of("trailmark");
        let mut args = serde_json::json!({
            "operation": "analyze",
            "language": "",
            "target": "   "
        });
        coerce_blank_optionals(&mut args, &schema);

        assert_eq!(args["language"], Value::Null);
        assert_eq!(args["target"], Value::Null);
        assert_eq!(args["operation"], "analyze");

        // ... and the tool then sees them as unset.
        let parsed: crate::trailmark::TrailmarkArgs = serde_json::from_value(args).unwrap();
        assert!(parsed.language.is_none());
        assert!(parsed.target.is_none());
    }

    #[test]
    fn the_word_null_is_read_as_absent() {
        // Observed live: required-all plus "null means X" docs get the model to
        // write the word rather than the JSON literal.
        let schema = schema_of("code_index");
        let mut args = serde_json::json!({
            "operation": "find",
            "symbol": "decodeCurrentElementWith",
            "path": "null",
            "kind": "NULL"
        });
        normalize_arguments(&mut args, &schema);

        assert_eq!(args["path"], Value::Null);
        assert_eq!(args["kind"], Value::Null);
        assert_eq!(args["symbol"], "decodeCurrentElementWith");
    }

    #[test]
    fn identifiers_that_merely_look_empty_are_preserved() {
        // `None`, `nil` and `undefined` are real symbols in Python, Ruby and
        // JavaScript; a search for one must not become "no symbol".
        let schema = schema_of("code_index");
        for symbol in ["None", "nil", "undefined", "NULL_TERMINATOR"] {
            let mut args = serde_json::json!({
                "operation": "find", "symbol": symbol, "path": null, "kind": null
            });
            normalize_arguments(&mut args, &schema);
            assert_eq!(args["symbol"], symbol, "{symbol} was swallowed");
        }
    }

    #[test]
    fn blanks_are_kept_where_the_tool_documents_them() {
        // `artifact` documents "" as the way to say "not applicable" for read,
        // delete and list, and its schema types those arguments as plain
        // strings rather than nullable ones.
        let schema = schema_of("artifact");
        let mut args = serde_json::json!({"operation": "list", "key": "", "content": ""});
        coerce_blank_optionals(&mut args, &schema);

        assert_eq!(args["key"], "");
        assert_eq!(args["content"], "");
    }

    #[test]
    fn blank_optionals_are_coerced_inside_nested_arguments() {
        let schema = schema_of("github_report_vulnerability");
        let mut args = serde_json::json!({
            "vulnerabilities": [
                { "ecosystem": "maven", "name": "a:b", "patched_versions": "", "vulnerable_version_range": "<= 1" }
            ]
        });
        coerce_blank_optionals(&mut args, &schema);

        let entry = &args["vulnerabilities"][0];
        assert_eq!(entry["patched_versions"], Value::Null);
        assert_eq!(entry["vulnerable_version_range"], "<= 1");
        assert_eq!(entry["name"], "a:b");
    }

    #[test]
    fn stringified_scalars_are_read_as_their_declared_type() {
        let schema = schema_of("read_file");
        let mut args = serde_json::json!({"path": "src/main.rs", "offset": "0", "limit": "80"});
        normalize_arguments(&mut args, &schema);

        assert_eq!(args["offset"], 0);
        assert_eq!(args["limit"], 80);
        // A string-typed argument is never reinterpreted, even when it looks
        // like a number.
        assert_eq!(args["path"], "src/main.rs");

        let parsed: crate::filesystem::ReadFileArgs = serde_json::from_value(args).unwrap();
        assert_eq!(parsed.limit, 80);
    }

    #[test]
    fn stringified_booleans_are_read_as_booleans() {
        let schema = schema_of("commitraider");
        let mut args = serde_json::json!({"operation": "scan", "stats": "true", "limit": "10"});
        normalize_arguments(&mut args, &schema);

        assert_eq!(args["stats"], Value::Bool(true));
        assert_eq!(args["limit"], 10);
    }

    #[test]
    fn numeric_looking_strings_that_are_not_numbers_are_left_alone() {
        let schema = schema_of("read_file");
        let mut args = serde_json::json!({"path": "a.rs", "offset": "start", "limit": 10});
        normalize_arguments(&mut args, &schema);

        // Left as-is so the error names the real problem instead of a silent
        // wrong value.
        assert_eq!(args["offset"], "start");
    }

    #[test]
    fn double_encoded_argument_objects_are_decoded() {
        let schema = schema_of("todo");
        let mut args =
            Value::String(r#"{"operation":"list","content":null,"id":null}"#.to_string());
        normalize_arguments(&mut args, &schema);

        assert_eq!(args["operation"], "list");
        let parsed: crate::todo::TodoArgs = serde_json::from_value(args).unwrap();
        assert!(matches!(parsed.operation, crate::todo::TodoOperation::List));
    }

    #[test]
    fn unknown_tools_are_distinguishable_from_forbidden_ones() {
        let registry = ToolRegistry::new();
        assert!(registry.contains("todo"));
        assert!(!registry.contains("choose_step"));
    }

    #[test]
    fn read_file_schema_requires_line_range() {
        let tools = ToolRegistry::new().tools_filtered(&["read_file".to_string()]);
        let required = tools[0].schema.as_ref().unwrap()["required"]
            .as_array()
            .unwrap();

        for name in ["path", "offset", "limit"] {
            assert!(required.iter().any(|value| value == name));
        }
    }

    #[test]
    fn artifact_schema_requires_every_argument() {
        let tools = ToolRegistry::new().tools_filtered(&["artifact".to_string()]);
        let required = tools[0].schema.as_ref().unwrap()["required"]
            .as_array()
            .unwrap();

        for name in ["operation", "key", "content"] {
            assert!(required.iter().any(|value| value == name));
        }
    }
}
