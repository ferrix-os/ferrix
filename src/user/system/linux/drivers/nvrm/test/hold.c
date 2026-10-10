/*
 * The init of `cargo xtask test-nvrm`'s boots: it holds the machine up
 * while nvrm, which devmgr started before init, says what it found. The
 * gate stops QEMU once it has read the line it waits for; this only keeps
 * the kernel from powering off first, as it does when init exits.
 *
 * It also asks nvrm to quiesce (docs/NVIDIA.md §13), as whoever takes the
 * machine away does: the skeleton has no GPU to shut down, and says so,
 * which is how a boot without the card shows that the request is seen.
 */
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

/* nvrm's QUIESCE_ASK (src/main.c). */
#define QUIESCE_ASK "/run/nvrm-quiesce"

int main(void)
{
	(void)mkdir("/run", 0755);
	int ask = open(QUIESCE_ASK, O_WRONLY | O_CREAT, 0600);
	if (ask >= 0)
		(void)close(ask);
	else
		dprintf(2, "nvrm-hold: cannot create %s\n", QUIESCE_ASK);
	for (;;)
		sleep(3600);
}
