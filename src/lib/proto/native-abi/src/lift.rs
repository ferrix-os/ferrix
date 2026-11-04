//! What the kernel gives `linuxd` at its start (`docs/microkernel/M5-LIFT.md` §6).

/// `CLOCK_READ`'s question for the counter's frequency: the TSC's, in hertz, when
/// the kernel's clock is the TSC and ring 3 can make the same sum; 0 when it is not.
pub const CLOCK_COUNTER_HZ: usize = 2;

/// `linuxd`'s exit status for `reboot(2)`'s restart: the kernel resets the machine.
pub const EXIT_RESTART: i32 = 82;
/// `linuxd`'s exit status for `reboot(2)`'s power-off.
pub const EXIT_POWER_OFF: i32 = 81;
/// `linuxd`'s exit status for `reboot(2)`'s halt.
pub const EXIT_HALT: i32 = 83;

/// One `SPACE_PLACE` entry's first word: the page-aligned virtual address, or'd with
/// these. The second is the VMO's byte offset, page-aligned.
pub const PLACE_WRITE: u64 = 1;
/// See [`PLACE_WRITE`].
pub const PLACE_EXECUTE: u64 = 2;
/// The most entries one `SPACE_PLACE` takes.
pub const PLACE_MOST: usize = 64;

/// A boot page's first word: "FXLINUXD" backwards in memory order.
pub const BOOT_MAGIC: u64 = u64::from_le_bytes(*b"FXLINUXD");

/// The boot page, at the address `linuxd`'s first argument register names: its
/// handles, in the shared table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct BootPage {
    /// [`BOOT_MAGIC`].
    pub magic: u64,
    /// The partition's region.
    pub region: u32,
    /// The partition's job.
    pub job: u32,
    /// The initramfs, read only.
    pub initramfs: u32,
    /// The debug log's write handle.
    pub debug_log: u32,
    /// The console's input.
    pub console: u32,
    /// `devmgr`'s channel, what was the kernel's end; 0 with no `devmgr`
    /// (`docs/microkernel/BOOT-AUTHORITY.md` §3).
    pub devmgr: u32,
    /// The device cores' channel, `devmgr`'s requests for a driver's control
    /// channel; 0 with no `devmgr`.
    pub cores: u32,
    /// `btrfsd`'s channel under `fs=server`; 0 in any other build.
    pub files: u32,
    /// The initramfs's length in bytes.
    pub initramfs_bytes: u64,
    /// The kernel command line's length: its bytes start at [`COMMAND_LINE_AT`].
    pub command_line_bytes: u32,
    /// How many processors there are.
    pub processors: u32,
    /// The frames linuxd may hold, its arena's size in use: what the kernel
    /// could give it at its start, so a program's memory runs out here, where the
    /// load's reclaim and OOM answer, before the kernel's does.
    pub memory_frames: u64,
    /// The channel `netd` opens this partition's rings on; 0 with no `devmgr`.
    pub net_cores: u64,
    /// How many [`DeviceFact`]s are at [`DEVICES_AT`] in the page.
    pub devices: u64,
    /// The time of day firmware gave at boot, in nanoseconds since the epoch; 0 for none.
    pub wall_clock: u64,
}

/// Where in the boot page the command line starts.
pub const COMMAND_LINE_AT: usize = 256;
/// The command line's room: it ends where the device table starts.
pub const COMMAND_LINE_MOST: usize = DEVICES_AT - COMMAND_LINE_AT;
/// Where in the boot page the device table starts.
pub const DEVICES_AT: usize = 1024;
/// The most device facts a boot page holds: past them a device is not in sysfs.
pub const DEVICES_MOST: usize = (4096 - DEVICES_AT) / DeviceFact::BYTES;

/// What the kernel's enumeration read of one PCI function: all linuxd knows of a
/// device, for sysfs. Its handle stays devmgr's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceFact {
    /// Segment in bits 31:16, bus in 15:8, device and function in 7:0.
    pub location: u32,
    /// Its vendor ID.
    pub vendor: u16,
    /// Its device ID.
    pub device: u16,
    /// Its class code.
    pub class: u32,
    /// Its subsystem's vendor ID.
    pub subsystem_vendor: u16,
    /// Its subsystem ID.
    pub subsystem: u16,
    /// Its revision.
    pub revision: u8,
    /// The bus a bridge leads to, 0xff for none.
    pub secondary_bus: u8,
}

impl DeviceFact {
    /// One fact's bytes.
    pub const BYTES: usize = 20;

    /// As the boot page holds it.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0_u8; Self::BYTES];
        out[0..4].copy_from_slice(&self.location.to_le_bytes());
        out[4..6].copy_from_slice(&self.vendor.to_le_bytes());
        out[6..8].copy_from_slice(&self.device.to_le_bytes());
        out[8..12].copy_from_slice(&self.class.to_le_bytes());
        out[12..14].copy_from_slice(&self.subsystem_vendor.to_le_bytes());
        out[14..16].copy_from_slice(&self.subsystem.to_le_bytes());
        out[16] = self.revision;
        out[17] = self.secondary_bus;
        out
    }

    /// From [`Self::to_bytes`].
    #[must_use]
    pub const fn from_bytes(bytes: &[u8; Self::BYTES]) -> DeviceFact {
        let [
            l0,
            l1,
            l2,
            l3,
            v0,
            v1,
            d0,
            d1,
            c0,
            c1,
            c2,
            c3,
            sv0,
            sv1,
            s0,
            s1,
            revision,
            secondary_bus,
            ..,
        ] = *bytes;
        DeviceFact {
            location: u32::from_le_bytes([l0, l1, l2, l3]),
            vendor: u16::from_le_bytes([v0, v1]),
            device: u16::from_le_bytes([d0, d1]),
            class: u32::from_le_bytes([c0, c1, c2, c3]),
            subsystem_vendor: u16::from_le_bytes([sv0, sv1]),
            subsystem: u16::from_le_bytes([s0, s1]),
            revision,
            secondary_bus,
        }
    }
}

impl BootPage {
    /// Nothing yet.
    pub const EMPTY: BootPage = BootPage {
        magic: 0,
        region: 0,
        job: 0,
        initramfs: 0,
        debug_log: 0,
        console: 0,
        devmgr: 0,
        cores: 0,
        files: 0,
        devices: 0,
        initramfs_bytes: 0,
        command_line_bytes: 0,
        processors: 0,
        memory_frames: 0,
        net_cores: 0,
        wall_clock: 0,
    };

    /// As the kernel writes it.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 88] {
        let mut out = [0_u8; 88];
        let words = [
            self.magic,
            u64::from(self.region) | u64::from(self.job) << 32,
            u64::from(self.initramfs) | u64::from(self.debug_log) << 32,
            u64::from(self.console) | u64::from(self.devmgr) << 32,
            u64::from(self.cores) | u64::from(self.files) << 32,
            self.initramfs_bytes,
            u64::from(self.command_line_bytes) | u64::from(self.processors) << 32,
            self.memory_frames,
            self.net_cores,
            self.devices,
            self.wall_clock,
        ];
        for (slot, word) in out.chunks_exact_mut(8).zip(words) {
            slot.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// What `OBJECT_KIND` answers, in bits 32 up: what crosses to linuxd's device cores.
pub const KIND_VMO: u64 = 1;
/// A port.
pub const KIND_PORT: u64 = 2;
/// A channel end.
pub const KIND_CHANNEL: u64 = 3;
/// Anything else, which no core is handed.
pub const KIND_OTHER: u64 = 4;
