# Builds nvrm, the ring-3 host of NVIDIA's resource manager
# (docs/NVIDIA.md §4.1), statically against ferrousli, Ferrix's own C
# library, for x86-64, and nvrm-core, RM's core linked on its own for nvrm
# to load from the NVIDIA volume.
#
#   make OUT=<dir> [CC=gcc] [FERROUSLI_TARGET=<dir>] [NVIDIA=<dir>]
#
# NVIDIA is what tools/common/fetch/fetch-nvidia.sh wrote: the sources in
# src/ and RM's core, nv-kernel.o, in objects/. The order is fixed by what
# each step needs from the one before:
#
#  1. nvrm's objects: its entry and main (src/), NVIDIA's kept C and the
#     glue (os/), and ferrix-nvos (os/nvos), the OS layer in Rust;
#  2. core-calls.S, nvrm's stubs for every function of the core its
#     objects call, each a jump through a table core.c fills at load;
#  3. nvrm.unpinned, linked with a build-id and a zeroed .nvrm_core_sha256;
#  4. nvrm-core: nv-kernel.o at 0x4000_0000 (core/nvrm-core.ld), its
#     imports bound to nvrm.unpinned's addresses and an export header
#     carrying nvrm's build-id, refused if it has more than the segments
#     the loader takes;
#  5. nvrm: nvrm.unpinned with the core's sha256 written in, refused if
#     that moved anything, or if nvrm is too large for the kernel to read
#     as a driver.
#
# Steps 3 to 5 make nvrm-link-test and nvrm-link-test-core the same way.
#
# `nvrm-hold` is the init `cargo xtask test-nvrm` boots beside nvrm, to
# keep the machine up while nvrm runs. Every program links against crt-less
# ferrousli and libgcc, and nothing from the host's C library.

OUT    ?= $(HOME)/.local/share/ferrix/target-nvidia/nvrm
CC     ?= gcc
LD     ?= ld
NVRM   := $(CURDIR)
NVIDIA ?= $(HOME)/.local/share/ferrix/nvidia/580.173.02
NVSRC  := $(NVIDIA)/src
CORE   := $(NVIDIA)/objects/nv-kernel.o
LINK   := python3 $(NVRM)/core/core-link.py

FERROUSLI        := $(abspath $(NVRM)/../../ferrousli)
FERROUSLI_TARGET ?= $(OUT)/ferrousli
FERROUSLI_LIB    := $(FERROUSLI_TARGET)/release/libferrousli.a
NVOS_LIB         := $(OUT)/nvos/release/libferrix_nvos.a

# The largest nvrm the kernel's driver read takes: one heap block of
# 4 MiB, less half a mebibyte of margin (docs/NVIDIA.md §4.1, "The core").
NVRM_LIMIT := 3670016

# Against ferrousli's headers alone, never the host's, with the stack
# protector on as a distribution's compiler leaves it.
CFLAGS := -std=gnu11 -O2 -g -Wall -Wextra -Werror -nostdinc \
	-isystem $(FERROUSLI)/include -fno-pie -fstack-protector-strong
# The entry runs before ferrousli has set the thread pointer, where the
# canary lives.
START_CFLAGS := $(filter-out -fstack-protector-strong,$(CFLAGS)) -fno-stack-protector

# NVIDIA's kept C and the glue see NVIDIA's interface headers, as Linux's
# glue does, but not as Linux: with __linux__ undefined, nv.h and its
# headers take their portable branch and never reach conftest.h. They
# declare the C library calls they make themselves (os/include/
# nv-ferrix.h), so no libc header is read.
OS_CFLAGS := -std=gnu11 -O2 -g -ffreestanding -nostdinc \
	-isystem $(shell $(CC) -print-file-name=include) \
	-I$(NVRM)/os/include -I$(NVSRC)/kernel-open/common/inc -I$(NVSRC)/kernel-open/nvidia \
	-U__linux__ -U__gnu_linux__ -Ulinux -U__linux \
	-DNVRM -DNV_VERSION_STRING=\"580.173.02\" -DNV_KERNEL_INTERFACE_LAYER -DNV_UNIX \
	-Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers -Werror \
	-fno-strict-aliasing -fno-common -fno-pie

OS_SOURCES := $(wildcard $(NVRM)/os/kept/*.c) $(wildcard $(NVRM)/os/glue/*.c)
OS_OBJECTS := $(patsubst $(NVRM)/os/%.c,$(OUT)/os/%.o,$(OS_SOURCES)) $(OUT)/os/glue/thunks.o

all: $(OUT)/nvrm $(OUT)/nvrm-core $(OUT)/nvrm-link-test $(OUT)/nvrm-link-test-core \
	$(OUT)/nvrm-hold

# cargo decides whether a library is stale, so these always run; the
# programs below are linked again when one wrote a new one.
$(FERROUSLI_LIB): FORCE
	cd $(FERROUSLI) && cargo build --release --lib --target-dir $(FERROUSLI_TARGET)

$(NVOS_LIB): FORCE
	cd $(NVRM)/os/nvos && cargo build --release --target-dir $(OUT)/nvos

$(CORE):
	@echo "nvrm: $(CORE) is missing; run tools/common/fetch/fetch-nvidia.sh" >&2; exit 1

$(OUT)/start.o: $(NVRM)/src/start.c
	@mkdir -p $(OUT)
	$(CC) $(START_CFLAGS) -c $< -o $@

$(OUT)/main.o: $(NVRM)/src/main.c $(NVRM)/src/native.h $(OUT)/core-calls.h
	@mkdir -p $(OUT)
	$(CC) $(CFLAGS) -I$(OUT) -c $< -o $@

$(OUT)/pin.o: $(NVRM)/src/pin.c
	@mkdir -p $(OUT)
	$(CC) $(CFLAGS) -c $< -o $@

# The link test is NVIDIA-facing C like the kept files, and reads one class
# header from NVIDIA's SDK, searched last so that this layer's own nvos.h
# still shadows the SDK's.
$(OUT)/link.o: $(NVRM)/test/link.c $(OUT)/core-calls.h $(wildcard $(NVRM)/os/include/*.h)
	@mkdir -p $(OUT)
	$(CC) $(OS_CFLAGS) -I$(OUT) -idirafter $(NVSRC)/src/common/sdk/nvidia/inc -c $< -o $@

$(OUT)/hold.o: $(NVRM)/test/hold.c
	@mkdir -p $(OUT)
	$(CC) $(CFLAGS) -c $< -o $@

$(OUT)/os/%.o: $(NVRM)/os/%.c $(wildcard $(NVRM)/os/include/*.h)
	@mkdir -p $(dir $@)
	$(CC) $(OS_CFLAGS) -c $< -o $@

$(OUT)/os/glue/thunks.o: $(NVRM)/os/glue/thunks.S
	@mkdir -p $(dir $@)
	$(CC) -c $< -o $@

# Step 2, from the C objects and the Rust library alike (workq.rs calls
# rm_execute_work_item). main.c is not among the objects read: it calls into the core
# only through nvos_core_load's table, and reads core-calls.h, which this
# writes, so reading it would be circular.
$(OUT)/core-calls.S $(OUT)/core-calls.h &: $(OS_OBJECTS) $(NVOS_LIB) $(CORE) $(NVRM)/core/core-link.py
	$(LINK) calls $(CORE) $(OUT)/core-calls.S $(OUT)/core-calls.h $(OS_OBJECTS) $(NVOS_LIB)

$(OUT)/core-calls.o: $(OUT)/core-calls.S
	$(CC) -c $< -o $@

# Steps 3 to 5, for each program: nvrm, started by devmgr from start.c's
# native entry, and nvrm-link-test, an ordinary program (crt1.o) that runs
# RM's no-GPU path on the host and on Ferrix. Each has its own core,
# linked against its own addresses, so each core runs under one program.
#
#   $(call program,NAME,OBJECTS,CRT1)   CRT1 non-empty links ferrousli's crt1.o
define program
$(OUT)/$(1).unpinned: $(2) $(OS_OBJECTS) $(OUT)/pin.o $(OUT)/core-calls.o $(NVOS_LIB) $(FERROUSLI_LIB)
	crt1=$$$$(ls -t $(FERROUSLI_TARGET)/release/build/ferrousli-*/out/crt1.o | head -1) && \
	$(CC) -static -no-pie -nostdlib -Wl,--strip-debug -Wl,--build-id=sha1 \
		-Wl,--fatal-warnings -o $$@ $(if $(3),$$$$crt1) $(2) $(OS_OBJECTS) \
		$(OUT)/pin.o $(OUT)/core-calls.o \
		-Wl,--start-group $(NVOS_LIB) $(FERROUSLI_LIB) -lgcc -Wl,--end-group

$(OUT)/$(1)-imports.ld: $(OUT)/$(1).unpinned $(CORE) $(NVRM)/core/core-link.py
	$(LINK) imports $(CORE) $(OUT)/$(1).unpinned $$@

$(OUT)/$(1)-exports.S: $(OUT)/$(1).unpinned $(OUT)/core-calls.h
	$(LINK) exports $(OUT)/core-calls.h $(OUT)/$(1).unpinned $$@

$(OUT)/$(1)-exports.o: $(OUT)/$(1)-exports.S
	$(CC) -c $$< -o $$@

$(OUT)/$(1)-core: $(NVRM)/core/nvrm-core.ld $(NVRM)/core/core-link.py $(OUT)/$(1)-imports.ld \
		$(OUT)/$(1)-exports.o $(CORE)
	$(LD) -static --no-undefined --fatal-warnings -z noexecstack -o $$@.tmp \
		-T $(NVRM)/core/nvrm-core.ld $(OUT)/$(1)-imports.ld $(OUT)/$(1)-exports.o $(CORE)
	$(LINK) check-core $$@.tmp
	mv $$@.tmp $$@

$(OUT)/$(1): $(OUT)/$(1).unpinned $(OUT)/$(1)-core
	sha256sum $(OUT)/$(1)-core | cut -d' ' -f1 | xxd -r -p > $(OUT)/$(1)-core.sha256
	objcopy --update-section .nvrm_core_sha256=$(OUT)/$(1)-core.sha256 \
		$(OUT)/$(1).unpinned $$@.tmp
	$(LINK) same-layout $(OUT)/$(1).unpinned $$@.tmp
	$(LINK) check-size $$@.tmp $(NVRM_LIMIT)
	mv $$@.tmp $$@
endef

$(eval $(call program,nvrm,$(OUT)/start.o $(OUT)/main.o,))
$(eval $(call program,nvrm-link-test,$(OUT)/link.o,crt1))

$(OUT)/nvrm-hold: $(OUT)/hold.o $(FERROUSLI_LIB)
	crt1=$$(ls -t $(FERROUSLI_TARGET)/release/build/ferrousli-*/out/crt1.o | head -1) && \
	$(CC) -static -no-pie -nostdlib -Wl,--strip-debug -o $@ $$crt1 $(OUT)/hold.o \
		-Wl,--start-group $(FERROUSLI_LIB) -lgcc -Wl,--end-group

clean:
	rm -rf $(OUT)

.PHONY: all clean FORCE
FORCE:
