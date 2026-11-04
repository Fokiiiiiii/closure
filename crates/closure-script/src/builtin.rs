use std::sync::Arc;

use closure_core::module::ModuleRuntime;

use crate::{RuntimeFactory, runtime::ScriptRuntime};

/// Runtimes enabled in this build.
#[derive(Debug)]
pub struct BuiltinRuntimes;

impl RuntimeFactory for BuiltinRuntimes {
    fn create(
        &self,
        kind: ModuleRuntime,
        entry: &[u8],
    ) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
        match kind {
            #[cfg(feature = "wasm")]
            ModuleRuntime::Wasm => Ok(Some(Arc::new(crate::wasm::WasmRuntime::load(entry)?))),
            #[cfg(feature = "lua")]
            ModuleRuntime::Lua => Ok(Some(Arc::new(crate::LuaRuntime::new(entry)?))),
            _ => {
                let _ = entry;
                Ok(None)
            }
        }
    }
}
