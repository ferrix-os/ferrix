use ferrix_bootinfo::BootView;
use ferrix_bootinfo::MemKind;
use ferrix_bootinfo::MemRegion;
use ferrix_bootinfo::SMBIOS_ENTRY_BYTES;
use ferrix_smbios::Error;
use ferrix_smbios::Smbios;
use ferrix_smbios::TABLE_MAX;
use ferrix_smbios::Value;
use ferrix_sync::Once;
pub fn tables() -> Option<Smbios<'static>> {
    None
}
