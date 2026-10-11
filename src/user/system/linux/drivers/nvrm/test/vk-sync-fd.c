#include <vulkan/vulkan.h>
#include <stdio.h>
int main(void) {
  VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, "s", 1, "s", 1, VK_API_VERSION_1_3};
  VkInstanceCreateInfo ci = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, 0, &app};
  VkInstance in; if (vkCreateInstance(&ci, 0, &in)) return 1;
  uint32_t n = 1; VkPhysicalDevice pd; vkEnumeratePhysicalDevices(in, &n, &pd);
  VkPhysicalDeviceExternalSemaphoreInfo si = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_INFO, 0, VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT};
  VkExternalSemaphoreProperties sp = {VK_STRUCTURE_TYPE_EXTERNAL_SEMAPHORE_PROPERTIES};
  vkGetPhysicalDeviceExternalSemaphoreProperties(pd, &si, &sp);
  printf("vksync: semaphore SYNC_FD export %x compat %x features %x\n", sp.exportFromImportedHandleTypes, sp.compatibleHandleTypes, sp.externalSemaphoreFeatures);
  VkPhysicalDeviceExternalFenceInfo fi = {VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_FENCE_INFO, 0, VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT};
  VkExternalFenceProperties fp = {VK_STRUCTURE_TYPE_EXTERNAL_FENCE_PROPERTIES};
  vkGetPhysicalDeviceExternalFenceProperties(pd, &fi, &fp);
  printf("vksync: fence SYNC_FD compat %x features %x\n", fp.compatibleHandleTypes, fp.externalFenceFeatures);
  return 0;
}
