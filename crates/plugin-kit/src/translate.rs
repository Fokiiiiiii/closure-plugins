//! Translating a text, through Google's public translate endpoint (no key, no account).
//!
//! shortcut: the text goes in the URL, so it is cut to [`MAX_CHARS`]; the endpoint is the one the
//! web page uses and can change without notice. Upgrade to an official API when that matters.

use std::fmt::Write as _;

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::net::Get;

/// The most characters sent in one request.
pub const MAX_CHARS: usize = 700;

/// A text in another language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    pub text: String,
    /// The language the text was in, as the service detected it (`en`, `ja`...).
    pub from: String,
}

/// `text` as it can stand in a URL.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The address that asks for `text` in the language `to`.
#[must_use]
pub fn request_url(text: &str, to: &str) -> String {
    let text: String = text.chars().take(MAX_CHARS).collect();
    format!(
        "https://translate.googleapis.com/translate_a/single?client=gtx&sl=auto&tl={}&dt=t&q={}",
        percent_encode(to),
        percent_encode(&text)
    )
}

/// Reads the answer of the service.
///
/// # Errors
/// The answer is not the shape the service gives.
pub fn parse(bytes: &[u8]) -> Result<Translation> {
    let answer: Value = serde_json::from_slice(bytes).context("reading the translation")?;
    let parts = answer
        .get(0)
        .and_then(Value::as_array)
        .context("the answer has no text")?;
    let text: String = parts
        .iter()
        .filter_map(|part| part.get(0).and_then(Value::as_str))
        .collect();
    if text.is_empty() {
        bail!("the answer is empty");
    }
    let from = answer
        .get(2)
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    Ok(Translation { text, from })
}

/// Translates `text` into the language `to`.
///
/// # Errors
/// The service is out of reach, or its answer cannot be read.
pub async fn translate(get: &dyn Get, text: &str, to: &str) -> Result<Translation> {
    parse(&get.get(&request_url(text, to)).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_is_made_fit_for_a_url() {
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_encode("あ"), "%E3%81%82");
        assert_eq!(percent_encode("safe-_.~09az"), "safe-_.~09az");
    }

    #[test]
    fn a_long_text_is_cut_in_the_request() {
        let url = request_url(&"a".repeat(MAX_CHARS + 50), "ja");
        assert_eq!(url.split("&q=").nth(1).map(str::len), Some(MAX_CHARS));
        assert!(url.contains("tl=ja"));
    }

    #[test]
    fn the_answer_is_the_parts_put_together_and_the_language_it_was_in() {
        let answer = r#"[[["Hello, ","こんにちは、",null,null,10],["world","世界",null,null,10]],null,"ja"]"#.as_bytes();
        assert_eq!(
            parse(answer).unwrap(),
            Translation {
                text: "Hello, world".into(),
                from: "ja".into()
            }
        );
        assert!(parse(b"[[]]").is_err());
        assert!(parse(b"nope").is_err());
    }
}
