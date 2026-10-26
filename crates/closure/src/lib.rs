//! The modules this build ships. The bot and the developer tooling (`closure-xtask`) both read
//! this list, so a module is registered in one place only.

use std::sync::Arc;

use anyhow::Result;
use closure_core::module::Module;
use closure_mod_diagnostics::DiagnosticsModule;

/// Every module this build ships. Adding a module means adding it here.
pub fn modules() -> Result<Vec<Arc<dyn Module>>> {
    Ok(vec![
        Arc::new(DiagnosticsModule::new()?),
        Arc::new(closure_mod_moderation::module()?),
        Arc::new(closure_mod_automod::module()?),
    ])
}
