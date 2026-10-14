/*
 * window32: the 32-bit Windows program `cargo xtask test-wine` runs in
 * Debian's 64-bit Wine on Ferrix, through Wine's WoW64 (stage 23, E1.6;
 * docs/WINE.md §7).
 *
 *   window32.exe RRGGBB SECONDS
 *
 * It says what it runs as, then makes a top-level window titled
 * "Ferrix WoW64" whose client area it paints in the colour given, and
 * closes it after SECONDS. The line the gate judges:
 *
 *   window32: pointer=4 wow64=1 process=014c native=8664
 *
 * pointer is sizeof(void *); wow64 is IsWow64Process's answer; process and
 * native are IsWow64Process2's machines (014c is i386, 8664 x86-64). The
 * colour comes from the command line so that the gate can ask for one the
 * program does not know in advance; the gate then finds it on the screen,
 * in the window hyprix lists by its title.
 *
 * Built by xtask with mingw-w64 (crate::wine::window32):
 *   i686-w64-mingw32-gcc -O2 -o window32.exe window32.c -lgdi32
 *
 * A console program, so that what it prints goes to the shell that
 * started it; the window is its own all the same.
 */

#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static COLORREF colour;

typedef BOOL (WINAPI *is_wow64_process2_fn)(HANDLE, USHORT *, USHORT *);

static LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM w, LPARAM l) {
    switch (message) {
    case WM_PAINT: {
        PAINTSTRUCT paint;
        RECT client;
        HDC dc = BeginPaint(window, &paint);
        HBRUSH brush = CreateSolidBrush(colour);
        GetClientRect(window, &client);
        FillRect(dc, &client, brush);
        DeleteObject(brush);
        EndPaint(window, &paint);
        return 0;
    }
    case WM_ERASEBKGND:
        return 1;
    case WM_TIMER:
        DestroyWindow(window);
        return 0;
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(window, message, w, l);
}

int main(int argc, char **argv) {
    BOOL wow64 = FALSE;
    USHORT process = 0, native = 0;
    is_wow64_process2_fn is_wow64_process2;
    unsigned long rgb;
    unsigned seconds;
    WNDCLASSA class = {0};
    HWND window;
    MSG message;

    if (argc != 3) {
        printf("window32: usage: window32.exe RRGGBB SECONDS\n");
        return 2;
    }
    rgb = strtoul(argv[1], NULL, 16);
    seconds = (unsigned)strtoul(argv[2], NULL, 10);
    colour = RGB((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);

    IsWow64Process(GetCurrentProcess(), &wow64);
    is_wow64_process2 = (is_wow64_process2_fn)(void *)GetProcAddress(
        GetModuleHandleA("kernel32.dll"), "IsWow64Process2");
    if (is_wow64_process2)
        is_wow64_process2(GetCurrentProcess(), &process, &native);
    printf("window32: pointer=%u wow64=%d process=%04x native=%04x\n",
           (unsigned)sizeof(void *), wow64 ? 1 : 0, process, native);
    fflush(stdout);

    class.lpfnWndProc = window_proc;
    class.hInstance = GetModuleHandleA(NULL);
    class.hCursor = LoadCursorA(NULL, (LPCSTR)IDC_ARROW);
    class.lpszClassName = "FerrixWow64";
    if (!RegisterClassA(&class)) {
        printf("window32: RegisterClass failed: %lu\n", GetLastError());
        return 1;
    }
    window = CreateWindowA("FerrixWow64", "Ferrix WoW64", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                           CW_USEDEFAULT, CW_USEDEFAULT, 480, 320, NULL, NULL,
                           class.hInstance, NULL);
    if (!window) {
        printf("window32: CreateWindow failed: %lu\n", GetLastError());
        return 1;
    }
    SetTimer(window, 1, seconds * 1000, NULL);
    printf("window32: window shown\n");
    fflush(stdout);
    while (GetMessageA(&message, NULL, 0, 0) > 0) {
        TranslateMessage(&message);
        DispatchMessageA(&message);
    }
    printf("window32: closed\n");
    return 0;
}
