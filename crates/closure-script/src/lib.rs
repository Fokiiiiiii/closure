//! Runtime-independent limits and owner grants for script modules.

use std::collections::BTreeMap;
use std::time::Duration;

use closure_core::capability::{Capability, CapabilitySet};
use closure_core::module::ModuleId;
use serde::Deserialize;

mod builtin;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
mod loader;
mod module;
pub mod runtime;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use builtin::BuiltinRuntimes;
pub use loader::{LoadError, Loaded, RuntimeFactory, ScriptSpec, load_dir, load_grants};
pub use module::ScriptModule;

#[cfg(test)]
mod module_tests;

#[cfg(test)]
mod loader_tests;

/// Host ceilings for one script invocation and its dedicated workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub cpu_ms: u64,
    pub wall_ms: u64,
    pub memory_bytes: usize,
    pub workers: usize,
    pub actions: usize,
    pub output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cpu_ms: 50,
            wall_ms: 250,
            memory_bytes: 32 * 1024 * 1024,
            workers: 4,
            actions: 25,
            output_bytes: 256 * 1024,
        }
    }
}

impl Limits {
    /// Reject zero and clamp requested values to the host's ceilings.
    pub fn bounded(self, ceiling: Self) -> Result<Self, &'static str> {
        if self.cpu_ms == 0
            || self.wall_ms == 0
            || self.memory_bytes == 0
            || self.workers == 0
            || self.actions == 0
            || self.output_bytes == 0
        {
            return Err("script limits must be positive");
        }
        Ok(Self {
            cpu_ms: self.cpu_ms.min(ceiling.cpu_ms),
            wall_ms: self.wall_ms.min(ceiling.wall_ms),
            memory_bytes: self.memory_bytes.min(ceiling.memory_bytes),
            workers: self.workers.min(ceiling.workers),
            actions: self.actions.min(ceiling.actions),
            output_bytes: self.output_bytes.min(ceiling.output_bytes),
        })
    }

    #[must_use]
    pub fn wall_time(self) -> Duration {
        Duration::from_millis(self.wall_ms)
    }
}

/// Owner-controlled grants, keyed by validated module ID.
pub type Grants = BTreeMap<ModuleId, Vec<Capability>>;

pub fn parse_grants(yaml: &str) -> Result<Grants, serde_saphyr::Error> {
    serde_saphyr::from_str(yaml)
}

/// A script receives only capabilities both requested and granted by the owner.
#[must_use]
pub fn effective_capabilities(
    requested: &CapabilitySet,
    grants: &Grants,
    id: &ModuleId,
) -> CapabilitySet {
    requested
        .iter()
        .filter(|cap| grants.get(id).is_some_and(|allowed| allowed.contains(cap)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_only_authorize_the_intersection() {
        let id = ModuleId::new("example").unwrap();
        let grants = parse_grants("example: [storage, ban_members]\n").unwrap();
        let requested = [Capability::Storage, Capability::SendMessages]
            .into_iter()
            .collect();
        let result = effective_capabilities(&requested, &grants, &id);
        assert_eq!(result.iter().collect::<Vec<_>>(), vec![Capability::Storage]);
        assert!(
            effective_capabilities(&requested, &Grants::new(), &id)
                .iter()
                .next()
                .is_none()
        );
        assert!(parse_grants("INVALID: [storage]").is_err());
        assert!(parse_grants("example: [not_a_capability]").is_err());
    }

    #[test]
    fn requested_limits_cannot_exceed_the_ceiling() {
        let ceiling = Limits::default();
        let requested = Limits {
            cpu_ms: 999,
            wall_ms: 5,
            workers: 99,
            ..ceiling
        };
        let actual = requested.bounded(ceiling).unwrap();
        assert_eq!(actual.cpu_ms, 50);
        assert_eq!(actual.wall_ms, 5);
        assert_eq!(actual.workers, 4);
        assert!(
            Limits {
                workers: 0,
                ..ceiling
            }
            .bounded(ceiling)
            .is_err()
        );
        assert!(serde_saphyr::from_str::<Limits>("unknown: 1").is_err());
    }
}
