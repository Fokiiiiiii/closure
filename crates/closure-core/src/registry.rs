use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use twilight_model::gateway::Intents;

use crate::error::CoreError;
use crate::event::EventKind;
use crate::module::{CommandSpec, ContextCommandSpec, ContextKind, Module, ModuleId};
use crate::permission::{PermissionKey, PermissionSpec};
use crate::subscription::Subscription;

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

    /// The modules that receive `kind` with the priority and filter of their subscription,
    /// highest priority first and, among equals, in dependency order.
    #[must_use]
    pub fn subscriptions(&self, kind: EventKind) -> Vec<(&Arc<dyn Module>, Subscription)> {
        let mut found: Vec<_> = self
            .modules
            .iter()
            .filter_map(|module| Some((module, module.manifest().subscription(kind)?)))
            .collect();
        found.sort_by_key(|(_, subscription)| std::cmp::Reverse(subscription.priority));
        found
    }

    /// Every permission key a module declares.
    pub fn permissions(&self) -> impl Iterator<Item = &PermissionSpec> {
        self.modules
            .iter()
            .flat_map(|module| module.manifest().permissions.iter())
    }

    /// The declaration of the permission `key`.
    #[must_use]
    pub fn permission(&self, key: &PermissionKey) -> Option<&PermissionSpec> {
        self.permissions().find(|spec| spec.key == *key)
    }

    /// The module whose component or modal carries `custom_id`: the one named by the part
    /// before the first `:`.
    #[must_use]
    pub fn interaction_owner(&self, custom_id: &str) -> Option<&Arc<dyn Module>> {
        let (owner, _) = custom_id.split_once(':')?;
        self.get(&ModuleId::new(owner).ok()?)
    }

    /// The module that owns the command named `name`.
    #[must_use]
    pub fn command_owner(&self, name: &str) -> Option<&Arc<dyn Module>> {
        self.modules
            .iter()
            .find(|module| module.manifest().commands.iter().any(|c| c.name == name))
    }

    /// The module that owns the context-menu command named `name` for `kind`.
    #[must_use]
    pub fn context_owner(&self, name: &str, kind: ContextKind) -> Option<&Arc<dyn Module>> {
        self.modules.iter().find(|module| {
            module
                .manifest()
                .context_commands
                .iter()
                .any(|c| c.name == name && c.kind == kind)
        })
    }

    /// Every context-menu command of every registered module.
    pub fn context_commands(&self) -> impl Iterator<Item = (&ModuleId, &ContextCommandSpec)> {
        self.modules.iter().flat_map(|module| {
            let manifest = module.manifest();
            manifest
                .context_commands
                .iter()
                .map(move |command| (&manifest.id, command))
        })
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
        let option = OptionSpec::new("user", "d", OptionKind::User).required();
        let sub = crate::module::SubcommandSpec {
            name: "add".into(),
            description: "d".into(),
            options: vec![],
            subcommands: vec![],
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
    fn a_subcommand_group_holds_subcommands_only_and_there_are_two_levels() {
        let sub = |name: &str, options, subcommands| crate::module::SubcommandSpec {
            name: name.into(),
            description: "d".into(),
            options,
            subcommands,
        };
        let check = |group| {
            let manifest = ModuleManifest::new(id("a"), "a", "0.1.0").command(CommandSpec {
                name: "warn".into(),
                description: "d".into(),
                options: vec![],
                subcommands: vec![group],
                default_member_permissions: None,
            });
            ModuleRegistry::new(vec![Arc::new(Stub(manifest))])
        };
        let option = OptionSpec::new("user", "d", OptionKind::User);
        assert!(
            check(sub(
                "list",
                vec![],
                vec![sub("add", vec![option.clone()], vec![])]
            ))
            .is_ok()
        );
        assert!(
            check(sub("list", vec![option], vec![sub("add", vec![], vec![])])).is_err(),
            "a group with an option of its own"
        );
        assert!(
            check(sub(
                "list",
                vec![],
                vec![sub("add", vec![], vec![sub("deep", vec![], vec![])])]
            ))
            .is_err(),
            "a third level"
        );
    }

    #[test]
    fn context_commands_are_limited_to_five_per_kind_and_named_once() {
        use crate::module::ContextKind;
        let spec = |name: &str, kind| ContextCommandSpec {
            name: name.into(),
            kind,
        };
        let module = |commands: Vec<ContextCommandSpec>| {
            let manifest = commands.into_iter().fold(
                ModuleManifest::new(id("a"), "a", "0.1.0"),
                ModuleManifest::context_command,
            );
            ModuleRegistry::new(vec![Arc::new(Stub(manifest))])
        };
        let users: Vec<_> = (0..5)
            .map(|n| spec(&format!("user {n}"), ContextKind::User))
            .collect();
        assert!(module(users.clone()).is_ok());
        let mut six = users;
        six.push(spec("user 5", ContextKind::User));
        assert!(module(six).is_err(), "six user commands");
        assert!(
            module(vec![spec("", ContextKind::Message)]).is_err(),
            "empty name"
        );
        assert!(
            module(vec![spec(&"x".repeat(33), ContextKind::Message)]).is_err(),
            "name over 32"
        );
        assert!(
            module(vec![
                spec("report", ContextKind::Message),
                spec("report", ContextKind::Message)
            ])
            .is_err(),
            "declared twice"
        );
        assert!(
            module(vec![
                spec("report", ContextKind::User),
                spec("report", ContextKind::Message)
            ])
            .is_ok(),
            "one name for a user and a message command"
        );
    }

    #[test]
    fn required_options_must_come_first() {
        let option = |name: &str, required| {
            let option = OptionSpec::new(name, "d", OptionKind::String);
            if required { option.required() } else { option }
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
    fn option_limits_must_fit_the_kind_of_the_option() {
        use crate::module::{ChoiceValue, OptionChoice};

        let pick = |value| OptionChoice {
            name: "x".into(),
            value,
        };
        let with = |kind: OptionKind, change: &dyn Fn(&mut OptionSpec)| {
            let mut option = OptionSpec::new("value", "d", kind);
            change(&mut option);
            let manifest = ModuleManifest::new(id("a"), "a", "0.1.0").command(CommandSpec {
                name: "warn".into(),
                description: "d".into(),
                options: vec![option],
                subcommands: vec![],
                default_member_permissions: None,
            });
            ModuleRegistry::new(vec![Arc::new(Stub(manifest))]).is_ok()
        };
        // What fits.
        assert!(with(OptionKind::Integer, &|o| {
            o.min_value = Some(1.0);
            o.max_value = Some(5.0);
        }));
        assert!(with(OptionKind::String, &|o| {
            o.min_length = Some(1);
            o.max_length = Some(6000);
        }));
        assert!(with(OptionKind::String, &|o| o.choices =
            vec![pick(ChoiceValue::String("a".into()))]));
        assert!(with(OptionKind::Number, &|o| o.choices =
            vec![pick(ChoiceValue::Number(1.5))]));
        // What does not.
        assert!(!with(OptionKind::Boolean, &|o| o.choices =
            vec![pick(ChoiceValue::Integer(1))]));
        assert!(!with(OptionKind::Integer, &|o| o.choices =
            vec![pick(ChoiceValue::String("a".into()))]));
        assert!(!with(OptionKind::Integer, &|o| {
            o.choices = vec![pick(ChoiceValue::Integer(1))];
            o.min_value = Some(0.0);
        }));
        assert!(!with(OptionKind::String, &|o| o.min_value = Some(1.0)));
        assert!(!with(OptionKind::Integer, &|o| o.max_length = Some(5)));
        assert!(!with(OptionKind::Integer, &|o| {
            o.min_value = Some(5.0);
            o.max_value = Some(1.0);
        }));
        assert!(!with(OptionKind::String, &|o| {
            o.min_length = Some(9);
            o.max_length = Some(2);
        }));
        assert!(!with(OptionKind::String, &|o| o.max_length = Some(6001)));
        assert!(!with(OptionKind::String, &|o| o.max_length = Some(0)));
        assert!(!with(OptionKind::String, &|o| {
            o.choices = (0..26)
                .map(|n| pick(ChoiceValue::String(n.to_string())))
                .collect();
        }));
        assert!(!with(OptionKind::String, &|o| o.choices =
            vec![pick(ChoiceValue::String(String::new()))]));
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

    fn with_permissions(name: &str, keys: &[&str]) -> Arc<dyn Module> {
        let mut manifest = ModuleManifest::new(id(name), name, "0.1.0");
        for key in keys {
            manifest = manifest.permission(
                PermissionSpec::new(*key, crate::permission::Level::MODERATOR, "d").unwrap(),
            );
        }
        Arc::new(Stub(manifest))
    }

    #[test]
    fn permission_keys_belong_to_the_module_that_declares_them() {
        let registry = ModuleRegistry::new(vec![
            with_permissions("warn", &["warn.add", "warn.clear"]),
            with_permissions("mute", &["mute.add"]),
        ])
        .unwrap();
        assert_eq!(registry.permissions().count(), 3);
        let add = PermissionKey::new("mute.add").unwrap();
        assert_eq!(
            registry.permission(&add).map(|spec| spec.key.as_str()),
            Some("mute.add")
        );
        assert!(
            registry
                .permission(&PermissionKey::new("mute.clear").unwrap())
                .is_none()
        );

        // Another module's namespace, and a key twice.
        assert!(matches!(
            ModuleRegistry::new(vec![with_permissions("warn", &["mute.add"])]),
            Err(CoreError::PermissionOutsideModule { .. })
        ));
        assert!(matches!(
            ModuleRegistry::new(vec![with_permissions("warn", &["warn.add", "warn.add"])]),
            Err(CoreError::DuplicatePermission { .. })
        ));
    }

    #[test]
    fn a_custom_id_belongs_to_the_module_it_starts_with() {
        let registry = ModuleRegistry::new(vec![stub("warn", &[]), stub("mute", &[])]).unwrap();
        let owner = |custom_id: &str| {
            registry
                .interaction_owner(custom_id)
                .map(|module| module.manifest().id.to_string())
        };
        assert_eq!(owner("warn:confirm:*:42").as_deref(), Some("warn"));
        assert_eq!(owner("mute:x").as_deref(), Some("mute"));
        for unowned in ["nobody:x", "warn", "", ":x", "Warn:x"] {
            assert_eq!(owner(unowned), None, "{unowned:?}");
        }
    }

    #[test]
    fn subscriptions_come_highest_priority_first_and_are_checked() {
        let named = |name: &str, priority: Option<i16>| {
            let mut manifest =
                ModuleManifest::new(id(name), name, "0.1.0").events([EventKind::MessageCreated]);
            if let Some(priority) = priority {
                manifest = manifest
                    .subscribe(Subscription::new(EventKind::MessageCreated).priority(priority));
            }
            Arc::new(Stub(manifest)) as Arc<dyn Module>
        };
        let registry = ModuleRegistry::new(vec![
            named("plain", None),
            named("low", Some(-5)),
            named("high", Some(10)),
            named("also-plain", None),
        ])
        .unwrap();
        let order: Vec<String> = registry
            .subscriptions(EventKind::MessageCreated)
            .into_iter()
            .map(|(module, _)| module.manifest().id.to_string())
            .collect();
        // Equal priorities keep the order of the registry.
        assert_eq!(order, ["high", "also-plain", "plain", "low"]);
        assert!(registry.subscriptions(EventKind::BanAdded).is_empty());

        // A subscription for a kind the module does not take, twice, or with a huge filter.
        let mut outside = ModuleManifest::new(id("a"), "a", "0.1.0");
        outside
            .subscriptions
            .push(Subscription::new(EventKind::BanAdded));
        let twice = ModuleManifest::new(id("a"), "a", "0.1.0")
            .subscribe(Subscription::new(EventKind::BanAdded))
            .subscribe(Subscription::new(EventKind::BanAdded).priority(1));
        let huge = ModuleManifest::new(id("a"), "a", "0.1.0").subscribe(
            Subscription::new(EventKind::BanAdded)
                .in_channels((1..=101).map(crate::event::ChannelId::new)),
        );
        for manifest in [outside, twice, huge] {
            assert!(matches!(
                ModuleRegistry::new(vec![Arc::new(Stub(manifest))]),
                Err(CoreError::InvalidSubscription { .. })
            ));
        }
    }
}
