use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use crate::integrations::{self, DynamicOptions, Integration};

use closure_core::action::{Action, MentionPolicy, ModalComponent, ModalOption};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, ModalValue};
use closure_core::module::EventContext;
use closure_core::storage::{MAX_VALUE_BYTES, StorageError};
use closure_sdk::ui;
use closure_sdk::{Args, Cx, EmbedBuilder, InteractionRef, UserError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::channel::message::component::{
    SelectMenu, SelectMenuOption, SelectMenuType, TextDisplay, TextInput,
    TextInputStyle as DiscordTextInputStyle,
};
use twilight_model::channel::message::{Component, Embed};

pub(crate) const MAX_FORM_QUESTIONS: usize = 5;
pub(crate) const MAX_FORM_QUESTION_CHARS: usize = 45;
pub(crate) const MAX_FORM_ANSWER_CHARS: u16 = 4000;
const MAX_EMBED_FIELD_CHARS: usize = 1024;
const MAX_EMBED_FIELD_NAME_CHARS: usize = 256;
const MAX_EMBED_FIELDS: usize = 25;
const MAX_EMBED_TOTAL_CHARS: usize = 6000;
const MAX_MESSAGE_EMBEDS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub(crate) enum FormQuestion {
    Legacy(String),
    Structured(FormQuestionKind),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FormQuestionKind {
    Text {
        label: String,
        #[serde(default)]
        style: FormTextStyle,
        #[serde(default = "default_form_required")]
        required: bool,
        #[serde(default)]
        min_length: Option<u16>,
        #[serde(default)]
        max_length: Option<u16>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        description: Option<String>,
    },
    StringSelect {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        #[serde(default)]
        options: Vec<FormOption>,
        #[serde(default)]
        dynamic_options: Option<DynamicOptions>,
        #[serde(default)]
        min_values: Option<u8>,
        #[serde(default)]
        max_values: Option<u8>,
    },
    UserSelect {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        #[serde(default)]
        min_values: Option<u8>,
        #[serde(default)]
        max_values: Option<u8>,
    },
    RoleSelect {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        #[serde(default)]
        min_values: Option<u8>,
        #[serde(default)]
        max_values: Option<u8>,
    },
    MentionableSelect {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        #[serde(default)]
        min_values: Option<u8>,
        #[serde(default)]
        max_values: Option<u8>,
    },
    ChannelSelect {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        #[serde(default)]
        min_values: Option<u8>,
        #[serde(default)]
        max_values: Option<u8>,
    },
    RadioGroup {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        options: Vec<FormOption>,
    },
    CheckboxGroup {
        label: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default = "default_form_required")]
        required: bool,
        options: Vec<FormOption>,
        #[serde(default)]
        min_values: Option<u8>,
        #[serde(default)]
        max_values: Option<u8>,
    },
    Display {
        content: String,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FormTextStyle {
    Short,
    #[default]
    Paragraph,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub(crate) struct FormOption {
    pub(crate) label: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
}

fn default_form_required() -> bool {
    true
}

#[allow(clippy::too_many_lines)]
pub(crate) fn form_questions_schema() -> serde_json::Value {
    json!({
        "type": "array",
        "maxItems": MAX_FORM_QUESTIONS,
        "description": "config-tickets-questions",
        "x-ui": { "level": "basic" },
        "items": {
            "oneOf": [
                {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_FORM_QUESTION_CHARS,
                    "description": "config-tickets-question"
                },
                {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type"],
                    "properties": {
                        "type": {
                            "type": "string",
                            "enum": ["text", "string_select", "user_select", "role_select", "mentionable_select", "channel_select", "radio_group", "checkbox_group", "display"],
                            "default": "text",
                            "description": "config-tickets-question-type"
                        },
                        "label": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": MAX_FORM_QUESTION_CHARS,
                            "description": "config-tickets-question-label"
                        },
                        "content": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 4000,
                            "description": "config-tickets-question-content"
                        },
                        "style": {
                            "type": "string",
                            "enum": ["short", "paragraph"],
                            "default": "paragraph",
                            "description": "config-tickets-question-style"
                        },
                        "required": {
                            "type": "boolean",
                            "default": true,
                            "description": "config-tickets-question-required"
                        },
                        "min_length": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": MAX_FORM_ANSWER_CHARS,
                            "description": "config-tickets-question-min-length"
                        },
                        "max_length": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_FORM_ANSWER_CHARS,
                            "default": MAX_FORM_ANSWER_CHARS,
                            "description": "config-tickets-question-max-length"
                        },
                        "placeholder": {
                            "type": "string",
                            "maxLength": 100,
                            "description": "config-tickets-question-placeholder"
                        },
                        "description": {
                            "type": "string",
                            "maxLength": 100,
                            "description": "config-tickets-question-description"
                        },
                        "dynamic_options": integrations::dynamic_options_schema(),
                        "options": {
                            "type": "array",
                            "maxItems": 25,
                            "description": "config-tickets-question-options",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["label", "value"],
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 100,
                                        "description": "config-tickets-option-label"
                                    },
                                    "value": {
                                        "type": "string",
                                        "minLength": 1,
                                        "maxLength": 100,
                                        "description": "config-tickets-option-value"
                                    },
                                    "description": {
                                        "type": "string",
                                        "maxLength": 100,
                                        "description": "config-tickets-option-description"
                                    }
                                }
                            }
                        },
                        "min_values": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 25,
                            "description": "config-tickets-question-min-values"
                        },
                        "max_values": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 25,
                            "description": "config-tickets-question-max-values"
                        }
                    }
                }
            ]
        }
    })
}

pub(crate) fn valid_questions(questions: &[FormQuestion]) -> bool {
    questions.len() <= MAX_FORM_QUESTIONS && questions.iter().all(FormQuestion::is_valid)
}

pub(crate) async fn resolve_form_questions(
    ctx: &EventContext,
    questions: &[FormQuestion],
    integrations: &[Integration],
    guild_id: u64,
) -> Result<Vec<FormQuestion>, UserError> {
    resolve_form_questions_with_timeout(
        ctx,
        questions,
        integrations,
        guild_id,
        Duration::from_secs(2),
        false,
    )
    .await
}

pub(crate) async fn resolve_form_questions_fresh(
    ctx: &EventContext,
    questions: &[FormQuestion],
    integrations: &[Integration],
    guild_id: u64,
) -> Result<Vec<FormQuestion>, UserError> {
    resolve_form_questions_with_timeout(
        ctx,
        questions,
        integrations,
        guild_id,
        Duration::from_secs(5),
        true,
    )
    .await
}

async fn resolve_form_questions_with_timeout(
    ctx: &EventContext,
    questions: &[FormQuestion],
    integrations: &[Integration],
    guild_id: u64,
    timeout: Duration,
    fresh: bool,
) -> Result<Vec<FormQuestion>, UserError> {
    if !valid_questions(questions) {
        return Err(UserError::message("tickets-form-invalid"));
    }
    let mut resolved = questions.to_vec();
    let mut pending = Vec::new();
    for (index, question) in resolved.iter().enumerate() {
        if let FormQuestion::Structured(FormQuestionKind::StringSelect {
            options,
            dynamic_options: Some(source),
            ..
        }) = question
        {
            if !options.is_empty() {
                return Err(UserError::message("tickets-form-invalid"));
            }
            pending.push((index, source.clone()));
        }
    }
    // One deadline for all questions: the wait is the slowest fetch, not the sum of them.
    let deadline = tokio::time::Instant::now() + timeout;
    let requests: Vec<_> = pending
        .iter()
        .map(|(_, source)| {
            fetch_dynamic_options(ctx, integrations, guild_id, source, timeout, fresh)
        })
        .collect();
    let fetched = join_before_deadline(requests, deadline).await;
    for ((index, _), result) in pending.into_iter().zip(fetched) {
        let options = match result {
            Some(result) => result?,
            None => return Err(UserError::message("tickets-form-options-unavailable")),
        };
        if let FormQuestion::Structured(FormQuestionKind::StringSelect { options: slot, .. }) =
            &mut resolved[index]
        {
            *slot = options;
        }
    }
    if !valid_questions(&resolved) {
        return Err(UserError::message("tickets-form-invalid"));
    }
    Ok(resolved)
}

// An empty fetch cannot satisfy a required select, so it is reported as unavailable.
fn checked_fetched_options(options: Vec<FormOption>) -> Result<Vec<FormOption>, UserError> {
    if options.is_empty() {
        return Err(UserError::message("tickets-form-options-unavailable"));
    }
    Ok(options)
}

async fn fetch_dynamic_options(
    ctx: &EventContext,
    integrations: &[Integration],
    guild_id: u64,
    source: &DynamicOptions,
    timeout: Duration,
    fresh: bool,
) -> Result<Vec<FormOption>, UserError> {
    let fetched = if fresh {
        integrations::fetch_options_fresh_with_timeout(ctx, integrations, guild_id, source, timeout)
            .await
    } else {
        integrations::fetch_options_with_timeout(ctx, integrations, guild_id, source, timeout).await
    };
    let options = fetched.map_err(|error| match error {
        integrations::OptionsError::TooManyOptions => {
            UserError::message("tickets-form-options-too-many")
        }
        _ => UserError::message("tickets-form-options-unavailable"),
    })?;
    checked_fetched_options(options)
}

// Each fetch is cut off at the shared deadline; a cut-off fetch yields None.
async fn join_before_deadline<F: std::future::Future>(
    futures: Vec<F>,
    deadline: tokio::time::Instant,
) -> Vec<Option<F::Output>> {
    join_all(
        futures
            .into_iter()
            .map(|future| async move { tokio::time::timeout_at(deadline, future).await.ok() })
            .collect::<Vec<_>>(),
    )
    .await
}

// Polls every future on this task, so the wait is the slowest one rather than their sum.
async fn join_all<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut futures: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut any_pending = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => *output = Some(value),
                    std::task::Poll::Pending => any_pending = true,
                }
            }
        }
        if any_pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

pub(crate) fn form_modal_revision(base_revision: u64, questions: &[FormQuestion]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    base_revision.hash(&mut hasher);
    questions.hash(&mut hasher);
    hasher.finish()
}

impl FormQuestion {
    fn label(&self) -> Option<&str> {
        match self {
            Self::Legacy(label) => Some(label),
            Self::Structured(FormQuestionKind::Display { .. }) => None,
            Self::Structured(question) => Some(match question {
                FormQuestionKind::Text { label, .. }
                | FormQuestionKind::StringSelect { label, .. }
                | FormQuestionKind::UserSelect { label, .. }
                | FormQuestionKind::RoleSelect { label, .. }
                | FormQuestionKind::MentionableSelect { label, .. }
                | FormQuestionKind::ChannelSelect { label, .. }
                | FormQuestionKind::RadioGroup { label, .. }
                | FormQuestionKind::CheckboxGroup { label, .. } => label,
                FormQuestionKind::Display { .. } => return None,
            }),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn is_valid(&self) -> bool {
        match self {
            Self::Legacy(label) => valid_label(label),
            Self::Structured(FormQuestionKind::Text {
                label,
                required,
                min_length,
                max_length,
                placeholder,
                description,
                ..
            }) => {
                valid_label(label)
                    && placeholder.as_deref().is_none_or(valid_short_text)
                    && description.as_deref().is_none_or(valid_short_text)
                    && max_length.is_none_or(|length| (1..=MAX_FORM_ANSWER_CHARS).contains(&length))
                    && min_length.is_none_or(|length| {
                        length <= max_length.unwrap_or(MAX_FORM_ANSWER_CHARS)
                            && (*required || length == 0)
                    })
            }
            Self::Structured(FormQuestionKind::StringSelect {
                label,
                description,
                required,
                options,
                dynamic_options,
                min_values,
                max_values,
            }) => {
                let dynamic = dynamic_options.is_some();
                valid_label(label)
                    && valid_description(description.as_deref())
                    && (if dynamic {
                        options.is_empty() || valid_options(options, 1, 25)
                    } else {
                        valid_options(options, 1, 25)
                    })
                    && valid_value_range(
                        *required,
                        *min_values,
                        *max_values,
                        if dynamic && options.is_empty() {
                            25
                        } else {
                            options.len()
                        },
                        false,
                        options,
                    )
            }
            Self::Structured(
                FormQuestionKind::UserSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                }
                | FormQuestionKind::RoleSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                }
                | FormQuestionKind::MentionableSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                }
                | FormQuestionKind::ChannelSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                },
            ) => {
                valid_label(label)
                    && valid_description(description.as_deref())
                    && valid_value_range(*required, *min_values, *max_values, 25, false, &[])
            }
            Self::Structured(FormQuestionKind::RadioGroup {
                label,
                description,
                options,
                ..
            }) => {
                valid_label(label)
                    && valid_description(description.as_deref())
                    && valid_options(options, 2, 10)
                    && max_selected_label_chars(options, 1) <= MAX_EMBED_FIELD_CHARS
            }
            Self::Structured(FormQuestionKind::CheckboxGroup {
                label,
                description,
                required,
                options,
                min_values,
                max_values,
            }) => {
                valid_label(label)
                    && valid_description(description.as_deref())
                    && valid_options(options, 1, 10)
                    && valid_value_range(
                        *required,
                        *min_values,
                        *max_values,
                        options.len(),
                        true,
                        options,
                    )
            }
            Self::Structured(FormQuestionKind::Display { content }) => {
                !content.trim().is_empty()
                    && content.chars().count() <= 4000
                    && !content.contains('\0')
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn component(
        &self,
        module: &closure_core::module::ModuleId,
        index: usize,
    ) -> ModalComponent {
        let custom_id = format!("{module}:question-{index}");
        match self {
            Self::Legacy(label) => text_component(
                custom_id,
                label.clone(),
                FormTextStyle::Paragraph,
                true,
                None,
                1024,
                None,
                None,
            ),
            Self::Structured(FormQuestionKind::Text {
                label,
                style,
                required,
                min_length,
                max_length,
                placeholder,
                description,
            }) => text_component(
                custom_id,
                label.clone(),
                *style,
                *required,
                *min_length,
                max_length.unwrap_or(MAX_FORM_ANSWER_CHARS),
                placeholder.clone(),
                description.clone(),
            ),
            Self::Structured(FormQuestionKind::StringSelect {
                label,
                description,
                required,
                options,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) =
                    value_range(*required, *min_values, *max_values, options.len(), false);
                let options = options
                    .iter()
                    .map(|option| SelectMenuOption {
                        default: false,
                        emoji: None,
                        description: option.description.clone(),
                        label: option.label.clone(),
                        value: option.value.clone(),
                    })
                    .collect();
                ui::label(
                    label.clone(),
                    description.clone(),
                    select_menu(
                        custom_id,
                        SelectMenuType::Text,
                        Some(options),
                        *required,
                        min,
                        max,
                    ),
                )
            }
            Self::Structured(
                FormQuestionKind::UserSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                }
                | FormQuestionKind::RoleSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                }
                | FormQuestionKind::MentionableSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                }
                | FormQuestionKind::ChannelSelect {
                    label,
                    description,
                    required,
                    min_values,
                    max_values,
                },
            ) => {
                let kind = match self {
                    Self::Structured(FormQuestionKind::UserSelect { .. }) => SelectMenuType::User,
                    Self::Structured(FormQuestionKind::RoleSelect { .. }) => SelectMenuType::Role,
                    Self::Structured(FormQuestionKind::MentionableSelect { .. }) => {
                        SelectMenuType::Mentionable
                    }
                    _ => SelectMenuType::Channel,
                };
                let (min, max) = value_range(*required, *min_values, *max_values, 25, false);
                ui::label(
                    label.clone(),
                    description.clone(),
                    select_menu(custom_id, kind, None, *required, min, max),
                )
            }
            Self::Structured(FormQuestionKind::RadioGroup {
                label,
                description,
                required,
                options,
            }) => ui::label(
                label.clone(),
                description.clone(),
                ModalComponent::RadioGroup {
                    custom_id,
                    required: Some(*required),
                    options: modal_options(options),
                },
            ),
            Self::Structured(FormQuestionKind::CheckboxGroup {
                label,
                description,
                required,
                options,
                min_values,
                max_values,
            }) => {
                let (min, max) =
                    value_range(*required, *min_values, *max_values, options.len(), true);
                ui::label(
                    label.clone(),
                    description.clone(),
                    ModalComponent::CheckboxGroup {
                        custom_id,
                        required: Some(*required),
                        min_values: Some(min),
                        max_values: Some(max),
                        options: modal_options(options),
                    },
                )
            }
            Self::Structured(FormQuestionKind::Display { content }) => {
                ModalComponent::Component(Component::TextDisplay(TextDisplay {
                    id: None,
                    content: content.clone(),
                }))
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn answer(
        &self,
        value: Option<&ModalValue>,
    ) -> Result<Option<(String, String)>, UserError> {
        match self {
            Self::Legacy(label) => text_answer(label, value, true, 0, 1024),
            Self::Structured(FormQuestionKind::Text {
                label,
                required,
                min_length,
                max_length,
                ..
            }) => text_answer(
                label,
                value,
                *required,
                min_length.unwrap_or(0),
                usize::from(max_length.unwrap_or(MAX_FORM_ANSWER_CHARS)),
            ),
            Self::Structured(FormQuestionKind::StringSelect {
                label,
                required,
                options,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) =
                    value_range(*required, *min_values, *max_values, options.len(), false);
                let Some(ModalValue::StringSelect(values)) = value else {
                    return missing(*required);
                };
                choice_answer(label, options, values, *required, min, max)
            }
            Self::Structured(FormQuestionKind::UserSelect {
                label,
                required,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) = value_range(*required, *min_values, *max_values, 25, false);
                let Some(ModalValue::UserSelect(values)) = value else {
                    return missing(*required);
                };
                entity_answer(label, values, *required, min, max, EntityAnswer::User)
            }
            Self::Structured(FormQuestionKind::RoleSelect {
                label,
                required,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) = value_range(*required, *min_values, *max_values, 25, false);
                let Some(ModalValue::RoleSelect(values)) = value else {
                    return missing(*required);
                };
                entity_answer(label, values, *required, min, max, EntityAnswer::Role)
            }
            Self::Structured(FormQuestionKind::MentionableSelect {
                label,
                required,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) = value_range(*required, *min_values, *max_values, 25, false);
                let Some(ModalValue::MentionableSelect(values)) = value else {
                    return missing(*required);
                };
                entity_answer(
                    label,
                    values,
                    *required,
                    min,
                    max,
                    EntityAnswer::Mentionable,
                )
            }
            Self::Structured(FormQuestionKind::ChannelSelect {
                label,
                required,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) = value_range(*required, *min_values, *max_values, 25, false);
                let Some(ModalValue::ChannelSelect(values)) = value else {
                    return missing(*required);
                };
                entity_answer(label, values, *required, min, max, EntityAnswer::Channel)
            }
            Self::Structured(FormQuestionKind::RadioGroup {
                label,
                required,
                options,
                ..
            }) => {
                let Some(ModalValue::RadioGroup(Some(value))) = value else {
                    return missing(*required);
                };
                choice_answer(label, options, std::slice::from_ref(value), *required, 1, 1)
            }
            Self::Structured(FormQuestionKind::CheckboxGroup {
                label,
                required,
                options,
                min_values,
                max_values,
                ..
            }) => {
                let (min, max) =
                    value_range(*required, *min_values, *max_values, options.len(), true);
                let Some(ModalValue::CheckboxGroup(values)) = value else {
                    return missing(*required);
                };
                choice_answer(label, options, values, *required, min, max)
            }
            Self::Structured(FormQuestionKind::Display { .. }) => {
                if value.is_some() {
                    Err(UserError::message("tickets-form-invalid"))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

fn valid_label(value: &str) -> bool {
    !value.trim().is_empty()
        && !value.chars().any(char::is_control)
        && value.chars().count() <= MAX_FORM_QUESTION_CHARS
}

fn valid_short_text(value: &str) -> bool {
    value.chars().count() <= 100 && !value.chars().any(char::is_control)
}

fn valid_description(value: Option<&str>) -> bool {
    value.is_none_or(valid_short_text)
}

fn valid_options(options: &[FormOption], min: usize, max: usize) -> bool {
    (min..=max).contains(&options.len())
        && options.iter().all(|option| {
            !option.label.trim().is_empty()
                && option.label.chars().count() <= 100
                && !option.label.chars().any(char::is_control)
                && !option.value.trim().is_empty()
                && option.value.chars().count() <= 100
                && !option.value.chars().any(char::is_control)
                && option.description.as_deref().is_none_or(valid_short_text)
        })
        && options
            .iter()
            .map(|option| &option.value)
            .collect::<HashSet<_>>()
            .len()
            == options.len()
}

fn valid_value_range(
    required: bool,
    min_values: Option<u8>,
    max_values: Option<u8>,
    option_count: usize,
    default_max_all: bool,
    options: &[FormOption],
) -> bool {
    let (min, max) = value_range(
        required,
        min_values,
        max_values,
        option_count,
        default_max_all,
    );
    min <= max
        && max > 0
        && max <= 25
        && usize::from(max) <= option_count
        && (required && min > 0 || !required && min == 0)
        && max_selected_label_chars(options, max) <= MAX_EMBED_FIELD_CHARS
}

fn value_range(
    required: bool,
    min_values: Option<u8>,
    max_values: Option<u8>,
    option_count: usize,
    default_max_all: bool,
) -> (u8, u8) {
    let min = min_values.unwrap_or(u8::from(required));
    let max = max_values.unwrap_or(if default_max_all {
        u8::try_from(option_count).unwrap_or(25)
    } else {
        1
    });
    (min, max)
}

fn max_selected_label_chars(options: &[FormOption], max: u8) -> usize {
    let mut lengths = options
        .iter()
        .map(|option| option.label.chars().count())
        .collect::<Vec<_>>();
    lengths.sort_unstable_by(|left, right| right.cmp(left));
    lengths
        .into_iter()
        .take(usize::from(max))
        .sum::<usize>()
        .saturating_add(usize::from(max.saturating_sub(1)) * 2)
}

#[allow(clippy::too_many_arguments)]
fn text_component(
    custom_id: String,
    label: String,
    style: FormTextStyle,
    required: bool,
    min_length: Option<u16>,
    max_length: u16,
    placeholder: Option<String>,
    description: Option<String>,
) -> ModalComponent {
    #[allow(deprecated)]
    let input = Component::TextInput(TextInput {
        id: None,
        custom_id,
        label: None,
        max_length: Some(max_length),
        min_length,
        placeholder,
        required: Some(required),
        style: match style {
            FormTextStyle::Short => DiscordTextInputStyle::Short,
            FormTextStyle::Paragraph => DiscordTextInputStyle::Paragraph,
        },
        value: None,
    });
    ui::label(label, description, input)
}

fn select_menu(
    custom_id: String,
    kind: SelectMenuType,
    options: Option<Vec<SelectMenuOption>>,
    required: bool,
    min_values: u8,
    max_values: u8,
) -> Component {
    Component::SelectMenu(SelectMenu {
        id: None,
        channel_types: None,
        custom_id,
        default_values: None,
        disabled: false,
        kind,
        max_values: Some(max_values),
        min_values: Some(min_values),
        options,
        placeholder: None,
        required: Some(required),
    })
}

fn modal_options(options: &[FormOption]) -> Vec<ModalOption> {
    options
        .iter()
        .map(|option| ModalOption {
            value: option.value.clone(),
            label: option.label.clone(),
            description: option.description.clone(),
            default: None,
        })
        .collect()
}

fn missing(required: bool) -> Result<Option<(String, String)>, UserError> {
    if required {
        Err(UserError::message("tickets-form-invalid"))
    } else {
        Ok(None)
    }
}

fn text_answer(
    label: &str,
    value: Option<&ModalValue>,
    required: bool,
    min: u16,
    max: usize,
) -> Result<Option<(String, String)>, UserError> {
    let Some(ModalValue::Text(text)) = value else {
        return missing(required);
    };
    if text.trim().is_empty() {
        return missing(required);
    }
    let size = answer_size(&(label.to_owned(), text.clone()))
        .map_err(|_| UserError::message("tickets-form-invalid"))?;
    let chars = text.chars().count();
    if chars < usize::from(min) || chars > max || text.contains('\0') || size > MAX_VALUE_BYTES {
        return Err(UserError::message("tickets-form-invalid"));
    }
    Ok(Some((label.to_owned(), text.clone())))
}

#[derive(Clone, Copy)]
enum EntityAnswer {
    User,
    Role,
    Mentionable,
    Channel,
}

fn entity_answer(
    label: &str,
    values: &[String],
    required: bool,
    min: u8,
    max: u8,
    kind: EntityAnswer,
) -> Result<Option<(String, String)>, UserError> {
    if values.is_empty() {
        return missing(required);
    }
    if values.len() < usize::from(min)
        || values.len() > usize::from(max)
        || values.iter().any(|value| !valid_snowflake(value))
        || values.iter().collect::<HashSet<_>>().len() != values.len()
    {
        return Err(UserError::message("tickets-form-invalid"));
    }
    let answer = values
        .iter()
        .map(|value| match kind {
            EntityAnswer::User => format!("<@{value}>"),
            EntityAnswer::Role => format!("<@&{value}>"),
            EntityAnswer::Mentionable => value.clone(),
            EntityAnswer::Channel => format!("<#{value}>"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if answer.chars().count() > MAX_EMBED_FIELD_CHARS {
        return Err(UserError::message("tickets-form-invalid"));
    }
    Ok(Some((label.to_owned(), answer)))
}

fn valid_snowflake(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}

fn choice_answer(
    label: &str,
    options: &[FormOption],
    values: &[String],
    required: bool,
    min: u8,
    max: u8,
) -> Result<Option<(String, String)>, UserError> {
    if values.is_empty() {
        return missing(required);
    }
    if values.len() < usize::from(min)
        || values.len() > usize::from(max)
        || values.iter().collect::<HashSet<_>>().len() != values.len()
    {
        return Err(UserError::message("tickets-form-invalid"));
    }
    let labels = values
        .iter()
        .map(|value| {
            options
                .iter()
                .find(|option| option.value.as_str() == value.as_str())
                .map(|option| option.label.clone())
                .ok_or_else(|| UserError::message("tickets-form-invalid"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let answer = labels.join(", ");
    if answer.chars().count() > MAX_EMBED_FIELD_CHARS {
        return Err(UserError::message("tickets-form-invalid"));
    }
    Ok(Some((label.to_owned(), answer)))
}

pub(crate) fn valid_form_answers(questions: &[FormQuestion], answers: &[(String, String)]) -> bool {
    let labels = questions
        .iter()
        .filter_map(FormQuestion::label)
        .collect::<Vec<_>>();
    answers.len() <= labels.len()
        && answers.iter().all(|answer| {
            labels.contains(&answer.0.as_str())
                && !answer.1.trim().is_empty()
                && answer.1.chars().count() <= usize::from(MAX_FORM_ANSWER_CHARS)
                && !answer.0.contains('\0')
                && !answer.1.contains('\0')
                && answer_size(answer).is_ok_and(|size| size <= MAX_VALUE_BYTES)
        })
}

fn answer_size(answer: &(String, String)) -> Result<usize, serde_json::Error> {
    serde_json::to_vec(answer).map(|encoded| encoded.len())
}

pub(crate) fn externalize_form_answers(
    ticket: &mut crate::Ticket,
    answers: &[(String, String)],
) -> Result<bool, StorageError> {
    let size = crate::ticket_storage_size(ticket)
        .map_err(|error| StorageError::Value(error.to_string()))?;
    if size <= crate::MAX_TICKET_BYTES {
        return Ok(false);
    }
    if answers.is_empty() || answers.len() > MAX_FORM_QUESTIONS {
        return Err(StorageError::TooLarge {
            size,
            limit: crate::MAX_TICKET_BYTES,
        });
    }
    ticket.form_answers.clear();
    ticket.form_answer_count =
        u8::try_from(answers.len()).map_err(|error| StorageError::Value(error.to_string()))?;
    let size = crate::ticket_storage_size(ticket)
        .map_err(|error| StorageError::Value(error.to_string()))?;
    if size > crate::MAX_TICKET_BYTES {
        return Err(StorageError::TooLarge {
            size,
            limit: crate::MAX_TICKET_BYTES,
        });
    }
    for answer in answers {
        if answer.0.contains('\0') || answer.1.contains('\0') {
            return Err(StorageError::Value("text must not contain NUL".to_owned()));
        }
        let size = answer_size(answer).map_err(|error| StorageError::Value(error.to_string()))?;
        if size > MAX_VALUE_BYTES {
            return Err(StorageError::TooLarge {
                size,
                limit: MAX_VALUE_BYTES,
            });
        }
    }
    Ok(true)
}

pub(crate) fn ticket_form_answer_key(number: i64, index: usize) -> String {
    format!("ticket-form:{number}:{index}")
}

pub(crate) async fn load_ticket_form_answers(
    ctx: &EventContext,
    ticket: &crate::Ticket,
) -> Result<Vec<(String, String)>, StorageError> {
    if ticket.form_answer_count == 0 {
        return Ok(ticket.form_answers.clone());
    }
    let count = usize::from(ticket.form_answer_count);
    if !ticket.form_answers.is_empty() || count > MAX_FORM_QUESTIONS {
        return Err(StorageError::Value(
            "invalid ticket form answer count".to_owned(),
        ));
    }
    let mut answers = Vec::with_capacity(count);
    for index in 0..count {
        let answer = ctx
            .store()
            .get::<(String, String)>(&ticket_form_answer_key(ticket.number, index))
            .await?
            .ok_or_else(|| StorageError::Value("ticket form answer is missing".to_owned()))?;
        answers.push(answer);
    }
    Ok(answers)
}

pub(crate) async fn write_ticket_form_answers(
    ctx: &EventContext,
    ticket: &crate::Ticket,
    answers: &[(String, String)],
) -> Result<(), StorageError> {
    if answers.len() != usize::from(ticket.form_answer_count) || answers.len() > MAX_FORM_QUESTIONS
    {
        return Err(StorageError::Value(
            "invalid ticket form answer count".to_owned(),
        ));
    }
    for answer in answers {
        let size = answer_size(answer).map_err(|error| StorageError::Value(error.to_string()))?;
        if answer.1.trim().is_empty()
            || answer.1.chars().count() > usize::from(MAX_FORM_ANSWER_CHARS)
            || answer.0.chars().count() > MAX_FORM_QUESTION_CHARS
            || answer.0.contains('\0')
            || answer.1.contains('\0')
        {
            return Err(StorageError::Value("invalid ticket form answer".to_owned()));
        }
        if size > MAX_VALUE_BYTES {
            return Err(StorageError::TooLarge {
                size,
                limit: MAX_VALUE_BYTES,
            });
        }
    }
    for (index, answer) in answers.iter().enumerate() {
        if let Err(error) = ctx
            .store()
            .set_for(
                &ticket_form_answer_key(ticket.number, index),
                answer,
                crate::ticket_ttl(ticket, ctx.now()),
            )
            .await
        {
            for cleanup in 0..answers.len() {
                let _ = ctx
                    .store()
                    .delete(&ticket_form_answer_key(ticket.number, cleanup))
                    .await;
            }
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) async fn refresh_ticket_form_answer_ttl(
    ctx: &EventContext,
    ticket: &crate::Ticket,
) -> Result<(), StorageError> {
    // Inline answers live in the ticket record; copying them to per-index keys would orphan those keys.
    if ticket.form_answer_count == 0 {
        return Ok(());
    }
    let answers = load_ticket_form_answers(ctx, ticket).await?;
    for (index, answer) in answers.iter().enumerate() {
        ctx.store()
            .set_for(
                &ticket_form_answer_key(ticket.number, index),
                answer,
                crate::ticket_ttl(ticket, ctx.now()),
            )
            .await?;
    }
    Ok(())
}

pub(crate) async fn delete_ticket_form_answers(
    ctx: &EventContext,
    ticket: &crate::Ticket,
) -> Result<(), StorageError> {
    if usize::from(ticket.form_answer_count) > MAX_FORM_QUESTIONS {
        return Err(StorageError::Value(
            "invalid ticket form answer count".to_owned(),
        ));
    }
    let mut first_error = None;
    for index in 0..usize::from(ticket.form_answer_count) {
        if let Err(error) = ctx
            .store()
            .delete(&ticket_form_answer_key(ticket.number, index))
            .await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn split_form_answer(answer: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (index, _) in answer.char_indices() {
        if chars == MAX_EMBED_FIELD_CHARS {
            chunks.push(answer[start..index].to_owned());
            start = index;
            chars = 0;
        }
        chars += 1;
    }
    if start < answer.len() {
        chunks.push(answer[start..].to_owned());
    }
    chunks
}

pub(crate) fn form_answers_embeds<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    answers: &[(String, String)],
) -> Result<Vec<Embed>, UserError> {
    let title = cx.t("tickets-form-answers-title", &Args::new());
    answer_embeds(&title, answers).ok_or_else(|| UserError::message("tickets-form-invalid"))
}

pub(crate) fn form_answers_embeds_in(
    ctx: &EventContext,
    answers: &[(String, String)],
) -> Result<Vec<Embed>, ModuleError> {
    let title = ctx.t_in(ctx.locale, "tickets-form-answers-title", &Args::new());
    answer_embeds(&title, answers).ok_or_else(|| ModuleError::other("invalid ticket form answer"))
}

// Fields are packed into embeds first, then twilight validates each embed as built.
fn answer_embeds(title: &str, answers: &[(String, String)]) -> Option<Vec<Embed>> {
    let fields = answer_fields(answers)?;
    pack_within(
        fields,
        title.chars().count(),
        MAX_EMBED_FIELDS,
        MAX_EMBED_TOTAL_CHARS,
        field_chars,
    )
    .into_iter()
    .map(|group| {
        group
            .into_iter()
            .fold(EmbedBuilder::new().title(title), |embed, (name, value)| {
                embed.field(name, value, false)
            })
            .build()
            .ok()
    })
    .collect()
}

fn answer_fields(answers: &[(String, String)]) -> Option<Vec<(String, String)>> {
    let mut fields = Vec::new();
    for (question, answer) in answers {
        if answer.chars().count() > usize::from(MAX_FORM_ANSWER_CHARS) || answer.contains('\0') {
            return None;
        }
        let parts = split_form_answer(answer);
        let count = parts.len();
        for (index, part) in parts.into_iter().enumerate() {
            let name = if count == 1 {
                question.clone()
            } else {
                format!("{question} ({}/{count})", index + 1)
            };
            if name.chars().count() > MAX_EMBED_FIELD_NAME_CHARS {
                return None;
            }
            fields.push((name, part));
        }
    }
    Some(fields)
}

fn field_chars(field: &(String, String)) -> usize {
    field.0.chars().count() + field.1.chars().count()
}

// Discord counts title, description, field names and values toward the 6000-character embed limit.
fn embed_text_chars(embed: &Embed) -> usize {
    let title = embed
        .title
        .as_deref()
        .map_or(0, |text| text.chars().count());
    let description = embed
        .description
        .as_deref()
        .map_or(0, |text| text.chars().count());
    embed
        .fields
        .iter()
        .fold(title + description, |total, field| {
            total + field.name.chars().count() + field.value.chars().count()
        })
}

// Greedy packing: a group is closed when adding an item would exceed the item or character limit.
fn pack_within<T>(
    items: Vec<T>,
    base_chars: usize,
    max_items: usize,
    max_chars: usize,
    chars_of: impl Fn(&T) -> usize,
) -> Vec<Vec<T>> {
    let mut groups: Vec<Vec<T>> = Vec::new();
    let mut chars = 0;
    for item in items {
        let item_chars = chars_of(&item);
        let fits = groups.last().is_some_and(|group| group.len() < max_items)
            && chars + item_chars <= max_chars;
        if !fits {
            groups.push(Vec::new());
            chars = base_chars;
        }
        chars += item_chars;
        if let Some(group) = groups.last_mut() {
            group.push(item);
        }
    }
    groups
}

pub(crate) async fn send_form_answers(
    ctx: &EventContext,
    channel: ChannelId,
    embeds: &[Embed],
) -> Result<(), UserError> {
    for message_embeds in pack_within(
        embeds.to_vec(),
        0,
        MAX_MESSAGE_EMBEDS,
        MAX_EMBED_TOTAL_CHARS,
        embed_text_chars,
    ) {
        ctx.api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: None,
                embeds: message_embeds,
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await
            .map_err(|error| crate::failed(&error))?;
    }
    Ok(())
}

#[cfg(test)]
mod dynamic_option_tests {
    use super::*;
    use crate::integrations::DynamicOptions;

    fn option(value: &str) -> FormOption {
        FormOption {
            label: format!("Option {value}"),
            value: value.to_owned(),
            description: None,
        }
    }

    fn dynamic_select(min_values: u8, max_values: u8, options: Vec<FormOption>) -> FormQuestion {
        FormQuestion::Structured(FormQuestionKind::StringSelect {
            label: "Pick".to_owned(),
            description: None,
            required: true,
            options,
            dynamic_options: Some(DynamicOptions {
                integration: "catalog".to_owned(),
                items_pointer: "/items".to_owned(),
                label_pointer: "/name".to_owned(),
                value_pointer: "/id".to_owned(),
                description_pointer: None,
            }),
            min_values: Some(min_values),
            max_values: Some(max_values),
        })
    }

    #[test]
    fn resolved_dynamic_select_checks_range_against_fetched_count() {
        let question = dynamic_select(3, 3, vec![option("1"), option("2")]);
        assert!(!valid_questions(&[question]));
    }

    #[test]
    fn unresolved_dynamic_select_accepts_range_within_source_limit() {
        let question = dynamic_select(3, 3, Vec::new());
        assert!(valid_questions(&[question]));
    }

    #[test]
    fn fetched_options_must_not_be_empty() {
        assert!(checked_fetched_options(Vec::new()).is_err());
        assert_eq!(
            checked_fetched_options(vec![option("1")])
                .map(|v| v.len())
                .ok(),
            Some(1)
        );
    }

    #[test]
    fn select_without_options_or_source_is_rejected() {
        let question = FormQuestion::Structured(FormQuestionKind::StringSelect {
            label: "Pick".to_owned(),
            description: None,
            required: true,
            options: Vec::new(),
            dynamic_options: None,
            min_values: None,
            max_values: None,
        });
        assert!(!valid_questions(&[question]));
    }

    #[tokio::test]
    async fn concurrent_fetches_wait_for_the_slowest_not_the_sum() {
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(5);
        let requests: Vec<_> = (0..3_u8)
            .map(|n| async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                n
            })
            .collect();
        let results = join_before_deadline(requests, deadline).await;
        assert_eq!(results, vec![Some(0), Some(1), Some(2)]);
        assert!(start.elapsed() < Duration::from_millis(450));
    }

    #[tokio::test]
    async fn fetch_past_the_shared_deadline_is_cut_off() {
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_millis(100);
        let requests = vec![async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            1_u8
        }];
        let results = join_before_deadline(requests, deadline).await;
        assert_eq!(results, vec![None]);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
#[cfg(test)]
mod answer_packing_tests {
    use super::*;

    fn long_answers() -> Vec<(String, String)> {
        (1..=MAX_FORM_QUESTIONS)
            .map(|n| {
                (
                    format!("Question {n}"),
                    "a".repeat(usize::from(MAX_FORM_ANSWER_CHARS)),
                )
            })
            .collect()
    }

    #[test]
    fn five_long_answers_fit_every_message_limit_without_losing_text() {
        let Some(embeds) = answer_embeds("Answers", &long_answers()) else {
            panic!("long answers must build into embeds");
        };
        assert!(
            embeds
                .iter()
                .all(|embed| embed.fields.len() <= MAX_EMBED_FIELDS)
        );
        let messages = pack_within(
            embeds,
            0,
            MAX_MESSAGE_EMBEDS,
            MAX_EMBED_TOTAL_CHARS,
            embed_text_chars,
        );
        assert!(messages.iter().all(|message| {
            message.len() <= MAX_MESSAGE_EMBEDS
                && message.iter().map(embed_text_chars).sum::<usize>() <= MAX_EMBED_TOTAL_CHARS
        }));
        let answer_chars: usize = messages
            .iter()
            .flatten()
            .flat_map(|embed| embed.fields.iter())
            .map(|field| field.value.chars().count())
            .sum();
        assert_eq!(
            answer_chars,
            MAX_FORM_QUESTIONS * usize::from(MAX_FORM_ANSWER_CHARS)
        );
    }

    #[test]
    fn pack_within_closes_a_group_at_the_item_limit() {
        let groups = pack_within(vec![1_usize, 1, 1], 0, 2, 100, |item| *item);
        let sizes: Vec<usize> = groups.iter().map(Vec::len).collect();
        assert_eq!(sizes, vec![2, 1]);
    }
}
