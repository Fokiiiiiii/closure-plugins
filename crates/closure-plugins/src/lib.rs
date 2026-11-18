//! Every plugin of this repository, as the list closure adds to its own modules.
//!
//! closure imports this crate and nothing else (see the README): a new plugin is a crate of this
//! repository and one line here.

use std::sync::Arc;

use anyhow::Result;
use closure_core::module::Module;
use closure_core::operator_settings::OperatorSettings;

/// # Errors
/// A module declaration is refused by the SDK.
pub fn modules() -> Result<Vec<Arc<dyn Module>>> {
    modules_with(&OperatorSettings::default())
}

/// The same list, for a host that hands the operator's settings to the plugins. The translating
/// plugins share the translator the settings ask for, and Pixiv reads adult artworks with the
/// operator's session when the settings give one.
///
/// # Errors
/// A module declaration is refused by the SDK.
pub fn modules_with(settings: &OperatorSettings) -> Result<Vec<Arc<dyn Module>>> {
    let translator = Arc::new(plugin_kit::translator::from_settings(settings));
    let linkexpand = linkexpand_settings(settings);
    let pixiv_session = settings
        .secret("pixiv_session")
        .map(plugin_kit::translator::ApiKey::from_secret);
    Ok(vec![
        Arc::new(plugin_linkexpand::module_with(
            Arc::clone(&translator),
            pixiv_session,
            &linkexpand.proxies,
        )?),
        Arc::new(plugin_starboard::module()?),
        Arc::new(plugin_leveling::module()?),
        Arc::new(plugin_translate::module_with(translator)?),
        Arc::new(plugin_giveaway::module()?),
        Arc::new(plugin_tickets::module()?),
        Arc::new(plugin_feeds::module_with(feed_settings(settings))?),
    ])
}

/// The linkexpand section of the operator's file. A section that does not fit is logged without its
/// values, and the defaults are used.
fn linkexpand_settings(settings: &OperatorSettings) -> plugin_linkexpand::LinkexpandSettings {
    settings
        .plugin_section::<plugin_linkexpand::LinkexpandSettings>("linkexpand")
        .unwrap_or_else(|_| {
            tracing::warn!("the linkexpand settings do not fit; using the defaults");
            None
        })
        .unwrap_or_default()
}

/// The feeds section of the operator's file. A section that does not fit is logged without its
/// values, and the defaults are used.
fn feed_settings(settings: &OperatorSettings) -> plugin_feeds::FeedSettings {
    settings
        .plugin_section::<plugin_feeds::FeedSettings>("feeds")
        .unwrap_or_else(|_| {
            tracing::warn!("the feeds settings do not fit; using the defaults");
            None
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use closure_core::operator_settings::OperatorSettings;

    use super::*;

    fn settings(label: &str, yaml: &str) -> OperatorSettings {
        let path = std::env::temp_dir().join(format!(
            "closure-plugins-{label}-{}.yaml",
            std::process::id()
        ));
        std::fs::write(&path, yaml).unwrap();
        let loaded = OperatorSettings::load(&path);
        std::fs::remove_file(&path).unwrap();
        loaded.unwrap()
    }

    #[test]
    fn no_feeds_section_means_the_default_interval() {
        assert_eq!(
            feed_settings(&settings("nofeeds", "")),
            plugin_feeds::FeedSettings::default()
        );
    }

    #[test]
    fn the_feeds_interval_is_read_from_the_yaml_file() {
        let yaml = "plugins:\n  feeds:\n    interval_secs: 120\n";
        assert_eq!(
            feed_settings(&settings("interval", yaml)).interval_secs,
            120
        );
    }

    #[test]
    fn a_feeds_section_that_does_not_fit_falls_back_to_the_defaults() {
        for yaml in [
            "plugins:\n  feeds:\n    interval_secs: soon\n",
            "plugins:\n  feeds:\n    intervl_secs: 120\n",
        ] {
            assert_eq!(
                feed_settings(&settings("misfit", yaml)),
                plugin_feeds::FeedSettings::default()
            );
        }
    }

    #[test]
    fn the_rsshub_address_is_read_from_the_yaml_file() {
        let yaml = "plugins:\n  feeds:\n    rsshub_base_url: https://rsshub.example.org\n";
        assert_eq!(
            feed_settings(&settings("rsshub", yaml))
                .rsshub_base_url
                .as_deref(),
            Some("https://rsshub.example.org")
        );
    }

    #[test]
    fn the_linkexpand_proxies_are_read_from_the_yaml_file() {
        let yaml =
            "plugins:\n  linkexpand:\n    proxies:\n      instagram: ddinstagram.example.org\n";
        let proxies = linkexpand_settings(&settings("proxies", yaml)).proxies;
        assert_eq!(
            proxies.get("instagram").map(String::as_str),
            Some("ddinstagram.example.org")
        );
        assert!(!proxies.contains_key("reddit"));
    }

    #[test]
    fn a_linkexpand_section_that_does_not_fit_falls_back_to_the_defaults() {
        let yaml = "plugins:\n  linkexpand:\n    proxies: nope\n";
        assert!(
            linkexpand_settings(&settings("linkmisfit", yaml))
                .proxies
                .is_empty()
        );
    }

    #[test]
    fn the_list_includes_the_feeds_module() {
        assert_eq!(modules().unwrap().len(), 7);
    }
}
