/* The selftest as Ferrix boots it: as init, built into the kernel, which
 * starts it as `sh -c <script>` (src/kernel/src/init.rs, run_built_in). The
 * harness would refuse `-c`, so its main is renamed and this one passes it no
 * arguments: every test, in order. */
#define main seccomp_bpf_main
#include "seccomp/seccomp_bpf.c"
#undef main

int main(void)
{
	char *argv[] = { "seccomp_bpf", 0 };

	return seccomp_bpf_main(1, argv);
}
