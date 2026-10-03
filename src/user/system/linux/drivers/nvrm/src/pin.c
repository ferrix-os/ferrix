/*
 * The sha256 of the one nvrm-core this nvrm runs (docs/NVIDIA.md §4.1,
 * "The core"). The core is linked after nvrm, against nvrm's addresses, so
 * its hash cannot be known when nvrm is: the section is linked as zeros and
 * the Makefile writes the hash in with objcopy --update-section, which
 * changes these 32 bytes and nothing else (core-link.py same-layout checks
 * that). A section still all zero is refused at load.
 *
 * The pin protects the product, so that nvrm runs only the core it was
 * built with. It is not an argument for the certified item.
 *
 * SPDX-License-Identifier: MIT
 */
__attribute__((section(".nvrm_core_sha256"), used, aligned(8)))
const unsigned char nvrm_core_sha256[32] = { 0 };
