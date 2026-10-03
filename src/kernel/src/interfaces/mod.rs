//! The kernel's interfaces to ring-3 drivers: one core per kind of device.
//!
//! Nothing here drives hardware. A driver is a process under
//! `src/user/system/native/drivers/`; what lives here is the kernel's end of the
//! control channel or ring that driver speaks, checked word for word against
//! its protocol crate in `src/lib/proto/`, and what the kernel publishes from
//! it to programs: a disk, a network interface, a card under `/dev/dri`, an
//! event node under `/dev/input`, a sound card, the kernel log's reader.
//!
//! | Core | Protocol | Design |
//! |---|---|---|
//! | [`block_ring`] | `ferrix-blkring` | `docs/BLOCK-RING.md` |
//! | [`net_ring`] | `ferrix-netring` | `docs/NET-RING.md` |
//! | [`display`] | `ferrix-displayctl` | `docs/DISPLAY.md` |
//! | [`render`] | `ferrix-renderctl` | `docs/GPU.md` |
//! | [`input`] | `ferrix-inputctl` | `docs/INPUT.md` |
//! | [`audio`] | `ferrix-sndctl` | `docs/AUDIO.md` |
//! | [`logctl`] | `ferrix-logctl` | the kernel log |
//!
//! The certification item puts every core in the load ring
//! (`tools/common/data/certification-item.json`): a defect in one is bounded
//! by the item's own enforcement, as a driver's fault is.

pub(crate) mod audio;
pub(crate) mod block_ring;
pub(crate) mod chardev;
pub(crate) mod display;
pub(crate) mod input;
pub(crate) mod logctl;
pub(crate) mod net_ring;
pub(crate) mod render;
