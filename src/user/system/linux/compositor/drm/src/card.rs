//! The card: `/dev/dri/card0` driven through the legacy mode-setting calls.

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};

use ferrix_linux_abi::drm::{
    self, CardRes, ClipRect, CreateDumb, Crtc, CrtcPageFlip, DestroyDumb, FbCmd2, FbDirtyCmd,
    Field, GetBlob, GetCap, GetConnector, GetEncoder, GetPlane, GetPlaneRes, GetProperty, Layout,
    MapDumb, ModeCursor2, ModeInfo, ObjGetProperties, PrimeHandle, PropertyEnum, SetClientCap,
    Version,
};
use ferrix_linux_abi::socket::Width;

use crate::modeset;

/// What the display test waits for, followed by the mode and the colour.
const MARKER: &str = "compositor: scanout";

/// The first card, and the name every other is made from.
const CARD: &CStr = c"/dev/dri/card0";

/// The most rectangles one [`Card::dirty`] names: `DRM_MODE_FB_DIRTY_MAX_CLIPS`.
const MAX_CLIPS: usize = drm::FB_DIRTY_MAX_CLIPS as usize;

/// The most cards looked for: a machine with more screens than this has
/// them on one card's connectors, which is where they are looked for next.
const MAX_CARDS: u32 = 8;

/// An open card.
pub struct Card {
    fd: libc::c_int,
    /// `card0`, `card1`: the file's own name, which a monitor's name and the
    /// compositor's log line carry.
    name: String,
}

impl core::fmt::Debug for Card {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Card")
            .field("name", &self.name)
            .field("fd", &self.fd)
            .finish()
    }
}

impl Card {
    /// Open the card.
    ///
    /// # Errors
    ///
    /// Whatever `open` said. A card that is not there is a compositor with
    /// no screen, which is the one thing it cannot do without.
    pub fn open() -> io::Result<Self> {
        // Through `sessiond` in a session that runs as its user.
        let fd = compositor_seat::open(CARD, libc::O_RDWR)?.into_raw_fd();
        Ok(Self {
            fd,
            name: "card0".to_owned(),
        })
    }

    /// Open `/dev/dri/card<index>`.
    ///
    /// # Errors
    ///
    /// Whatever `open` said, which for a card that is not there is `ENOENT`.
    pub fn open_index(index: u32) -> io::Result<Self> {
        let name = format!("card{index}");
        let path = std::ffi::CString::new(format!("/dev/dri/{name}"))
            .map_err(|_| io::Error::other("a card path with a NUL in it"))?;
        let fd = compositor_seat::open(&path, libc::O_RDWR)?.into_raw_fd();
        Ok(Self { fd, name })
    }

    /// What the card is called, which is what a monitor's name is made from.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The card's descriptor, for a loop to wait on: it is readable with a
    /// page-flip event to read, and for good once the card's driver has gone.
    #[must_use]
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.fd
    }

    /// Whether the card's driver has gone, which a read of the descriptor
    /// says by returning 0, as Linux's does for an unplugged card. Any
    /// page-flip events waiting are read and dropped on the way: nothing here
    /// waits on them (see [`Card::page_flip`]). Never blocks.
    #[must_use]
    pub fn gone(&self) -> bool {
        self.news().gone
    }

    /// What the card has to say, read off its descriptor until nothing is
    /// left: whether its driver has gone, and whether its connectors changed
    /// -- Ferrix's own `EVENT_FERRIX_CONNECTORS`, which a virtio-gpu whose
    /// host window was resized sends. Page-flip events are read and dropped,
    /// as [`Card::gone`] says. Never blocks.
    #[must_use]
    pub fn news(&self) -> News {
        let mut news = News::default();
        let mut events = [0u8; 4096];
        loop {
            let mut poll = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live `pollfd`, and a zero timeout.
            let ready = unsafe { libc::poll(&raw mut poll, 1, 0) };
            if ready <= 0 || poll.revents & libc::POLLIN == 0 {
                return news;
            }
            // SAFETY: `events` is a live buffer of the length passed.
            let read = unsafe { libc::read(self.fd, events.as_mut_ptr().cast(), events.len()) };
            match usize::try_from(read) {
                Ok(0) => {
                    news.gone = true;
                    return news;
                }
                Ok(read) => {
                    news.connectors |= connectors_changed(events.get(..read).unwrap_or(&[]));
                }
                Err(_) => {
                    news.gone = io::Error::last_os_error().raw_os_error() == Some(libc::ENODEV);
                    return news;
                }
            }
        }
    }

    /// Run `request` with `value` as its argument, and read the kernel's
    /// answer back into it.
    fn ioctl<L: Layout>(&self, request: u32, value: &mut L) -> io::Result<()> {
        ioctl_on(self.fd, request, value)
    }
}

/// [`Card::ioctl`] on any descriptor of a card, a [`Flusher`]'s too.
pub(crate) fn ioctl_on<L: Layout>(fd: libc::c_int, request: u32, value: &mut L) -> io::Result<()> {
    {
        let mut bytes = vec![0u8; L::SIZE];
        value
            .write(&mut bytes)
            .ok_or_else(|| io::Error::other("a structure larger than its buffer"))?;
        // SAFETY: `bytes` is a live buffer of exactly the size the request's
        // number encodes, which the kernel reads and writes within.
        let result = unsafe { libc::ioctl(fd, request as _, bytes.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        *value = L::read(&bytes).ok_or_else(|| io::Error::other("a short answer"))?;
        Ok(())
    }
}

/// The address of `items` as a user pointer field.
fn address<T>(items: &mut [T]) -> u64 {
    items.as_mut_ptr() as usize as u64
}

/// What the modeset settled on.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The connector the screen is on.
    pub connector: u32,
    /// The CRTC driving it.
    pub crtc: u32,
    /// The CRTC's index in `GETRESOURCES`' list, which `possible_crtcs` bit
    /// masks count in.
    pub crtc_index: usize,
    /// The mode it was set to.
    pub mode: ModeInfo,
    /// Every mode the connector lists, which [`Plan::take`] chooses among.
    pub modes: Vec<ModeInfo>,
    /// The monitor's name, made from the connector's type and number.
    pub name: String,
    /// What the monitor says it is: its `EDID`, read. `None` for a
    /// connector with no `EDID` property or a blob that is not one -- a
    /// virtual screen has neither.
    pub edid: Option<crate::Edid>,
}

impl Plan {
    /// Take the listed mode that is `width` by `height`, the one nearest
    /// `refresh` hertz where several are, and say whether there was one.
    ///
    /// What a `monitor = name, 1920x1080@60, ...` line asks for. A size the
    /// connector does not list is left alone and the plan keeps the mode it
    /// had, as Hyprland falls back to the preferred one: a mode a monitor
    /// never offered is a black screen on real hardware.
    pub fn take(&mut self, (width, height): (u32, u32), refresh: Option<f64>) -> bool {
        let wanted = refresh.unwrap_or(60.0);
        let found = self
            .modes
            .iter()
            .filter(|mode| (u32::from(mode.hdisplay), u32::from(mode.vdisplay)) == (width, height))
            .min_by(|one, other| {
                let off = |mode: &ModeInfo| (f64::from(mode.vrefresh) - wanted).abs();
                off(one).total_cmp(&off(other))
            })
            .copied();
        if let Some(mode) = found {
            self.mode = mode;
        }
        found.is_some()
    }

    /// The mode's size in pixels.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.mode.hdisplay as u32, self.mode.vdisplay as u32)
    }
}

/// Every card the machine has, in order, from `/dev/dri/card0` up until one
/// is not there.
///
/// A machine's screens are on one card's connectors or on a card each, and
/// which it is is the machine's business rather than the compositor's: both
/// are monitors to everything above this.
#[must_use]
pub fn cards() -> Vec<Card> {
    let mut cards = Vec::new();
    for index in 0..MAX_CARDS {
        match Card::open_index(index) {
            Ok(card) => cards.push(card),
            // The first gap ends the search, as `/dev/dri` numbers cards
            // from zero with no holes.
            Err(_) => break,
        }
    }
    cards
}

/// Find a connected connector with a mode, and a CRTC that can drive it.
///
/// # Errors
///
/// A card with no connected connector, or one whose encoder has no CRTC.
pub fn plan(card: &Card) -> io::Result<Plan> {
    plans(card)?
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("no connected connector with a mode"))
}

/// Every connected connector of `card` with a mode, each with a CRTC of its
/// own.
///
/// A CRTC drives one connector at a time, so a connector whose only CRTC is
/// already taken by another is left out rather than made to share: two
/// monitors on one CRTC would be a clone, which is not what a second monitor
/// is for.
///
/// # Errors
///
/// Whatever the card said.
pub fn plans(card: &Card) -> io::Result<Vec<Plan>> {
    let mut resources = CardRes::ZERO;
    card.ioctl(drm::IOCTL_MODE_GETRESOURCES, &mut resources)?;
    let mut crtcs = vec![0u32; resources.count_crtcs as usize];
    let mut connectors = vec![0u32; resources.count_connectors as usize];
    let mut encoders = vec![0u32; resources.count_encoders as usize];
    resources = CardRes {
        crtc_id_ptr: address(&mut crtcs),
        connector_id_ptr: address(&mut connectors),
        encoder_id_ptr: address(&mut encoders),
        count_fbs: 0,
        ..resources
    };
    card.ioctl(drm::IOCTL_MODE_GETRESOURCES, &mut resources)?;

    let mut found: Vec<Plan> = Vec::new();
    let mut taken: Vec<u32> = Vec::new();
    for &connector_id in &connectors {
        let mut connector = GetConnector {
            connector_id,
            ..GetConnector::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_GETCONNECTOR, &mut connector)?;
        if connector.connection != drm::CONNECTION_CONNECTED || connector.count_modes == 0 {
            continue;
        }
        let mut modes = vec![0u8; connector.count_modes as usize * ModeInfo::SIZE];
        let mut connector_encoders = vec![0u32; connector.count_encoders as usize];
        connector = GetConnector {
            modes_ptr: address(&mut modes),
            encoders_ptr: address(&mut connector_encoders),
            count_props: 0,
            ..connector
        };
        card.ioctl(drm::IOCTL_MODE_GETCONNECTOR, &mut connector)?;
        let modes: Vec<ModeInfo> = modes
            .chunks_exact(ModeInfo::SIZE)
            .filter_map(ModeInfo::read)
            .collect();
        let Some(mode) = modeset::choose_mode(&modes) else {
            continue;
        };
        let encoder_id = if connector.encoder_id != 0 {
            connector.encoder_id
        } else {
            connector_encoders.first().copied().unwrap_or(0)
        };
        let mut encoder = GetEncoder {
            encoder_id,
            ..GetEncoder::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_GETENCODER, &mut encoder)?;
        // The CRTCs another connector of this card already has are not
        // offered again.
        let free: Vec<u32> = crtcs
            .iter()
            .copied()
            .filter(|id| !taken.contains(id))
            .collect();
        let already = (!taken.contains(&encoder.crtc_id)).then_some(encoder.crtc_id);
        let Some(crtc) = modeset::choose_crtc(
            already.unwrap_or(0),
            encoder.possible_crtcs & mask_of(&free, &crtcs),
            &free,
        ) else {
            continue;
        };
        let crtc_index = crtcs
            .iter()
            .position(|&id| id == crtc)
            .ok_or_else(|| io::Error::other("the encoder's CRTC is not the card's"))?;
        taken.push(crtc);
        found.push(Plan {
            connector: connector_id,
            crtc,
            crtc_index,
            mode,
            modes,
            name: connector_name(&connector),
            // A connector that will not say what it is gets no
            // description, which is one monitor a `desc:` rule cannot
            // name -- not a card the compositor refuses to open.
            edid: connector_edid(card, connector_id).ok().flatten(),
        });
    }
    Ok(found)
}

/// The `possible_crtcs` bits of the CRTCs in `free`, which count in the
/// order `all` lists them.
fn mask_of(free: &[u32], all: &[u32]) -> u32 {
    let mut mask = 0u32;
    for (index, id) in all.iter().enumerate() {
        if free.contains(id)
            && let Ok(bit) = u32::try_from(index)
            && let Some(one) = 1u32.checked_shl(bit)
        {
            mask |= one;
        }
    }
    mask
}

/// What to call the monitor on a connector.
///
/// Hyprland names a monitor after its connector -- `DP-1`, `HDMI-A-2` -- and
/// a person's `monitor =` lines and `hyprctl monitors` both use that name.
/// The type is the connector's; the number is the caller's, because two
/// cards each have a `Virtual-1` and a person's two screens must not share
/// a name. [`rename`] gives them theirs.
fn connector_name(connector: &GetConnector) -> String {
    format!(
        "{}-{}",
        modeset::connector_type_name(connector.connector_type),
        connector.connector_type_id
    )
}

/// Number `plans` as wlroots numbers outputs: each connector type counted
/// from one across every card, so a machine with two virtio-gpu cards has
/// `Virtual-1` and `Virtual-2` rather than two `Virtual-1`s.
pub fn rename(plans: &mut [Plan]) {
    let mut counts: Vec<(String, u32)> = Vec::new();
    for plan in plans {
        let kind = plan
            .name
            .rsplit_once('-')
            .map_or(plan.name.clone(), |(kind, _)| kind.to_owned());
        let number = match counts.iter_mut().find(|(known, _)| *known == kind) {
            Some((_, count)) => {
                *count = count.saturating_add(1);
                *count
            }
            None => {
                counts.push((kind.clone(), 1));
                1
            }
        };
        plan.name = format!("{kind}-{number}");
    }
}

/// Fill the screen with one colour and show it: `src/user/system/linux/compositor/blank`'s whole
/// job, and the proof that the path from a program to QEMU's window works.
///
/// # Errors
///
/// Whatever the card said.
pub fn show(card: &Card) -> io::Result<String> {
    let mut plan = plan(card)?;
    let (width, height) = (u32::from(plan.mode.hdisplay), u32::from(plan.mode.vdisplay));

    let mut dumb = CreateDumb {
        width,
        height,
        bpp: 32,
        ..CreateDumb::ZERO
    };
    card.ioctl(drm::IOCTL_MODE_CREATE_DUMB, &mut dumb)?;
    let mut map = MapDumb {
        handle: dumb.handle,
        ..MapDumb::ZERO
    };
    card.ioctl(drm::IOCTL_MODE_MAP_DUMB, &mut map)?;

    let size = usize::try_from(dumb.size).map_err(io::Error::other)?;
    let offset = libc::off_t::try_from(map.offset).map_err(io::Error::other)?;
    // SAFETY: a new shared mapping of the card at the offset MAP_DUMB gave,
    // of the size CREATE_DUMB gave; nothing else maps it.
    let base = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            card.fd,
            offset,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `base` maps `size` bytes, readable and writable, for the rest
    // of the program, which never unmaps it.
    let pixels = unsafe { core::slice::from_raw_parts_mut(base.cast::<u8>(), size) };
    modeset::fill(
        pixels,
        width as usize,
        height as usize,
        dumb.pitch as usize,
        modeset::BACKGROUND,
        cfg!(feature = "negative-control"),
    );

    let mut framebuffer = FbCmd2 {
        width,
        height,
        pixel_format: drm::FORMAT_XRGB8888,
        handles: [dumb.handle, 0, 0, 0],
        pitches: [dumb.pitch, 0, 0, 0],
        ..FbCmd2::ZERO
    };
    card.ioctl(drm::IOCTL_MODE_ADDFB2, &mut framebuffer)?;

    let mut connectors = [plan.connector];
    let mut crtc = Crtc {
        set_connectors_ptr: address(&mut connectors),
        count_connectors: 1,
        crtc_id: plan.crtc,
        fb_id: framebuffer.fb_id,
        mode_valid: 1,
        mode: plan.mode,
        ..Crtc::ZERO
    };
    card.ioctl(drm::IOCTL_MODE_SETCRTC, &mut crtc)?;
    plan.mode = crtc.mode;

    let planes = planes(card)?;
    let described =
        modeset::describe_planes(&planes, plan.crtc_index, plan.crtc, framebuffer.fb_id)
            .map_err(io::Error::other)?;

    let events = events(card, plan.crtc, framebuffer.fb_id);
    Ok(format!(
        "{MARKER} {} {width}x{height} colour 0x{:06x} {events} {described}",
        modeset::mode_name(&plan.mode),
        modeset::BACKGROUND
    ))
}

/// What reads of the card smaller and larger than an event said, around a
/// page flip that promised one: `events EAGAIN 0 32`, or `events none <what
/// went otherwise>`.
///
/// What [`Card::news`] read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct News {
    /// The card's driver has gone.
    pub gone: bool,
    /// Its connectors changed, so their modes are worth reading again.
    pub connectors: bool,
}

/// Whether the events a read gave hold a connector change: each event is a
/// `drm_event` header, its type and its whole length, stepped over by that
/// length as libdrm's `drmHandleEvent` does.
pub(crate) fn connectors_changed(mut events: &[u8]) -> bool {
    let mut changed = false;
    while let (Some(kind), Some(length)) = (word(events, 0), word(events, 4)) {
        changed |= kind == drm::EVENT_FERRIX_CONNECTORS;
        let Some(rest) = usize::try_from(length)
            .ok()
            .filter(|&length| length >= 8)
            .and_then(|length| events.get(length..))
        else {
            break;
        };
        events = rest;
    }
    changed
}

/// The little-endian word at `at`.
fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Linux's `drm_read`, under `O_NONBLOCK` with nothing queued, is `EAGAIN`
/// whatever the count -- measured on a 7.0 host, `card1` (amdgpu) and
/// `card2` (nvidia) answer a read of 8 and of 0 so -- and with an event
/// queued that does not fit, it puts the event back and answers what it
/// copied, 0 (`drm_file.c`: `if (length > count - ret) goto put_back_event`),
/// which a read of 4096 then takes whole. So: a read of 8 with nothing
/// queued, a flip to the framebuffer already shown asking for its event, a
/// wait for it, a read of 8 and a read of 4096. The descriptor's flags are
/// put back after.
fn events(card: &Card, crtc: u32, framebuffer: u32) -> String {
    // SAFETY: the descriptor is the card's own and open; F_GETFL takes no
    // argument.
    let flags = unsafe { libc::fcntl(card.fd, libc::F_GETFL) };
    // SAFETY: as above, with the flags it answered and one more.
    if flags < 0 || unsafe { libc::fcntl(card.fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return format!("events none fcntl {}", io::Error::last_os_error().kind());
    }
    let said = read_events(card, crtc, framebuffer);
    // SAFETY: as above, putting back the flags the card had.
    let _ = unsafe { libc::fcntl(card.fd, libc::F_SETFL, flags) };
    said
}

/// [`events`]'s reads, the card non-blocking.
fn read_events(card: &Card, crtc: u32, framebuffer: u32) -> String {
    let mut buffer = [0u8; 4096];
    let mut read = |count: usize| {
        // SAFETY: `buffer` is live and at least `count` bytes long.
        let got = unsafe { libc::read(card.fd, buffer.as_mut_ptr().cast(), count) };
        match got {
            -1 => match io::Error::last_os_error().raw_os_error() {
                Some(libc::EAGAIN) => "EAGAIN".to_owned(),
                Some(errno) => format!("errno{errno}"),
                None => "failed".to_owned(),
            },
            got => got.to_string(),
        }
    };
    let empty = read(8);
    let mut flip = CrtcPageFlip {
        crtc_id: crtc,
        fb_id: framebuffer,
        flags: drm::PAGE_FLIP_EVENT,
        ..CrtcPageFlip::ZERO
    };
    if let Err(error) = card.ioctl(drm::IOCTL_MODE_PAGE_FLIP, &mut flip) {
        return format!("events none flip {}", error.kind());
    }
    let mut poll = libc::pollfd {
        fd: card.fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live `pollfd`.
    if unsafe { libc::poll(&raw mut poll, 1, 5000) } != 1 {
        return format!("events none {empty} no event came");
    }
    let small = read(8);
    let whole = read(4096);
    format!("events {empty} {small} {whole}")
}

/// Every plane the card lists once universal planes are asked for, read the
/// way Smithay's legacy path reads them (`backend/drm/mod.rs`, `planes` and
/// `plane_type`): `GETPLANERESOURCES`, `GETPLANE`, then each plane's
/// properties through `OBJ_GETPROPERTIES` and `GETPROPERTY`, looking for
/// `type`. Each list is asked for twice, counts first.
/// Every plane the card lists.
///
/// # Errors
///
/// Whatever the card said.
pub fn planes(card: &Card) -> io::Result<Vec<modeset::Plane>> {
    let mut universal = SetClientCap {
        capability: drm::CLIENT_CAP_UNIVERSAL_PLANES,
        value: 1,
    };
    card.ioctl(drm::IOCTL_SET_CLIENT_CAP, &mut universal)?;

    let mut resources = GetPlaneRes::ZERO;
    card.ioctl(drm::IOCTL_MODE_GETPLANERESOURCES, &mut resources)?;
    let mut ids = vec![0u32; resources.count_planes as usize];
    resources.plane_id_ptr = address(&mut ids);
    card.ioctl(drm::IOCTL_MODE_GETPLANERESOURCES, &mut resources)?;
    ids.truncate(resources.count_planes as usize);

    let mut planes = Vec::with_capacity(ids.len());
    for id in ids {
        let mut plane = GetPlane {
            plane_id: id,
            ..GetPlane::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_GETPLANE, &mut plane)?;
        planes.push(modeset::Plane {
            id,
            crtc: plane.crtc_id,
            framebuffer: plane.fb_id,
            possible_crtcs: plane.possible_crtcs,
            kind: plane_type(card, id)?,
        });
    }
    Ok(planes)
}

/// What connector `id` says it is: its `EDID` property's blob, read.
///
/// `None` where the connector has no `EDID` property, where its value is
/// zero -- which is a connector with nothing plugged in, or a virtual one
/// that has nothing to say -- or where the bytes are not an EDID.
fn connector_edid(card: &Card, id: u32) -> io::Result<Option<crate::Edid>> {
    let mut request = ObjGetProperties {
        obj_id: id,
        obj_type: drm::MODE_OBJECT_CONNECTOR,
        ..ObjGetProperties::ZERO
    };
    card.ioctl(drm::IOCTL_MODE_OBJ_GETPROPERTIES, &mut request)?;
    let mut properties = vec![0u32; request.count_props as usize];
    let mut values = vec![0u64; request.count_props as usize];
    request.props_ptr = address(&mut properties);
    request.prop_values_ptr = address(&mut values);
    card.ioctl(drm::IOCTL_MODE_OBJ_GETPROPERTIES, &mut request)?;

    for (&property_id, &value) in properties.iter().zip(&values) {
        let mut property = GetProperty {
            prop_id: property_id,
            ..GetProperty::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_GETPROPERTY, &mut property)?;
        if modeset::c_name(&property.name) != "EDID" {
            continue;
        }
        let Ok(blob_id) = u32::try_from(value) else {
            return Ok(None);
        };
        if blob_id == 0 {
            return Ok(None);
        }
        // Twice, as every counted DRM call is: once for the length, once
        // for the bytes.
        let mut blob = GetBlob {
            blob_id,
            ..GetBlob::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_GETPROPBLOB, &mut blob)?;
        if blob.length == 0 {
            return Ok(None);
        }
        let mut bytes = vec![0u8; blob.length as usize];
        blob.data = address(&mut bytes);
        card.ioctl(drm::IOCTL_MODE_GETPROPBLOB, &mut blob)?;
        return Ok(crate::Edid::parse(&bytes));
    }
    Ok(None)
}

/// The name of plane `id`'s `type` value in the property's enum list, if the
/// plane has a `type` property.
fn plane_type(card: &Card, id: u32) -> io::Result<Option<String>> {
    let mut request = ObjGetProperties {
        obj_id: id,
        obj_type: drm::MODE_OBJECT_PLANE,
        ..ObjGetProperties::ZERO
    };
    card.ioctl(drm::IOCTL_MODE_OBJ_GETPROPERTIES, &mut request)?;
    let mut properties = vec![0u32; request.count_props as usize];
    let mut values = vec![0u64; request.count_props as usize];
    request.props_ptr = address(&mut properties);
    request.prop_values_ptr = address(&mut values);
    card.ioctl(drm::IOCTL_MODE_OBJ_GETPROPERTIES, &mut request)?;

    for (&property_id, &value) in properties.iter().zip(&values) {
        let mut property = GetProperty {
            prop_id: property_id,
            ..GetProperty::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_GETPROPERTY, &mut property)?;
        if modeset::c_name(&property.name) != "type" {
            continue;
        }
        let mut enums = vec![0u8; property.count_enum_blobs as usize * PropertyEnum::SIZE];
        property = GetProperty {
            enum_blob_ptr: address(&mut enums),
            count_values: 0,
            ..property
        };
        card.ioctl(drm::IOCTL_MODE_GETPROPERTY, &mut property)?;
        let names: Vec<(u64, String)> = enums
            .chunks_exact(PropertyEnum::SIZE)
            .filter_map(PropertyEnum::read)
            .map(|entry| (entry.value, modeset::c_name(&entry.name)))
            .collect();
        return Ok(Some(
            modeset::enum_name(value, &names).unwrap_or_else(|| format!("value {value}")),
        ));
    }
    Ok(None)
}

/// Something the card can be told to show: a framebuffer id, whatever the
/// pixels behind it are.
pub trait Shown {
    /// Its `ADDFB2` id, which a `SETCRTC`, a `PAGE_FLIP` or a `DIRTYFB`
    /// names.
    fn framebuffer(&self) -> u32;
}

/// A dumb buffer: memory the card can scan out of, mapped for the program to
/// draw into.
///
/// "Dumb" is the kernel's own word for a buffer with no format beyond a size
/// and a pitch: there is no GPU here, so a compositor draws every pixel
/// itself and hands the card the result.
#[derive(Debug)]
pub struct Dumb {
    /// The `ADDFB2` id, which a `SETCRTC` or a `PAGE_FLIP` names.
    pub framebuffer: u32,
    /// Bytes from one row's start to the next.
    pub pitch: u32,
    /// The mapping, for the whole of the program's life.
    pixels: &'static mut [u8],
    /// The handle, kept so the buffer can be freed.
    handle: u32,
}

impl Dumb {
    /// Make a `width` by `height` `XRGB8888` buffer on `card`, map it, and
    /// give the card a framebuffer id for it.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn new(card: &Card, width: u32, height: u32) -> io::Result<Self> {
        let mut dumb = CreateDumb {
            width,
            height,
            bpp: 32,
            ..CreateDumb::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_CREATE_DUMB, &mut dumb)?;
        let mut map = MapDumb {
            handle: dumb.handle,
            ..MapDumb::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_MAP_DUMB, &mut map)?;

        let size = usize::try_from(dumb.size).map_err(io::Error::other)?;
        let offset = libc::off_t::try_from(map.offset).map_err(io::Error::other)?;
        let pixels = card.map(size, offset)?;

        let mut framebuffer = FbCmd2 {
            width,
            height,
            pixel_format: drm::FORMAT_XRGB8888,
            handles: [dumb.handle, 0, 0, 0],
            pitches: [dumb.pitch, 0, 0, 0],
            ..FbCmd2::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_ADDFB2, &mut framebuffer)?;

        Ok(Self {
            framebuffer: framebuffer.fb_id,
            pitch: dumb.pitch,
            pixels,
            handle: dumb.handle,
        })
    }

    /// The memory, to draw into.
    #[must_use]
    pub fn pixels(&mut self) -> &mut [u8] {
        self.pixels
    }

    /// The memory, to read back: what a test compares and what a PPM dump
    /// writes out.
    #[must_use]
    pub fn shown(&self) -> &[u8] {
        self.pixels
    }

    /// The handle the kernel knows the buffer by.
    #[must_use]
    pub const fn handle(&self) -> u32 {
        self.handle
    }
}

impl Shown for Dumb {
    fn framebuffer(&self) -> u32 {
        self.framebuffer
    }
}

/// A buffer object made somewhere else and imported into this card: what a
/// compositor that drew on the GPU shows.
///
/// `DRM_IOCTL_PRIME_FD_TO_HANDLE` takes the descriptor the render node gave
/// for a resource the device holds, and `ADDFB2` makes a framebuffer of it.
/// Showing it is the ordinary `SETCRTC`, page flip and `DIRTYFB` -- and on a
/// virtio-gpu the dirty call costs a `RESOURCE_FLUSH` and nothing else,
/// because the host has the pixels already. That is the whole of what this
/// saves: a frame that would otherwise be fetched out of the device and
/// handed straight back to it.
#[derive(Debug)]
pub struct Imported {
    framebuffer: u32,
    handle: u32,
}

impl Imported {
    /// Import the buffer object `fd` names and give the card a framebuffer
    /// id for it.
    ///
    /// # Errors
    ///
    /// Whatever the card said; `EINVAL` for a descriptor that is not a
    /// buffer object of this card.
    pub fn new(
        card: &Card,
        fd: BorrowedFd<'_>,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> io::Result<Self> {
        let mut prime = PrimeHandle {
            handle: 0,
            flags: 0,
            fd: fd.as_raw_fd(),
        };
        card.ioctl(drm::IOCTL_PRIME_FD_TO_HANDLE, &mut prime)?;
        let mut framebuffer = FbCmd2 {
            width,
            height,
            pixel_format: drm::FORMAT_XRGB8888,
            handles: [prime.handle, 0, 0, 0],
            pitches: [pitch, 0, 0, 0],
            ..FbCmd2::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_ADDFB2, &mut framebuffer)?;
        Ok(Self {
            framebuffer: framebuffer.fb_id,
            handle: prime.handle,
        })
    }

    /// The handle the kernel knows the buffer by.
    #[must_use]
    pub const fn handle(&self) -> u32 {
        self.handle
    }

    /// The handle `fd` has on `card`: the card gives one handle a buffer,
    /// so a buffer imported before answers the handle it was given then,
    /// which is how a caller keeping [`Imported`]s finds the one it has.
    /// A buffer imported now for the asking is the caller's to make an
    /// [`Imported`] of, or to let go of.
    ///
    /// # Errors
    ///
    /// Whatever the card said; `EINVAL` for a descriptor the card will not
    /// show.
    pub fn handle_of(card: &Card, fd: BorrowedFd<'_>) -> io::Result<u32> {
        let mut prime = PrimeHandle {
            handle: 0,
            flags: 0,
            fd: fd.as_raw_fd(),
        };
        card.ioctl(drm::IOCTL_PRIME_FD_TO_HANDLE, &mut prime)?;
        Ok(prime.handle)
    }

    /// The handle `handle` already has, given a framebuffer of `width` ×
    /// `height` rows `pitch` bytes apart.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn with_handle(
        card: &Card,
        handle: u32,
        width: u32,
        height: u32,
        pitch: u32,
    ) -> io::Result<Self> {
        let mut framebuffer = FbCmd2 {
            width,
            height,
            pixel_format: drm::FORMAT_XRGB8888,
            handles: [handle, 0, 0, 0],
            pitches: [pitch, 0, 0, 0],
            ..FbCmd2::ZERO
        };
        card.ioctl(drm::IOCTL_MODE_ADDFB2, &mut framebuffer)?;
        Ok(Self {
            framebuffer: framebuffer.fb_id,
            handle,
        })
    }

    /// Let go of it: its framebuffer removed -- which takes it off any
    /// screen showing it, so show another first -- then its handle.
    ///
    /// # Errors
    ///
    /// Whatever the card said; the handle is let go of either way.
    pub fn release(self, card: &Card) -> io::Result<()> {
        let mut framebuffer = self.framebuffer;
        // SAFETY: `DRM_IOCTL_MODE_RMFB`'s argument is one `unsigned int`,
        // which `framebuffer` is, live for the call.
        let removed = unsafe {
            libc::ioctl(
                card.raw_fd(),
                drm::IOCTL_MODE_RMFB as _,
                core::ptr::from_mut(&mut framebuffer),
            )
        };
        let removed = if removed < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        Self::forget_handle(card, self.handle).and(removed)
    }

    /// Let go of a handle no framebuffer was made for.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn forget_handle(card: &Card, handle: u32) -> io::Result<()> {
        let mut destroy = DestroyDumb { handle };
        card.ioctl(drm::IOCTL_MODE_DESTROY_DUMB, &mut destroy)
    }
}

impl Shown for Imported {
    fn framebuffer(&self) -> u32 {
        self.framebuffer
    }
}

impl Card {
    /// Show `buffer` on the plan's CRTC, which is the first frame: a page
    /// flip needs a mode already set.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn set_mode(&self, plan: &Plan, buffer: &dyn Shown) -> io::Result<ModeInfo> {
        let mut connectors = [plan.connector];
        let mut crtc = Crtc {
            set_connectors_ptr: address(&mut connectors),
            count_connectors: 1,
            crtc_id: plan.crtc,
            fb_id: buffer.framebuffer(),
            mode_valid: 1,
            mode: plan.mode,
            ..Crtc::ZERO
        };
        self.ioctl(drm::IOCTL_MODE_SETCRTC, &mut crtc)?;
        Ok(crtc.mode)
    }

    /// Show `buffer` at the next vertical blank.
    ///
    /// The event the flip promises is read from the card's descriptor and is
    /// what a frame loop waits on; this asks for it and does not wait, so a
    /// caller that never reads the descriptor still flips -- one frame
    /// behind, which for a compositor drawing on demand is no frame behind at
    /// all.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn page_flip(&self, plan: &Plan, buffer: &dyn Shown) -> io::Result<()> {
        let mut flip = CrtcPageFlip {
            crtc_id: plan.crtc,
            fb_id: buffer.framebuffer(),
            flags: drm::PAGE_FLIP_EVENT,
            ..CrtcPageFlip::ZERO
        };
        self.ioctl(drm::IOCTL_MODE_PAGE_FLIP, &mut flip)
    }

    /// The kernel driver's name, as `DRM_IOCTL_VERSION` gives it:
    /// `virtio_gpu`, `amdgpu`, `i915`.
    ///
    /// What says whether the card scans out of the memory a dumb buffer is
    /// or out of a copy of it, which is what [`Card::dirty`] is for.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn driver(&self) -> io::Result<String> {
        let width = if size_of::<usize>() == 4 {
            Width::Bits32
        } else {
            Width::Bits64
        };
        let mut name = [0u8; 64];
        let version = Version {
            version_major: 0,
            version_minor: 0,
            version_patchlevel: 0,
            name_len: name.len() as u64,
            name: address(&mut name),
            date_len: 0,
            date: 0,
            desc_len: 0,
            desc: 0,
        };
        let mut bytes = vec![0u8; Version::size(width)];
        version
            .write(width, &mut bytes)
            .ok_or_else(|| io::Error::other("a structure larger than its buffer"))?;
        // SAFETY: `bytes` is a live buffer of the size the request's number
        // encodes, and the one pointer in it is to `name`, which outlives the
        // call and whose length is beside it.
        let result =
            unsafe { libc::ioctl(self.fd, drm::ioctl_version(width) as _, bytes.as_mut_ptr()) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let answered =
            Version::read(width, &bytes).ok_or_else(|| io::Error::other("a short answer"))?;
        let len = usize::try_from(answered.name_len)
            .unwrap_or(usize::MAX)
            .min(name.len());
        Ok(String::from_utf8_lossy(name.get(..len).unwrap_or(&[])).into_owned())
    }

    /// Tell the card that `clips` of `buffer` have been drawn into, each
    /// `(x, y, width, height)`.
    ///
    /// `DRM_IOCTL_MODE_DIRTYFB`. A card that scans out of a copy of the
    /// buffer -- a virtio-gpu, whose host holds the copy -- takes this as
    /// the order to bring that much of the copy up to date, which is the
    /// whole of showing a frame on such a card and costs what changed
    /// rather than the screen.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn dirty(&self, buffer: &dyn Shown, clips: &[(u32, u32, u32, u32)]) -> io::Result<()> {
        dirty_on(self.fd, buffer.framebuffer(), clips)
    }

    /// A second descriptor on this card, for a thread of the compositor's
    /// own to make its [`Card::dirty`] calls on: on a virtio-gpu each waits
    /// until the host has shown the frame, which a compositor's loop should
    /// not.
    ///
    /// # Errors
    ///
    /// Whatever `fcntl(F_DUPFD_CLOEXEC)` said.
    pub fn flusher(&self) -> io::Result<Flusher> {
        // SAFETY: `fcntl` duplicates a descriptor this card owns; the copy
        // is owned by the `Flusher`, which closes it.
        let fd = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just made and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Flusher { fd })
    }

    /// The size of the image this card's cursor plane shows, if it has one:
    /// `DRM_CAP_CURSOR_WIDTH` by `DRM_CAP_CURSOR_HEIGHT`.
    #[must_use]
    pub fn cursor_size(&self) -> Option<(u32, u32)> {
        let cap = |capability| {
            let mut request = GetCap {
                capability,
                ..GetCap::ZERO
            };
            self.ioctl(drm::IOCTL_GET_CAP, &mut request)
                .ok()
                .and_then(|()| u32::try_from(request.value).ok())
                .filter(|&size| size > 0)
        };
        Some((cap(drm::CAP_CURSOR_WIDTH)?, cap(drm::CAP_CURSOR_HEIGHT)?))
    }

    /// Show `image` -- a dumb buffer [`Card::cursor_size`] big, or none --
    /// as `plan`'s cursor, with its hotspot at `hot` and its top-left corner
    /// at `at`.
    ///
    /// `DRM_IOCTL_MODE_CURSOR2`, the image and the place together. It
    /// returns once the card shows the image, and the image shown before is
    /// no longer read. virtio-gpu's host has taken a copy by then; the DK1's
    /// LTDC shows the plane from the buffer itself and goes on reading it,
    /// so the next image goes into another buffer, as Linux's compositors
    /// keep a swapchain for the cursor.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn set_cursor(
        &self,
        plan: &Plan,
        image: Option<&Dumb>,
        hot: (i32, i32),
        at: (i32, i32),
    ) -> io::Result<()> {
        let (width, height) = match image {
            Some(_) => self
                .cursor_size()
                .ok_or_else(|| io::Error::other("a card with no cursor plane"))?,
            None => (0, 0),
        };
        let mut request = ModeCursor2 {
            flags: drm::MODE_CURSOR_BO | drm::MODE_CURSOR_MOVE,
            crtc_id: plan.crtc,
            x: at.0,
            y: at.1,
            width,
            height,
            handle: image.map_or(0, Dumb::handle),
            hot_x: hot.0,
            hot_y: hot.1,
        };
        self.ioctl(drm::IOCTL_MODE_CURSOR2, &mut request)
    }

    /// Put `plan`'s cursor's top-left corner at `at`.
    ///
    /// `DRM_IOCTL_MODE_CURSOR2` with the place alone, which waits for
    /// nothing: a pointer moving costs the card no frame.
    ///
    /// # Errors
    ///
    /// Whatever the card said.
    pub fn move_cursor(&self, plan: &Plan, at: (i32, i32)) -> io::Result<()> {
        let mut request = ModeCursor2 {
            flags: drm::MODE_CURSOR_MOVE,
            crtc_id: plan.crtc,
            x: at.0,
            y: at.1,
            ..ModeCursor2::ZERO
        };
        self.ioctl(drm::IOCTL_MODE_CURSOR2, &mut request)
    }

    /// Map `size` bytes of the card at `offset`.
    ///
    /// The mapping lives as long as the program: a compositor's scanout
    /// buffers are made once and drawn into for ever, and unmapping one while
    /// the card scans it out is a screen full of whatever came next.
    fn map(&self, size: usize, offset: libc::off_t) -> io::Result<&'static mut [u8]> {
        #[expect(
            unsafe_code,
            reason = "AUDIT: mmap is not in std; the call maps the card this process opened at the offset MAP_DUMB gave, and the result is checked against MAP_FAILED"
        )]
        // SAFETY: a null hint lets the kernel choose the address; `self.fd`
        // is the card this process opened.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.fd,
                offset,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        #[expect(
            unsafe_code,
            reason = "AUDIT: the mapping is never unmapped, so a 'static slice of it is sound; a scanout buffer unmapped while the card reads it is a screen full of whatever came next"
        )]
        // SAFETY: `base` maps `size` bytes, readable and writable, and
        // nothing ever unmaps it.
        let pixels = unsafe { core::slice::from_raw_parts_mut(base.cast::<u8>(), size) };
        Ok(pixels)
    }
}

/// A second descriptor on a card, which makes [`Card::dirty`]'s call and no
/// other ([`Card::flusher`]).
#[derive(Debug)]
pub struct Flusher {
    fd: OwnedFd,
}

impl Flusher {
    /// [`Card::dirty`] for the framebuffer `framebuffer`.
    ///
    /// # Errors
    ///
    /// Whatever the card said; `ENODEV` once its driver has gone.
    pub fn dirty(&self, framebuffer: u32, clips: &[(u32, u32, u32, u32)]) -> io::Result<()> {
        dirty_on(self.fd.as_raw_fd(), framebuffer, clips)
    }
}

/// `DRM_IOCTL_MODE_DIRTYFB` on descriptor `fd`: [`Card::dirty`]'s body.
fn dirty_on(fd: libc::c_int, framebuffer: u32, clips: &[(u32, u32, u32, u32)]) -> io::Result<()> {
    let edge = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
    let mut bytes = vec![0u8; clips.len().min(MAX_CLIPS) * ClipRect::SIZE];
    for (&(x, y, width, height), out) in clips.iter().zip(bytes.chunks_exact_mut(ClipRect::SIZE)) {
        ClipRect {
            x1: edge(x),
            y1: edge(y),
            x2: edge(x.saturating_add(width)),
            y2: edge(y.saturating_add(height)),
        }
        .write(out)
        .ok_or_else(|| io::Error::other("a clip larger than its buffer"))?;
    }
    let mut command = FbDirtyCmd {
        fb_id: framebuffer,
        // More clips than the call takes is all of it, which is what
        // none at all says.
        num_clips: if clips.len() > MAX_CLIPS {
            0
        } else {
            u32::try_from(clips.len()).unwrap_or(0)
        },
        clips_ptr: if clips.is_empty() || clips.len() > MAX_CLIPS {
            0
        } else {
            address(&mut bytes)
        },
        ..FbDirtyCmd::ZERO
    };
    ioctl_on(fd, drm::IOCTL_MODE_DIRTYFB, &mut command)
}
