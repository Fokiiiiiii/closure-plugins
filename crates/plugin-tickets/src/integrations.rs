use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use closure_core::integration_secrets::{ExposeSecret, SecretString};
use closure_core::module::EventContext;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::advanced_forms::FormOption;

const MAX_INTEGRATIONS: usize = 20;
const MAX_INTEGRATION_ID_CHARS: usize = 32;
const MAX_ENDPOINT_CHARS: usize = 2048;
const MAX_JSON_POINTER_CHARS: usize = 512;
const MAX_DYNAMIC_OPTIONS: usize = 25;
const MAX_OPTION_CHARS: usize = 100;
const MAX_INTEGRATION_HEADERS: usize = 5;
const MAX_INTEGRATION_SECRETS: usize = 5;
const MAX_RESPONSE_MAPPINGS: usize = 15;
const MAX_SECRET_NAME_CHARS: usize = 64;
const MAX_HEADER_NAME_CHARS: usize = 64;
const MAX_HEADER_TEMPLATE_CHARS: usize = 256;
const MAX_MAPPING_NAME_CHARS: usize = 32;
const MAX_MAPPING_PATH_CHARS: usize = 256;
const MAX_PLACEHOLDER_VALUE_CHARS: usize = 1000;
const WELCOME_INTEGRATION_TIMEOUT: Duration = Duration::from_secs(2);
const OPTIONS_CACHE_TTL: Duration = Duration::from_secs(30);
const OPTIONS_CACHE_LIMIT: usize = 256;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthKind {
    #[default]
    None,
    Bearer,
    ApiKey,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub(crate) struct Integration {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) generation: String,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) auth: AuthKind,
    #[serde(default)]
    pub(crate) method: RequestMethod,
    #[serde(default)]
    pub(crate) headers: Vec<IntegrationHeader>,
    #[serde(default)]
    pub(crate) secrets: Vec<String>,
    #[serde(default)]
    pub(crate) response_mappings: Vec<PlaceholderMapping>,
    #[serde(default)]
    pub(crate) send_form_data: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestMethod {
    #[default]
    Get,
    Post,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntegrationHeader {
    pub(crate) name: String,
    pub(crate) value_template: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaceholderMapping {
    pub(crate) name: String,
    pub(crate) path: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WelcomeContext<'a> {
    pub(crate) guild_id: u64,
    pub(crate) user_id: u64,
    pub(crate) ticket_id: u64,
    pub(crate) ticket_channel_id: u64,
    pub(crate) form_data: &'a [(String, String)],
}

#[derive(Debug, Clone, Copy)]
struct RequestContext<'a> {
    guild_id: u64,
    user_id: Option<u64>,
    ticket_id: Option<u64>,
    ticket_channel_id: Option<u64>,
    form_data: Option<&'a [(String, String)]>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub(crate) struct DynamicOptions {
    pub(crate) integration: String,
    #[serde(default)]
    pub(crate) items_pointer: String,
    pub(crate) label_pointer: String,
    pub(crate) value_pointer: String,
    #[serde(default)]
    pub(crate) description_pointer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OptionsError {
    InvalidConfiguration,
    SecretUnavailable,
    FetchFailed,
    InvalidResponse,
    TooManyOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OptionsCacheKey {
    guild_id: u64,
    integration: Integration,
    source: DynamicOptions,
}

struct OptionsCacheEntry {
    stored_at: Instant,
    options: Vec<FormOption>,
}

static OPTIONS_CACHE: OnceLock<Mutex<HashMap<OptionsCacheKey, OptionsCacheEntry>>> =
    OnceLock::new();

pub(crate) fn config_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_INTEGRATIONS,
        "description": "config-tickets-integrations",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "url"],
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[a-z0-9]{1,32}$",
                    "description": "config-tickets-integration-id"
                },
                "generation": {
                    "type": "string",
                    "minLength": 32,
                    "maxLength": 32,
                    "pattern": "^[a-f0-9]{32}$",
                    "readOnly": true,
                    "x-ui": { "widget": "hidden" }
                },
                "url": {
                    "type": "string",
                    "minLength": 9,
                    "maxLength": MAX_ENDPOINT_CHARS,
                    "pattern": "^https://[^#\\s]+$",
                    "description": "config-tickets-integration-url"
                },
                "auth": {
                    "type": "string",
                    "enum": ["none", "bearer", "api_key"],
                    "default": "none",
                    "description": "config-tickets-integration-auth"
                },
                "method": {
                    "type": "string",
                    "enum": ["get", "post"],
                    "default": "get",
                    "description": "config-tickets-integration-method"
                },
                "headers": {
                    "type": "array",
                    "maxItems": MAX_INTEGRATION_HEADERS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "value_template"],
                        "properties": {
                            "name": { "type": "string", "minLength": 1, "maxLength": MAX_HEADER_NAME_CHARS, "pattern": "^[A-Za-z0-9-]+$", "description": "config-tickets-integration-header-name" },
                            "value_template": { "type": "string", "minLength": 1, "maxLength": MAX_HEADER_TEMPLATE_CHARS, "description": "config-tickets-integration-header-value-template" }
                        }
                    },
                    "x-ui": { "widget": "nested-list" },
                    "description": "config-tickets-integration-headers"
                },
                "secrets": {
                    "type": "array",
                    "maxItems": MAX_INTEGRATION_SECRETS,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_SECRET_NAME_CHARS,
                        "pattern": "^[a-z][a-z0-9_-]{0,63}$",
                        "description": "config-tickets-integration-secret-name"
                    },
                    "description": "config-tickets-integration-secrets"
                },
                "response_mappings": {
                    "type": "array",
                    "maxItems": MAX_RESPONSE_MAPPINGS,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "path"],
                        "properties": {
                            "name": { "type": "string", "minLength": 1, "maxLength": MAX_MAPPING_NAME_CHARS, "pattern": "^[A-Za-z][A-Za-z0-9_-]*$", "description": "config-tickets-integration-mapping-name" },
                            "path": { "type": "string", "minLength": 1, "maxLength": MAX_MAPPING_PATH_CHARS, "description": "config-tickets-integration-mapping-path" }
                        }
                    },
                    "x-ui": { "widget": "nested-list" },
                    "description": "config-tickets-integration-response-mappings"
                },
                "send_form_data": {
                    "type": "boolean",
                    "default": false,
                    "description": "config-tickets-integration-send-form-data"
                }
            }
        },
        "x-ui": { "level": "advanced" }
    })
}

pub(crate) fn dynamic_options_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["integration", "label_pointer", "value_pointer"],
        "properties": {
            "integration": {
                "type": "string",
                "pattern": "^[a-z0-9]{1,32}$",
                "description": "config-tickets-form-dynamic-options-integration"
            },
            "items_pointer": {
                "type": "string",
                "maxLength": MAX_JSON_POINTER_CHARS,
                "default": "",
                "description": "config-tickets-form-dynamic-options-items_pointer"
            },
            "label_pointer": {
                "type": "string",
                "maxLength": MAX_JSON_POINTER_CHARS,
                "description": "config-tickets-form-dynamic-options-label_pointer"
            },
            "value_pointer": {
                "type": "string",
                "maxLength": MAX_JSON_POINTER_CHARS,
                "description": "config-tickets-form-dynamic-options-value_pointer"
            },
            "description_pointer": {
                "anyOf": [
                    { "type": "string", "maxLength": MAX_JSON_POINTER_CHARS },
                    { "type": "null" }
                ],
                "default": null,
                "description": "config-tickets-form-dynamic-options-description_pointer"
            }
        },
        "description": "config-tickets-form-dynamic-options"
    })
}

pub(crate) fn valid_integrations(integrations: &[Integration]) -> bool {
    let mut ids = HashSet::with_capacity(integrations.len());
    let mut placeholders = HashSet::new();
    integrations.len() <= MAX_INTEGRATIONS
        && integrations.iter().all(|integration| {
            valid_integration(integration)
                && ids.insert(integration.id.as_str())
                && integration
                    .response_mappings
                    .iter()
                    .all(|mapping| placeholders.insert(mapping.name.to_lowercase()))
        })
}

pub(crate) fn valid_dynamic_options(source: &DynamicOptions) -> bool {
    valid_id(&source.integration)
        && valid_pointer(&source.items_pointer)
        && valid_pointer(&source.label_pointer)
        && valid_pointer(&source.value_pointer)
        && source
            .description_pointer
            .as_deref()
            .is_none_or(valid_pointer)
}

pub(crate) async fn fetch_options_with_timeout(
    ctx: &EventContext,
    integrations: &[Integration],
    guild_id: u64,
    source: &DynamicOptions,
    timeout: Duration,
) -> Result<Vec<FormOption>, OptionsError> {
    if ctx.guild_id != guild_id {
        return Err(OptionsError::InvalidConfiguration);
    }
    let integration = integration_for_options(integrations, guild_id, source)?;
    let key = OptionsCacheKey {
        guild_id,
        integration: integration.clone(),
        source: source.clone(),
    };
    if let Some(options) = cached_options(&key) {
        return Ok(options);
    }
    let options = fetch_options_uncached(ctx, integration, guild_id, source, timeout).await?;
    cache_options(key, options.clone());
    Ok(options)
}

pub(crate) async fn fetch_options_fresh_with_timeout(
    ctx: &EventContext,
    integrations: &[Integration],
    guild_id: u64,
    source: &DynamicOptions,
    timeout: Duration,
) -> Result<Vec<FormOption>, OptionsError> {
    if ctx.guild_id != guild_id {
        return Err(OptionsError::InvalidConfiguration);
    }
    let integration = integration_for_options(integrations, guild_id, source)?;
    let options = fetch_options_uncached(ctx, integration, guild_id, source, timeout).await?;
    cache_options(
        OptionsCacheKey {
            guild_id,
            integration: integration.clone(),
            source: source.clone(),
        },
        options.clone(),
    );
    Ok(options)
}

fn integration_for_options<'a>(
    integrations: &'a [Integration],
    guild_id: u64,
    source: &DynamicOptions,
) -> Result<&'a Integration, OptionsError> {
    if guild_id == 0 || !valid_integrations(integrations) || !valid_dynamic_options(source) {
        return Err(OptionsError::InvalidConfiguration);
    }
    integrations
        .iter()
        .find(|integration| integration.id == source.integration)
        .ok_or(OptionsError::InvalidConfiguration)
}

async fn fetch_options_uncached(
    ctx: &EventContext,
    integration: &Integration,
    guild_id: u64,
    source: &DynamicOptions,
    timeout: Duration,
) -> Result<Vec<FormOption>, OptionsError> {
    let request = RequestContext {
        guild_id,
        user_id: None,
        ticket_id: None,
        ticket_channel_id: None,
        form_data: None,
    };
    let body = fetch_integration_response(ctx, integration, &request, timeout).await?;
    parse_options(&body, source)
}

async fn fetch_integration_response(
    ctx: &EventContext,
    integration: &Integration,
    request: &RequestContext<'_>,
    timeout: Duration,
) -> Result<Vec<u8>, OptionsError> {
    if ctx.guild_id != request.guild_id
        || !valid_generation(&integration.generation)
        || !valid_integration(integration)
    {
        return Err(OptionsError::InvalidConfiguration);
    }
    let token_referenced = integration.auth != AuthKind::None
        || integration
            .headers
            .iter()
            .any(|header| header.value_template.contains("%token%"));
    let token = if token_referenced {
        Some(read_secret(ctx, integration, "token").await?)
    } else {
        None
    };
    let mut named_secrets = HashMap::new();
    for name in &integration.secrets {
        let marker = format!("%{name}%");
        if integration
            .headers
            .iter()
            .any(|header| header.value_template.contains(&marker))
        {
            named_secrets.insert(name.clone(), read_secret(ctx, integration, name).await?);
        }
    }
    let url = map_template(&integration.url, |name| context_value(request, name))
        .map_err(|_| OptionsError::InvalidConfiguration)?;
    let mut headers = Vec::with_capacity(
        integration.headers.len() + usize::from(integration.auth == AuthKind::ApiKey),
    );
    for header in &integration.headers {
        let value = map_template(&header.value_template, |name| {
            context_value(request, name).or_else(|| {
                if name == "token" {
                    token
                        .as_ref()
                        .map(|secret| secret.expose_secret().to_owned())
                } else {
                    named_secrets
                        .get(name)
                        .map(|secret| secret.expose_secret().to_owned())
                }
            })
        })
        .map_err(|_| OptionsError::SecretUnavailable)?;
        headers.push((header.name.clone(), value));
    }
    if integration.auth == AuthKind::ApiKey {
        let secret = token
            .as_ref()
            .ok_or(OptionsError::SecretUnavailable)?
            .expose_secret();
        headers.push(("x-api-key".to_owned(), secret.to_owned()));
    }
    let header_refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let fetcher = plugin_kit::safe_fetch::SafeFetcher::new();
    match (integration.method, integration.auth) {
        (RequestMethod::Get, AuthKind::Bearer) => {
            let secret = token
                .as_ref()
                .ok_or(OptionsError::SecretUnavailable)?
                .expose_secret();
            fetcher
                .get_with_bearer(&url, &header_refs, secret, timeout)
                .await
                .map_err(|_| OptionsError::FetchFailed)
        }
        (RequestMethod::Post, AuthKind::Bearer) => {
            let secret = token
                .as_ref()
                .ok_or(OptionsError::SecretUnavailable)?
                .expose_secret();
            let body = integration_request_body(integration, request)?;
            fetcher
                .post_json_with_bearer(&url, &header_refs, secret, &body, timeout)
                .await
                .map_err(|_| OptionsError::FetchFailed)
        }
        (RequestMethod::Get, _) => fetcher
            .get_with_timeout(&url, &header_refs, timeout)
            .await
            .map_err(|_| OptionsError::FetchFailed),
        (RequestMethod::Post, _) => {
            let body = integration_request_body(integration, request)?;
            fetcher
                .post_json_with_timeout(&url, &header_refs, &body, timeout)
                .await
                .map_err(|_| OptionsError::FetchFailed)
        }
    }
}

async fn read_secret(
    ctx: &EventContext,
    integration: &Integration,
    name: &str,
) -> Result<SecretString, OptionsError> {
    if !valid_generation(&integration.generation) || !valid_secret_name(name) {
        return Err(OptionsError::SecretUnavailable);
    }
    let secret = ctx
        .integration_secret(&integration.id, &integration.generation, name)
        .await
        .map_err(|_| OptionsError::SecretUnavailable)?
        .ok_or(OptionsError::SecretUnavailable)?;
    let value = secret.expose_secret();
    if value.is_empty() || value.chars().count() > 255 || value.chars().any(char::is_control) {
        return Err(OptionsError::SecretUnavailable);
    }
    Ok(secret)
}

fn context_value(request: &RequestContext<'_>, name: &str) -> Option<String> {
    match name {
        "guild_id" => Some(request.guild_id.to_string()),
        "user_id" => request.user_id.map(|id| id.to_string()),
        "ticket_id" => request.ticket_id.map(|id| id.to_string()),
        "ticket_channel_id" => request.ticket_channel_id.map(|id| id.to_string()),
        _ => None,
    }
}

fn integration_request_body(
    integration: &Integration,
    request: &RequestContext<'_>,
) -> Result<Value, OptionsError> {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "guild_id".to_owned(),
        Value::String(request.guild_id.to_string()),
    );
    if let Some(user_id) = request.user_id {
        payload.insert("user_id".to_owned(), Value::String(user_id.to_string()));
    }
    if let Some(ticket_id) = request.ticket_id {
        payload.insert("ticket_id".to_owned(), json!(ticket_id));
        payload.insert("is_new_ticket".to_owned(), Value::Bool(true));
    }
    if let Some(ticket_channel_id) = request.ticket_channel_id {
        payload.insert(
            "ticket_channel_id".to_owned(),
            Value::String(ticket_channel_id.to_string()),
        );
    }
    if integration.send_form_data
        && let Some(answers) = request.form_data
    {
        let mut fields = serde_json::Map::new();
        for (question, answer) in answers {
            if question.trim().is_empty()
                || question.chars().any(char::is_control)
                || answer.contains('\0')
            {
                return Err(OptionsError::InvalidConfiguration);
            }
            fields.insert(question.clone(), Value::String(answer.clone()));
        }
        payload.insert("form_data".to_owned(), Value::Object(fields));
    }
    Ok(Value::Object(payload))
}

pub(crate) async fn resolve_welcome_placeholders(
    ctx: &EventContext,
    integrations: &[Integration],
    welcome: &WelcomeContext<'_>,
) -> HashMap<String, String> {
    let mut values = HashMap::new();
    if !valid_integrations(integrations) {
        return values;
    }
    let request = RequestContext {
        guild_id: welcome.guild_id,
        user_id: Some(welcome.user_id),
        ticket_id: Some(welcome.ticket_id),
        ticket_channel_id: Some(welcome.ticket_channel_id),
        form_data: Some(welcome.form_data),
    };
    for integration in integrations {
        if integration.response_mappings.is_empty() {
            continue;
        }
        for mapping in &integration.response_mappings {
            values.insert(mapping.name.clone(), "N/A".to_owned());
        }
        if ctx.guild_id != welcome.guild_id {
            continue;
        }
        let result =
            fetch_integration_response(ctx, integration, &request, WELCOME_INTEGRATION_TIMEOUT)
                .await
                .and_then(|body| {
                    serde_json::from_slice::<Value>(&body)
                        .map_err(|_| OptionsError::InvalidResponse)
                });
        match result {
            Ok(payload) if payload.is_object() => {
                for mapping in &integration.response_mappings {
                    let mapped = value_at_path(&payload, &mapping.path)
                        .and_then(scalar_text)
                        .and_then(sanitize_mapped_text)
                        .unwrap_or_else(|| "N/A".to_owned());
                    values.insert(mapping.name.clone(), mapped);
                }
            }
            Ok(_) | Err(_) => {
                tracing::warn!(
                    guild = %ctx.guild_id,
                    integration = %integration.id,
                    "ticket welcome integration could not be resolved"
                );
            }
        }
    }
    values
}

pub(crate) async fn dashboard_secret_state(
    ctx: &EventContext,
    integrations: &[Integration],
) -> Result<Value, closure_core::module::DashboardActionError> {
    use closure_core::module::DashboardActionError;

    if !valid_integrations(integrations) {
        return Err(DashboardActionError::Internal(
            "integration configuration is invalid".to_owned(),
        ));
    }
    let mut states = Vec::new();
    for integration in integrations {
        let names = integration_secret_names(integration);
        let mut secrets = Vec::with_capacity(names.len());
        for name in names {
            let configured = if valid_generation(&integration.generation) {
                ctx.integration_secret_configured(&integration.id, &integration.generation, &name)
                    .await
                    .map_err(|_| {
                        DashboardActionError::Internal(
                            "integration secret status is unavailable".to_owned(),
                        )
                    })?
            } else {
                false
            };
            secrets.push(json!({
                "name": name,
                "configured": configured,
            }));
        }
        states.push(json!({
            "integration_id": integration.id,
            "generation": integration.generation,
            "secrets": secrets,
        }));
    }
    Ok(json!({ "integrations": states }))
}

fn integration_secret_names(integration: &Integration) -> Vec<String> {
    let mut names = integration.secrets.clone();
    if integration.auth != AuthKind::None {
        names.push("token".to_owned());
    }
    names.sort();
    names.dedup();
    names
}

pub(crate) fn invalidate_options_cache(guild_id: u64, integration_id: &str) {
    let Ok(mut cache) = OPTIONS_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    else {
        return;
    };
    cache.retain(|key, _| key.guild_id != guild_id || key.integration.id != integration_id);
}

pub(crate) fn apply_welcome_placeholders(
    message: &str,
    placeholders: &HashMap<String, String>,
) -> String {
    let mut output = String::with_capacity(message.len());
    let mut index = 0;
    while index < message.len() {
        if message.as_bytes()[index] != b'%' {
            let Some(character) = message[index..].chars().next() else {
                break;
            };
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        let Some(close_offset) = message[index + 1..].find('%') else {
            output.push_str(&message[index..]);
            break;
        };
        let close = index + 1 + close_offset;
        let name = &message[index + 1..close];
        if let Some(value) = placeholders.get(name) {
            output.push_str(value);
        } else {
            output.push_str(&message[index..=close]);
        }
        index = close + 1;
    }
    output
}

fn value_at_path<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    let mut value = payload;
    for part in path.split('.') {
        value = match value {
            Value::Object(object) => object.get(part)?,
            Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(value)
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn sanitize_mapped_text(value: String) -> Option<String> {
    if value.chars().count() > MAX_PLACEHOLDER_VALUE_CHARS || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.replace('@', "@\u{200b}"))
}

fn cached_options(key: &OptionsCacheKey) -> Option<Vec<FormOption>> {
    let now = Instant::now();
    let mut cache = OPTIONS_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()?;
    cache.retain(|_, entry| now.saturating_duration_since(entry.stored_at) < OPTIONS_CACHE_TTL);
    cache.get(key).map(|entry| entry.options.clone())
}

fn cache_options(key: OptionsCacheKey, options: Vec<FormOption>) {
    let now = Instant::now();
    let Ok(mut cache) = OPTIONS_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    else {
        return;
    };
    cache.retain(|_, entry| now.saturating_duration_since(entry.stored_at) < OPTIONS_CACHE_TTL);
    if !cache.contains_key(&key) && cache.len() >= OPTIONS_CACHE_LIMIT {
        let oldest = cache
            .iter()
            .min_by_key(|(_, entry)| entry.stored_at)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            cache.remove(&oldest);
        }
    }
    cache.insert(
        key,
        OptionsCacheEntry {
            stored_at: now,
            options,
        },
    );
}

fn valid_integration(integration: &Integration) -> bool {
    valid_id(&integration.id)
        && (integration.generation.is_empty() || valid_generation(&integration.generation))
        && valid_endpoint(&integration.url)
        && valid_url_template(&integration.url)
        && integration.headers.len() <= MAX_INTEGRATION_HEADERS
        && integration.headers.iter().all(|header| {
            valid_header_name(&header.name) && valid_template(&header.value_template, integration)
        })
        && integration
            .headers
            .iter()
            .map(|header| header.name.to_ascii_lowercase())
            .collect::<HashSet<_>>()
            .len()
            == integration.headers.len()
        && integration.secrets.len() <= MAX_INTEGRATION_SECRETS
        && integration
            .secrets
            .iter()
            .all(|name| valid_secret_name(name))
        && integration.secrets.iter().collect::<HashSet<_>>().len() == integration.secrets.len()
        && !integration.secrets.iter().any(|name| {
            matches!(
                name.as_str(),
                "token" | "guild_id" | "user_id" | "ticket_id" | "ticket_channel_id"
            )
        })
        && integration.response_mappings.len() <= MAX_RESPONSE_MAPPINGS
        && integration
            .response_mappings
            .iter()
            .all(|mapping| valid_mapping_name(&mapping.name) && valid_mapping_path(&mapping.path))
        && integration
            .response_mappings
            .iter()
            .map(|mapping| mapping.name.to_lowercase())
            .collect::<HashSet<_>>()
            .len()
            == integration.response_mappings.len()
        && (!integration.send_form_data || integration.method == RequestMethod::Post)
        && !(integration.auth == AuthKind::ApiKey
            && integration
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("x-api-key")))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_INTEGRATION_ID_CHARS
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn valid_endpoint(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    if url.len() > MAX_ENDPOINT_CHARS
        || url.chars().any(char::is_whitespace)
        || url.chars().any(char::is_control)
        || url.contains('#')
    {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty() && !authority.contains('@')
}

fn valid_generation(generation: &str) -> bool {
    generation.len() == 32
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SECRET_NAME_CHARS
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_header_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= MAX_HEADER_NAME_CHARS
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !matches!(
            lower.as_str(),
            "authorization"
                | "cookie"
                | "host"
                | "user-agent"
                | "content-type"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
        && !lower.starts_with("x-forwarded-")
        && !lower.starts_with("x-proxy-")
        && !lower.starts_with("cf-")
}

fn valid_mapping_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MAPPING_NAME_CHARS
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_mapping_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_MAPPING_PATH_CHARS
        && !path.chars().any(char::is_control)
        && !path.starts_with('.')
        && !path.ends_with('.')
        && path.split('.').all(|part| !part.is_empty())
}

fn valid_url_template(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.contains('%') && valid_template_tokens(url, |token| is_context_token(token))
}

fn valid_template(template: &str, integration: &Integration) -> bool {
    template.len() <= MAX_HEADER_TEMPLATE_CHARS
        && !template.chars().any(char::is_control)
        && valid_template_tokens(template, |token| {
            is_context_token(token)
                || (token == "token" && integration.auth != AuthKind::None)
                || integration.secrets.iter().any(|secret| secret == token)
        })
}

fn valid_template_tokens(template: &str, allowed: impl Fn(&str) -> bool) -> bool {
    map_template(template, |token| allowed(token).then(String::new)).is_ok()
}

fn map_template(
    template: &str,
    mut resolve: impl FnMut(&str) -> Option<String>,
) -> Result<String, OptionsError> {
    let mut output = String::with_capacity(template.len());
    let mut index = 0;
    let bytes = template.as_bytes();
    while index < bytes.len() {
        if bytes[index] != b'%' {
            let character = template[index..]
                .chars()
                .next()
                .ok_or(OptionsError::InvalidConfiguration)?;
            output.push(character);
            index += character.len_utf8();
            continue;
        }
        if index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
            && (index + 3 == bytes.len() || bytes[index + 3] != b'%')
        {
            output.push('%');
            output.push(bytes[index + 1] as char);
            output.push(bytes[index + 2] as char);
            index += 3;
            continue;
        }
        let Some(close_offset) = template[index + 1..].find('%') else {
            return Err(OptionsError::InvalidConfiguration);
        };
        let close = index + 1 + close_offset;
        let token = &template[index + 1..close];
        if token.is_empty() {
            return Err(OptionsError::InvalidConfiguration);
        }
        output.push_str(&resolve(token).ok_or(OptionsError::InvalidConfiguration)?);
        index = close + 1;
    }
    Ok(output)
}

fn is_context_token(token: &str) -> bool {
    matches!(
        token,
        "guild_id" | "user_id" | "ticket_id" | "ticket_channel_id"
    )
}

fn valid_pointer(pointer: &str) -> bool {
    if pointer.len() > MAX_JSON_POINTER_CHARS
        || pointer.chars().any(char::is_control)
        || (!pointer.is_empty() && !pointer.starts_with('/'))
    {
        return false;
    }
    let mut escaped = false;
    for byte in pointer.bytes() {
        if escaped {
            if byte != b'0' && byte != b'1' {
                return false;
            }
            escaped = false;
        } else if byte == b'~' {
            escaped = true;
        }
    }
    !escaped
}

fn parse_options(body: &[u8], source: &DynamicOptions) -> Result<Vec<FormOption>, OptionsError> {
    let payload: Value = serde_json::from_slice(body).map_err(|_| OptionsError::InvalidResponse)?;
    let items = payload
        .pointer(&source.items_pointer)
        .and_then(Value::as_array)
        .ok_or(OptionsError::InvalidResponse)?;
    if items.len() > MAX_DYNAMIC_OPTIONS {
        return Err(OptionsError::TooManyOptions);
    }
    if items.is_empty() {
        return Err(OptionsError::InvalidResponse);
    }

    let mut seen = HashSet::with_capacity(items.len());
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        let label = option_text(item, &source.label_pointer)?;
        let value = option_text(item, &source.value_pointer)?;
        if !valid_option_text(&label) || !valid_option_text(&value) || !seen.insert(value.clone()) {
            return Err(OptionsError::InvalidResponse);
        }
        let description = match source.description_pointer.as_deref() {
            None => None,
            Some(pointer) => item
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned),
        };
        if description
            .as_deref()
            .is_some_and(|text| !valid_option_text(text))
        {
            return Err(OptionsError::InvalidResponse);
        }
        options.push(FormOption {
            label,
            value,
            description,
        });
    }
    Ok(options)
}

fn option_text(item: &Value, pointer: &str) -> Result<String, OptionsError> {
    match item.pointer(pointer) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Number(number)) => Ok(number.to_string()),
        _ => Err(OptionsError::InvalidResponse),
    }
}

fn valid_option_text(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().count() <= MAX_OPTION_CHARS
        && !text.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::{
        AuthKind, DynamicOptions, Integration, OptionsCacheKey, OptionsError, parse_options,
    };

    #[test]
    fn parses_json_pointer_options_and_rejects_more_than_twenty_five() {
        let source = DynamicOptions {
            integration: "catalog".to_owned(),
            items_pointer: "/items".to_owned(),
            label_pointer: "/name".to_owned(),
            value_pointer: "/id".to_owned(),
            description_pointer: None,
        };
        let options = parse_options(
            br#"{"items":[{"name":"Alpha","id":1},{"name":"Beta","id":2}]}"#,
            &source,
        );
        assert!(matches!(options, Ok(options) if options.len() == 2 && options[0].value == "1"));

        let too_many = format!(
            r#"{{"items":[{}]}}"#,
            (0..26)
                .map(|index| format!(r#"{{"name":"item-{index}","id":"{index}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(matches!(
            parse_options(too_many.as_bytes(), &source),
            Err(OptionsError::TooManyOptions)
        ));
    }

    #[test]
    fn options_cache_key_includes_guild_and_relevant_settings() {
        let integration = Integration {
            id: "catalog".to_owned(),
            generation: String::new(),
            url: "https://api.example.com/options".to_owned(),
            auth: AuthKind::None,
            method: super::RequestMethod::default(),
            headers: Vec::new(),
            secrets: Vec::new(),
            response_mappings: Vec::new(),
            send_form_data: false,
        };
        let source = DynamicOptions {
            integration: "catalog".to_owned(),
            items_pointer: "/items".to_owned(),
            label_pointer: "/name".to_owned(),
            value_pointer: "/id".to_owned(),
            description_pointer: None,
        };
        let first = OptionsCacheKey {
            guild_id: 1,
            integration: integration.clone(),
            source: source.clone(),
        };
        assert_eq!(
            first,
            OptionsCacheKey {
                guild_id: 1,
                integration: integration.clone(),
                source: source.clone(),
            }
        );
        assert_ne!(
            first,
            OptionsCacheKey {
                guild_id: 2,
                integration: integration.clone(),
                source: source.clone(),
            }
        );
        assert_ne!(
            first,
            OptionsCacheKey {
                guild_id: 1,
                integration: Integration {
                    url: "https://api.example.com/other".to_owned(),
                    ..integration
                },
                source,
            }
        );
    }
}
