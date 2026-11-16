use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use closure_sdk::testing::{ContextUse, Harness};
use plugin_kit::net::{Get, Post};
use plugin_kit::translator::{ApiKey, Translator};
use serde_json::Value;

use crate::builder_with;

struct Fake(bool);

#[async_trait]
impl Post for Fake {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &Value,
    ) -> Result<Vec<u8>> {
        bail!("down")
    }
}

/// Gemini answers; the free endpoint is down, so only Gemini can translate.
struct GeminiUp;

#[async_trait]
impl Get for GeminiUp {
    async fn get(&self, _url: &str) -> Result<Vec<u8>> {
        bail!("the free endpoint is down")
    }
}

#[async_trait]
impl Post for GeminiUp {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &Value,
    ) -> Result<Vec<u8>> {
        let structured = r#"{"translation":"Gemini says hi","source_language":"en"}"#;
        Ok(
            serde_json::json!({"candidates": [{"content": {"parts": [{"text": structured}]}}]})
                .to_string()
                .into_bytes(),
        )
    }
}

#[async_trait]
impl Get for Fake {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        if self.0 && url.contains("translate_a") {
            Ok(
                r#"[[["こんにちは世界","hello world",null,null,10]],null,"en"]"#
                    .as_bytes()
                    .to_vec(),
            )
        } else {
            bail!("down")
        }
    }
}

fn harness(up: bool) -> Harness {
    let translator = Arc::new(Translator::free());
    Harness::new(builder_with(translator, Arc::new(Fake(up))).unwrap()).unwrap()
}

fn use_on(text: &str) -> ContextUse {
    ContextUse::on_message("Translate", 5, 7, text).user(50)
}

#[tokio::test]
async fn a_message_is_translated_for_the_one_who_asked() {
    let harness = harness(true);
    let replies = harness
        .context_command(use_on("hello world"))
        .await
        .unwrap();
    assert!(
        replies
            .iter()
            .any(|r| r.content.contains("こんにちは世界") && r.ephemeral),
        "{replies:?}"
    );
}

#[tokio::test]
async fn a_message_is_translated_by_gemini_when_it_is_set_up() {
    let translator = Arc::new(Translator::gemini(
        ApiKey::new("k"),
        "gemini-2.5-flash".to_owned(),
    ));
    let harness = Harness::new(builder_with(translator, Arc::new(GeminiUp)).unwrap()).unwrap();
    let replies = harness.context_command(use_on("hi")).await.unwrap();
    assert!(
        replies
            .iter()
            .any(|r| r.content.contains("Gemini says hi") && r.ephemeral),
        "{replies:?}"
    );
}

#[tokio::test]
async fn a_message_without_text_or_a_translator_that_is_down_is_told() {
    let harness = harness(true);
    let replies = harness.context_command(use_on("   ")).await.unwrap();
    assert!(
        replies
            .iter()
            .all(|r| !r.content.contains("こんにちは世界"))
    );
    let down = self::harness(false);
    let replies = down.context_command(use_on("hello")).await.unwrap();
    assert!(
        replies
            .iter()
            .all(|r| !r.content.contains("こんにちは世界"))
    );
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(
        closure_i18n::missing_keys(crate::EN, crate::JA).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        closure_i18n::missing_keys(crate::JA, crate::EN).unwrap(),
        Vec::<String>::new()
    );
}
