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

/// How many requests one text may take: a longer text is cut, and the cut is shown.
pub const MAX_CHUNKS: usize = 3;

/// `text` in pieces of at most [`MAX_CHARS`], each ended where a line, a sentence or a word ends if
/// there is such a place in its second half; the pieces are at most [`MAX_CHUNKS`].
#[must_use]
pub fn chunks_of(text: &str) -> Vec<String> {
    let mut rest: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    while !rest.is_empty() && out.len() < MAX_CHUNKS {
        if rest.len() <= MAX_CHARS {
            out.push(rest.iter().collect());
            rest.clear();
            break;
        }
        let window = &rest[..MAX_CHARS];
        let cut = window
            .iter()
            .rposition(|c| matches!(c, '\n' | '。' | '.' | '!' | '?' | '！' | '？' | ' '))
            .filter(|at| *at >= MAX_CHARS / 2)
            .map_or(MAX_CHARS, |at| at + 1);
        out.push(rest[..cut].iter().collect());
        rest.drain(..cut);
    }
    // What is left over is shown by the mark the translation ends with.
    if !rest.is_empty() {
        out.push(String::new());
    }
    out
}

/// Translates `text` into `to`, in pieces when long; a text beyond the allowed pieces ends with an ellipsis.
/// # Errors
/// The service is out of reach, or its answer cannot be read.
pub async fn translate(get: &dyn Get, text: &str, to: &str) -> Result<Translation> {
    let mut chunks = chunks_of(text);
    // An empty last piece says that there was more.
    let cut = chunks.last().is_some_and(String::is_empty);
    if cut {
        chunks.pop();
    }
    let mut joined = String::new();
    let mut from = String::from("?");
    for (n, chunk) in chunks.iter().enumerate() {
        let part = parse(&get.get(&request_url(chunk, to)).await?)?;
        if n == 0 {
            from = part.from;
        }
        joined.push_str(&part.text);
    }
    if cut {
        joined.push('…');
    }
    Ok(Translation { text: joined, from })
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
    fn a_long_text_is_cut_in_pieces_at_a_line_or_sentence_end_and_the_rest_is_marked() {
        let sentence = format!("{}. ", "a".repeat(500));
        let text = sentence.repeat(6);
        let chunks = chunks_of(&text);
        assert_eq!(chunks.len(), MAX_CHUNKS + 1);
        assert!(
            chunks[..MAX_CHUNKS]
                .iter()
                .all(|c| c.chars().count() <= MAX_CHARS)
        );
        assert!(chunks[0].ends_with(". "));
        assert!(chunks[MAX_CHUNKS].is_empty());
        assert_eq!(chunks_of("short"), ["short"]);
        assert!(chunks_of("").is_empty());
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
