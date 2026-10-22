/*
 * dual-socket.exe: the socket calls a Unity IL2CPP (Mono class library)
 * game makes to reach its game server, in Wine on Ferrix.
 *
 * .NET's `TcpClient` makes a dual-mode IPv6 socket when it can: AF_INET6
 * with IPV6_V6ONLY off, connected to an IPv4 server by its IPv4-mapped
 * address (::ffff:a.b.c.d). Before connecting the game sets its send timeout
 * and buffer sizes, and the class library reads options back through
 * `getsockopt` -- `LingerState`, keep-alive, Don't Fragment. Wine answers
 * those from the Linux socket, and a Linux `ENOPROTOOPT` is `WSAEINVAL` to
 * the program, which Arknights: Endfield took as "SocketException: Invalid
 * arguments" from its connect and never reached its game server.
 *
 * Each socket, AF_INET and dual-mode AF_INET6, connects three ways: a
 * non-blocking connect (select, then SO_ERROR), ConnectEx on a socket bound
 * to the wildcard address, and a blocking connect; each reads every option
 * of `survey` before and after, all of which Linux answers. The peer is a
 * listener of this program's own on 127.0.0.1. Each step prints its answer;
 * the program prints `DUAL-SOCKET PASS` and exits 0 when every connection
 * carried a byte and no option was refused, and `DUAL-SOCKET FAIL: <step>
 * <WSA error>` and exits 1 at the first that was not.
 *
 * Built with x86_64-w64-mingw32-gcc -O2 -o dual-socket.exe dual-socket.c
 * -lws2_32.
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <mswsock.h>
#include <stdio.h>
#include <string.h>

/* The listener's port. */
#define PORT 45681

/* Stop at the first step that failed. */
static int fail(const char *step)
{
    printf("DUAL-SOCKET FAIL: %s %d\n", step, WSAGetLastError());
    fflush(stdout);
    return 1;
}

/* Say that a step answered. */
static void step(const char *what, int result)
{
    printf("dual: %s = %d (%d)\n", what, result, result == SOCKET_ERROR ? WSAGetLastError() : 0);
    fflush(stdout);
}

/* The options the game sets on its socket before it connects. */
static int options(SOCKET s, int dual)
{
    DWORD off = 0, on = 1, timeout = 5000, buffer = 1 << 20;
    int r;
    if (dual) {
        r = setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, (const char *)&off, sizeof off);
        step("setsockopt IPV6_V6ONLY 0", r);
        if (r) return fail("IPV6_V6ONLY");
    }
    r = setsockopt(s, SOL_SOCKET, SO_SNDTIMEO, (const char *)&timeout, sizeof timeout);
    step("setsockopt SO_SNDTIMEO", r);
    if (r) return fail("SO_SNDTIMEO");
    r = setsockopt(s, SOL_SOCKET, SO_RCVBUF, (const char *)&buffer, sizeof buffer);
    step("setsockopt SO_RCVBUF", r);
    if (r) return fail("SO_RCVBUF");
    r = setsockopt(s, SOL_SOCKET, SO_SNDBUF, (const char *)&buffer, sizeof buffer);
    step("setsockopt SO_SNDBUF", r);
    if (r) return fail("SO_SNDBUF");
    r = setsockopt(s, IPPROTO_TCP, TCP_NODELAY, (const char *)&on, sizeof on);
    step("setsockopt TCP_NODELAY", r);
    if (r) return fail("TCP_NODELAY");
    r = setsockopt(s, SOL_SOCKET, SO_KEEPALIVE, (const char *)&on, sizeof on);
    step("setsockopt SO_KEEPALIVE", r);
    if (r) return fail("SO_KEEPALIVE");
    return 0;
}

/* Read the options .NET's Socket and TcpClient properties read, the IPv6
 * ones on an AF_INET6 socket only, and fail at the first one refused.
 * (SO_RCVLOWAT is left out: Wine refuses it itself, on Linux too.) */
static int survey(SOCKET s, int dual)
{
    static const struct { int level, name; const char *what; } all[] = {
        { SOL_SOCKET, SO_LINGER, "SO_LINGER" }, { SOL_SOCKET, SO_DONTLINGER, "SO_DONTLINGER" },
        { SOL_SOCKET, SO_KEEPALIVE, "SO_KEEPALIVE" }, { SOL_SOCKET, SO_OOBINLINE, "SO_OOBINLINE" },
        { SOL_SOCKET, SO_RCVBUF, "SO_RCVBUF" }, { SOL_SOCKET, SO_SNDBUF, "SO_SNDBUF" },
        { SOL_SOCKET, SO_RCVTIMEO, "SO_RCVTIMEO" }, { SOL_SOCKET, SO_SNDTIMEO, "SO_SNDTIMEO" },
        { SOL_SOCKET, SO_REUSEADDR, "SO_REUSEADDR" }, { SOL_SOCKET, SO_EXCLUSIVEADDRUSE, "SO_EXCLUSIVEADDRUSE" },
        { SOL_SOCKET, SO_BROADCAST, "SO_BROADCAST" }, { SOL_SOCKET, SO_DONTROUTE, "SO_DONTROUTE" },
        { SOL_SOCKET, SO_ERROR, "SO_ERROR" }, { SOL_SOCKET, SO_TYPE, "SO_TYPE" },
        { SOL_SOCKET, SO_ACCEPTCONN, "SO_ACCEPTCONN" },
        { IPPROTO_TCP, TCP_NODELAY, "TCP_NODELAY" }, { IPPROTO_TCP, 3, "TCP_KEEPALIVE" },
        { IPPROTO_TCP, 17, "TCP_KEEPINTVL" }, { IPPROTO_TCP, 16, "TCP_KEEPCNT" },
        { IPPROTO_IP, IP_TTL, "IP_TTL" }, { IPPROTO_IP, IP_DONTFRAGMENT, "IP_DONTFRAGMENT" },
        { IPPROTO_IP, IP_TOS, "IP_TOS" }, { IPPROTO_IPV6, IPV6_V6ONLY, "IPV6_V6ONLY" },
        { IPPROTO_IPV6, IPV6_UNICAST_HOPS, "IPV6_UNICAST_HOPS" }, { IPPROTO_IPV6, 14, "IPV6_DONTFRAG" },
    };
    unsigned i;
    for (i = 0; i < sizeof all / sizeof all[0]; i++) {
        char value[64];
        int length = sizeof value, r;
        memset(value, 0, sizeof value);
        if (all[i].level == IPPROTO_IPV6 && !dual) continue;
        r = getsockopt(s, all[i].level, all[i].name, value, &length);
        if (r) {
            printf("dual: getsockopt %s refused\n", all[i].what);
            return fail(all[i].what);
        }
    }
    printf("dual: every option read\n");
    fflush(stdout);
    return 0;
}

/* The server's end of one connection: accept it and read its byte. */
static int served(SOCKET listener)
{
    char byte = 0;
    SOCKET peer = accept(listener, NULL, NULL);
    if (peer == INVALID_SOCKET) return fail("accept");
    if (recv(peer, &byte, 1, 0) != 1 || byte != 'x') {
        closesocket(peer);
        return fail("recv");
    }
    closesocket(peer);
    return 0;
}

/* Wait for a non-blocking connect as Mono does: select for writing, then
 * SO_ERROR. */
static int finished(SOCKET s)
{
    fd_set write, except;
    struct timeval wait = { 5, 0 };
    int error = -1, length = sizeof error, r;
    FD_ZERO(&write);
    FD_ZERO(&except);
    FD_SET(s, &write);
    FD_SET(s, &except);
    r = select(0, NULL, &write, &except, &wait);
    step("select", r);
    if (r != 1) return fail("select");
    r = getsockopt(s, SOL_SOCKET, SO_ERROR, (char *)&error, &length);
    printf("dual: SO_ERROR = %d\n", error);
    if (r || error) {
        WSASetLastError(error);
        return fail("SO_ERROR");
    }
    return 0;
}

/* One connection: `dual` an AF_INET6 socket to ::ffff:127.0.0.1, else an
 * AF_INET one to 127.0.0.1; `connect_ex` by ConnectEx, else by a
 * non-blocking connect. */
static int one(SOCKET listener, int dual, int connect_ex)
{
    struct sockaddr_storage to;
    int length, r;
    SOCKET s;
    memset(&to, 0, sizeof to);
    printf("dual: --- %s, %s\n", dual ? "AF_INET6 dual-mode" : "AF_INET",
           connect_ex == 2 ? "blocking connect" : connect_ex ? "ConnectEx" : "non-blocking connect");
    s = WSASocketW(dual ? AF_INET6 : AF_INET, SOCK_STREAM, IPPROTO_TCP, NULL, 0,
                   WSA_FLAG_OVERLAPPED);
    if (s == INVALID_SOCKET) return fail("socket");
    if (options(s, dual)) return 1;
    if (survey(s, dual)) return 1;
    if (dual) {
        struct sockaddr_in6 *six = (struct sockaddr_in6 *)&to;
        six->sin6_family = AF_INET6;
        six->sin6_port = htons(PORT);
        if (inet_pton(AF_INET6, "::ffff:127.0.0.1", &six->sin6_addr) != 1) return fail("inet_pton");
        length = sizeof *six;
    } else {
        struct sockaddr_in *four = (struct sockaddr_in *)&to;
        four->sin_family = AF_INET;
        four->sin_port = htons(PORT);
        four->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        length = sizeof *four;
    }
    if (connect_ex == 2) {
        r = connect(s, (struct sockaddr *)&to, length);
        step("blocking connect", r);
        if (r) return fail("blocking connect");
        if (survey(s, dual)) return 1;
    } else if (connect_ex) {
        GUID guid = WSAID_CONNECTEX;
        LPFN_CONNECTEX connect_ex_fn = NULL;
        DWORD bytes = 0, flags = 0;
        WSAOVERLAPPED overlapped;
        struct sockaddr_storage any;
        memset(&any, 0, sizeof any);
        any.ss_family = dual ? AF_INET6 : AF_INET;
        r = WSAIoctl(s, SIO_GET_EXTENSION_FUNCTION_POINTER, &guid, sizeof guid, &connect_ex_fn,
                     sizeof connect_ex_fn, &bytes, NULL, NULL);
        step("WSAIoctl ConnectEx", r);
        if (r) return fail("WSAIoctl");
        r = bind(s, (struct sockaddr *)&any,
                 dual ? sizeof(struct sockaddr_in6) : sizeof(struct sockaddr_in));
        step("bind any", r);
        if (r) return fail("bind");
        memset(&overlapped, 0, sizeof overlapped);
        overlapped.hEvent = WSACreateEvent();
        r = connect_ex_fn(s, (struct sockaddr *)&to, length, NULL, 0, NULL, &overlapped) ? 0 : SOCKET_ERROR;
        step("ConnectEx", r);
        if (r && WSAGetLastError() != ERROR_IO_PENDING) return fail("ConnectEx");
        if (!WSAGetOverlappedResult(s, &overlapped, &bytes, TRUE, &flags)) return fail("ConnectEx result");
        WSACloseEvent(overlapped.hEvent);
        r = setsockopt(s, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, NULL, 0);
        step("SO_UPDATE_CONNECT_CONTEXT", r);
    } else {
        u_long on = 1;
        r = ioctlsocket(s, FIONBIO, &on);
        step("ioctlsocket FIONBIO", r);
        if (r) return fail("FIONBIO");
        r = connect(s, (struct sockaddr *)&to, length);
        step("connect", r);
        if (r && WSAGetLastError() != WSAEWOULDBLOCK) return fail("connect");
        if (r && finished(s)) return 1;
        on = 0;
        if (ioctlsocket(s, FIONBIO, &on)) return fail("FIONBIO off");
    }
    r = send(s, "x", 1, 0);
    step("send", r);
    if (r != 1) return fail("send");
    if (served(listener)) return 1;
    closesocket(s);
    return 0;
}

int main(void)
{
    WSADATA data;
    struct sockaddr_in at;
    SOCKET listener;
    int dual, connect_ex;
    if (WSAStartup(MAKEWORD(2, 2), &data)) return fail("WSAStartup");
    listener = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (listener == INVALID_SOCKET) return fail("listener");
    memset(&at, 0, sizeof at);
    at.sin_family = AF_INET;
    at.sin_port = htons(PORT);
    at.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(listener, (struct sockaddr *)&at, sizeof at)) return fail("listener bind");
    if (listen(listener, 4)) return fail("listen");
    for (dual = 0; dual < 2; dual++)
        for (connect_ex = 0; connect_ex < 3; connect_ex++)
            if (one(listener, dual, connect_ex)) return 1;
    closesocket(listener);
    printf("DUAL-SOCKET PASS\n");
    return 0;
}
