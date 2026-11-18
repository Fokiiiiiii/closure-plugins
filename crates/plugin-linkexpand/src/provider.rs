//! What a site must say for its posts to be expanded: where its links are, how its service answers for
//! a post, and which of its files may be fetched.

use anyhow::Result;

use crate::Config;
use crate::links::PostLink;
use crate::model::Post;

/// A site whose posts are expanded. A site is added by implementing this, listing it among the
/// default providers, and giving the server a setting `<site>_links`.
pub(crate) trait Provider: Send + Sync {
    /// Whether the server switches the site on.
    fn enabled(&self, config: &Config) -> bool;

    /// The posts linked in `text`, at most `MAX_PER_MESSAGE` of them.
    fn detect(&self, text: &str) -> Vec<PostLink>;

    /// The address the service answers for the post the link names.
    fn api_url(&self, link: &PostLink) -> String;

    /// The address the post is shown at, when the service's answer does not name it. The author is
    /// shown at the same address.
    fn post_url(&self, _link: &PostLink) -> Option<String> {
        None
    }

    /// Whether the translate button is shown under the posts of the site: the button reads a post by
    /// its X id.
    fn translatable(&self) -> bool {
        true
    }

    /// The post in the service's answer.
    ///
    /// # Errors
    /// The answer is not JSON, or has no post in it.
    fn parse(&self, bytes: &[u8]) -> Result<Post>;

    /// The address the pictures of the post `id` are listed at, when the service does not give them
    /// in the post.
    fn pages_url(&self, _id: u64) -> Option<String> {
        None
    }

    /// The pictures of the post, from the answer to [`Provider::pages_url`].
    ///
    /// # Errors
    /// The answer is not what the service gives for the pictures.
    fn parse_pages(&self, _bytes: &[u8]) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// The `Referer` the file server of `url` needs to give the file, when it needs one.
    fn media_referer(&self, _url: &str) -> Option<&'static str> {
        None
    }

    /// The headers the request for `url` is sent with, when the site needs the operator's session
    /// for it. Only the addresses the session belongs to get one.
    fn request_headers(&self, _url: &str) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// The hosts of the site's pictures and video: the only files that are fetched or linked.
    fn media_hosts(&self) -> &'static [&'static str];

    /// Whether `url` is one of the site's media files.
    fn is_media(&self, url: &str) -> bool {
        is_at_hosts(url, self.media_hosts())
    }
}

/// Whether `url` is an https address at one of `hosts`, or below one of them. A user name in the
/// address, a port, or another host that only ends the same way is not.
#[must_use]
pub fn is_at_hosts(url: &str, hosts: &[&str]) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains(['@', '\\', ':']) {
        return false;
    }
    let host = authority.to_ascii_lowercase();
    hosts.iter().any(|&allowed| {
        host == allowed
            || host
                .strip_suffix(allowed)
                .and_then(|rest| rest.strip_suffix('.'))
                .is_some_and(|name| !name.is_empty())
    })
}
