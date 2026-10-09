/*
 * vk-dmabuf: a block-linear image in video memory shared between two
 * processes as a dmabuf, and its pixels compared (docs/NVIDIA.md §4.4, N3b,
 * name-only dmabufs).
 *
 * The parent forks first, before either side touches Vulkan. The exporter
 * (parent) makes a B8G8R8A8 image with a DRM format modifier the driver
 * picks from its non-linear ones (VK_EXT_image_drm_format_modifier), in
 * device-local memory it allocates exportable as a dmabuf
 * (VK_EXT_external_memory_dma_buf), uploads a pattern into it, and sends
 * the dmabuf's descriptor and the image's layout over a socket. The
 * importer (child) makes an image with the same modifier and layout over
 * that descriptor, copies it into host memory and checks every pixel.
 *
 * It also prints what the kernel says of the descriptor: its size, and
 * whether mmap refuses it (a name-only dmabuf of video memory is ENODEV).
 *
 * Built on the host against the system's Vulkan headers and run on Ferrix
 * from the NVIDIA volume with Debian's loader and NVIDIA's ICD. Every line
 * starts "vk-dmabuf:"; it exits 0 only if every pixel came back.
 *
 * SPDX-License-Identifier: MIT
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define WIDTH 512
#define HEIGHT 256
#define FORMAT VK_FORMAT_B8G8R8A8_UNORM
#define MAX_MODIFIERS 64

struct layout {
    uint64_t modifier;
    uint64_t offset;
    uint64_t row_pitch;
    uint64_t size;
};

struct gpu {
    const char *who;
    VkInstance instance;
    VkPhysicalDevice physical;
    VkDevice device;
    VkQueue queue;
    uint32_t family;
    VkCommandPool pool;
    int foreign;
    PFN_vkGetMemoryFdKHR get_fd;
    PFN_vkGetMemoryFdPropertiesKHR fd_properties;
    PFN_vkGetImageDrmFormatModifierPropertiesEXT modifier_of;
};

static int fail(const char *who, const char *what, VkResult result)
{
    printf("vk-dmabuf: %s: %s failed (VkResult %d)\n", who, what, result);
    fflush(stdout);
    return 1;
}

static uint32_t pattern(uint32_t x, uint32_t y)
{
    /* B, G, R, A: every pixel distinct within a row and a column. */
    return (x & 0xff) | ((y & 0xff) << 8) | (((x >> 8) * 64 + (y >> 8) * 16 + 7) << 16) |
           0xff000000u;
}

static uint32_t memory_type(VkPhysicalDevice physical, uint32_t bits, VkMemoryPropertyFlags want)
{
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    for (uint32_t i = 0; i < memory.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (memory.memoryTypes[i].propertyFlags & want) == want)
            return i;
    return UINT32_MAX;
}

static int has_extension(VkPhysicalDevice physical, const char *name)
{
    uint32_t count = 0;
    vkEnumerateDeviceExtensionProperties(physical, NULL, &count, NULL);
    VkExtensionProperties *all = calloc(count, sizeof(*all));
    int found = 0;
    if (all == NULL)
        return 0;
    vkEnumerateDeviceExtensionProperties(physical, NULL, &count, all);
    for (uint32_t i = 0; i < count; i++)
        if (strcmp(all[i].extensionName, name) == 0)
            found = 1;
    free(all);
    return found;
}

static int gpu_open(struct gpu *g, const char *who)
{
    VkResult r;
    memset(g, 0, sizeof(*g));
    g->who = who;
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "vk-dmabuf",
                              .apiVersion = VK_API_VERSION_1_2 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                 .pApplicationInfo = &app };
    if ((r = vkCreateInstance(&ici, NULL, &g->instance)) != VK_SUCCESS)
        return fail(who, "vkCreateInstance", r);
    uint32_t count = 1;
    r = vkEnumeratePhysicalDevices(g->instance, &count, &g->physical);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || count == 0)
        return fail(who, "vkEnumeratePhysicalDevices", r);

    uint32_t families = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(g->physical, &families, NULL);
    VkQueueFamilyProperties family[16];
    if (families > 16)
        families = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(g->physical, &families, family);
    g->family = UINT32_MAX;
    for (uint32_t i = 0; i < families; i++)
        if (family[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            g->family = i;
            break;
        }
    if (g->family == UINT32_MAX)
        return fail(who, "finding a graphics queue", VK_ERROR_FEATURE_NOT_PRESENT);

    const char *wanted[] = {
        VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
        VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
        VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
        VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME,
    };
    uint32_t enabled = 0;
    for (uint32_t i = 0; i < 4; i++) {
        if (has_extension(g->physical, wanted[i]))
            wanted[enabled++] = wanted[i];
        else if (i < 3) {
            printf("vk-dmabuf: %s: the device lacks %s\n", who, wanted[i]);
            return 1;
        }
    }
    g->foreign = enabled == 4;

    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = g->family,
                                    .queueCount = 1,
                                    .pQueuePriorities = &priority };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1,
                               .pQueueCreateInfos = &qci,
                               .enabledExtensionCount = enabled,
                               .ppEnabledExtensionNames = wanted };
    if ((r = vkCreateDevice(g->physical, &dci, NULL, &g->device)) != VK_SUCCESS)
        return fail(who, "vkCreateDevice", r);
    vkGetDeviceQueue(g->device, g->family, 0, &g->queue);
    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .queueFamilyIndex = g->family };
    if ((r = vkCreateCommandPool(g->device, &pci, NULL, &g->pool)) != VK_SUCCESS)
        return fail(who, "vkCreateCommandPool", r);
    g->get_fd = (PFN_vkGetMemoryFdKHR)vkGetDeviceProcAddr(g->device, "vkGetMemoryFdKHR");
    g->fd_properties = (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(
        g->device, "vkGetMemoryFdPropertiesKHR");
    g->modifier_of = (PFN_vkGetImageDrmFormatModifierPropertiesEXT)vkGetDeviceProcAddr(
        g->device, "vkGetImageDrmFormatModifierPropertiesEXT");
    if (g->get_fd == NULL || g->fd_properties == NULL || g->modifier_of == NULL)
        return fail(who, "vkGetDeviceProcAddr", VK_ERROR_EXTENSION_NOT_PRESENT);
    return 0;
}

/* A host-visible buffer of `bytes`, mapped at `*mapped`. */
static int host_buffer(struct gpu *g, VkDeviceSize bytes, VkBufferUsageFlags usage, VkBuffer *buffer,
                       VkDeviceMemory *memory, void **mapped)
{
    VkResult r;
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                               .size = bytes,
                               .usage = usage };
    if ((r = vkCreateBuffer(g->device, &bci, NULL, buffer)) != VK_SUCCESS)
        return fail(g->who, "vkCreateBuffer", r);
    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(g->device, *buffer, &req);
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = req.size,
        .memoryTypeIndex = memory_type(g->physical, req.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                           VK_MEMORY_PROPERTY_HOST_COHERENT_BIT),
    };
    if ((r = vkAllocateMemory(g->device, &mai, NULL, memory)) != VK_SUCCESS)
        return fail(g->who, "vkAllocateMemory (host)", r);
    if ((r = vkBindBufferMemory(g->device, *buffer, *memory, 0)) != VK_SUCCESS)
        return fail(g->who, "vkBindBufferMemory", r);
    if ((r = vkMapMemory(g->device, *memory, 0, VK_WHOLE_SIZE, 0, mapped)) != VK_SUCCESS)
        return fail(g->who, "vkMapMemory", r);
    return 0;
}

/* Record `copy` between the image and the buffer, with the image moved
 * from `from` to the copy's layout and on to `to`, and wait for it. */
static int submit_copy(struct gpu *g, VkImage image, VkBuffer buffer, int upload, uint32_t from_family,
                       uint32_t to_family)
{
    VkResult r;
    VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = g->pool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                        .commandBufferCount = 1 };
    VkCommandBuffer cmd;
    if ((r = vkAllocateCommandBuffers(g->device, &cai, &cmd)) != VK_SUCCESS)
        return fail(g->who, "vkAllocateCommandBuffers", r);
    VkCommandBufferBeginInfo begin = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                       .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
    vkBeginCommandBuffer(cmd, &begin);
    VkImageLayout copy_layout = upload ? VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL
                                       : VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    VkImageMemoryBarrier in = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = 0,
        .dstAccessMask = upload ? VK_ACCESS_TRANSFER_WRITE_BIT : VK_ACCESS_TRANSFER_READ_BIT,
        .oldLayout = upload ? VK_IMAGE_LAYOUT_UNDEFINED : VK_IMAGE_LAYOUT_GENERAL,
        .newLayout = copy_layout,
        .srcQueueFamilyIndex = upload ? VK_QUEUE_FAMILY_IGNORED : from_family,
        .dstQueueFamilyIndex = upload ? VK_QUEUE_FAMILY_IGNORED : (from_family == VK_QUEUE_FAMILY_IGNORED ? VK_QUEUE_FAMILY_IGNORED : g->family),
        .image = image,
        .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
    };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0,
                         NULL, 0, NULL, 1, &in);
    VkBufferImageCopy region = {
        .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
        .imageExtent = { WIDTH, HEIGHT, 1 },
    };
    if (upload)
        vkCmdCopyBufferToImage(cmd, buffer, image, copy_layout, 1, &region);
    else
        vkCmdCopyImageToBuffer(cmd, image, copy_layout, buffer, 1, &region);
    VkImageMemoryBarrier out = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = upload ? VK_ACCESS_TRANSFER_WRITE_BIT : VK_ACCESS_TRANSFER_READ_BIT,
        .dstAccessMask = upload ? 0 : VK_ACCESS_HOST_READ_BIT,
        .oldLayout = copy_layout,
        .newLayout = VK_IMAGE_LAYOUT_GENERAL,
        .srcQueueFamilyIndex = (upload && to_family != VK_QUEUE_FAMILY_IGNORED) ? g->family : VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = upload ? to_family : VK_QUEUE_FAMILY_IGNORED,
        .image = image,
        .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 },
    };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         upload ? VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT : VK_PIPELINE_STAGE_HOST_BIT, 0,
                         0, NULL, 0, NULL, 1, &out);
    vkEndCommandBuffer(cmd);
    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    if ((r = vkCreateFence(g->device, &fci, NULL, &fence)) != VK_SUCCESS)
        return fail(g->who, "vkCreateFence", r);
    VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                        .commandBufferCount = 1,
                        .pCommandBuffers = &cmd };
    if ((r = vkQueueSubmit(g->queue, 1, &si, fence)) != VK_SUCCESS)
        return fail(g->who, "vkQueueSubmit", r);
    if ((r = vkWaitForFences(g->device, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000)) != VK_SUCCESS)
        return fail(g->who, "vkWaitForFences", r);
    return 0;
}

/* What the kernel says of the descriptor, as any program would see it. */
static void describe(const char *who, int fd)
{
    struct stat st;
    off_t end = lseek(fd, 0, SEEK_END);
    void *at = mmap(NULL, 4096, PROT_READ, MAP_SHARED, fd, 0);
    int mapped_errno = at == MAP_FAILED ? errno : 0;
    if (at != MAP_FAILED)
        munmap(at, 4096);
    printf("vk-dmabuf: %s: fd %d fstat size %lld, lseek end %lld, mmap %s\n", who, fd,
           fstat(fd, &st) == 0 ? (long long)st.st_size : -1LL, (long long)end,
           at == MAP_FAILED ? strerror(mapped_errno) : "mapped");
}

static int send_fd(int sock, int fd, const struct layout *l)
{
    char control[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { .iov_base = (void *)l, .iov_len = sizeof(*l) };
    struct msghdr msg = { .msg_iov = &iov,
                          .msg_iovlen = 1,
                          .msg_control = control,
                          .msg_controllen = sizeof(control) };
    memset(control, 0, sizeof(control));
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof(int));
    return sendmsg(sock, &msg, 0) == (ssize_t)sizeof(*l) ? 0 : -1;
}

static int receive_fd(int sock, struct layout *l)
{
    char control[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { .iov_base = l, .iov_len = sizeof(*l) };
    struct msghdr msg = { .msg_iov = &iov,
                          .msg_iovlen = 1,
                          .msg_control = control,
                          .msg_controllen = sizeof(control) };
    if (recvmsg(sock, &msg, 0) != (ssize_t)sizeof(*l))
        return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (c == NULL || c->cmsg_type != SCM_RIGHTS)
        return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(c), sizeof(int));
    return fd;
}

static int exporter(int sock)
{
    struct gpu g;
    VkResult r;
    if (gpu_open(&g, "exporter") != 0)
        return 1;

    /* The driver's modifiers for the format: every non-linear one-plane
     * modifier that can be copied to and from. */
    VkDrmFormatModifierPropertiesEXT mods[MAX_MODIFIERS];
    VkDrmFormatModifierPropertiesListEXT list = {
        .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT,
        .drmFormatModifierCount = MAX_MODIFIERS,
        .pDrmFormatModifierProperties = mods,
    };
    VkFormatProperties2 fp = { .sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &list };
    vkGetPhysicalDeviceFormatProperties2(g.physical, FORMAT, &fp);
    uint64_t chosen[MAX_MODIFIERS];
    uint32_t n = 0;
    VkFormatFeatureFlags need = VK_FORMAT_FEATURE_TRANSFER_SRC_BIT | VK_FORMAT_FEATURE_TRANSFER_DST_BIT;
    for (uint32_t i = 0; i < list.drmFormatModifierCount; i++) {
        printf("vk-dmabuf: exporter: modifier 0x%016llx planes %u features 0x%x\n",
               (unsigned long long)mods[i].drmFormatModifier, mods[i].drmFormatModifierPlaneCount,
               mods[i].drmFormatModifierTilingFeatures);
        if (mods[i].drmFormatModifier != 0 && mods[i].drmFormatModifierPlaneCount == 1 &&
            (mods[i].drmFormatModifierTilingFeatures & need) == need)
            chosen[n++] = mods[i].drmFormatModifier;
    }
    if (n == 0) {
        printf("vk-dmabuf: exporter: no block-linear modifier for B8G8R8A8\n");
        return 1;
    }

    VkImageDrmFormatModifierListCreateInfoEXT mod_list = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
        .drmFormatModifierCount = n,
        .pDrmFormatModifiers = chosen,
    };
    VkExternalMemoryImageCreateInfo external = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .pNext = &mod_list,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .pNext = &external,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = FORMAT,
        .extent = { WIDTH, HEIGHT, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
        .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImage image;
    if ((r = vkCreateImage(g.device, &ici, NULL, &image)) != VK_SUCCESS)
        return fail(g.who, "vkCreateImage (modifier list)", r);
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(g.device, image, &req);
    VkExportMemoryAllocateInfo export = {
        .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkMemoryDedicatedAllocateInfo dedicated = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .pNext = &export,
        .image = image,
    };
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &dedicated,
        .allocationSize = req.size,
        .memoryTypeIndex = memory_type(g.physical, req.memoryTypeBits,
                                       VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT),
    };
    VkDeviceMemory memory;
    if ((r = vkAllocateMemory(g.device, &mai, NULL, &memory)) != VK_SUCCESS)
        return fail(g.who, "vkAllocateMemory (exportable, device-local)", r);
    if ((r = vkBindImageMemory(g.device, image, memory, 0)) != VK_SUCCESS)
        return fail(g.who, "vkBindImageMemory", r);

    VkImageDrmFormatModifierPropertiesEXT picked = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_PROPERTIES_EXT
    };
    if ((r = g.modifier_of(g.device, image, &picked)) != VK_SUCCESS)
        return fail(g.who, "vkGetImageDrmFormatModifierPropertiesEXT", r);
    VkImageSubresource plane = { .aspectMask = VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT };
    VkSubresourceLayout sub;
    vkGetImageSubresourceLayout(g.device, image, &plane, &sub);
    struct layout l = { .modifier = picked.drmFormatModifier,
                        .offset = sub.offset,
                        .row_pitch = sub.rowPitch,
                        .size = req.size };
    printf("vk-dmabuf: exporter: image %ux%u modifier 0x%016llx offset %llu pitch %llu, %llu bytes\n",
           WIDTH, HEIGHT, (unsigned long long)l.modifier, (unsigned long long)l.offset,
           (unsigned long long)l.row_pitch, (unsigned long long)l.size);

    VkBuffer staging;
    VkDeviceMemory staging_memory;
    uint32_t *pixels;
    if (host_buffer(&g, (VkDeviceSize)WIDTH * HEIGHT * 4, VK_BUFFER_USAGE_TRANSFER_SRC_BIT, &staging,
                    &staging_memory, (void **)&pixels) != 0)
        return 1;
    for (uint32_t y = 0; y < HEIGHT; y++)
        for (uint32_t x = 0; x < WIDTH; x++)
            pixels[y * WIDTH + x] = pattern(x, y);
    if (submit_copy(&g, image, staging, 1, VK_QUEUE_FAMILY_IGNORED,
                    g.foreign ? VK_QUEUE_FAMILY_FOREIGN_EXT : VK_QUEUE_FAMILY_IGNORED) != 0)
        return 1;

    VkMemoryGetFdInfoKHR gfi = { .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
                                 .memory = memory,
                                 .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT };
    int fd = -1;
    if ((r = g.get_fd(g.device, &gfi, &fd)) != VK_SUCCESS)
        return fail(g.who, "vkGetMemoryFdKHR (dma_buf)", r);
    describe("exporter", fd);
    if (send_fd(sock, fd, &l) != 0) {
        printf("vk-dmabuf: exporter: sending the descriptor failed: %s\n", strerror(errno));
        return 1;
    }
    close(fd);
    fflush(stdout);
    /* Keep the memory until the importer is done with it. */
    char done;
    if (read(sock, &done, 1) != 1)
        done = 0;
    vkDestroyImage(g.device, image, NULL);
    vkFreeMemory(g.device, memory, NULL);
    return done == 'y' ? 0 : 1;
}

static int importer(int sock)
{
    struct gpu g;
    VkResult r;
    struct layout l;
    int fd = receive_fd(sock, &l);
    if (fd < 0) {
        printf("vk-dmabuf: importer: no descriptor came\n");
        return 1;
    }
    describe("importer", fd);
    if (gpu_open(&g, "importer") != 0)
        return 1;

    VkMemoryFdPropertiesKHR fdp = { .sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR };
    if ((r = g.fd_properties(g.device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fdp)) !=
        VK_SUCCESS)
        return fail(g.who, "vkGetMemoryFdPropertiesKHR", r);

    VkSubresourceLayout plane = { .offset = l.offset, .rowPitch = l.row_pitch };
    VkImageDrmFormatModifierExplicitCreateInfoEXT explicit_mod = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
        .drmFormatModifier = l.modifier,
        .drmFormatModifierPlaneCount = 1,
        .pPlaneLayouts = &plane,
    };
    VkExternalMemoryImageCreateInfo external = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .pNext = &explicit_mod,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };
    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .pNext = &external,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = FORMAT,
        .extent = { WIDTH, HEIGHT, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
        .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    VkImage image;
    if ((r = vkCreateImage(g.device, &ici, NULL, &image)) != VK_SUCCESS)
        return fail(g.who, "vkCreateImage (explicit modifier)", r);
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(g.device, image, &req);
    VkImportMemoryFdInfoKHR import = { .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
                                       .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
                                       .fd = fd };
    VkMemoryDedicatedAllocateInfo dedicated = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
        .pNext = &import,
        .image = image,
    };
    uint32_t type = memory_type(g.physical, req.memoryTypeBits & fdp.memoryTypeBits, 0);
    VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .pNext = &dedicated,
                                 .allocationSize = req.size > l.size ? req.size : l.size,
                                 .memoryTypeIndex = type };
    printf("vk-dmabuf: importer: fd memory types 0x%x, image's 0x%x, %llu bytes\n",
           fdp.memoryTypeBits, req.memoryTypeBits, (unsigned long long)req.size);
    VkDeviceMemory memory;
    if ((r = vkAllocateMemory(g.device, &mai, NULL, &memory)) != VK_SUCCESS)
        return fail(g.who, "vkAllocateMemory (import dma_buf)", r);
    if ((r = vkBindImageMemory(g.device, image, memory, 0)) != VK_SUCCESS)
        return fail(g.who, "vkBindImageMemory", r);

    VkBuffer readback;
    VkDeviceMemory readback_memory;
    uint32_t *pixels;
    if (host_buffer(&g, (VkDeviceSize)WIDTH * HEIGHT * 4, VK_BUFFER_USAGE_TRANSFER_DST_BIT, &readback,
                    &readback_memory, (void **)&pixels) != 0)
        return 1;
    memset(pixels, 0, (size_t)WIDTH * HEIGHT * 4);
    if (submit_copy(&g, image, readback, 0,
                    g.foreign ? VK_QUEUE_FAMILY_FOREIGN_EXT : VK_QUEUE_FAMILY_IGNORED,
                    VK_QUEUE_FAMILY_IGNORED) != 0)
        return 1;
    uint32_t wrong = 0, first_x = 0, first_y = 0;
    for (uint32_t y = 0; y < HEIGHT; y++)
        for (uint32_t x = 0; x < WIDTH; x++)
            if (pixels[y * WIDTH + x] != pattern(x, y) && wrong++ == 0) {
                first_x = x;
                first_y = y;
            }
    if (wrong != 0) {
        printf("vk-dmabuf: importer: %u of %u pixels differ, first at %u,%u: 0x%08x, wanted 0x%08x\n",
               wrong, WIDTH * HEIGHT, first_x, first_y, pixels[first_y * WIDTH + first_x],
               pattern(first_x, first_y));
        return 1;
    }
    printf("vk-dmabuf: importer: all %u pixels match through modifier 0x%016llx\n", WIDTH * HEIGHT,
           (unsigned long long)l.modifier);
    return 0;
}

int main(void)
{
    int pair[2];
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
        printf("vk-dmabuf: socketpair: %s\n", strerror(errno));
        return 1;
    }
    pid_t child = fork();
    if (child < 0) {
        printf("vk-dmabuf: fork: %s\n", strerror(errno));
        return 1;
    }
    if (child == 0) {
        close(pair[0]);
        int failed = importer(pair[1]);
        char answer = failed ? 'n' : 'y';
        if (write(pair[1], &answer, 1) != 1)
            failed = 1;
        fflush(stdout);
        _exit(failed);
    }
    close(pair[1]);
    int failed = exporter(pair[0]);
    int status = 0;
    if (failed)
        kill(child, SIGKILL);
    waitpid(child, &status, 0);
    if (!failed && WIFEXITED(status) && WEXITSTATUS(status) == 0) {
        printf("vk-dmabuf: PASS\n");
        return 0;
    }
    printf("vk-dmabuf: FAIL (exporter %d, importer status 0x%x)\n", failed, status);
    return 1;
}
