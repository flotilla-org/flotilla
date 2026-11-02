//! The surfaces connected to this daemon, as each declared itself.

use std::{collections::HashMap, sync::Mutex};

use flotilla_protocol::{PrincipalRef, SurfaceCharacter, SurfaceDeclaration};
use uuid::Uuid;

#[derive(Default)]
pub struct Surfaces {
    declared: Mutex<HashMap<Uuid, SurfaceDeclaration>>,
}

impl Surfaces {
    pub fn connect(&self, surface_id: Uuid, declaration: SurfaceDeclaration) {
        self.declared.lock().expect("surfaces lock poisoned").insert(surface_id, declaration);
    }

    pub fn disconnect(&self, surface_id: Uuid) {
        self.declared.lock().expect("surfaces lock poisoned").remove(&surface_id);
    }

    pub fn principal(&self, surface_id: Uuid) -> Result<Option<PrincipalRef>, String> {
        Ok(self
            .declared
            .lock()
            .map_err(|_| "surfaces lock poisoned".to_string())?
            .get(&surface_id)
            .map(|declaration| declaration.principal_ref.clone()))
    }

    /// Whether a headless presentation connector is currently projecting
    /// daemon state into a presentation manager.
    pub fn has_ambient(&self) -> bool {
        self.declared.lock().expect("surfaces lock poisoned").values().any(|declaration| declaration.character == SurfaceCharacter::Ambient)
    }
}
