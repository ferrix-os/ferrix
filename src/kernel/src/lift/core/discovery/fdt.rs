use ferrix_bootinfo::BootView;
use ferrix_fdt::Fdt;
pub fn open(view: &BootView<'_>) -> Result<Fdt<'static>, &'static str> {
    Err("linuxd reads no device tree yet (M5-LIFT.md §2.1)")
}
