//! Translates a message: right-click it, choose Translate, and only you see the answer, in your
//! language.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::Arc;

use closure_sdk::{
    ContextCommand, ContextCx, CoreError, InteractionReply, Level, Locale, ModuleBuilder,
    PermissionSpec, SdkModule, UserError,
};
use plugin_kit::net::{Fetcher, Get};
use plugin_kit::translate::{self, MAX_CHARS};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

async fn translate_message(get: Arc<dyn Get>, cx: ContextCx) -> Result<(), UserError> {
    let Some(message) = cx.target_message() else {
        return Err(UserError::message("translate-nothing"));
    };
    let text = if message.content.trim().is_empty() {
        message.embeds_text.clone()
    } else {
        message.content.clone()
    };
    if text.trim().is_empty() {
        return Err(UserError::message("translate-nothing"));
    }
    cx.defer(true).await?;
    let to = cx.locale().tag();
    let done = translate::translate(&*get, &text, to)
        .await
        .map_err(|_| UserError::message("translate-failed"))?;
    let cut = text.chars().count() > MAX_CHARS;
    let footer = if cut {
        format!("{} → {to}  ·  {MAX_CHARS}", done.from)
    } else {
        format!("{} → {to}", done.from)
    };
    let embed = closure_sdk::EmbedBuilder::new()
        .description(plugin_kit::truncate(&done.text, 3500))
        .footer(footer)
        .build()
        .map_err(|_| UserError::message("translate-failed"))?;
    cx.reply_with(InteractionReply {
        content: None,
        embeds: vec![embed],
        ephemeral: true,
        components: Vec::new(),
    })
    .await
}

/// The module, with the network it reaches the translator through.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder_with(get: Arc<dyn Get>) -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("translate", "Translate", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::RespondToInteractions)
            .permission(PermissionSpec::new(
                "translate.use",
                Level::GENERAL,
                "permission-translate-use",
            )?)
            .context(
                ContextCommand::message("Translate")
                    .requires("translate.use")
                    .handler(move |cx| translate_message(Arc::clone(&get), cx)),
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

/// The module, reaching the network as it is.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder_with(Arc::new(Fetcher::new()))?.build()
}

#[cfg(test)]
mod tests;
