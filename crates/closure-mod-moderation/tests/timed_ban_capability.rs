#![allow(clippy::unwrap_used)]

use closure_core::module::Module;
use closure_sdk::Capability;

#[test]
fn timed_ban_recovery_can_read_bans() {
    let module = closure_mod_moderation::module().unwrap();
    assert!(
        module
            .manifest()
            .capabilities
            .contains(Capability::ReadBans)
    );
}
