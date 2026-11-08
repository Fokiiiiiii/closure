//! The modules this build ships. The bot and the developer tooling (`closure-xtask`) both read
//! this list, so a module is registered in one place only.

use std::sync::Arc;

use anyhow::Result;
use closure_core::module::Module;

pub mod message_history;

#[cfg(test)]
#[path = "message_files_tests.rs"]
mod message_files_tests;

/// Every module this build ships. Adding a module means adding it here.
pub fn modules() -> Result<Vec<Arc<dyn Module>>> {
    #[allow(unused_mut, reason = "the plugins feature adds to it")]
    let mut modules: Vec<Arc<dyn Module>> = vec![
        Arc::new(closure_mod_dashboard::module()?),
        Arc::new(closure_mod_diagnostics::module()?),
        Arc::new(closure_mod_logs::module()?),
        Arc::new(closure_mod_moderation::module()?),
        Arc::new(closure_mod_tags::module()?),
        Arc::new(closure_mod_automod::module()?),
        Arc::new(closure_mod_channels::module()?),
        Arc::new(closure_mod_welcome::module()?),
        Arc::new(closure_mod_remind::module()?),
        Arc::new(closure_mod_roles::module()?),
        Arc::new(closure_mod_verify::module()?),
        Arc::new(closure_mod_post::module()?),
        Arc::new(closure_mod_phishing::module(Arc::new(
            closure_discord::AntiFish::new(),
        ))?),
        Arc::new(closure_mod_utility::module()?),
        Arc::new(closure_mod_companion::module()?),
        Arc::new(closure_mod_persist::module()?),
    ];
    #[cfg(feature = "plugins")]
    modules.extend(closure_plugins::modules()?);
    Ok(modules)
}
