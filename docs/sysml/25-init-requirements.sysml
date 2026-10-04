/*
 * Ferrix — SysML v2 model, part 25: the low-level requirements of pid 1's
 * inputs.
 *
 * What init does with what an image gives it to start as pid 1, as
 * `ItemLowLevel` requirements in the pilot's format (part 13 defines it,
 * part 14 is the pilot). Since 2026-10-04 the program init starts when
 * nothing is named, the script for its `sh -c` and the list of commands are
 * not compiled into the kernel: an image carries them in its initramfs under
 * `.ferrix/init/`, and `fs::init` reads them where the archive is and hands
 * them to `init::set_inputs` (docs/certification/ITEM.md section 2). The
 * certification consultant's OK IF of 2026-10-04 (ledger lines 328 and 332)
 * asked for these rows and their parent, H.BOOT.15 in part 13; that
 * `ferrix-vfs`'s unpacker, in no ring, creates none of the inputs is
 * SAFETY-MANUAL AoU-24 rather than a row (line 333), and L.init.4, reserved
 * for it, is not written.
 *
 * What proves what. `init::check` (the `inputs` line, FX-1503) judges sets of
 * entries made up as an archive could carry them. xtask's `test-shell`, `test-vfs` and `test-init` boot each way init can
 * start pid 1.
 *
 * Ids are `L.init.<n>`. They are never renumbered once a check names them.
 */
package FerrixInitRequirements {
    private import FerrixItemRequirements::*;

    package Inputs {
        doc /* What pid 1 is started with, and from where. */

        requirement <'L.init.1'> pid1InItsOrder : ItemLowLevel {
            attribute :>> statement = "run shall start pid 1 from the file
                ferrix.init= names when it starts, else run the commands input,
                else start the program input with sh -c and the script input or
                sh -i, else start /sbin/init.";
            attribute :>> criterion = "test-shell's boot starts its program input
                with `sh -c` and its script, test-vfs's runs its command list, and
                test-init's starts the file ferrix.init names.";
            attribute :>> parent = "H.BOOT.15";
            attribute :>> unit = ("init::run", "init::run_built_in", "init::run_default",
                "init::run_file", "init::run_commands");
        }
        requirement <'L.init.2'> inputsFromTheBootArchiveOnly : ItemLowLevel {
            attribute :>> statement = "set_inputs shall take pid 1's program, script
                and commands only from the boot initramfs's regular entries
                .ferrix/init/program, .ferrix/init/script and .ferrix/init/commands,
                once, an input the archive does not carry being empty.";
            attribute :>> criterion = "All three entries give all three inputs, none
                gives three empty ones, and a second set_inputs is refused and leaves
                the inputs as they were (the `inputs` line).";
            attribute :>> parent = "H.BOOT.15";
            attribute :>> unit = ("init::set_inputs", "init::inputs");
        }
        requirement <'L.init.3'> anInputThatCannotBeOneIsRefused : ItemLowLevel {
            attribute :>> statement = "judge shall refuse, with a line naming the
                entry and why, a second entry of an input's name, an input that is
                not a regular file or has more than one link, any other name under
                .ferrix, a script holding a NUL and a command list not ending in two
                NULs, and take that input as absent.";
            attribute :>> criterion = "Each of seven malformed sets of entries is
                refused once and gives the inputs set_inputs says, the rest taken as
                given (the `inputs` line).";
            attribute :>> parent = "H.BOOT.15";
            attribute :>> unit = ("init::judge", "init::refuse");
        }
    }
}
