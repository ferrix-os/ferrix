/*
 * The real kernel's verdict on a seccomp filter, call by call.
 *
 *     cc -O2 -o oracle oracle.c
 *     ./oracle data/chrome-1.bpf > data/chrome-1.verdicts
 *     ./oracle --accept data/scratch-1.bpf > data/scratch-1.answer
 *
 * With --accept it does one thing: installs the filter and prints "accepted",
 * or "refused" and the errno, which is what the kernel's verifier said of it.
 *
 * For each call number 0 to 449 a child installs the filter (after
 * PR_SET_NO_NEW_PRIVS) and makes the call with six zero arguments. Its fate is
 * the filter's answer, read from outside:
 *
 *   trap N     SIGSYS arrived with si_code SYS_SECCOMP and si_errno N
 *   errno E    the call failed with E, and failed differently (or not at all)
 *              without the filter
 *   allow      the call ran: it returned what it returns without the filter,
 *              exited, or was still blocked when the parent gave up on it
 *   kill       the child was ended by a signal other than SIGSYS
 *
 * `errno E` with E the errno the call itself gives cannot be told from
 * `allow`; the Rust test accepts either there. Run as an ordinary user: every
 * call has zero arguments, and the dangerous ones need privilege.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_INSNS 4096
#define LAST_CALL 449

static struct sock_filter program[MAX_INSNS];
static int length;
static int report;

static void on_sigsys(int sig, siginfo_t *info, void *ctx)
{
    (void)sig;
    (void)ctx;
    char line[64];
    int n = snprintf(line, sizeof line, "T %d %d\n", info->si_code, info->si_errno);
    if (write(report, line, n) < 0)
        _exit(2);
    _exit(0);
}

/* The call with six zero arguments: (return, errno). */
static void make_call(long nr, long *ret, int *err)
{
    errno = 0;
    *ret = syscall(nr, 0L, 0L, 0L, 0L, 0L, 0L);
    *err = errno;
}

static void child(long nr, int filtered, int fd)
{
    report = fd;
    struct sigaction action;
    memset(&action, 0, sizeof action);
    action.sa_sigaction = on_sigsys;
    action.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigaction(SIGSYS, &action, NULL);
    alarm(1);
    if (filtered) {
        struct sock_fprog fprog = {.len = (unsigned short)length, .filter = program};
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0)
            _exit(3);
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &fprog) != 0)
            _exit(4);
    }
    long ret;
    int err;
    make_call(nr, &ret, &err);
    char line[64];
    int n = snprintf(line, sizeof line, "R %ld %d\n", ret, err);
    if (write(fd, line, n) < 0)
        _exit(2);
    _exit(0);
}

/* What one run of the child reports, as text; empty if it reported nothing. */
static void run(long nr, int filtered, char *out, size_t size, int *status)
{
    int fds[2];
    if (pipe(fds) != 0)
        exit(1);
    pid_t pid = fork();
    if (pid < 0)
        exit(1);
    if (pid == 0) {
        close(fds[0]);
        child(nr, filtered, fds[1]);
    }
    close(fds[1]);
    struct pollfd wait = {.fd = fds[0], .events = POLLIN};
    poll(&wait, 1, 400);
    out[0] = 0;
    ssize_t got = read(fds[0], out, size - 1);
    if (got > 0)
        out[got] = 0;
    else
        out[0] = 0;
    kill(pid, SIGKILL);
    waitpid(pid, status, 0);
    close(fds[0]);
}

int main(int argc, char **argv)
{
    int accept = argc == 3 && strcmp(argv[1], "--accept") == 0;
    if (argc != 2 && !accept)
        return 1;
    FILE *file = fopen(argv[accept ? 2 : 1], "rb");
    if (!file)
        return 1;
    length = (int)fread(program, sizeof program[0], MAX_INSNS, file);
    fclose(file);
    if (accept) {
        struct sock_fprog fprog = {.len = (unsigned short)length, .filter = program};
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0)
            return 3;
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &fprog) == 0)
            printf("accepted\n");
        else
            printf("refused %d\n", errno);
        return 0;
    }
    for (long nr = 0; nr <= LAST_CALL; nr++) {
        char with[64], without[64];
        int with_status = 0, without_status = 0;
        run(nr, 1, with, sizeof with, &with_status);
        run(nr, 0, without, sizeof without, &without_status);
        if (with[0] == 'T') {
            int code = 0, data = 0;
            sscanf(with, "T %d %d", &code, &data);
            printf("%ld trap %d\n", nr, data);
        } else if (with[0] == 'R' && without[0] == 'R') {
            long r1 = 0, r2 = 0;
            int e1 = 0, e2 = 0;
            sscanf(with, "R %ld %d", &r1, &e1);
            sscanf(without, "R %ld %d", &r2, &e2);
            /* A result that differs with no errno set is the call's own (a
             * pid, a time); only a failure the call does not give is the
             * filter's ERRNO. ERRNO|0 is not told from a call returning 0. */
            if (e1 != 0 && (r1 != r2 || e1 != e2))
                printf("%ld errno %d\n", nr, e1);
            else
                printf("%ld allow\n", nr);
        } else if (with[0] == 0 && WIFSIGNALED(with_status) && WTERMSIG(with_status) != SIGKILL
                   && WTERMSIG(with_status) != SIGSYS
                   && !(WIFSIGNALED(without_status)
                        && WTERMSIG(without_status) == WTERMSIG(with_status))) {
            printf("%ld kill\n", nr);
        } else {
            /* Exited, or still blocked: the call ran. */
            printf("%ld allow\n", nr);
        }
    }
    return 0;
}
