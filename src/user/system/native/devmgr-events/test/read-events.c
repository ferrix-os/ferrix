/*
 * read-events: a reader of devmgr-events (docs/DESKTOP.md §4.2), for
 * `cargo xtask test-restart --boot crashloop`.
 *
 * Connects to the abstract AF_UNIX name ferrix.devmgr.events -- trying
 * again each 100 ms until the helper listens -- and writes every line the
 * helper sends to standard output, each after "devmgr-event: ". It exits 0
 * once a line starts with the word given as its first argument, and 1 when
 * the seconds given as its second have passed first.
 *
 * Built statically by xtask with the host's cc; it runs the same on Linux.
 */
#include <errno.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

static const char NAME[] = "ferrix.devmgr.events";

static double now(void)
{
	struct timespec at;
	clock_gettime(CLOCK_MONOTONIC, &at);
	return (double)at.tv_sec + (double)at.tv_nsec / 1e9;
}

static int connected(void)
{
	struct sockaddr_un address;
	memset(&address, 0, sizeof address);
	address.sun_family = AF_UNIX;
	memcpy(address.sun_path + 1, NAME, sizeof NAME - 1);
	socklen_t length = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + sizeof NAME - 1);
	int fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (fd < 0)
		return -1;
	if (connect(fd, (struct sockaddr *)&address, length) == 0)
		return fd;
	close(fd);
	return -1;
}

int main(int argc, char **argv)
{
	if (argc != 3) {
		fprintf(stderr, "usage: read-events <last word> <seconds>\n");
		return 2;
	}
	const char *last = argv[1];
	double end = now() + atof(argv[2]);
	int fd;
	while ((fd = connected()) < 0) {
		if (now() > end) {
			printf("devmgr-event: none: no helper listened\n");
			return 1;
		}
		usleep(100000);
	}
	char line[256];
	size_t used = 0;
	for (;;) {
		double left = end - now();
		if (left <= 0)
			break;
		struct pollfd watched = { .fd = fd, .events = POLLIN };
		int ready = poll(&watched, 1, (int)(left * 1000) + 1);
		if (ready < 0 && errno == EINTR)
			continue;
		if (ready <= 0)
			break;
		char byte;
		ssize_t got = read(fd, &byte, 1);
		if (got <= 0) {
			printf("devmgr-event: none: the helper closed\n");
			return 1;
		}
		if (byte != '\n') {
			if (used < sizeof line - 1)
				line[used++] = byte;
			continue;
		}
		line[used] = '\0';
		used = 0;
		printf("devmgr-event: %s\n", line);
		fflush(stdout);
		size_t word = strlen(last);
		if (strncmp(line, last, word) == 0 && line[word] == ' ')
			return 0;
	}
	printf("devmgr-event: none: no \"%s\" within %s s\n", last, argv[2]);
	return 1;
}
