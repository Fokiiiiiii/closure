//! Dashboard permissions.

use closure_sdk::{CoreError, Level, Locale, ModuleBuilder, PermissionSpec, SdkModule};

pub fn module() -> Result<SdkModule, CoreError> {
    ModuleBuilder::new("dashboard", "Dashboard", env!("CARGO_PKG_VERSION"))?
        .permission(PermissionSpec::new(
            "dashboard.edit",
            Level::ADMIN,
            "permission-dashboard-edit",
        )?)
        .catalog(Locale::En, include_str!("../locales/en.ftl"))
        .catalog(Locale::Ja, include_str!("../locales/ja.ftl"))
        .enabled_by_default()
        .build()
}

#[cfg(test)]
mod tests {
    use super::module;
    use closure_core::module::Module;
    use closure_sdk::Level;

    #[test]
    fn dashboard_declares_edit_without_commands() {
        let module = module().unwrap();
        let manifest = module.manifest();
        assert!(manifest.commands.is_empty());
        assert_eq!(manifest.permissions.len(), 1);
        assert_eq!(manifest.permissions[0].key.as_str(), "dashboard.edit");
        assert_eq!(manifest.permissions[0].default_level, Level::ADMIN);
    }
}
