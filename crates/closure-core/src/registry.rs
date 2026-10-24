use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use twilight_model::gateway::Intents;

use crate::error::CoreError;
use crate::event::EventKind;
use crate::module::{CommandSpec, Module, ModuleId};

/// The validated, dependency-ordered set of modules the bot runs.
///
/// Construction fails on any inconsistency (duplicate ids, unknown or cyclic dependencies,
/// colliding or malformed commands) so a broken module set stops the process at startup.
pub struct ModuleRegistry {
    modules: Vec<Arc<dyn Module>>,
    index: HashMap<ModuleId, usize>,
}

impl std::fmt::Debug for ModuleRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.modules.iter().map(|module| &module.manifest().id))
            .finish()
    }
}

impl ModuleRegistry {
    pub fn new(modules: Vec<Arc<dyn Module>>) -> Result<Self, CoreError> {
        let mut by_id: BTreeMap<ModuleId, Arc<dyn Module>> = BTreeMap::new();
        for module in modules {
            let manifest = module.manifest();
            manifest.validate()?;
            let id = manifest.id.clone();
            if by_id.insert(id.clone(), module).is_some() {
                return Err(CoreError::DuplicateModule(id));
            }
        }

        for module in by_id.values() {
            let manifest = module.manifest();
            for dependency in &manifest.dependencies {
                if !by_id.contains_key(dependency) {
                    return Err(CoreError::UnknownDependency {
                        module: manifest.id.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
        }

        let ordered = topological_order(&by_id)?;
        let modules: Vec<Arc<dyn Module>> = ordered
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect();
        let index = modules
            .iter()
            .enumerate()
            .map(|(position, module)| (module.manifest().id.clone(), position))
            .collect();

        let registry = Self { modules, index };
        registry.check_command_collisions()?;
        Ok(registry)
    }

    fn check_command_collisions(&self) -> Result<(), CoreError> {
        let mut owners: HashMap<&str, &ModuleId> = HashMap::new();
        for module in &self.modules {
            let manifest = module.manifest();
            for command in &manifest.commands {
                if let Some(first) = owners.insert(command.name.as_str(), &manifest.id) {
                    return Err(CoreError::CommandCollision {
                        command: command.name.clone(),
                        first: first.clone(),
                        second: manifest.id.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Modules in dependency order: a module always comes after the modules it depends on.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Module>> {
        self.modules.iter()
    }

    #[must_use]
    pub fn get(&self, id: &ModuleId) -> Option<&Arc<dyn Module>> {
        self.index.get(id).map(|&position| &self.modules[position])
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.modules.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Modules subscribed to `kind`, in dependency order.
    pub fn subscribers(&self, kind: EventKind) -> impl Iterator<Item = &Arc<dyn Module>> {
        self.modules
            .iter()
            .filter(move |module| module.manifest().events.contains(&kind))
    }

    /// The union of the intents of the given modules.
    pub fn intents_of<'a>(&self, enabled: impl IntoIterator<Item = &'a ModuleId>) -> Intents {
        enabled
            .into_iter()
            .filter_map(|id| self.get(id))
            .fold(Intents::empty(), |acc, module| {
                acc | module.manifest().intents
            })
    }

    /// The union of the intents of every registered module.
    #[must_use]
    pub fn all_intents(&self) -> Intents {
        self.modules.iter().fold(Intents::empty(), |acc, module| {
            acc | module.manifest().intents
        })
    }

    /// Every command with the module that owns it.
    pub fn commands(&self) -> impl Iterator<Item = (&ModuleId, &CommandSpec)> {
        self.modules.iter().flat_map(|module| {
            let manifest = module.manifest();
            manifest
                .commands
                .iter()
                .map(move |command| (&manifest.id, command))
        })
    }

    /// The module that owns the command named `name`.
    #[must_use]
    pub fn command_owner(&self, name: &str) -> Option<&Arc<dyn Module>> {
        self.modules
            .iter()
            .find(|module| module.manifest().commands.iter().any(|c| c.name == name))
    }
}

/// Depth-first topological sort with deterministic order (ids sorted) and cycle detection.
fn topological_order(
    modules: &BTreeMap<ModuleId, Arc<dyn Module>>,
) -> Result<Vec<ModuleId>, CoreError> {
    enum Mark {
        Visiting,
        Done,
    }

    fn visit(
        id: &ModuleId,
        modules: &BTreeMap<ModuleId, Arc<dyn Module>>,
        marks: &mut HashMap<ModuleId, Mark>,
        order: &mut Vec<ModuleId>,
    ) -> Result<(), CoreError> {
        match marks.get(id) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => return Err(CoreError::DependencyCycle(id.clone())),
            None => {}
        }
        marks.insert(id.clone(), Mark::Visiting);
        if let Some(module) = modules.get(id) {
            let mut dependencies: Vec<&ModuleId> = module.manifest().dependencies.iter().collect();
            dependencies.sort();
            dependencies.dedup();
            for dependency in dependencies {
                visit(dependency, modules, marks, order)?;
            }
        }
        marks.insert(id.clone(), Mark::Done);
        order.push(id.clone());
        Ok(())
    }

    let mut marks = HashMap::new();
    let mut order = Vec::with_capacity(modules.len());
    let mut seen = HashSet::new();
    for id in modules.keys() {
        if seen.insert(id) {
            visit(id, modules, &mut marks, &mut order)?;
        }
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::error::ModuleError;
    use crate::event::{Event, EventKinds};
    use crate::module::{EventContext, ModuleManifest, OptionKind, OptionSpec};

    struct Stub(ModuleManifest);

    #[async_trait]
    impl Module for Stub {
        fn manifest(&self) -> &ModuleManifest {
            &self.0
        }

        async fn on_event(&self, _: &EventContext, _: &Event) -> Result<(), ModuleError> {
            Ok(())
        }
    }

    fn id(name: &str) -> ModuleId {
        ModuleId::new(name).unwrap()
    }

    fn stub(name: &str, deps: &[&str]) -> Arc<dyn Module> {
        let mut manifest = ModuleManifest::new(id(name), name, "0.1.0");
        manifest.dependencies = deps.iter().map(|d| id(d)).collect();
        Arc::new(Stub(manifest))
    }

    fn with_command(name: &str, command: &str) -> Arc<dyn Module> {
        let manifest = ModuleManifest::new(id(name), name, "0.1.0").command(CommandSpec {
            name: command.into(),
            description: "d".into(),
            options: vec![],
            subcommands: vec![],
            default_member_permissions: None,
        });
        Arc::new(Stub(manifest))
    }

    fn order(registry: &ModuleRegistry) -> Vec<String> {
        registry
            .iter()
            .map(|m| m.manifest().id.to_string())
            .collect()
    }

    #[test]
    fn module_ids_are_validated() {
        for bad in ["", "A", "1abc", "has space", "ünï", &"a".repeat(33)] {
            assert!(ModuleId::new(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["a", "mod-log", "auto_mod2"] {
            assert!(ModuleId::new(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn dependencies_start_first() {
        let registry =
            ModuleRegistry::new(vec![stub("c", &["b"]), stub("b", &["a"]), stub("a", &[])])
                .unwrap();
        assert_eq!(order(&registry), ["a", "b", "c"]);
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let result = ModuleRegistry::new(vec![stub("a", &[]), stub("a", &[])]);
        assert_eq!(result.unwrap_err(), CoreError::DuplicateModule(id("a")));
    }

    #[test]
    fn unknown_dependency_is_rejected() {
        let result = ModuleRegistry::new(vec![stub("a", &["ghost"])]);
        assert_eq!(
            result.unwrap_err(),
            CoreError::UnknownDependency {
                module: id("a"),
                dependency: id("ghost")
            }
        );
    }

    #[test]
    fn cycles_are_rejected() {
        let result = ModuleRegistry::new(vec![stub("a", &["b"]), stub("b", &["a"])]);
        assert!(matches!(result.unwrap_err(), CoreError::DependencyCycle(_)));
        let selfish = ModuleRegistry::new(vec![stub("a", &["a"])]);
        assert!(matches!(
            selfish.unwrap_err(),
            CoreError::DependencyCycle(_)
        ));
    }

    #[test]
    fn command_names_must_be_unique_across_modules() {
        let result = ModuleRegistry::new(vec![with_command("a", "ban"), with_command("b", "ban")]);
        assert!(matches!(
            result.unwrap_err(),
            CoreError::CommandCollision { .. }
        ));
    }

    #[test]
    fn malformed_commands_are_rejected() {
        let result = ModuleRegistry::new(vec![with_command("a", "Bad Name")]);
        assert!(matches!(
            result.unwrap_err(),
            CoreError::InvalidCommand { .. }
        ));
    }

    #[test]
    fn a_command_cannot_mix_options_and_subcommands() {
        let option = OptionSpec {
            name: "user".into(),
            description: "d".into(),
            kind: OptionKind::User,
            required: true,
        };
        let sub = crate::module::SubcommandSpec {
            name: "add".into(),
            description: "d".into(),
            options: vec![],
        };
        let manifest = ModuleManifest::new(id("a"), "a", "0.1.0").command(CommandSpec {
            name: "warn".into(),
            description: "d".into(),
            options: vec![option],
            subcommands: vec![sub],
            default_member_permissions: None,
        });
        let result = ModuleRegistry::new(vec![Arc::new(Stub(manifest))]);
        assert!(matches!(
            result.unwrap_err(),
            CoreError::InvalidCommand { .. }
        ));
    }

    #[test]
    fn required_options_must_come_first() {
        let option = |name: &str, required| OptionSpec {
            name: name.into(),
            description: "d".into(),
            kind: OptionKind::String,
            required,
        };
        let command = |options| CommandSpec {
            name: "warn".into(),
            description: "d".into(),
            options,
            subcommands: vec![],
            default_member_permissions: None,
        };
        let registry = |options| {
            let manifest = ModuleManifest::new(id("a"), "a", "0.1.0").command(command(options));
            ModuleRegistry::new(vec![Arc::new(Stub(manifest))])
        };
        assert!(registry(vec![option("a", true), option("b", false)]).is_ok());
        assert!(registry(vec![option("a", false), option("b", true)]).is_err());
    }

    #[test]
    fn subscribers_intents_and_command_owner_follow_manifests() {
        let mut manifest = ModuleManifest::new(id("a"), "a", "0.1.0")
            .intents(Intents::GUILD_MESSAGES)
            .events([EventKind::MessageCreated]);
        manifest.events.insert(EventKind::MessageDeleted);
        let a: Arc<dyn Module> = Arc::new(Stub(manifest));
        let b = with_command("b", "ping");
        let registry = ModuleRegistry::new(vec![a, b]).unwrap();

        assert_eq!(registry.subscribers(EventKind::MessageCreated).count(), 1);
        assert_eq!(registry.subscribers(EventKind::BanAdded).count(), 0);
        assert_eq!(registry.all_intents(), Intents::GUILD_MESSAGES);
        assert_eq!(registry.intents_of([&id("b")]), Intents::empty());
        assert_eq!(registry.intents_of([&id("a")]), Intents::GUILD_MESSAGES);
        assert_eq!(
            registry
                .command_owner("ping")
                .map(|m| m.manifest().id.to_string()),
            Some("b".to_string())
        );
        let _ = EventKinds::new();
    }
}
