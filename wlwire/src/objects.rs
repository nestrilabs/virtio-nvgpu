//! Which object id is which interface, per connection, mirrored from both
//! directions of traffic the way libwayland keeps it on each end.
//!
//! Ids pass through the proxy unchanged (one host connection per guest client),
//! so this table never allocates anything: it only has to know, for every
//! message it sees, what the target is -- for the signature, and so for how
//! many descriptors the message consumes. The subtle part is when an entry may
//! go away:
//!
//! - A client-created object destroyed by a destructor request (or event) stays
//!   as a *zombie* until the server's `wl_display.delete_id` for it. Events the
//!   server sent before it saw the destructor still arrive and must still be
//!   parsed -- one of them may carry a descriptor -- and the client may not
//!   reuse the id until `delete_id` anyway (libwayland `proxy_destroy`).
//! - A server-created object destroyed by a destructor event is gone at once
//!   (the server frees the id when it sends it). Destroyed by a request, it is
//!   kept as a zombie until the server reuses the id, again so late events
//!   still parse.
//! - A `new_id` over a live id is an error; over a zombie it is allowed only
//!   where libwayland would allow it (the server reusing one of its own ids).

use std::collections::HashMap;

use crate::proto::{IfaceId, WL_DISPLAY};
use crate::wire::SERVER_ID_START;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object {
    pub iface: IfaceId,
    pub version: u32,
    pub zombie: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjError {
    /// A new_id naming an id that is still in use.
    InUse(u32),
    /// A new_id in the wrong range for who created it.
    WrongRange(u32),
    /// The per-connection cap on live objects.
    TooMany,
}

/// Enough for any real client (a busy browser holds a few thousand), small
/// enough that a hostile peer cannot grow the table without bound.
pub const MAX_OBJECTS: usize = 1 << 17;

pub struct Objects {
    map: HashMap<u32, Object>,
}

impl Default for Objects {
    fn default() -> Self {
        Self::new()
    }
}

impl Objects {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(
            1,
            Object {
                iface: WL_DISPLAY,
                version: 1,
                zombie: false,
            },
        );
        Self { map }
    }

    pub fn get(&self, id: u32) -> Option<Object> {
        self.map.get(&id).copied()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Record a new object. `by_client` says which end allocated the id (a
    /// request's new_id, or an event's).
    pub fn create(
        &mut self,
        id: u32,
        iface: IfaceId,
        version: u32,
        by_client: bool,
    ) -> Result<(), ObjError> {
        let client_range = id < SERVER_ID_START;
        if client_range != by_client || id == 0 {
            return Err(ObjError::WrongRange(id));
        }
        match self.map.get(&id) {
            Some(o) if !o.zombie => return Err(ObjError::InUse(id)),
            // A client id is not free again until delete_id removed it.
            Some(_) if by_client => return Err(ObjError::InUse(id)),
            _ => {}
        }
        if self.map.len() >= MAX_OBJECTS && !self.map.contains_key(&id) {
            return Err(ObjError::TooMany);
        }
        self.map.insert(
            id,
            Object {
                iface,
                version,
                zombie: false,
            },
        );
        Ok(())
    }

    /// A destructor request was sent on `id`.
    pub fn destroyed_by_request(&mut self, id: u32) {
        if let Some(o) = self.map.get_mut(&id) {
            o.zombie = true;
        }
    }

    /// A destructor event was sent on `id`.
    pub fn destroyed_by_event(&mut self, id: u32) {
        if id >= SERVER_ID_START {
            self.map.remove(&id);
        } else if let Some(o) = self.map.get_mut(&id) {
            o.zombie = true;
        }
    }

    /// `wl_display.delete_id(id)`: the server is done with a client id.
    /// Returns whether the object was still live (not a zombie) -- the case of
    /// a server that destroyed an object without the destructor event the
    /// protocol promises.
    pub fn delete_id(&mut self, id: u32) -> bool {
        if id == 1 || id >= SERVER_ID_START {
            return false;
        }
        matches!(self.map.remove(&id), Some(o) if !o.zombie)
    }
}
