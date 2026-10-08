# Boot Linux 7.2.9 on the STM32MP157D-DK1 for the board bench
# (docs/BOARD-BENCH.md, item B1): one U-Boot command per line, sent one at a
# time (two lines back to back overflow this U-Boot's UART input). Not yet run
# on the board.
#
# Files on the card's bootfs partition (mmc 0:4, FAT32), from build.sh's
# out/card/:
#   bench/linux/zImage
#   bench/linux/stm32mp157a-dk1.dtb   Linux 7.2.9's own tree, not ${fdtcontroladdr}
#   bench/linux/initramfs.cpio.gz     a raw gzip cpio; bootz takes it as
#                                     address:size (SUPPORT_RAW_INITRD=y)
#
# Memory: DDR is 0xc0000000-0xdfffffff and OP-TEE owns the top 32 MiB,
# 0xde000000-0xdfffffff. The load addresses are this U-Boot's stm32mp15
# defaults (kernel_addr_r, fdt_addr_r, ramdisk_addr_r), written out:
#   0xc2000000  zImage, 12.4 MB, up to 0xc2bd5000
#   0xc4000000  the tree, 56 KB
#   0xc4400000  the initramfs, under 1 MB
# The zImage decompresses itself to 0xc0008000 (moving itself first if the
# kernel would overlap it), and bootz moves the tree and the initramfs up into
# free memory below U-Boot, which sits below OP-TEE.
#
# OP-TEE's reservation: Linux's stm32mp157a-dk1 tree, unlike its -scmi
# variant, reserves nothing for OP-TEE, and Linux takes its first pages from
# the top of memory. This U-Boot copies OP-TEE's own reserved-memory nodes into
# the tree it boots (OPTEE_LIB). The five `fdt` lines add the -scmi tree's
# node as well, so the reservation does not depend on that; lbench prints the
# reserved-memory nodes and /proc/iomem it got, which shows whether they are
# needed. `fdt addr` sets only the running environment's fdtaddr; nothing here
# saves the environment.
#
# cpufreq.off=1: the tree has no OPP table, so there is no cpufreq driver to
# stop; the MPU stays at the 650 MHz TF-A set. panic=-1 resets on a panic, and
# lbench resets after `board-bench end lbench`: either way back to U-Boot.

# ---- Entry 1, the main table: one core.
setenv bootargs console=ttySTM0,115200 maxcpus=1 cpufreq.off=1 panic=-1
load mmc 0:4 0xc2000000 bench/linux/zImage
load mmc 0:4 0xc4000000 bench/linux/stm32mp157a-dk1.dtb
fdt addr 0xc4000000
fdt resize 1024
fdt mknode /reserved-memory optee@de000000
fdt set /reserved-memory/optee@de000000 reg <0xde000000 0x2000000>
fdt set /reserved-memory/optee@de000000 no-map
load mmc 0:4 0xc4400000 bench/linux/initramfs.cpio.gz
bootz 0xc2000000 0xc4400000:${filesize} 0xc4000000

# ---- Entry 2, the second table: both cores. The same lines; only bootargs
# differs.
setenv bootargs console=ttySTM0,115200 cpufreq.off=1 panic=-1
load mmc 0:4 0xc2000000 bench/linux/zImage
load mmc 0:4 0xc4000000 bench/linux/stm32mp157a-dk1.dtb
fdt addr 0xc4000000
fdt resize 1024
fdt mknode /reserved-memory optee@de000000
fdt set /reserved-memory/optee@de000000 reg <0xde000000 0x2000000>
fdt set /reserved-memory/optee@de000000 no-map
load mmc 0:4 0xc4400000 bench/linux/initramfs.cpio.gz
bootz 0xc2000000 0xc4400000:${filesize} 0xc4000000
