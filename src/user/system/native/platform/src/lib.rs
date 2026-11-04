//! Clients of the shared platform resources ring-3 providers serve
//! (`docs/DRIVER-MODEL.md` §4.1): a clock, a reset, a regulator.
//!
//! A client is what a driver's `#[clock]`, `#[reset]` and
//! `#[power_supply]` fields resolve to: one reference of its node, its
//! provider and specifier cells as the tree gave them. devmgr's wiring (step
//! 2) will hand each consumer a channel already bound to that specifier, so
//! a consumer can name no other clock; until then a client is resolved --
//! a node that lacks the reference, or names a missing provider, still
//! refuses the driver -- and every operation answers `ENOSYS`.

#![no_std]

use ferrix_driver::{Errno, FromNode, Node, Reference, Requirement, Result, What};

/// The reference `requirement` names in `property`, its place found by
/// index or by its tree name in `names`: `ENOENT` when the node has none,
/// `ENXIO` when its provider is missing or disabled.
fn resolve(
    node: &Node,
    requirement: &Requirement<'_>,
    property: &str,
    names: &str,
) -> Result<Reference> {
    let place = node.place(requirement, names)?;
    node.reference(property, place)
}

/// A clock of the consumer's, served by its provider.
#[derive(Debug, Clone, Copy)]
pub struct ClockClient {
    reference: Reference,
}

impl ClockClient {
    /// The reference it is bound to.
    #[must_use]
    pub const fn reference(&self) -> &Reference {
        &self.reference
    }

    /// Ungate it. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn enable(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Gate it. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn disable(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Set its rate, nearest `hertz`: the rate it now runs at. `ENOSYS`
    /// until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn set_rate(&self, _hertz: u64) -> Result<u64> {
        Err(Errno::ENOSYS)
    }

    /// Its rate. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn rate(&self) -> Result<u64> {
        Err(Errno::ENOSYS)
    }
}

impl FromNode for ClockClient {
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self> {
        if requirement.what != What::Clock {
            return Err(Errno::EINVAL);
        }
        let reference = resolve(node, requirement, "clocks", "clock-names")?;
        Ok(ClockClient { reference })
    }
}

/// A reset line of the consumer's, served by its provider.
#[derive(Debug, Clone, Copy)]
pub struct ResetClient {
    reference: Reference,
}

impl ResetClient {
    /// The reference it is bound to.
    #[must_use]
    pub const fn reference(&self) -> &Reference {
        &self.reference
    }

    /// Hold the device in reset. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn assert(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Let it out of reset. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn deassert(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Reset it once. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn pulse(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }
}

impl FromNode for ResetClient {
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self> {
        if requirement.what != What::Reset {
            return Err(Errno::EINVAL);
        }
        let reference = resolve(node, requirement, "resets", "reset-names")?;
        Ok(ResetClient { reference })
    }
}

/// A supply of the consumer's, served by its regulator's provider.
#[derive(Debug, Clone, Copy)]
pub struct RegulatorClient {
    reference: Reference,
}

impl RegulatorClient {
    /// The reference it is bound to.
    #[must_use]
    pub const fn reference(&self) -> &Reference {
        &self.reference
    }

    /// Turn it on. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn enable(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Turn it off. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn disable(&self) -> Result<()> {
        Err(Errno::ENOSYS)
    }

    /// Set its voltage, in microvolts. `ENOSYS` until step 2's providers.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn set_voltage(&self, _microvolts: u32) -> Result<()> {
        Err(Errno::ENOSYS)
    }
}

impl FromNode for RegulatorClient {
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self> {
        if requirement.what != What::PowerSupply {
            return Err(Errno::EINVAL);
        }
        // `<name>-supply`, one phandle.
        let mut property = [0u8; 264];
        let name = requirement.name.as_bytes();
        let suffix = b"-supply";
        let slot = property
            .get_mut(..name.len() + suffix.len())
            .ok_or(Errno::EINVAL)?;
        let (head, tail) = slot.split_at_mut(name.len());
        head.copy_from_slice(name);
        tail.copy_from_slice(suffix);
        let property = core::str::from_utf8(slot).map_err(|_| Errno::EINVAL)?;
        let reference = node.reference(property, 0)?;
        Ok(RegulatorClient { reference })
    }
}
