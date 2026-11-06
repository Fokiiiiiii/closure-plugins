//! Every plugin of this repository, as the list closure adds to its own modules.
//!
//! closure imports this crate and nothing else (see the README): a new plugin is a crate of this
//! repository and one line here.

use std::sync::Arc;

use anyhow::Result;
use closure_core::module::Module;

/// The modules of every plugin.
///
/// # Errors
/// A module declaration is refused by the SDK.
pub fn modules() -> Result<Vec<Arc<dyn Module>>> {
    Ok(vec![
        Arc::new(plugin_linkexpand::module()?),
        Arc::new(plugin_starboard::module()?),
    ])
}
