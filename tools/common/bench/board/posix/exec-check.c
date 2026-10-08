/* exec-check.c: does lat_proc's exec really run hello? (B5, docs/BOARD-BENCH.md)
 *
 * lmbench's lat_proc times fork+execve of /tmp/hello and fork+/bin/sh -c
 * /tmp/hello, but never reads the child's exit status, so a kernel whose
 * execve fails still gets a figure: fork plus a failed exec. This makes the
 * same calls lat_proc makes, once each, with lat_proc's arguments (an execve
 * with a null environment, stdout closed), and says whether hello ran.
 *
 * Output, one line per call:
 *   exec-check <execve|shell> ok
 *   exec-check <execve|shell> FAIL <why>
 * Exit status: the number of FAIL lines.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#define PROG "/tmp/hello"

static int check(const char *what, int shell)
{
	char *nav[2];
	int status;
	pid_t pid;

	fflush(stdout);
	pid = fork();
	if (pid < 0) {
		printf("exec-check %s FAIL fork: errno %d (%s)\n", what, errno, strerror(errno));
		return 1;
	}
	if (pid == 0) {
		close(1);
		if (shell) {
			execlp("/bin/sh", "sh", "-c", PROG, (char *)0);
		} else {
			nav[0] = PROG;
			nav[1] = 0;
			execve(PROG, nav, 0);
		}
		/* The exec failed: report its errno as the exit status. */
		_exit(errno ? errno : 255);
	}
	if (waitpid(pid, &status, 0) != pid) {
		printf("exec-check %s FAIL waitpid: errno %d (%s)\n", what, errno, strerror(errno));
		return 1;
	}
	if (WIFEXITED(status) && WEXITSTATUS(status) == 0) {
		printf("exec-check %s ok\n", what);
		return 0;
	}
	if (WIFEXITED(status)) {
		/* For execve, a status is the errno of the failed exec; for the
		 * shell it may also be the shell's own 126 or 127. */
		printf("exec-check %s FAIL child exited %d (%s, if an errno)\n", what,
		       WEXITSTATUS(status), strerror(WEXITSTATUS(status)));
	} else if (WIFSIGNALED(status)) {
		printf("exec-check %s FAIL child killed by signal %d\n", what, WTERMSIG(status));
	} else {
		printf("exec-check %s FAIL wait status 0x%x\n", what, status);
	}
	return 1;
}

int main(void)
{
	int failures = check("execve", 0);

	failures += check("shell", 1);
	return failures;
}
