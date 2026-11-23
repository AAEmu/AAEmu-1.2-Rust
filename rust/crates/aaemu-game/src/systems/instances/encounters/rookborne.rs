//! Rookborne's Amed cannon.

// ---- keys

const CONTROLLER_ID: u32 = 6678;

// ---- tables

pub(super) fn is_amed_cannon_controller(controller_id: u32) -> bool {
    controller_id == CONTROLLER_ID
}

// ---- hooks

pub(super) struct Rookborne;

impl super::hooks::Encounter for Rookborne {}
