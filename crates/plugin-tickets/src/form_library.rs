use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::advanced_forms::{self, FormQuestion, FormQuestionKind};

const FORM_ID_MAX_CHARS: usize = 32;
const FORM_TITLE_MAX_CHARS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct FormDefinition {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) questions: Vec<FormQuestion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GalleryFormPayload {
    title: String,
    questions: Vec<FormQuestion>,
}

pub(crate) fn schema() -> Value {
    json!({
        "type": "array",
        "description": "config-tickets-forms",
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "title", "questions"],
            "properties": {
                "id": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": FORM_ID_MAX_CHARS,
                    "pattern": "^[a-z0-9_-]+$"
                },
                "title": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": FORM_TITLE_MAX_CHARS,
                    "pattern": "^\\S(?:[^\\x00-\\x1F\\x7F\\u0080-\\u009F]*\\S)?$"
                },
                "questions": advanced_forms::form_questions_schema()
            }
        }
    })
}

pub(crate) fn valid_id(id: &str) -> bool {
    (1..=FORM_ID_MAX_CHARS).contains(&id.len())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub(crate) fn valid_forms(forms: &[FormDefinition]) -> bool {
    let mut ids = HashSet::with_capacity(forms.len());
    forms
        .iter()
        .all(|form| valid_form(form) && ids.insert(form.id.as_str()))
}

pub(crate) fn resolve_form<'a>(
    forms: &'a [FormDefinition],
    id: &str,
) -> Option<&'a FormDefinition> {
    if !valid_id(id) {
        return None;
    }
    forms.iter().find(|form| form.id == id && valid_form(form))
}

pub(crate) fn gallery_payload(form: &FormDefinition) -> Option<Value> {
    if !valid_form(form) || has_dynamic_options(&form.questions) {
        return None;
    }
    let mut questions = serde_json::to_value(&form.questions).ok()?;
    let questions = questions.as_array_mut()?;
    for question in &mut *questions {
        let Some(question) = question.as_object_mut() else {
            continue;
        };
        if question.get("type").and_then(Value::as_str) == Some("string_select") {
            if question
                .get("dynamic_options")
                .is_some_and(|options| !options.is_null())
            {
                return None;
            }
            question.remove("dynamic_options");
        }
    }
    Some(json!({
        "title": form.title,
        "questions": questions
    }))
}

pub(crate) fn from_gallery(
    id: &str,
    payload: Value,
    existing: &[FormDefinition],
) -> Result<FormDefinition, &'static str> {
    if !valid_id(id) {
        return Err("invalid form id");
    }
    if existing.iter().any(|form| form.id == id) {
        return Err("form id already exists");
    }
    if payload_has_dynamic_options(&payload) {
        return Err("gallery forms cannot contain dynamic options");
    }
    let payload: GalleryFormPayload =
        serde_json::from_value(payload).map_err(|_| "invalid gallery form payload")?;
    let form = FormDefinition {
        id: id.to_owned(),
        title: payload.title,
        questions: payload.questions,
    };
    if !valid_form(&form) {
        return Err("invalid form definition");
    }
    Ok(form)
}

fn valid_form(form: &FormDefinition) -> bool {
    valid_id(&form.id)
        && valid_title(&form.title)
        && advanced_forms::valid_questions(&form.questions)
}

fn valid_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.trim() == title
        && title.chars().count() <= FORM_TITLE_MAX_CHARS
        && !title.chars().any(char::is_control)
}

fn has_dynamic_options(questions: &[FormQuestion]) -> bool {
    questions.iter().any(|question| {
        matches!(
            question,
            FormQuestion::Structured(FormQuestionKind::StringSelect {
                dynamic_options: Some(_),
                ..
            })
        )
    })
}

fn payload_has_dynamic_options(payload: &Value) -> bool {
    payload
        .get("questions")
        .and_then(Value::as_array)
        .is_some_and(|questions| {
            questions.iter().any(|question| {
                question
                    .as_object()
                    .is_some_and(|question| question.contains_key("dynamic_options"))
            })
        })
}
