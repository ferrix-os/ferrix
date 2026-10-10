//! The globals a client may bind, and the names it knows them by.

use compositor_protocol::Interface;

use crate::role::Role;

/// One global the registry advertises.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Global {
    /// The `name` `wl_registry.global` carries and `wl_registry.bind` takes
    /// back. It is not an object id: it names the global itself, which
    /// outlives every object bound from it.
    pub name: u32,
    /// What binding it makes.
    pub interface: &'static Interface,
    /// The highest version this compositor implements, which may be below the
    /// interface's own if part of it is not written yet.
    pub version: u32,
    /// The role a bound object takes.
    pub role: Role,
}

/// Every global, in the order the registry announces them.
///
/// Names are handed out in order from 1 and never reused, as Hyprland's and
/// libwayland's are: a client that binds a name it saw in an earlier
/// `wl_registry.global` gets what it expected or nothing, never something
/// else.
#[derive(Clone, Debug, Default)]
pub struct Globals {
    entries: Vec<Global>,
    /// The modifiers `zwp_linux_dmabuf_v1` announces with each format, when
    /// the compositor has said which; linear and the implicit layout when
    /// it has not.
    dmabuf_modifiers: Option<Vec<u64>>,
}

impl Globals {
    /// No globals.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            dmabuf_modifiers: None,
        }
    }

    /// Announce `modifiers` with each format of `zwp_linux_dmabuf_v1`, in
    /// this order: the ones the compositor can take a buffer in with
    /// ([`crate::modifiers_offered`]). At most
    /// [`crate::MODIFIERS_OFFERED_MOST`] are kept.
    pub fn offer_dmabuf_modifiers(&mut self, modifiers: &[u64]) {
        let kept = modifiers.len().min(crate::MODIFIERS_OFFERED_MOST);
        self.dmabuf_modifiers = modifiers.get(..kept).map(<[u64]>::to_vec);
    }

    /// The modifiers `zwp_linux_dmabuf_v1` announces with each format.
    #[must_use]
    pub fn dmabuf_modifiers(&self) -> &[u64] {
        self.dmabuf_modifiers
            .as_deref()
            .unwrap_or(&[crate::MOD_LINEAR, crate::MOD_INVALID])
    }

    /// Advertise `interface` at `version`, which must not be above what the
    /// interface itself offers. Gives the name the registry will carry.
    ///
    /// `None` when the version asked for is above the interface's, which is a
    /// mistake in the compositor rather than anything a client did.
    pub fn add(&mut self, interface: &'static Interface, version: u32, role: Role) -> Option<u32> {
        if version == 0 || version > interface.version {
            return None;
        }
        let name = u32::try_from(self.entries.len()).ok()?.checked_add(1)?;
        self.entries.push(Global {
            name,
            interface,
            version,
            role,
        });
        Some(name)
    }

    /// The global `name` names, if there is one.
    #[must_use]
    pub fn get(&self, name: u32) -> Option<&Global> {
        let index = usize::try_from(name.checked_sub(1)?).ok()?;
        self.entries.get(index)
    }

    /// Every global, in announcement order.
    #[must_use]
    pub fn all(&self) -> &[Global] {
        &self.entries
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
