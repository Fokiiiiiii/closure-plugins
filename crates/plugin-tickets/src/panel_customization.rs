use std::collections::HashSet;

use closure_sdk::{Color, EmbedBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use twilight_model::channel::message::Embed;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChannelNameScheme {
    Id,
    Username,
    IdUsername,
    Nickname,
    #[default]
    IdPadded,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccessAction {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccessRule {
    pub(crate) role_id: String,
    pub(crate) action: AccessAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WelcomeEmbedField {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PanelCustomization {
    pub(crate) title: Option<String>,
    pub(crate) color: Option<u32>,
    pub(crate) content: Option<String>,
    pub(crate) welcome_title: Option<String>,
    pub(crate) welcome_title_url: Option<String>,
    pub(crate) welcome_color: Option<u32>,
    pub(crate) welcome_description: Option<String>,
    pub(crate) welcome_author_name: Option<String>,
    pub(crate) welcome_author_url: Option<String>,
    pub(crate) welcome_author_icon_url: Option<String>,
    pub(crate) welcome_thumbnail_url: Option<String>,
    pub(crate) welcome_image_url: Option<String>,
    pub(crate) welcome_footer_text: Option<String>,
    pub(crate) welcome_footer_icon_url: Option<String>,
    pub(crate) welcome_timestamp: bool,
    pub(crate) welcome_fields: Vec<WelcomeEmbedField>,
    pub(crate) close_reasons: Vec<String>,
    pub(crate) allow_custom_close_reasons: bool,
    pub(crate) rating_enabled: bool,
    pub(crate) channel_name_scheme: ChannelNameScheme,
    pub(crate) channel_name_custom: Option<String>,
    pub(crate) open_cooldown_seconds: u32,
    pub(crate) access_rules: Vec<AccessRule>,
}

impl Default for PanelCustomization {
    fn default() -> Self {
        Self {
            title: None,
            color: None,
            content: None,
            welcome_title: None,
            welcome_title_url: None,
            welcome_color: None,
            welcome_description: None,
            welcome_author_name: None,
            welcome_author_url: None,
            welcome_author_icon_url: None,
            welcome_thumbnail_url: None,
            welcome_image_url: None,
            welcome_footer_text: None,
            welcome_footer_icon_url: None,
            welcome_timestamp: false,
            welcome_fields: Vec::new(),
            close_reasons: Vec::new(),
            allow_custom_close_reasons: true,
            rating_enabled: true,
            channel_name_scheme: ChannelNameScheme::IdPadded,
            channel_name_custom: None,
            open_cooldown_seconds: 0,
            access_rules: Vec::new(),
        }
    }
}

impl PanelCustomization {
    pub(crate) fn valid(&self) -> bool {
        fn text(value: Option<&str>, max: usize, allow_empty: bool) -> bool {
            value.is_none_or(|value| {
                value.chars().count() <= max
                    && !value.contains('\0')
                    && (allow_empty || !value.trim().is_empty())
            })
        }

        fn url(value: Option<&str>) -> bool {
            value.is_none_or(|value| {
                let Some((scheme, address)) = value.split_once("://") else {
                    return false;
                };
                let host = address.split(['/', '?', '#']).next().unwrap_or_default();
                value.len() <= 2048
                    && !value
                        .chars()
                        .any(|character| character.is_control() || character.is_whitespace())
                    && matches!(scheme, "https" | "http")
                    && !host.is_empty()
            })
        }

        fn valid_name_pattern(value: &str) -> bool {
            let mut remainder = value.to_owned();
            for token in ["%id_padded%", "%username%", "%nickname%", "%id%"] {
                remainder = remainder.replace(token, "");
            }
            !remainder.contains('%')
        }

        if !text(self.title.as_deref(), 256, false)
            || !text(self.content.as_deref(), 1000, true)
            || self.color.is_some_and(|color| color > 0xFF_FFFF)
            || !text(self.welcome_title.as_deref(), 256, false)
            || !url(self.welcome_title_url.as_deref())
            || self.welcome_color.is_some_and(|color| color > 0xFF_FFFF)
            || !text(self.welcome_description.as_deref(), 1000, true)
            || !text(self.welcome_author_name.as_deref(), 256, false)
            || !url(self.welcome_author_url.as_deref())
            || !url(self.welcome_author_icon_url.as_deref())
            || !url(self.welcome_thumbnail_url.as_deref())
            || !url(self.welcome_image_url.as_deref())
            || !text(self.welcome_footer_text.as_deref(), 2048, false)
            || !url(self.welcome_footer_icon_url.as_deref())
            || self.welcome_fields.len() > 25
            || self.close_reasons.len() > 25
            || self.close_reasons.iter().any(|reason| {
                reason.trim().is_empty() || reason.chars().count() > 100 || reason.contains('\0')
            })
        {
            return false;
        }

        if self.welcome_title_url.is_some()
            && self.welcome_title.as_deref().is_none_or(str::is_empty)
        {
            return false;
        }
        if (self.welcome_author_url.is_some() || self.welcome_author_icon_url.is_some())
            && self
                .welcome_author_name
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return false;
        }
        if self.welcome_footer_icon_url.is_some()
            && self
                .welcome_footer_text
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return false;
        }
        if self.channel_name_custom.as_ref().is_some_and(|name| {
            name.chars().count() > 100
                || name.contains('\0')
                || (self.channel_name_scheme == ChannelNameScheme::Custom
                    && !name.trim().is_empty()
                    && !valid_name_pattern(name))
        }) {
            return false;
        }
        if self.welcome_fields.iter().any(|field| {
            field.name.trim().is_empty()
                || field.value.trim().is_empty()
                || field.name.chars().count() > 256
                || field.value.chars().count() > 1024
                || field.name.contains('\0')
                || field.value.contains('\0')
        }) {
            return false;
        }

        let mut role_ids = HashSet::new();
        if self.access_rules.iter().any(|rule| {
            let valid_snowflake = !rule.role_id.is_empty()
                && rule.role_id.len() <= 20
                && rule.role_id.bytes().all(|byte| byte.is_ascii_digit());
            !valid_snowflake
                || rule
                    .role_id
                    .parse::<u64>()
                    .ok()
                    .filter(|role| *role > 0)
                    .is_none_or(|role| !role_ids.insert(role))
        }) {
            return false;
        }

        let has_welcome_content = self
            .welcome_title
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
            || self
                .welcome_description
                .as_ref()
                .is_some_and(|text| !text.trim().is_empty())
            || !self.welcome_fields.is_empty();
        let has_welcome_style = self.welcome_title_url.is_some()
            || self.welcome_color.is_some()
            || self.welcome_author_name.is_some()
            || self.welcome_author_url.is_some()
            || self.welcome_author_icon_url.is_some()
            || self.welcome_thumbnail_url.is_some()
            || self.welcome_image_url.is_some()
            || self.welcome_footer_text.is_some()
            || self.welcome_footer_icon_url.is_some()
            || self.welcome_timestamp;
        if has_welcome_style && !has_welcome_content {
            return false;
        }

        let has_panel_content = self
            .title
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
            || self
                .content
                .as_ref()
                .is_some_and(|text| !text.trim().is_empty());
        (self.color.is_none() || has_panel_content)
            && (!self.welcome_timestamp || has_welcome_content)
            && self.panel_embed().is_ok()
            && self.welcome_embed(0).is_ok()
    }

    pub(crate) fn cooldown_remaining(&self, last_opened: Option<i64>, now: i64) -> u32 {
        let Some(last_opened) = last_opened else {
            return 0;
        };
        let cooldown = i64::from(self.open_cooldown_seconds);
        let remaining = last_opened.saturating_add(cooldown).saturating_sub(now);
        u32::try_from(remaining.max(0)).unwrap_or(u32::MAX)
    }

    pub(crate) fn allows(&self, roles: &[u64], everyone: u64) -> bool {
        self.access_rules
            .iter()
            .find_map(|rule| {
                let role = rule.role_id.parse::<u64>().ok()?;
                let matches = role == everyone || roles.contains(&role);
                matches.then_some(rule.action == AccessAction::Allow)
            })
            .unwrap_or(true)
    }

    pub(crate) fn close_reason_allowed(&self, reason: Option<&str>) -> bool {
        let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) else {
            return true;
        };
        self.allow_custom_close_reasons
            || self
                .close_reasons
                .iter()
                .any(|allowed| allowed.trim().eq_ignore_ascii_case(reason))
    }

    pub(crate) fn channel_name(
        &self,
        number: i64,
        username: &str,
        nickname: Option<&str>,
        reservation_token: Option<i64>,
    ) -> String {
        let padded = format!("{number:04}");
        let safe_username = sanitize_channel_part(username);
        let safe_nickname = sanitize_channel_part(nickname.unwrap_or(username));
        let template = match self.channel_name_scheme {
            ChannelNameScheme::Id => format!("ticket-{number}"),
            ChannelNameScheme::Username => format!("ticket-{safe_username}"),
            ChannelNameScheme::IdUsername => format!("ticket-{number}-{safe_username}"),
            ChannelNameScheme::Nickname => format!("ticket-{safe_nickname}"),
            ChannelNameScheme::IdPadded => format!("ticket-{padded}"),
            ChannelNameScheme::Custom => self
                .channel_name_custom
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("ticket-%id_padded%")
                .replace("%id_padded%", &padded)
                .replace("%username%", &safe_username)
                .replace("%nickname%", &safe_nickname)
                .replace("%id%", &number.to_string()),
        };
        let suffix = reservation_token.map(|token| format!("-r{token}"));
        sanitize_channel_name(&template, suffix.as_deref())
    }

    pub(crate) fn panel_embed(&self) -> Result<Option<Embed>, serde_json::Error> {
        let title = self
            .title
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let description = self
            .content
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        if title.is_none() && description.is_none() {
            return Ok(None);
        }
        let mut builder = EmbedBuilder::new();
        if let Some(title) = title {
            builder = builder.title(title);
        }
        if let Some(description) = description {
            builder = builder.description(description);
        }
        if let Some(color) = self.color {
            builder = builder.color(Color::Rgb(color));
        }
        builder
            .build()
            .map(Some)
            .map_err(|error| serde_json::Error::io(std::io::Error::other(error.to_string())))
    }

    pub(crate) fn welcome_embed(&self, now: i64) -> Result<Option<Embed>, serde_json::Error> {
        let title = self
            .welcome_title
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        let description = self
            .welcome_description
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        if title.is_none() && description.is_none() && self.welcome_fields.is_empty() {
            return Ok(None);
        }

        let mut builder = EmbedBuilder::new();
        if let Some(title) = title {
            builder = builder.title(title);
        }
        if let Some(url) = self.welcome_title_url.as_deref() {
            builder = builder.url(url);
        }
        if let Some(color) = self.welcome_color {
            builder = builder.color(Color::Rgb(color));
        }
        if let Some(description) = description {
            builder = builder.description(description);
        }
        if let Some(author) = self.welcome_author_name.as_deref() {
            builder = builder.author(author);
        }
        if let Some(footer) = self.welcome_footer_text.as_deref() {
            builder = builder.footer(footer);
        }
        if self.welcome_timestamp {
            builder = builder.timestamp(now);
        }
        for field in &self.welcome_fields {
            builder = builder.field(field.name.clone(), field.value.clone(), field.inline);
        }
        let embed = builder
            .build()
            .map_err(|error| serde_json::Error::io(std::io::Error::other(error.to_string())))?;
        let mut value = serde_json::to_value(embed)?;
        if let Some(author) = value.get_mut("author").and_then(Value::as_object_mut) {
            if let Some(url) = self.welcome_author_url.as_ref() {
                author.insert("url".to_owned(), json!(url));
            }
            if let Some(icon) = self.welcome_author_icon_url.as_ref() {
                author.insert("icon_url".to_owned(), json!(icon));
            }
        }
        if let Some(footer) = value.get_mut("footer").and_then(Value::as_object_mut)
            && let Some(icon) = self.welcome_footer_icon_url.as_ref()
        {
            footer.insert("icon_url".to_owned(), json!(icon));
        }
        if let Some(icon) = self.welcome_thumbnail_url.as_ref() {
            value["thumbnail"] = json!({ "url": icon });
        }
        if let Some(image) = self.welcome_image_url.as_ref() {
            value["image"] = json!({ "url": image });
        }
        serde_json::from_value(value).map(Some)
    }

    pub(crate) fn schema_properties() -> Value {
        json!({
            "title": {
                "type": ["string", "null"],
                "maxLength": 256,
                "description": "tickets-panel-title",
                "x-ui": { "level": "basic" }
            },
            "color": {
                "type": ["integer", "null"],
                "minimum": 0,
                "maximum": 16777215,
                "description": "tickets-panel-color",
                "x-ui": { "level": "basic" }
            },
            "content": {
                "type": ["string", "null"],
                "maxLength": 1000,
                "description": "tickets-panel-content",
                "x-ui": { "level": "basic" }
            },
            "welcome_title": {
                "type": ["string", "null"],
                "maxLength": 256,
                "description": "tickets-welcome-title",
                "x-ui": { "level": "basic" }
            },
            "welcome_title_url": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-title-url",
                "x-ui": { "level": "basic" }
            },
            "welcome_color": {
                "type": ["integer", "null"],
                "minimum": 0,
                "maximum": 16777215,
                "description": "tickets-welcome-color",
                "x-ui": { "level": "basic" }
            },
            "welcome_description": {
                "type": ["string", "null"],
                "maxLength": 1000,
                "description": "tickets-welcome-description",
                "x-ui": { "level": "basic" }
            },
            "welcome_author_name": {
                "type": ["string", "null"],
                "maxLength": 256,
                "description": "tickets-welcome-author-name",
                "x-ui": { "level": "basic" }
            },
            "welcome_author_url": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-author-url",
                "x-ui": { "level": "basic" }
            },
            "welcome_author_icon_url": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-author-icon-url",
                "x-ui": { "level": "basic" }
            },
            "welcome_thumbnail_url": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-thumbnail-url",
                "x-ui": { "level": "basic" }
            },
            "welcome_image_url": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-image-url",
                "x-ui": { "level": "basic" }
            },
            "welcome_footer_text": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-footer-text",
                "x-ui": { "level": "basic" }
            },
            "welcome_footer_icon_url": {
                "type": ["string", "null"],
                "maxLength": 2048,
                "description": "tickets-welcome-footer-icon-url",
                "x-ui": { "level": "basic" }
            },
            "welcome_timestamp": {
                "type": "boolean",
                "default": false,
                "description": "tickets-welcome-timestamp",
                "x-ui": { "level": "basic" }
            },
            "welcome_fields": {
                "type": "array",
                "maxItems": 25,
                "description": "tickets-welcome-fields",
                "x-ui": { "level": "basic", "widget": "embed-fields" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "value"],
                    "properties": {
                        "name": { "type": "string", "minLength": 1, "maxLength": 256 },
                        "value": { "type": "string", "minLength": 1, "maxLength": 1024 },
                        "inline": { "type": "boolean", "default": false }
                    }
                }
            },
            "close_reasons": {
                "type": "array",
                "maxItems": 25,
                "description": "tickets-close-reasons",
                "x-ui": { "level": "basic", "widget": "text-list" },
                "items": { "type": "string", "minLength": 1, "maxLength": 100 }
            },
            "allow_custom_close_reasons": {
                "type": "boolean",
                "default": true,
                "description": "tickets-allow-custom-close-reasons",
                "x-ui": { "level": "basic" }
            },
            "rating_enabled": {
                "type": "boolean",
                "default": true,
                "description": "tickets-rating-enabled",
                "x-ui": { "level": "basic" }
            },
            "channel_name_scheme": {
                "type": "string",
                "enum": ["id", "username", "id_username", "nickname", "id_padded", "custom"],
                "default": "id_padded",
                "description": "tickets-channel-name-scheme",
                "x-ui": { "level": "basic" }
            },
            "channel_name_custom": {
                "type": ["string", "null"],
                "maxLength": 100,
                "default": "ticket-%id_padded%",
                "description": "tickets-channel-name-custom",
                "x-ui": { "level": "basic" }
            },
            "open_cooldown_seconds": {
                "type": "integer",
                "minimum": 0,
                "maximum": 4294967295,
                "default": 0,
                "description": "tickets-open-cooldown-seconds",
                "x-ui": { "level": "basic" }
            },
            "access_rules": {
                "type": "array",
                "description": "tickets-access-rules",
                "x-ui": { "level": "basic", "widget": "access-rules" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["role_id", "action"],
                    "properties": {
                        "role_id": {
                            "type": "string",
                            "pattern": "^[0-9]{1,20}$",
                            "description": "tickets-access-role",
                            "x-ui": { "widget": "role" }
                        },
                        "action": {
                            "type": "string",
                            "enum": ["allow", "deny"],
                            "description": "tickets-access-action"
                        }
                    }
                }
            }
        })
    }
}

pub(crate) fn open_cooldown_key(user_id: u64, panel_id: &str) -> String {
    format!("ticket-open-cooldown:{user_id}:{panel_id}")
}

fn sanitize_channel_part(value: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-');
            }
            separator = false;
            result.push(character);
        } else if character == '-' || character == '_' || character.is_whitespace() {
            separator = true;
        }
    }
    result.trim_matches(['-', '_']).to_owned()
}

fn sanitize_channel_name(value: &str, suffix: Option<&str>) -> String {
    let safe = sanitize_channel_part(value);
    let fallback = "ticket";
    let base = if safe.is_empty() { fallback } else { &safe };
    let suffix = suffix.unwrap_or_default();
    let keep = 100_usize.saturating_sub(suffix.encode_utf16().count());
    let mut used = 0_usize;
    let mut shortened = String::new();
    for character in base.chars() {
        let units = character.len_utf16();
        if used + units > keep {
            break;
        }
        shortened.push(character);
        used += units;
    }
    shortened = shortened.trim_matches(['-', '_']).to_owned();
    if shortened.is_empty() {
        shortened.push_str(fallback);
    }
    format!("{shortened}{suffix}")
}
