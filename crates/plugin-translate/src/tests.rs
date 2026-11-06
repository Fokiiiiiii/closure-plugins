use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use closure_sdk::testing::{ContextUse, Harness};
use plugin_kit::net::Get;

use crate::builder_with;

struct Fake(bool);

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
    Harness::new(builder_with(Arc::new(Fake(up))).unwrap()).unwrap()
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
