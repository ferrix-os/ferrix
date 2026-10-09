#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdlib.h>
#include <unistd.h>
int ioctl(int fd, unsigned long req, ...) {
  va_list ap; va_start(ap, req); void *arg = va_arg(ap, void*); va_end(ap);
  static int (*real)(int, unsigned long, void*);
  if (!real) real = dlsym(RTLD_NEXT, "ioctl");
  unsigned nr = req & 0xff, ty = (req >> 8) & 0xff;
  int r = real(fd, req, arg);
  if (ty == 0x64 && nr == 0x43 && r == 0 && getenv("DEVINFO_GUEST")) { uint32_t *p = arg; const char *m = getenv("DEVINFO_GUEST"); for (; *m; m++) p[*m - '0'] = 0; }
  if (ty == 0x64 && getenv("DEVINFO_LOG")) fprintf(stderr, "devinfo: drm req 0x%lx = %d\n", req, r);
  return r;
}
