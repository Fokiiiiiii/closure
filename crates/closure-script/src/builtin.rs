use std::sync::Arc;

use closure_core::module::ModuleRuntime;

use crate::{RuntimeFactory, runtime::ScriptRuntime};

#[derive(Debug)]
pub struct BuiltinRuntimes;

impl RuntimeFactory for BuiltinRuntimes {
    fn create(
        &self,
        kind: ModuleRuntime,
        entry: &[u8],
    ) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
        #[cfg(feature = "wasm")]
        if kind == ModuleRuntime::Wasm {
            return crate::wasm::WasmRuntime::load(entry)
                .map(|runtime| Some(Arc::new(runtime) as Arc<dyn ScriptRuntime>));
        }
        let _ = (kind, entry);
        Ok(None)
    }
}
