#include <gbm.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#ifndef DRM_FORMAT_MOD_LINEAR
#define DRM_FORMAT_MOD_LINEAR 0
#endif
int main(int argc, char **argv) {
  const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD129";
  int fd = open(node, O_RDWR | O_CLOEXEC);
  if (fd < 0) { printf("gbmtest: open %s: %s\n", node, strerror(errno)); return 1; }
  struct gbm_device *dev = gbm_create_device(fd);
  printf("gbmtest: gbm_create_device %p backend %s\n", (void*)dev, dev ? gbm_device_get_backend_name(dev) : "-");
  if (!dev) return 2;
  uint32_t fmts[] = {GBM_FORMAT_ARGB8888, GBM_FORMAT_XRGB8888, GBM_FORMAT_ABGR8888};
  uint32_t uses[] = {GBM_BO_USE_RENDERING, GBM_BO_USE_SCANOUT, GBM_BO_USE_SCANOUT|GBM_BO_USE_RENDERING, GBM_BO_USE_LINEAR|GBM_BO_USE_RENDERING};
  for (int f = 0; f < 3; f++) for (int u = 0; u < 4; u++)
    printf("gbmtest: is_format_supported %08x use %x = %d\n", fmts[f], uses[u], gbm_device_is_format_supported(dev, fmts[f], uses[u]));
  for (int u = 0; u < 4; u++) {
    errno = 0;
    struct gbm_bo *bo = gbm_bo_create(dev, 607, 88, GBM_FORMAT_ARGB8888, uses[u]);
    printf("gbmtest: bo_create use %x: %p errno %d", uses[u], (void*)bo, errno);
    if (bo) { int bfd = gbm_bo_get_fd(bo); printf(" fd %d stride %u mod %llx", bfd, gbm_bo_get_stride(bo), (unsigned long long)gbm_bo_get_modifier(bo)); gbm_bo_destroy(bo); }
    printf("\n");
  }
  uint64_t mods[] = {DRM_FORMAT_MOD_LINEAR};
  uint64_t mods2[] = {DRM_FORMAT_MOD_LINEAR, 0x00ffffffffffffffULL};
  struct gbm_bo *bo = gbm_bo_create_with_modifiers(dev, 607, 88, GBM_FORMAT_ARGB8888, mods, 1);
  printf("gbmtest: bo_create_with_modifiers(LINEAR): %p errno %d\n", (void*)bo, errno);
  if (bo) { printf("gbmtest:  fd %d stride %u mod %llx\n", gbm_bo_get_fd(bo), gbm_bo_get_stride(bo), (unsigned long long)gbm_bo_get_modifier(bo)); gbm_bo_destroy(bo);}
  bo = gbm_bo_create_with_modifiers(dev, 607, 88, GBM_FORMAT_ARGB8888, mods2, 2);
  printf("gbmtest: bo_create_with_modifiers(LINEAR,INVALID): %p errno %d\n", (void*)bo, errno);
  bo = gbm_bo_create_with_modifiers2(dev, 607, 88, GBM_FORMAT_ARGB8888, mods, 1, GBM_BO_USE_SCANOUT|GBM_BO_USE_RENDERING);
  printf("gbmtest: bo_create_with_modifiers2(LINEAR, SCANOUT|RENDERING): %p errno %d\n", (void*)bo, errno);
  return 0;
}
