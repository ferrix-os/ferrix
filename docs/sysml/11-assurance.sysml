/*
 * Ferrix — SysML v2 model, part 11: assurance.
 *
 * docs/RELIABILITY.md and docs/ASSEMBLY.md: the quality gates, what each one
 * verifies, and what the tests can actually reach. The gates cargo xtask
 * check runs are in CI. Of the xtask boot gates, CI runs test-boot and
 * test-rustc; the ones that need a binary the repository does not carry, a
 * disk judged on the host or a screendump run in the landing gates of
 * docs/BACKLOG.md instead (docs/ROADMAP.md, Continuously).
 */
package FerrixAssurance {
    private import ScalarValues::*;
    private import VerificationCases::*;
    private import FerrixLifecycle::*;
    private import FerrixStructure::*;
    private import FerrixRequirements::Principles::*;

    verification def Gate {
        doc /* A check that fails the build. Cheapest first in cargo xtask
             * check, so the gate most likely to fail on a work-in-progress
             * tree fails first. */
        subject os : Ferrix;
        attribute command : String;
    }

    #implemented verification hooksArmed : Gate {
        attribute :>> command = "python3 tools/common/check/check-commit-authors.py --hooks";
        doc /* The first gate in cargo xtask check: fails when this clone has
             * not run `git config core.hooksPath .githooks`, so the missing
             * line reports itself rather than waiting to be noticed. */
        objective { verify aGateNeedsNoArming; }
    }
    #implemented verification commitAuthorship : Gate {
        attribute :>> command = "python3 tools/common/check/check-commit-authors.py BASE HEAD";
        doc /* The One-author-per-commit CI job, over the range a push or pull
             * request adds and never the whole tree, so public history that
             * already carries a trailer cannot make the gate permanent red.
             * Matches a trailer at the start of a line only, so prose about
             * the rule passes. The commit-msg and pre-push hooks check the
             * same thing a second earlier, where the fix is a message edit
             * rather than a rebase; they are dormant until armed, and this
             * job is the one that is not. */
        objective { verify oneAuthorPerCommit; verify aGateNeedsNoArming; }
    }
    #implemented verification formatting : Gate {
        attribute :>> command = "cargo fmt --all -- --check";
    }
    #implemented verification lineEndings : Gate {
        attribute :>> command = "python3 tools/common/check/check-line-endings.py";
        doc /* A CRLF in a shell script makes its shebang unparseable. */
    }
    #implemented verification assemblyAllowList : Gate {
        attribute :>> command = "python3 tools/common/check/check-asm-budget.py";
        doc /* Fails on an assembly site not in tools/common/data/asm-allowlist.json, on
             * a file over its budget, and on a stale entry. Reports the Rust
             * share on every run. */
        objective { verify assemblyOnlyWhereTheMachineDefinesIt; }
    }
    #implemented verification deviceAccessAllowList : Gate {
        attribute :>> command = "python3 tools/common/check/check-device-access.py";
        doc /* The seam docs/ARCHITECTURE.md §7 is built on: the kernel
             * enumerates devices and drives none, so a register access in
             * the wrong file fails the build rather than waiting for a
             * review. */
    }
    #implemented verification unsafeAudit : Gate {
        attribute :>> command = "python3 tools/common/check/check-unsafe-audit.py";
        doc /* Every unsafe block has a SAFETY comment and one operation;
             * every unsafe fn a Safety section. Prints the per-crate count so
             * growth is noticed rather than happening. */
        objective { verify unsafeIsExpensive; }
    }
    #implemented verification panicAudit : Gate {
        attribute :>> command = "python3 tools/common/check/check-panic-audit.py";
        doc /* Every panic-lint exemption is an #[expect] whose reason begins
             * AUDIT:. expect fails once the lint stops firing, so a site
             * refactored into safety loses its exemption. */
        objective { verify noReachablePanic; }
    }
    #implemented verification traceability : Gate {
        attribute :>> command = "python3 tools/common/check/check-traceability.py --check";
        doc /* The certified item's requirements (part 13) against their
             * parents, their units and the checks that name them with
             * `/// Verifies:`. Fails on an id no requirement defines, a
             * requirement with no statement, criterion or parent, a unit that
             * does not resolve, and a requirement no check names that is not
             * in the baseline of the unverified, which may only shrink.
             * Writes docs/certification/TRACEABILITY.md. */
    }
    #implemented verification architectureDocument : Gate {
        attribute :>> command = "python3 tools/common/gen/sysml/tests.py && python3 tools/common/gen/gen-arch-doc.py --check";
        doc /* docs/generated is generated from this model and committed; a
             * model edited without regenerating fails here. */
    }
    #implemented verification waylandProtocolTables : Gate {
        attribute :>> command = "python3 tools/common/gen/gen-wayland-protocol.py --check";
        doc /* The compositor's interface tables against the protocol XML
             * vendored beside them. */
    }
    #implemented verification xkbTables : Gate {
        attribute :>> command = "python3 tools/common/gen/gen-xkb-tables.py --check";
        doc /* The keymap every client is handed, and its modifier bits,
             * against libxkbcommon's own output through a committed probe. */
    }
    #implemented verification panicFont : Gate {
        attribute :>> command = "python3 tools/common/gen/gen-font.py --check";
        doc /* The panic screen's font against the BDF committed beside it. */
    }
    #implemented verification terminalFont : Gate {
        attribute :>> command = "cargo xtask check (terminal font: the term app's tools/gen-font.py --check)";
        doc /* The terminal's font, rasterised from the TrueType faces
             * committed beside it by a rasteriser in the repository, so the
             * result is byte-identical on every checkout. */
    }
    #implemented verification panicCatalog : Gate {
        attribute :>> command = "python3 tools/common/gen/gen-panic-catalog.py --check";
        doc /* The explanations a panic prints, rendered into a document that
             * goes stale the moment an entry changes without it. */
    }
    #implemented verification crateLayering : Gate {
        attribute :>> command = "tools/common/check/check-crate-layering.sh";
        doc /* src/lib/ depend on nothing above them; generic kernel code names
             * no architecture; cfg(target_arch) only under arch/. */
        objective { verify pureFunctionsInLibs; verify oneArchFacade; }
    }
    #implemented verification clippy : Gate {
        attribute :>> command = "cargo clippy -- -D warnings";
        doc /* Ten runs: the host (libraries and xtask), and the kernel, the
             * loader, and the native runtime with its programs for each of
             * the three targets, because cfg(target_arch) code is invisible
             * to a lint pass for another machine. The lint table denies
             * unwrap, expect, panic, unreachable, indexing and string slicing
             * in production code. */
        attribute runs : Natural = 10;
    }
    #implemented verification hostTests : Gate {
        attribute :>> command = "cargo test --workspace --exclude ferrix-kernel --exclude ferrix-boot";
        doc /* 1950 unit tests across src/lib/ on 2026-09-23 (about 520 when
             * this gate was first written down), plus doc tests and xtask's
             * 242. */
    }
    #implemented verification cargoDeny : Gate {
        attribute :>> command = "cargo deny check";
    }
    #implemented verification docsBuild : Gate {
        attribute :>> command = "cargo doc";
        doc /* Broken intra-doc links are denied. */
    }
    #implemented verification miri : Gate {
        attribute :>> command = "cargo miri test";
        doc /* src/lib/platform/elf, bootinfo, ustack, objects, vfs, pci, block, blkring, native, virtio-blk, frame, heap and paging. cargo xtask check --miri runs the same list,
             * and an xtask test holds it to the workflow. A test too slow to
             * interpret runs shorter under cfg(miri) rather than being
             * ignored. */
    }
    #implemented verification fuzz : Gate {
        attribute :>> command = "cargo fuzz run <target>, for every target cargo fuzz list names";
        doc /* Replays the committed corpus first, searches second: an input
             * that crashed once fails again in seconds. ustack_build asserts a
             * round trip rather than the absence of a crash -- whatever the
             * builder accepts, a walk that knows only the stack pointer must
             * recover exactly what went in -- which is how it found that a
             * string containing a NUL built an image that read back short. */
    }
    #implemented verification buildImages : Gate {
        attribute :>> command = "cargo xtask build --arch all --release";
        doc /* One bootable FAT32 image per architecture, byte-for-byte
             * reproducible, uploaded as a CI artifact. xtask refuses to build
             * a kernel with RUSTFLAGS set, because cargo lets that variable
             * replace the per-target flags and silently drop the linker
             * script. */
    }
    #implemented verification bootTest : Gate {
        attribute :>> command = "cargo xtask test-boot --arch all";
        doc /* The gate that answers the question the others cannot. An OS
             * that compiles and does not boot is not a passing build. */
        objective { verify everyStageEndsInSomethingThatRuns; verify provedOnEveryBoot; }
    }

    // -----------------------------------------------------------------------
    // The assembly budget
    // -----------------------------------------------------------------------

    attribute def AssemblySite {
        attribute file : String;
        attribute maxLines : Natural;
    }

    #implemented part def AssemblyBudget {
        doc /* An absolute cap, because assembly here is a fixed cost that a
             * scheduler, a filesystem or a driver must add nothing to. The
             * ratio is a backstop and the target is the direction of travel:
             * reached by writing the operating system, not by shrinking the
             * vector table. 303 lines and about 99.2% Rust when this was
             * written, across fifteen sites; tools/common/data/asm-allowlist.json now
             * names twenty-four: the x86-64 SYSCALL entry and the native
             * runtime's three (docs/ASSEMBLY.md, A native program), the
             * Pixel 7's loader entry, the x86-64 vDSO and the three
             * side-channel sequences having joined them. The cap was 800 until 2026-09-27, when the count
             * learned to read raw strings and found 1569 lines, not 733
             * (docs/ASSEMBLY.md, The budget). */
        attribute maxTotalLines : Natural = 1600;
        attribute maxRatio : Real = 0.06;
        attribute targetRatio : Real = 0.001;
        attribute sites : AssemblySite[24] = (
            AssemblySite(file = "src/boot/common/uefi/src/arch/x86_64.rs", maxLines = 30),
            AssemblySite(file = "src/boot/common/uefi/src/arch/aarch64.rs", maxLines = 60),
            AssemblySite(file = "src/boot/common/uefi/src/arch/armv7a.rs", maxLines = 63),
            AssemblySite(file = "src/kernel/src/arch/x86_64/cpu.rs", maxLines = 100),
            AssemblySite(file = "src/kernel/src/arch/aarch64/cpu.rs", maxLines = 100),
            AssemblySite(file = "src/kernel/src/arch/armv7a/cpu.rs", maxLines = 102),
            AssemblySite(file = "src/kernel/src/arch/x86_64/trap.rs", maxLines = 171),
            AssemblySite(file = "src/kernel/src/arch/aarch64/trap.rs", maxLines = 145),
            AssemblySite(file = "src/kernel/src/arch/armv7a/trap.rs", maxLines = 130),
            AssemblySite(file = "src/kernel/src/arch/x86_64/smp.rs", maxLines = 41),
            AssemblySite(file = "src/kernel/src/arch/aarch64/smp.rs", maxLines = 58),
            AssemblySite(file = "src/kernel/src/arch/armv7a/smp.rs", maxLines = 40),
            AssemblySite(file = "src/kernel/src/arch/x86_64/switch.rs", maxLines = 32),
            AssemblySite(file = "src/kernel/src/arch/aarch64/switch.rs", maxLines = 75),
            AssemblySite(file = "src/kernel/src/arch/armv7a/switch.rs", maxLines = 67),
            AssemblySite(file = "src/kernel/src/arch/x86_64/syscall.rs", maxLines = 118),
            AssemblySite(file = "src/user/system/native/rt/src/arch/x86_64.rs", maxLines = 20),
            AssemblySite(file = "src/user/system/native/rt/src/arch/aarch64.rs", maxLines = 20),
            AssemblySite(file = "src/user/system/native/rt/src/arch/armv7a.rs", maxLines = 20),
            AssemblySite(file = "src/boot/vendor/google/pixel7/src/entry.rs", maxLines = 130),
            AssemblySite(file = "src/kernel/src/arch/x86_64/vdso.rs", maxLines = 120),
            AssemblySite(file = "src/kernel/src/arch/x86_64/speculation.rs", maxLines = 16),
            AssemblySite(file = "src/kernel/src/arch/aarch64/speculation.rs", maxLines = 16),
            AssemblySite(file = "src/kernel/src/arch/armv7a/speculation.rs", maxLines = 16)
        );
        #implemented attribute syscallEntrySites : AssemblySite[1] {
            @stage { number = 6; }
            doc /* The SYSCALL / SVC trampolines: the last entries the list was
                 * meant to gain. Only x86-64 needed one,
                 * src/kernel/src/arch/x86_64/syscall.rs, in the list above: on the
                 * Arm pair svc arrives through the vector table already
                 * there. */
        }
    }

    // -----------------------------------------------------------------------
    // Reachability: what each layer's tests can drive
    // -----------------------------------------------------------------------

    enum def TestReach {
        CargoTest;
        Miri;
        Fuzzer;
        QemuBoot;
    }

    part def LayerReach {
        attribute layer : String;
        attribute reach : TestReach[1..*];
    }

    part libsReach : LayerReach {
        attribute :>> layer = "src/lib/";
        attribute :>> reach = (TestReach::CargoTest, TestReach::Miri, TestReach::Fuzzer);
    }
    part bootReach : LayerReach {
        attribute :>> layer = "src/boot/common/uefi/";
        attribute :>> reach = (TestReach::QemuBoot);
    }
    part kernelReach : LayerReach {
        attribute :>> layer = "src/kernel/";
        attribute :>> reach = (TestReach::QemuBoot);
        doc /* Miri cannot interpret a privileged instruction and a fuzzer
             * cannot drive a page-fault handler, which is the whole argument
             * for src/lib/. */
    }
    part xtaskReach : LayerReach {
        attribute :>> layer = "tools/common/xtask/";
        attribute :>> reach = (TestReach::CargoTest);
    }

    // -----------------------------------------------------------------------
    // Verification the later stages owe, as the architecture states it
    // -----------------------------------------------------------------------

    #implemented verification def BtrfsCheck {
        @stage { number = 12; }
        doc /* Every boot writes a tree on a blank volume real mkfs.btrfs
             * made, and cargo xtask test-btrfs hands the result to host btrfs
             * check. A filesystem only our own reader can read is not a
             * filesystem. */
        subject os : Ferrix;
    }
    #implemented verification def PowerFailInjection {
        @stage { number = 12; }
        doc /* Kill QEMU at a random point inside a transaction, remount,
             * replay, btrfs check again, over hundreds of seeds: cargo xtask
             * test-powerfail. Its adversarial half, every unflushed write
             * kept or dropped at random, is src/lib/fs/btrfs-write's powerfail
             * tests. The test a CoW filesystem actually has to pass. */
        subject os : Ferrix;
        attribute seeds : Natural;
    }

    // The other xtask boot gates, each an exit criterion that needs something
    // the boot test cannot carry. test-jobs and test-rustc are JobsSession
    // and RustcTest in FerrixRoadmap.
    #implemented verification def ShellTest {
        @stage { number = 7; }
        doc /* cargo xtask test-shell: a static busybox named with --init runs
             * a builtins-only script given to sh -c, and its lines and its
             * exit status of 7 are required after the boot marker; without
             * --init it runs zinc. All three architectures. */
        subject os : Ferrix;
    }
    #implemented verification def ThreadsTest {
        @stage { number = 7; }
        doc /* cargo xtask test-threads: src/tests/threads/, a static musl Rust
             * program of five threads using std::thread, Mutex and mpsc,
             * booted as init on all three architectures, counting its
             * threads through /proc/self; a build that expects one thread
             * too many must fail on that count. */
        subject os : Ferrix;
    }
    #implemented verification def VfsTest {
        @stage { number = 8; }
        doc /* cargo xtask test-vfs: ls -R /proc, cat /proc/self/maps and a
             * shell script whose applets are forked programs, on all three
             * architectures; then su becomes the image's ferrix user, and
             * every refusal the kernel owes a user who is not root is
             * required of it. */
        subject os : Ferrix;
    }
    #implemented verification def NetTest {
        doc /* cargo xtask test-net, Networking's exit, under xtask's own NAT
             * gateway: eth0 configured with ip, route and netstat reporting
             * through /proc/net, a name resolved, a file fetched byte for
             * byte, and a stream over the loopback, on all three
             * architectures. */
        subject os : Ferrix;
    }
    #implemented verification def RestartTest {
        @stage { number = 10; }
        doc /* cargo xtask test-restart, x86-64: a shell kills the gpu driver
             * twice, and devmgr's DIED, the driver started again and card0
             * published again are required each time, with the shell still
             * answering. test-compositor --boot restart does the same under
             * the compositor. */
        subject os : Ferrix;
    }
    #implemented verification def DisplayTest {
        @stage { number = 17; }
        doc /* cargo xtask test-display: a compositor's colour on card0,
             * served by the ring-3 virtio-gpu driver, required pixel for
             * pixel from QEMU's screendump, on x86-64 and AArch64; with --gl
             * the render node as well. */
        subject os : Ferrix;
    }
    #implemented verification def InputTest {
        @stage { number = 17; }
        doc /* cargo xtask test-input: a key and a touch sent in at QEMU's far
             * end over QMP and required back out of /dev/input/eventN, on
             * x86-64 and AArch64, with a negative control that must fail. */
        subject os : Ferrix;
    }
    #implemented verification def SeatTest {
        @stage { number = 17; }
        doc /* cargo xtask test-seat: keys typed at QEMU's far end reach a
             * window on Ferrix and change the screen, and a keybind closes
             * the window it is bound to close, on x86-64 and AArch64. */
        subject os : Ferrix;
    }
    #implemented verification def CompositorTest {
        @stage { number = 18; }
        doc /* cargo xtask test-compositor: the compositor booted as init,
             * and two Wayland clients tiled on the card, pixel for pixel as
             * the renderer draws them, on x86-64 and AArch64; a later boot
             * requires the picture the terminal makes. */
        subject os : Ferrix;
    }
    #implemented verification def PtyTest {
        @stage { number = 18; }
        doc /* cargo xtask test-pty: the term app's term --headless runs a program
             * on a pseudoterminal pair, and what it wrote comes back through
             * the master and the terminal's grid. */
        subject os : Ferrix;
    }
    #implemented verification def VideoTest {
        @stage { number = 19; }
        doc /* cargo xtask test-video: a four-frame AV1 test pattern checked
             * into the repository plays as a wallpaper, and the screen must
             * show its frames in turn, where QEMU's machine has a
             * virtio-gpu. */
        subject os : Ferrix;
    }
    #planned verification def CyclicTest {
        @stage { number = 14; }
        doc /* Wake-up latency on a HardRt domain while a Throughput domain on
             * other cores is saturated; the maximum must be inside the stated
             * bound. */
        subject os : Ferrix;
        attribute maxLatencyNanos : Natural;
    }
}
