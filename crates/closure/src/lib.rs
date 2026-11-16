//! The modules this build ships; the bot and developer tooling (`closure-xtask`) both read this list, so a module is registered in one place only.

use std::sync::Arc;

use anyhow::Result;
use closure_core::module::Module;
use closure_core::operator_settings::OperatorSettings;

pub mod message_history;

#[cfg(test)]
#[path = "message_files_tests.rs"]
mod message_files_tests;

/// Every module this build ships, with the operator's settings for the plugins.
/// Adding a module means adding it here.
#[cfg_attr(
    not(feature = "plugins"),
    allow(unused_variables, reason = "only the plugins read the settings")
)]
pub fn modules_with(settings: &OperatorSettings) -> Result<Vec<Arc<dyn Module>>> {
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
        Arc::new(closure_mod_guard::module()?),
        Arc::new(closure_mod_honeypot::module()?),
        Arc::new(closure_mod_dehoist::module()?),
        Arc::new(closure_mod_follow::module()?),
        Arc::new(closure_mod_backup::module()?),
        Arc::new(closure_mod_sticky::module()?),
        Arc::new(closure_mod_post::module()?),
        Arc::new(closure_mod_phishing::module(Arc::new(
            closure_discord::AntiFish::new(),
        ))?),
        Arc::new(closure_mod_utility::module()?),
        Arc::new(closure_mod_companion::module()?),
        Arc::new(closure_mod_persist::module()?),
    ];
    #[cfg(feature = "plugins")]
    modules.extend(closure_plugins::modules_with(settings)?);
    Ok(modules)
}

/// Every module this build ships, with no operator settings.
pub fn modules() -> Result<Vec<Arc<dyn Module>>> {
    modules_with(&OperatorSettings::default())
}
