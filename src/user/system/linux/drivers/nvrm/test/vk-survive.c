/*
 * vk-survive: a Vulkan program that keeps working while the GPU's driver is
 * killed under it (docs/INTEL-GPU.md §11, the driver started again). It
 * records one clear-and-copy, then submits it and waits for its fence, round
 * after round, every 2 ms, and between rounds checks a host-visible buffer it
 * filled once still holds what it wrote: the GPU's objects and the program's
 * mappings of them.
 *
 * It says "vk-survive: ready" once the first round has completed, so a
 * gate can kill the driver while it runs, then one line per hundred rounds,
 * and stops when /tmp/vk-survive.stop exists. What it ends with is what the
 * gate judges:
 *
 *   0  every round answered and the buffer kept its bytes ("survived");
 *   2  the submit or the wait answered VK_ERROR_DEVICE_LOST ("device
 *      lost"), the fallback the driver's death must at least give: never a
 *      hang. A fence reset that failed before it is said, not counted;
 *   3  any other failure, said.
 *
 * Built on the host against the system's Vulkan headers, as vk-offscreen is.
 *
 * SPDX-License-Identifier: MIT
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#define WIDTH 64
#define HEIGHT 64
#define KEPT_BYTES 65536

static int fail(const char *what, VkResult result)
{
    printf("vk-survive: %s failed (VkResult %d)\n", what, result);
    return 3;
}

static uint32_t memory_type(VkPhysicalDevice gpu, uint32_t bits, VkMemoryPropertyFlags want)
{
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(gpu, &memory);
    for (uint32_t i = 0; i < memory.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (memory.memoryTypes[i].propertyFlags & want) == want)
            return i;
    return UINT32_MAX;
}

static int ended(unsigned round, const char *what, VkResult result)
{
    if (result == VK_ERROR_DEVICE_LOST) {
        printf("vk-survive: device lost at round %u: %s answered VK_ERROR_DEVICE_LOST\n", round,
               what);
        return 2;
    }
    printf("vk-survive: round %u: %s failed (VkResult %d)\n", round, what, result);
    return 3;
}

int main(void)
{
    VkResult r;
    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "vk-survive",
                              .apiVersion = VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                 .pApplicationInfo = &app };
    VkInstance instance;
    if ((r = vkCreateInstance(&ici, NULL, &instance)) != VK_SUCCESS)
        return fail("vkCreateInstance", r);

    uint32_t count = 1;
    VkPhysicalDevice gpu;
    r = vkEnumeratePhysicalDevices(instance, &count, &gpu);
    if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || count == 0)
        return fail("vkEnumeratePhysicalDevices", r);
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(gpu, &props);

    uint32_t families = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &families, NULL);
    VkQueueFamilyProperties family[16];
    if (families > 16)
        families = 16;
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &families, family);
    uint32_t qf = UINT32_MAX;
    for (uint32_t i = 0; i < families; i++)
        if (family[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            qf = i;
            break;
        }
    if (qf == UINT32_MAX)
        return fail("finding a graphics queue", VK_ERROR_FEATURE_NOT_PRESENT);

    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                    .queueFamilyIndex = qf, .queueCount = 1,
                                    .pQueuePriorities = &priority };
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
    VkDevice device;
    if ((r = vkCreateDevice(gpu, &dci, NULL, &device)) != VK_SUCCESS)
        return fail("vkCreateDevice", r);
    VkQueue queue;
    vkGetDeviceQueue(device, qf, 0, &queue);

    /* The image, in the GPU's own memory. */
    VkImageCreateInfo imci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                               .imageType = VK_IMAGE_TYPE_2D,
                               .format = VK_FORMAT_R8G8B8A8_UNORM,
                               .extent = { WIDTH, HEIGHT, 1 }, .mipLevels = 1,
                               .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
                               .tiling = VK_IMAGE_TILING_OPTIMAL,
                               .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT |
                                        VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                               .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    VkImage image;
    if ((r = vkCreateImage(device, &imci, NULL, &image)) != VK_SUCCESS)
        return fail("vkCreateImage", r);
    VkMemoryRequirements ireq;
    vkGetImageMemoryRequirements(device, image, &ireq);
    VkMemoryAllocateInfo iai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .allocationSize = ireq.size,
                                 .memoryTypeIndex = memory_type(gpu, ireq.memoryTypeBits,
                                     VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) };
    VkDeviceMemory imem;
    if ((r = vkAllocateMemory(device, &iai, NULL, &imem)) != VK_SUCCESS)
        return fail("vkAllocateMemory (image)", r);
    vkBindImageMemory(device, image, imem, 0);

    /* The buffer the GPU copies into, which the processor reads. */
    VkDeviceSize bytes = (VkDeviceSize)WIDTH * HEIGHT * 4;
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = bytes,
                               .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT };
    VkBuffer buffer;
    if ((r = vkCreateBuffer(device, &bci, NULL, &buffer)) != VK_SUCCESS)
        return fail("vkCreateBuffer", r);
    VkMemoryRequirements breq;
    vkGetBufferMemoryRequirements(device, buffer, &breq);
    VkMemoryAllocateInfo bai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .allocationSize = breq.size,
                                 .memoryTypeIndex = memory_type(gpu, breq.memoryTypeBits,
                                     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
    VkDeviceMemory bmem;
    if ((r = vkAllocateMemory(device, &bai, NULL, &bmem)) != VK_SUCCESS)
        return fail("vkAllocateMemory (buffer)", r);
    vkBindBufferMemory(device, buffer, bmem, 0);
    unsigned char *pixels;
    if ((r = vkMapMemory(device, bmem, 0, bytes, 0, (void **)&pixels)) != VK_SUCCESS)
        return fail("vkMapMemory", r);
    memset(pixels, 0xee, bytes);
    setvbuf(stdout, NULL, _IOLBF, 0);

    VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                                    .queueFamilyIndex = qf };
    VkCommandPool pool;
    if ((r = vkCreateCommandPool(device, &pci, NULL, &pool)) != VK_SUCCESS)
        return fail("vkCreateCommandPool", r);
    VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                                        .commandPool = pool,
                                        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                                        .commandBufferCount = 1 };
    VkCommandBuffer cmd;
    if ((r = vkAllocateCommandBuffers(device, &cai, &cmd)) != VK_SUCCESS)
        return fail("vkAllocateCommandBuffers", r);
    VkCommandBufferBeginInfo begin = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    vkBeginCommandBuffer(cmd, &begin);
    VkImageSubresourceRange range = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 };
    VkImageMemoryBarrier to_dst = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                                    .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                                    .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                    .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                    .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                    .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
                                    .image = image, .subresourceRange = range };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         0, 0, NULL, 0, NULL, 1, &to_dst);
    VkClearColorValue colour = { .float32 = { 0.25f, 0.5f, 0.75f, 1.0f } };
    vkCmdClearColorImage(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &colour, 1, &range);
    VkImageMemoryBarrier to_src = to_dst;
    to_src.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    to_src.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    to_src.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
    to_src.newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         0, 0, NULL, 0, NULL, 1, &to_src);
    VkBufferImageCopy copy = { .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 },
                               .imageExtent = { WIDTH, HEIGHT, 1 } };
    vkCmdCopyImageToBuffer(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buffer, 1, &copy);
    vkEndCommandBuffer(cmd);

    /* A buffer filled once by the processor, checked every round. */
    VkBufferCreateInfo kci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                               .size = KEPT_BYTES, .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT };
    VkBuffer kept;
    if ((r = vkCreateBuffer(device, &kci, NULL, &kept)) != VK_SUCCESS)
        return fail("vkCreateBuffer (kept)", r);
    VkMemoryRequirements kreq;
    vkGetBufferMemoryRequirements(device, kept, &kreq);
    VkMemoryAllocateInfo kai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                 .allocationSize = kreq.size,
                                 .memoryTypeIndex = memory_type(gpu, kreq.memoryTypeBits,
                                     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                     VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) };
    VkDeviceMemory kmem;
    if ((r = vkAllocateMemory(device, &kai, NULL, &kmem)) != VK_SUCCESS)
        return fail("vkAllocateMemory (kept)", r);
    vkBindBufferMemory(device, kept, kmem, 0);
    unsigned *words;
    if ((r = vkMapMemory(device, kmem, 0, KEPT_BYTES, 0, (void **)&words)) != VK_SUCCESS)
        return fail("vkMapMemory (kept)", r);
    for (unsigned i = 0; i < KEPT_BYTES / 4; i++)
        words[i] = 0x5eed0000u ^ (i * 2654435761u);

    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
    VkFence fence;
    if ((r = vkCreateFence(device, &fci, NULL, &fence)) != VK_SUCCESS)
        return fail("vkCreateFence", r);
    VkSubmitInfo submit = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                            .commandBufferCount = 1, .pCommandBuffers = &cmd };
    unsigned round = 0;
    for (;;) {
        round++;
        /* A reset that fails is said and the submit made all the same:
         * Mesa answers a failed DRM_IOCTL_SYNCOBJ_RESET VK_ERROR_UNKNOWN, as
         * on Linux after an unplug, and it is the submit or the wait that
         * must say the device is lost. */
        if ((r = vkResetFences(device, 1, &fence)) != VK_SUCCESS)
            printf("vk-survive: round %u: vkResetFences failed (VkResult %d); submitting all the same\n",
                   round, r);
        if ((r = vkQueueSubmit(queue, 1, &submit, fence)) != VK_SUCCESS)
            return ended(round, "vkQueueSubmit", r);
        r = vkWaitForFences(device, 1, &fence, VK_TRUE, 30ull * 1000 * 1000 * 1000);
        if (r != VK_SUCCESS)
            return ended(round, "vkWaitForFences", r);
        for (unsigned i = 0; i < KEPT_BYTES / 4; i++)
            if (words[i] != (0x5eed0000u ^ (i * 2654435761u))) {
                printf("vk-survive: round %u: the kept buffer lost its bytes at word %u\n",
                       round, i);
                return 3;
            }
        if (round == 1) {
            printf("vk-survive: ready on %s\n", props.deviceName);
            fflush(stdout);
        }
        if (round % 100 == 0) {
            printf("vk-survive: round %u answered\n", round);
            fflush(stdout);
        }
        if (access("/tmp/vk-survive.stop", F_OK) == 0)
            break;
        usleep(2000);
    }
    printf("vk-survive: survived: %u rounds, every one answered, the kept buffer kept its bytes\n",
           round);
    (void)pixels;
    vkDestroyFence(device, fence, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    return 0;
}
