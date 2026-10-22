/*
 * vk-shader-cache: compile PIPELINES compute pipelines, each a different
 * specialisation of one shader, then destroy the device so that Mesa's
 * MESA_SHADER_CACHE_SHOW_STATS prints its disk cache's hits and misses.
 * Run twice against one cache directory, the second run must hit every
 * pipeline the first compiled (docs/INTEL-GPU.md: a game's "Shaders are
 * being compiled" step on each start).
 *
 * The shader (GLSL, compiled with glslangValidator -V):
 *
 *   layout(local_size_x = 64) in;
 *   layout(constant_id = 0) const uint K = 1;
 *   layout(std430, binding = 0) buffer B { uint v[]; };
 *   void main() {
 *       uint i = gl_GlobalInvocationID.x;
 *       uint x = v[i];
 *       for (uint j = 0; j < 8; j++) x = x * K + (x >> 3) ^ (K << j);
 *       v[i] = x;
 *   }
 *
 * Prints "vk-shader-cache: ..." lines and exits 0 when every pipeline was
 * created and the device destroyed.
 *
 * SPDX-License-Identifier: MIT
 */
#include <stdio.h>
#include <time.h>
#include <vulkan/vulkan.h>

#define PIPELINES 32

static const uint32_t spirv[] = {
    0x07230203, 0x00010000, 0x0008000b, 0x00000038, 0x00000000, 0x00020011,
    0x00000001, 0x0006000b, 0x00000001, 0x4c534c47, 0x6474732e, 0x3035342e,
    0x00000000, 0x0003000e, 0x00000000, 0x00000001, 0x0006000f, 0x00000005,
    0x00000004, 0x6e69616d, 0x00000000, 0x0000000b, 0x00060010, 0x00000004,
    0x00000011, 0x00000040, 0x00000001, 0x00000001, 0x00030003, 0x00000002,
    0x000001c2, 0x00040005, 0x00000004, 0x6e69616d, 0x00000000, 0x00030005,
    0x00000008, 0x00000069, 0x00080005, 0x0000000b, 0x475f6c67, 0x61626f6c,
    0x766e496c, 0x7461636f, 0x496e6f69, 0x00000044, 0x00030005, 0x00000010,
    0x00000078, 0x00030005, 0x00000012, 0x00000042, 0x00040006, 0x00000012,
    0x00000000, 0x00000076, 0x00030005, 0x00000014, 0x00000000, 0x00030005,
    0x0000001b, 0x0000006a, 0x00030005, 0x00000026, 0x0000004b, 0x00040047,
    0x0000000b, 0x0000000b, 0x0000001c, 0x00040047, 0x00000011, 0x00000006,
    0x00000004, 0x00030047, 0x00000012, 0x00000003, 0x00050048, 0x00000012,
    0x00000000, 0x00000023, 0x00000000, 0x00040047, 0x00000014, 0x00000021,
    0x00000000, 0x00040047, 0x00000014, 0x00000022, 0x00000000, 0x00040047,
    0x00000026, 0x00000001, 0x00000000, 0x00040047, 0x00000037, 0x0000000b,
    0x00000019, 0x00020013, 0x00000002, 0x00030021, 0x00000003, 0x00000002,
    0x00040015, 0x00000006, 0x00000020, 0x00000000, 0x00040020, 0x00000007,
    0x00000007, 0x00000006, 0x00040017, 0x00000009, 0x00000006, 0x00000003,
    0x00040020, 0x0000000a, 0x00000001, 0x00000009, 0x0004003b, 0x0000000a,
    0x0000000b, 0x00000001, 0x0004002b, 0x00000006, 0x0000000c, 0x00000000,
    0x00040020, 0x0000000d, 0x00000001, 0x00000006, 0x0003001d, 0x00000011,
    0x00000006, 0x0003001e, 0x00000012, 0x00000011, 0x00040020, 0x00000013,
    0x00000002, 0x00000012, 0x0004003b, 0x00000013, 0x00000014, 0x00000002,
    0x00040015, 0x00000015, 0x00000020, 0x00000001, 0x0004002b, 0x00000015,
    0x00000016, 0x00000000, 0x00040020, 0x00000018, 0x00000002, 0x00000006,
    0x0004002b, 0x00000006, 0x00000022, 0x00000008, 0x00020014, 0x00000023,
    0x00040032, 0x00000006, 0x00000026, 0x00000001, 0x0004002b, 0x00000015,
    0x00000029, 0x00000003, 0x0004002b, 0x00000015, 0x00000030, 0x00000001,
    0x0004002b, 0x00000006, 0x00000035, 0x00000040, 0x0004002b, 0x00000006,
    0x00000036, 0x00000001, 0x0006002c, 0x00000009, 0x00000037, 0x00000035,
    0x00000036, 0x00000036, 0x00050036, 0x00000002, 0x00000004, 0x00000000,
    0x00000003, 0x000200f8, 0x00000005, 0x0004003b, 0x00000007, 0x00000008,
    0x00000007, 0x0004003b, 0x00000007, 0x00000010, 0x00000007, 0x0004003b,
    0x00000007, 0x0000001b, 0x00000007, 0x00050041, 0x0000000d, 0x0000000e,
    0x0000000b, 0x0000000c, 0x0004003d, 0x00000006, 0x0000000f, 0x0000000e,
    0x0003003e, 0x00000008, 0x0000000f, 0x0004003d, 0x00000006, 0x00000017,
    0x00000008, 0x00060041, 0x00000018, 0x00000019, 0x00000014, 0x00000016,
    0x00000017, 0x0004003d, 0x00000006, 0x0000001a, 0x00000019, 0x0003003e,
    0x00000010, 0x0000001a, 0x0003003e, 0x0000001b, 0x0000000c, 0x000200f9,
    0x0000001c, 0x000200f8, 0x0000001c, 0x000400f6, 0x0000001e, 0x0000001f,
    0x00000000, 0x000200f9, 0x00000020, 0x000200f8, 0x00000020, 0x0004003d,
    0x00000006, 0x00000021, 0x0000001b, 0x000500b0, 0x00000023, 0x00000024,
    0x00000021, 0x00000022, 0x000400fa, 0x00000024, 0x0000001d, 0x0000001e,
    0x000200f8, 0x0000001d, 0x0004003d, 0x00000006, 0x00000025, 0x00000010,
    0x00050084, 0x00000006, 0x00000027, 0x00000025, 0x00000026, 0x0004003d,
    0x00000006, 0x00000028, 0x00000010, 0x000500c2, 0x00000006, 0x0000002a,
    0x00000028, 0x00000029, 0x00050080, 0x00000006, 0x0000002b, 0x00000027,
    0x0000002a, 0x0004003d, 0x00000006, 0x0000002c, 0x0000001b, 0x000500c4,
    0x00000006, 0x0000002d, 0x00000026, 0x0000002c, 0x000500c6, 0x00000006,
    0x0000002e, 0x0000002b, 0x0000002d, 0x0003003e, 0x00000010, 0x0000002e,
    0x000200f9, 0x0000001f, 0x000200f8, 0x0000001f, 0x0004003d, 0x00000006,
    0x0000002f, 0x0000001b, 0x00050080, 0x00000006, 0x00000031, 0x0000002f,
    0x00000030, 0x0003003e, 0x0000001b, 0x00000031, 0x000200f9, 0x0000001c,
    0x000200f8, 0x0000001e, 0x0004003d, 0x00000006, 0x00000032, 0x00000008,
    0x0004003d, 0x00000006, 0x00000033, 0x00000010, 0x00060041, 0x00000018,
    0x00000034, 0x00000014, 0x00000016, 0x00000032, 0x0003003e, 0x00000034,
    0x00000033, 0x000100fd, 0x00010038
};

static int fail(const char *what, VkResult result)
{
    printf("vk-shader-cache: %s failed (VkResult %d)\n", what, result);
    return 1;
}

int main(void)
{
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .pApplicationName = "vk-shader-cache",
                             .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo instance_info = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                          .pApplicationInfo = &app};
    VkInstance instance;
    VkResult result = vkCreateInstance(&instance_info, NULL, &instance);
    if (result != VK_SUCCESS)
        return fail("vkCreateInstance", result);
    uint32_t count = 1;
    VkPhysicalDevice gpu;
    result = vkEnumeratePhysicalDevices(instance, &count, &gpu);
    if ((result != VK_SUCCESS && result != VK_INCOMPLETE) || count == 0)
        return fail("vkEnumeratePhysicalDevices", result);
    VkPhysicalDeviceProperties properties;
    vkGetPhysicalDeviceProperties(gpu, &properties);

    float priority = 1.0f;
    VkDeviceQueueCreateInfo queue = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                     .queueFamilyIndex = 0,
                                     .queueCount = 1,
                                     .pQueuePriorities = &priority};
    VkDeviceCreateInfo device_info = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                                      .queueCreateInfoCount = 1,
                                      .pQueueCreateInfos = &queue};
    VkDevice device;
    result = vkCreateDevice(gpu, &device_info, NULL, &device);
    if (result != VK_SUCCESS)
        return fail("vkCreateDevice", result);

    VkShaderModuleCreateInfo module_info = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                            .codeSize = sizeof(spirv),
                                            .pCode = spirv};
    VkShaderModule module;
    result = vkCreateShaderModule(device, &module_info, NULL, &module);
    if (result != VK_SUCCESS)
        return fail("vkCreateShaderModule", result);
    VkDescriptorSetLayoutBinding binding = {.binding = 0,
                                            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                                            .descriptorCount = 1,
                                            .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT};
    VkDescriptorSetLayoutCreateInfo set_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 1,
        .pBindings = &binding};
    VkDescriptorSetLayout set;
    result = vkCreateDescriptorSetLayout(device, &set_info, NULL, &set);
    if (result != VK_SUCCESS)
        return fail("vkCreateDescriptorSetLayout", result);
    VkPipelineLayoutCreateInfo layout_info = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                              .setLayoutCount = 1,
                                              .pSetLayouts = &set};
    VkPipelineLayout layout;
    result = vkCreatePipelineLayout(device, &layout_info, NULL, &layout);
    if (result != VK_SUCCESS)
        return fail("vkCreatePipelineLayout", result);

    struct timespec start, end;
    clock_gettime(CLOCK_MONOTONIC, &start);
    VkPipeline pipelines[PIPELINES];
    for (uint32_t i = 0; i < PIPELINES; i++) {
        uint32_t k = i + 2;
        VkSpecializationMapEntry entry = {.constantID = 0, .offset = 0, .size = sizeof(k)};
        VkSpecializationInfo specialization = {.mapEntryCount = 1,
                                               .pMapEntries = &entry,
                                               .dataSize = sizeof(k),
                                               .pData = &k};
        VkComputePipelineCreateInfo pipeline_info = {
            .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
            .stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                      .stage = VK_SHADER_STAGE_COMPUTE_BIT,
                      .module = module,
                      .pName = "main",
                      .pSpecializationInfo = &specialization},
            .layout = layout};
        result = vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL,
                                          &pipelines[i]);
        if (result != VK_SUCCESS)
            return fail("vkCreateComputePipelines", result);
    }
    clock_gettime(CLOCK_MONOTONIC, &end);
    long ms = (end.tv_sec - start.tv_sec) * 1000 + (end.tv_nsec - start.tv_nsec) / 1000000;
    printf("vk-shader-cache: %d pipelines on %s in %ld ms\n", PIPELINES, properties.deviceName, ms);

    for (uint32_t i = 0; i < PIPELINES; i++)
        vkDestroyPipeline(device, pipelines[i], NULL);
    vkDestroyPipelineLayout(device, layout, NULL);
    vkDestroyDescriptorSetLayout(device, set, NULL);
    vkDestroyShaderModule(device, module, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    printf("vk-shader-cache: device destroyed\n");
    return 0;
}
