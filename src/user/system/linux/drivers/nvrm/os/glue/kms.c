/*
 * The first light on a GPU's displays (docs/NVIDIA.md §4.6): nvrm, as
 * NVKMS's only client, through KAPI, the kernel-client interface
 * nvidia-drm uses on Linux.
 *
 * For each GPU NVKMS enumerates, this takes ownership of the display
 * engine, says what each connector has attached -- the monitor's EDID name
 * and its preferred mode -- and on the first connected display sets that
 * mode with a pitch-linear X8R8G8B8 surface in video memory, filled with a
 * test pattern through a kernel mapping. That head is then offered to the
 * kernel's display core as a card (N6), whose frames are copied onto the
 * same surface. Every step says how far it got, as nvrm's start does.
 *
 * SPDX-License-Identifier: MIT
 */
#include "nv-ferrix.h"
#include "nvidia-modeset-os-interface.h"
#include "nvkms.h"

#include "class/cl0000.h"
#include "class/cl0073.h"
#include "class/cl0080.h"
#include "ctrl/ctrl0000/ctrl0000gpu.h"
#include "ctrl/ctrl0073/ctrl0073specific.h"
#include "ctrl/ctrl0073/ctrl0073system.h"

/* NVKMS's RM calls (nvkms-rmapi.h), which the core exports. */
NvU32 nvRmApiAlloc(NvU32 hClient, NvU32 hParent, NvU32 hObject, NvU32 hClass,
                   void *pAllocParams);
NvU32 nvRmApiControl(NvU32 hClient, NvU32 hObject, NvU32 cmd, void *pParams,
                     NvU32 paramsSize);
NvU32 nvRmApiFree(NvU32 hClient, NvU32 hParent, NvU32 hObject);

#define kms_say(...)   nv_printf(NV_DBG_ERRORS, "nvrm: kms: " __VA_ARGS__)

/* How many one-second looks at the connectors before giving up. */
#define KMS_TRIES 8

static struct NvKmsKapiFunctionsTable kapi = {
    .versionString = NV_VERSION_STRING,
};

static nv_gpu_info_t kms_gpus[NV_MAX_GPUS];

/* The first GPU's id and generic page kind, for os/glue/drm.c's GET_DEV_INFO. */
static NvU32 kms_drm_gpu_id;
static NvU32 kms_drm_page_kind;
static NvU32 kms_gpu_count;

static void kms_gpu_found(const struct NvKmsKapiGpuInfo *info)
{
    if (kms_gpu_count < NV_MAX_GPUS && info->migDevice == NO_MIG_DEVICE)
        kms_gpus[kms_gpu_count++] = info->gpuInfo;
}

static void kms_event(const struct NvKmsKapiEvent *event)
{
    kms_say("event %d\n", (int)event->type);
}

/* The monitor's name from an EDID's display descriptors, or "". */
static void kms_edid_name(const NvU8 *edid, NvU32 size, char *out, NvU32 out_size)
{
    NvU32 at, i, n = 0;

    out[0] = '\0';
    if (size < 128)
        return;
    for (at = 54; at + 18 <= 126; at += 18)
    {
        if (edid[at] != 0 || edid[at + 1] != 0 || edid[at + 3] != 0xfc)
            continue;
        for (i = 5; i < 18 && n + 1 < out_size; i++)
        {
            if (edid[at + i] == '\n')
                break;
            out[n++] = (char)edid[at + i];
        }
        out[n] = '\0';
        return;
    }
}

/* Colour bars over a grey ramp, so a wrong pitch or format shows. */
static void kms_pattern(NvU8 *base, NvU32 width, NvU32 height, NvU32 pitch)
{
    static const NvU32 bars[8] = {
        0xffffff, 0xffff00, 0x00ffff, 0x00ff00, 0xff00ff, 0xff0000, 0x0000ff, 0x000000,
    };
    NvU32 x, y;

    for (y = 0; y < height; y++)
    {
        volatile NvU32 *row = (volatile NvU32 *)(base + (NvU64)y * pitch);
        for (x = 0; x < width; x++)
        {
            NvU32 pixel;
            if (y < height * 2 / 3)
            {
                pixel = bars[(NvU64)x * 8 / width];
            }
            else
            {
                NvU32 level = (NvU32)((NvU64)x * 255 / (width - 1));
                pixel = level << 16 | level << 8 | level;
            }
            /* A one-pixel white frame marks the edges of the mode. */
            if (x == 0 || y == 0 || x == width - 1 || y == height - 1)
                pixel = 0xffffff;
            row[x] = pixel;
        }
    }
}

/*
 * When NVKMS finds no display: ask RM directly, from a client of nvrm's
 * own, which displays it supports, which it calls connected by each
 * method, and whether each answers an EDID read over DDC or AUX. This
 * separates a hot-plug detect RM does not see from no monitor at all.
 */
static void kms_rm_probe(NvU32 gpu_id)
{
    NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS id = { .gpuId = gpu_id };
    NV0080_ALLOC_PARAMETERS dev = { 0 };
    NV0073_CTRL_SYSTEM_GET_SUPPORTED_PARAMS supported = { 0 };
    NV0073_CTRL_SPECIFIC_GET_EDID_V2_PARAMS *edid;
    static const NvU32 methods[3] = {
        NV0073_CTRL_SYSTEM_GET_CONNECT_STATE_FLAGS_METHOD_DEFAULT,
        NV0073_CTRL_SYSTEM_GET_CONNECT_STATE_FLAGS_METHOD_CACHED,
        NV0073_CTRL_SYSTEM_GET_CONNECT_STATE_FLAGS_METHOD_ECONODDC,
    };
    const NvU32 hDevice = 0xf0e10001, hDisp = 0xf0e10002;
    NvU32 hClient = 0, status, m, bit;

    status = nvRmApiAlloc(0, 0, 0, NV01_ROOT, &hClient);
    if (status != NV_OK)
    {
        kms_say("probe: root alloc 0x%x\n", status);
        return;
    }
    status = nvRmApiControl(hClient, hClient, NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2, &id,
                            sizeof(id));
    dev.deviceId = id.deviceInstance;
    if (status == NV_OK)
        status = nvRmApiAlloc(hClient, hClient, hDevice, NV01_DEVICE_0, &dev);
    if (status == NV_OK)
        status = nvRmApiAlloc(hClient, hDevice, hDisp, NV04_DISPLAY_COMMON, NULL);
    if (status != NV_OK)
    {
        kms_say("probe: device/display alloc 0x%x\n", status);
        goto out;
    }

    supported.subDeviceInstance = id.subDeviceInstance;
    status = nvRmApiControl(hClient, hDisp, NV0073_CTRL_CMD_SYSTEM_GET_SUPPORTED, &supported,
                            sizeof(supported));
    kms_say("probe: supported 0x%x (DDC 0x%x), status 0x%x\n", supported.displayMask,
            supported.displayMaskDDC, status);

    for (m = 0; m < 3; m++)
    {
        NV0073_CTRL_SYSTEM_GET_CONNECT_STATE_PARAMS connect = { 0 };
        NvU32 tries = 0;
        do
        {
            connect.subDeviceInstance = id.subDeviceInstance;
            connect.flags = methods[m];
            connect.displayMask = supported.displayMask;
            connect.retryTimeMs = 0;
            status = nvRmApiControl(hClient, hDisp, NV0073_CTRL_CMD_SYSTEM_GET_CONNECT_STATE,
                                    &connect, sizeof(connect));
            if (connect.retryTimeMs > 0)
                nvkms_usleep(connect.retryTimeMs * 1000ull);
        } while (connect.retryTimeMs > 0 && ++tries < 50);
        kms_say("probe: connect state by method %u: 0x%x, status 0x%x\n", methods[m],
                connect.displayMask, status);
    }

    edid = calloc(1, sizeof(*edid));
    if (edid == NULL)
        goto out;
    for (bit = 0; bit < 32; bit++)
    {
        char name[16];
        if (!(supported.displayMask & (1u << bit)))
            continue;
        memset(edid, 0, sizeof(*edid));
        edid->subDeviceInstance = id.subDeviceInstance;
        edid->displayId = 1u << bit;
        edid->bufferSize = sizeof(edid->edidBuffer);
        status = nvRmApiControl(hClient, hDisp, NV0073_CTRL_CMD_SPECIFIC_GET_EDID_V2, edid,
                                sizeof(*edid));
        kms_edid_name(edid->edidBuffer, status == NV_OK ? edid->bufferSize : 0, name,
                      sizeof(name));
        kms_say("probe: EDID of 0x%x: status 0x%x, %u bytes%s%s\n", 1u << bit, status,
                status == NV_OK ? edid->bufferSize : 0, name[0] ? ", " : "", name);
    }
    free(edid);

out:
    (void)nvRmApiFree(hClient, hClient, hClient);
}

struct kms_head {
    struct NvKmsKapiDevice *device;
    struct NvKmsKapiMemory *memory;
    struct NvKmsKapiSurface *surface;
    void *mapped;
    struct NvKmsKapiDisplayMode mode;
    NvU32 head;
    NvKmsKapiDisplay display;
    NvU32 pitch;
    NvU32 connector;    /* NvKmsConnectorType */
    NvU32 edid_bytes;
    NvU8 edid[NVKMS_KAPI_EDID_BUFFER_SIZE];
    /* What the first light found, for a light at another mode. */
    NvU32 pitch_align;
    NvBool video_memory;
    /* Every mode NVKMS validated for the display, the running one first,
     * as the card lists them (displayctl v11, docs/DISPLAY.md section 9.4). */
    struct NvKmsKapiDisplayMode modes[NVOS_DISPLAY_MODES];
    NvU32 mode_count;
    NvU32 preferred;
};

/* What is on screen, kept so that it stays there. */
static struct kms_head kms_lit[NV_MAX_GPUS];

static int kms_light(struct kms_head *lit, NvBool pattern)
{
    struct NvKmsKapiDevice *device = lit->device;
    struct NvKmsKapiAllocateMemoryParams alloc;
    struct NvKmsKapiCreateSurfaceParams surf;
    struct NvKmsKapiRequestedModeSetConfig *request;
    struct NvKmsKapiModeSetReplyConfig *reply;
    struct NvKmsKapiHeadRequestedConfig *head;
    struct NvKmsKapiLayerRequestedConfig *layer;
    NvU32 width = lit->mode.timings.hVisible;
    NvU32 height = lit->mode.timings.vVisible;
    NvU32 align = lit->pitch_align ? lit->pitch_align : 256;
    NvU8 compressible = 0;
    NvBool ok;

    lit->pitch = (width * 4 + align - 1) / align * align;

    memset(&alloc, 0, sizeof(alloc));
    alloc.layout = NvKmsSurfaceMemoryLayoutPitch;
    alloc.type = NVKMS_KAPI_ALLOCATION_TYPE_SCANOUT;
    alloc.size = (NvU64)lit->pitch * height;
    alloc.useVideoMemory = lit->video_memory;
    alloc.compressible = &compressible;
    lit->memory = kapi.allocateMemory(device, &alloc);
    if (lit->memory == NULL)
    {
        kms_say("allocateMemory of %llu bytes (video %d) failed\n",
                (unsigned long long)alloc.size, (int)alloc.useVideoMemory);
        return -ENOMEM;
    }

    if (!kapi.mapMemory(device, lit->memory, NVKMS_KAPI_MAPPING_TYPE_KERNEL, &lit->mapped))
    {
        kms_say("mapMemory failed\n");
        return -EIO;
    }
    if (pattern)
    {
        kms_pattern(lit->mapped, width, height, lit->pitch);
        kms_say("%ux%u surface, pitch %u, filled with the test pattern\n", width, height,
                lit->pitch);
    }
    else
    {
        /* A mode a program set: black until its first frame is copied. */
        NvU32 row;
        for (row = 0; row < height; row++)
            memset((NvU8 *)lit->mapped + (NvU64)row * lit->pitch, 0, (size_t)width * 4);
    }

    memset(&surf, 0, sizeof(surf));
    surf.planes[0].memory = lit->memory;
    surf.planes[0].offset = 0;
    surf.planes[0].pitch = lit->pitch;
    surf.width = width;
    surf.height = height;
    surf.format = NvKmsSurfaceMemoryFormatX8R8G8B8;
    lit->surface = kapi.createSurface(device, &surf);
    if (lit->surface == NULL)
    {
        kms_say("createSurface failed\n");
        return -EIO;
    }

    request = calloc(1, sizeof(*request));
    reply = calloc(1, sizeof(*reply));
    if (request == NULL || reply == NULL)
    {
        free(request);
        free(reply);
        return -ENOMEM;
    }

    request->headsMask = 1u << lit->head;
    head = &request->headRequestedConfig[lit->head];
    head->modeSetConfig.bActive = NV_TRUE;
    head->modeSetConfig.numDisplays = 1;
    head->modeSetConfig.displays[0] = lit->display;
    head->modeSetConfig.mode = lit->mode;
    head->modeSetConfig.colorimetry = NVKMS_OUTPUT_COLORIMETRY_DEFAULT;
    head->modeSetConfig.olutFpNormScale = NVKMS_OLUT_FP_NORM_SCALE_DEFAULT;
    head->flags.activeChanged = NV_TRUE;
    head->flags.displaysChanged = NV_TRUE;
    head->flags.modeChanged = NV_TRUE;

    layer = &head->layerRequestedConfig[NVKMS_KAPI_LAYER_PRIMARY_IDX];
    layer->config.surface = lit->surface;
    layer->config.compParams.compMode = NVKMS_COMPOSITION_BLENDING_MODE_OPAQUE;
    layer->config.compParams.surfaceAlpha = 0xff;
    layer->config.csc = (struct NvKmsCscMatrix)NVKMS_IDENTITY_CSC_MATRIX;
    layer->config.inputTf = NVKMS_INPUT_TF_LINEAR;
    layer->config.outputTf = NVKMS_OUTPUT_TF_NONE;
    layer->config.inputColorSpace = NVKMS_INPUT_COLOR_SPACE_NONE;
    layer->config.inputColorRange = NVKMS_INPUT_COLOR_RANGE_DEFAULT;
    layer->config.minPresentInterval = 1;
    layer->config.srcWidth = (NvU16)width;
    layer->config.srcHeight = (NvU16)height;
    layer->config.dstWidth = (NvU16)width;
    layer->config.dstHeight = (NvU16)height;
    layer->flags.surfaceChanged = NV_TRUE;
    layer->flags.srcXYChanged = NV_TRUE;
    layer->flags.srcWHChanged = NV_TRUE;
    layer->flags.dstXYChanged = NV_TRUE;
    layer->flags.dstWHChanged = NV_TRUE;
    layer->flags.cscChanged = NV_TRUE;
    layer->flags.inputTfChanged = NV_TRUE;
    layer->flags.outputTfChanged = NV_TRUE;
    layer->flags.inputColorSpaceChanged = NV_TRUE;
    layer->flags.inputColorRangeChanged = NV_TRUE;

    ok = kapi.applyModeSetConfig(device, request, reply, NV_FALSE);
    if (!ok)
    {
        kms_say("the mode set did not validate\n");
    }
    else
    {
        ok = kapi.applyModeSetConfig(device, request, reply, NV_TRUE);
        if (!ok)
            kms_say("the mode set did not commit (flip result %d)\n", (int)reply->flipResult);
    }
    free(request);
    free(reply);
    if (!ok)
        return -EIO;

    kms_say("head %u shows the test pattern at %ux%u@%u.%03u Hz\n", lit->head, width,
            height, lit->mode.timings.refreshRate / 1000, lit->mode.timings.refreshRate % 1000);
    return 0;
}

/* Find the preferred mode of a connected display. */
static NvBool kms_preferred_mode(struct NvKmsKapiDevice *device, NvKmsKapiDisplay display,
                                 struct NvKmsKapiDisplayMode *out)
{
    struct NvKmsKapiDisplayMode mode;
    NvBool valid, preferred, found = NV_FALSE;
    NvU32 index;
    int ret;

    for (index = 0; index < 512; index++)
    {
        valid = preferred = NV_FALSE;
        ret = kapi.getDisplayMode(device, display, index, &mode, &valid, &preferred);
        if (ret <= 0)
            break;
        if (!valid)
            continue;
        if (!found || preferred)
        {
            *out = mode;
            found = NV_TRUE;
        }
        if (preferred)
            break;
    }
    return found;
}

/* Whether two NVKMS modes are the same timing. */
static NvBool kms_same_mode(const struct NvKmsKapiDisplayMode *one,
                            const struct NvKmsKapiDisplayMode *other)
{
    const struct NvKmsKapiDisplayModeTimings *a = &one->timings, *b = &other->timings;

    return a->pixelClockHz == b->pixelClockHz && a->hVisible == b->hVisible &&
           a->hSyncStart == b->hSyncStart && a->hSyncEnd == b->hSyncEnd &&
           a->hTotal == b->hTotal && a->vVisible == b->vVisible &&
           a->vSyncStart == b->vSyncStart && a->vSyncEnd == b->vSyncEnd &&
           a->vTotal == b->vTotal && a->flags.hSyncPos == b->flags.hSyncPos &&
           a->flags.vSyncPos == b->flags.vSyncPos;
}

/*
 * Every mode NVKMS validates for the lit display, as nvidia-drm's connector
 * lists them on Linux: the running one (lit->mode) first, then the rest in
 * NVKMS's order, at most NVOS_DISPLAY_MODES; `preferred` is the one NVKMS
 * calls preferred.
 */
static void kms_list_modes(struct kms_head *lit)
{
    struct NvKmsKapiDisplayMode mode;
    NvBool valid, preferred;
    NvU32 index, at;
    int ret;

    lit->modes[0] = lit->mode;
    lit->mode_count = 1;
    lit->preferred = 0;
    for (index = 0; index < 512 && lit->mode_count < NVOS_DISPLAY_MODES; index++)
    {
        valid = preferred = NV_FALSE;
        ret = kapi.getDisplayMode(lit->device, lit->display, index, &mode, &valid, &preferred);
        if (ret <= 0)
            break;
        if (!valid)
            continue;
        for (at = 0; at < lit->mode_count; at++)
            if (kms_same_mode(&lit->modes[at], &mode))
                break;
        if (at == lit->mode_count)
            lit->modes[lit->mode_count++] = mode;
        if (preferred)
            lit->preferred = at;
    }
    kms_say("%u modes listed, %ux%u preferred\n", lit->mode_count,
            lit->modes[lit->preferred].timings.hVisible,
            lit->modes[lit->preferred].timings.vVisible);
}

/*
 * Run the lit head at `mode`, one of lit->modes: a VRAM surface of its size
 * made, the mode set on it (validated, then committed, as the first light),
 * and the surface before it let go. 0, or why not, with the head as it was.
 */
static int kms_relight(struct kms_head *lit, const struct NvKmsKapiDisplayMode *mode)
{
    struct NvKmsKapiDevice *device = lit->device;
    struct NvKmsKapiDisplayMode was = lit->mode;
    struct NvKmsKapiMemory *old_memory = lit->memory;
    struct NvKmsKapiSurface *old_surface = lit->surface;
    void *old_mapped = lit->mapped;
    NvU32 old_pitch = lit->pitch;
    int ret;

    lit->mode = *mode;
    lit->memory = NULL;
    lit->surface = NULL;
    lit->mapped = NULL;
    ret = kms_light(lit, NV_FALSE);
    if (ret != 0)
    {
        /* What the failed light made goes; the old surface is still shown. */
        if (lit->surface != NULL)
            kapi.destroySurface(device, lit->surface);
        if (lit->mapped != NULL)
            kapi.unmapMemory(device, lit->memory, NVKMS_KAPI_MAPPING_TYPE_KERNEL, lit->mapped);
        if (lit->memory != NULL)
            kapi.freeMemory(device, lit->memory);
        lit->mode = was;
        lit->memory = old_memory;
        lit->surface = old_surface;
        lit->mapped = old_mapped;
        lit->pitch = old_pitch;
        kms_say("mode %ux%u not set (%d); the head stays %ux%u\n", mode->timings.hVisible,
                mode->timings.vVisible, ret, was.timings.hVisible, was.timings.vVisible);
        return ret;
    }
    kapi.destroySurface(device, old_surface);
    kapi.unmapMemory(device, old_memory, NVKMS_KAPI_MAPPING_TYPE_KERNEL, old_mapped);
    kapi.freeMemory(device, old_memory);
    kms_say("head %u is %ux%u@%u.%03u Hz now\n", lit->head, mode->timings.hVisible,
            mode->timings.vVisible, mode->timings.refreshRate / 1000,
            mode->timings.refreshRate % 1000);
    return 0;
}

/*
 * One look at every display: light the first connected one. 0 once one is
 * lit, -ENODEV while none is connected, or why lighting failed. `verbose`
 * says what each display is, connected or not.
 */
static int kms_scan(struct kms_head *lit, const struct NvKmsKapiDeviceResourcesInfo *res,
                    struct NvKmsKapiDynamicDisplayParams *dyn,
                    const NvKmsKapiDisplay *displays, NvU32 count, NvBool verbose)
{
    NvU32 i;

    for (i = 0; i < count; i++)
    {
        struct NvKmsKapiStaticDisplayInfo info;
        char name[16];

        memset(&info, 0, sizeof(info));
        if (!kapi.getStaticDisplayInfo(lit->device, displays[i], &info))
            continue;
        memset(dyn, 0, sizeof(*dyn));
        dyn->handle = displays[i];
        if (!kapi.getDynamicDisplayInfo(lit->device, dyn))
            continue;
        kms_edid_name(dyn->edid.buffer, dyn->edid.bufferSize, name, sizeof(name));
        if (dyn->connected || verbose)
            kms_say("display %x on connector %u: %s%s%s, heads %x\n", displays[i],
                    info.connectorHandle, dyn->connected ? "connected" : "not connected",
                    name[0] ? ", " : "", name, info.headMask);
        if (!dyn->connected || info.headMask == 0)
            continue;

        if (!kms_preferred_mode(lit->device, displays[i], &lit->mode))
        {
            kms_say("display %x has no valid mode\n", displays[i]);
            continue;
        }
        kms_say("mode %s: %ux%u, pixel clock %u kHz\n", lit->mode.name,
                lit->mode.timings.hVisible, lit->mode.timings.vVisible,
                lit->mode.timings.pixelClockHz / 1000);
        lit->display = displays[i];
        lit->head = (NvU32)__builtin_ctz(info.headMask);
        {
            struct NvKmsKapiConnectorInfo socket;
            memset(&socket, 0, sizeof(socket));
            lit->connector = kapi.getConnectorInfo(lit->device, info.connectorHandle, &socket)
                                 ? socket.type : NVKMS_CONNECTOR_TYPE_UNKNOWN;
        }
        lit->edid_bytes = dyn->edid.bufferSize <= sizeof(lit->edid) ? dyn->edid.bufferSize : 0;
        memcpy(lit->edid, dyn->edid.buffer, lit->edid_bytes);
        lit->pitch_align = res->caps.pitchAlignment;
        lit->video_memory = res->caps.hasVideoMemory != 0;
        kms_list_modes(lit);
        return kms_light(lit, NV_TRUE);
    }
    return -ENODEV;
}

static int kms_show_gpu(NvU32 index)
{
    struct NvKmsKapiAllocateDeviceParams params;
    struct NvKmsKapiDeviceResourcesInfo *res;
    struct NvKmsKapiDynamicDisplayParams *dyn;
    struct kms_head *lit = &kms_lit[index];
    NvKmsKapiDisplay displays[NVKMS_KAPI_MAX_CONNECTORS * 4];
    NvU32 count = NV_ARRAY_ELEMENTS(displays), c, try;
    int status = -ENODEV;

    memset(&params, 0, sizeof(params));
    params.gpuId = kms_gpus[index].gpu_id;
    params.migDevice = NO_MIG_DEVICE;
    params.privateData = lit;
    params.eventCallback = kms_event;
    lit->device = kapi.allocateDevice(&params);
    if (lit->device == NULL)
    {
        kms_say("allocateDevice for GPU %x failed\n", params.gpuId);
        return -ENODEV;
    }
    if (!kapi.grabOwnership(lit->device))
    {
        kms_say("grabOwnership failed\n");
        return -EBUSY;
    }

    res = calloc(1, sizeof(*res));
    dyn = calloc(1, sizeof(*dyn));
    if (res == NULL || dyn == NULL)
    {
        status = -ENOMEM;
        goto out;
    }
    if (!kapi.getDeviceResourcesInfo(lit->device, res))
    {
        kms_say("getDeviceResourcesInfo failed\n");
        status = -EIO;
        goto out;
    }
    if (index == 0)
    {
        kms_drm_gpu_id = params.gpuId;
        kms_drm_page_kind = res->caps.genericPageKind;
    }
    kms_say("GPU %x: %u heads, %u connectors, video memory %u, max %ux%u\n",
            params.gpuId, res->numHeads, res->numConnectors, res->caps.hasVideoMemory,
            res->caps.maxWidthInPixels, res->caps.maxHeightInPixels);

    for (c = 0; c < res->numConnectors; c++)
    {
        struct NvKmsKapiConnectorInfo info;
        memset(&info, 0, sizeof(info));
        if (kapi.getConnectorInfo(lit->device, res->connectorHandles[c], &info))
            kms_say("connector %u: %s, physical index %u\n", info.handle,
                    NvKmsConnectorTypeString(info.type), info.physicalIndex);
    }

    if (!kapi.getDisplays(lit->device, &count, displays))
    {
        kms_say("getDisplays failed\n");
        status = -EIO;
        goto out;
    }

    /*
     * The DisplayPort library settles a connector's state from AUX
     * transactions after the device is allocated; look again for a while
     * before calling every connector empty.
     */
    for (try = 0; try < KMS_TRIES && status == -ENODEV; try++)
    {
        if (try > 0)
            nvkms_usleep(1000000);
        status = kms_scan(lit, res, dyn, displays, count,
                          try == 0 || try == KMS_TRIES - 1);
    }
    if (status == -ENODEV)
    {
        kms_say("no connected display on GPU %x\n", params.gpuId);
        kms_rm_probe(params.gpuId);
    }

out:
    free(res);
    free(dyn);
    return status;
}

/* ------------------------------------------------------------------------
 * The card (N6): nvrm as the kernel display core's driver for the head lit
 * above. Each FLUSH copies the flushed rectangle of the buffer it names
 * from the buffer's mapping, which nvos makes read-only at ATTACH, into the VRAM surface the
 * head scans out, and is answered at once: the copy is the flip. Every mode
 * NVKMS validated is offered, and SET_MODE sets one (kms_relight).
 * ---------------------------------------------------------------------- */

#define KMS_BUFFERS 64

struct kms_buffer {
    NvU32 id;
    const NvU8 *pixels;
    NvU32 width, height, stride;
};

struct kms_card {
    struct kms_head *lit;
    struct kms_buffer buffers[KMS_BUFFERS];
    /* A SET_MODE that could not be set, which the next FLUSH answers. */
    NvBool mode_failed;
    NvU32 shown;
    NvU64 flushes;
    NvU64 copy_usec;
};

static struct kms_card kms_card;

static struct kms_buffer *kms_buffer(struct kms_card *card, NvU32 id)
{
    NvU32 i;

    for (i = 0; id != 0 && i < KMS_BUFFERS; i++)
        if (card->buffers[i].id == id)
            return &card->buffers[i];
    return NULL;
}

/*
 * One row into the surface. The surface is video memory mapped
 * write-combining through BAR1, where the C library's byte-at-a-time copy
 * costs a frame most of its budget; x86-64's string move writes it in whole
 * lines.
 */
static void kms_copy_row(NvU8 *to, const NvU8 *from, size_t bytes)
{
#if defined(__x86_64__)
    __asm__ volatile("rep movsb" : "+D"(to), "+S"(from), "+c"(bytes) : : "memory");
#else
    memcpy(to, from, bytes);
#endif
}

/*
 * Copy a rectangle of `buffer` to the same place on the surface, clipped
 * to both. The buffer side was checked against its mapping by nvos's ATTACH
 * validation: stride x height lies inside it.
 */
static void kms_copy(struct kms_card *card, const struct kms_buffer *buffer, NvS32 x, NvS32 y,
                     NvU32 w, NvU32 h)
{
    struct kms_head *lit = card->lit;
    NvU32 width = lit->mode.timings.hVisible;
    NvU32 height = lit->mode.timings.vVisible;
    NvU32 left, top, right, bottom, row;

    if (buffer->width < width)
        width = buffer->width;
    if (buffer->height < height)
        height = buffer->height;
    left = x < 0 ? 0 : (NvU32)x;
    top = y < 0 ? 0 : (NvU32)y;
    right = (NvU64)left + w > width ? width : left + w;
    bottom = (NvU64)top + h > height ? height : top + h;
    if (left >= right || top >= bottom)
        return;

    for (row = top; row < bottom; row++)
    {
        const NvU8 *from = buffer->pixels + (NvU64)row * buffer->stride + left * 4;
        NvU8 *to = (NvU8 *)lit->mapped + (NvU64)row * lit->pitch + left * 4;
        kms_copy_row(to, from, (size_t)(right - left) * 4);
    }
}

static void kms_serve(void *argument)
{
    struct kms_card *card = argument;
    struct nvos_display_event event;
    NvU32 i;

    for (;;)
    {
        if (nvos_display_next(&event) != NV_OK)
            break;
        switch (event.kind)
        {
        case NVOS_DISPLAY_ATTACH:
            for (i = 0; i < KMS_BUFFERS && card->buffers[i].id != 0; i++)
                ;
            if (i == KMS_BUFFERS || event.buffer == 0)
            {
                nvos_display_attached(event.buffer, NVOS_DISPLAY_INVALID);
                break;
            }
            card->buffers[i].id = event.buffer;
            card->buffers[i].pixels = (const NvU8 *)(NvUPtr)event.pixels;
            card->buffers[i].width = event.width;
            card->buffers[i].height = event.height;
            card->buffers[i].stride = event.stride;
            nvos_display_attached(event.buffer, NVOS_DISPLAY_OK);
            break;
        case NVOS_DISPLAY_SCANOUT:
            /* Shown from the next flush; buffer 0 leaves the last frame up. */
            card->shown = kms_buffer(card, event.buffer) != NULL ? event.buffer : 0;
            if (card->shown != 0)
                kms_copy(card, kms_buffer(card, card->shown), 0, 0, 0xffffffffu, 0xffffffffu);
            break;
        case NVOS_DISPLAY_FLUSH:
        {
            const struct kms_buffer *buffer = kms_buffer(card, event.buffer);
            NvU64 began = nvkms_get_usec();
            if (buffer != NULL)
            {
                card->shown = event.buffer;
                kms_copy(card, buffer, event.x, event.y, event.w, event.h);
            }
            card->copy_usec += nvkms_get_usec() - began;
            if (++card->flushes == 1)
                kms_say("the first frame is on the screen\n");
            /* What a frame's copy costs, every 300 flushes. */
            if (card->flushes % 300 == 0)
            {
                kms_say("300 flushes, a copy %llu us on average\n",
                        (unsigned long long)(card->copy_usec / 300));
                card->copy_usec = 0;
            }
            nvos_display_flipped(event.sequence, buffer != NULL && !card->mode_failed
                                                     ? NVOS_DISPLAY_OK
                                                     : NVOS_DISPLAY_INVALID);
            card->mode_failed = NV_FALSE;
            break;
        }
        case NVOS_DISPLAY_MODE:
        {
            /* One of the listed modes: find it, and set it unless it runs. */
            struct kms_head *lit = card->lit;
            const struct nvos_display_timing *t = &event.timing;
            NvU32 i;

            for (i = 0; i < lit->mode_count; i++)
            {
                const struct NvKmsKapiDisplayModeTimings *m = &lit->modes[i].timings;
                if (m->hVisible == t->hdisplay && m->vVisible == t->vdisplay &&
                    m->hTotal == t->htotal && m->vTotal == t->vtotal &&
                    m->hSyncStart == t->hsync_start && m->hSyncEnd == t->hsync_end &&
                    m->vSyncStart == t->vsync_start && m->vSyncEnd == t->vsync_end &&
                    m->pixelClockHz / 1000 == t->clock_khz)
                    break;
            }
            if (i == lit->mode_count)
            {
                kms_say("SET_MODE for %ux%u, which is not listed\n", t->hdisplay, t->vdisplay);
                card->mode_failed = NV_TRUE;
            }
            else if (!kms_same_mode(&lit->modes[i], &lit->mode) &&
                     kms_relight(lit, &lit->modes[i]) != 0)
            {
                card->mode_failed = NV_TRUE;
            }
            /* The old surface is gone: the next SCANOUT copies afresh. */
            break;
        }
        case NVOS_DISPLAY_DETACH:
        {
            struct kms_buffer *buffer = kms_buffer(card, event.buffer);
            if (card->shown == event.buffer)
                card->shown = 0;
            if (buffer != NULL)
                memset(buffer, 0, sizeof(*buffer));
            nvos_display_detached(event.buffer,
                                  buffer != NULL ? NVOS_DISPLAY_OK : NVOS_DISPLAY_INVALID);
            break;
        }
        case NVOS_DISPLAY_STOP:
            nvos_display_stopped();
            kms_say("the display core stopped the card\n");
            return;
        case NVOS_DISPLAY_CLOSED:
            kms_say("the display core closed the card\n");
            return;
        default:
            /* CURSOR and MOVE: never sent to a card without a cursor plane. */
            break;
        }
    }
}

/* Offer the lit head to the display core and serve it on a thread. */
static int kms_card_start(struct kms_head *lit)
{
    struct nvos_display_hello hello;
    const struct NvKmsKapiDisplayModeTimings *t = &lit->mode.timings;
    NV_STATUS status;

    memset(&hello, 0, sizeof(hello));
    hello.width = t->hVisible;
    hello.height = t->vVisible;
    hello.refresh_mhz = t->refreshRate;
    hello.connector = lit->connector;
    hello.edid_bytes = lit->edid_bytes;
    memcpy(hello.edid, lit->edid, lit->edid_bytes);
    /* Every mode NVKMS validated, the running one first (kms_list_modes). */
    for (NvU32 i = 0; i < lit->mode_count && i < NVOS_DISPLAY_MODES; i++)
    {
        const struct NvKmsKapiDisplayModeTimings *m = &lit->modes[i].timings;
        struct nvos_display_timing *into = &hello.timings[i];

        into->clock_khz = m->pixelClockHz / 1000;
        into->hdisplay = (NvU16)m->hVisible;
        into->hsync_start = (NvU16)m->hSyncStart;
        into->hsync_end = (NvU16)m->hSyncEnd;
        into->htotal = (NvU16)m->hTotal;
        into->vdisplay = (NvU16)m->vVisible;
        into->vsync_start = (NvU16)m->vSyncStart;
        into->vsync_end = (NvU16)m->vSyncEnd;
        into->vtotal = (NvU16)m->vTotal;
        into->flags = (m->flags.hSyncPos ? 1u : 0u) | (m->flags.vSyncPos ? 2u : 0u);
        hello.count = i + 1;
    }
    hello.preferred = lit->preferred;

    status = nvos_display_start(&hello);
    if (status != NV_OK)
    {
        kms_say("the display core did not take the card (0x%x)\n", status);
        return -EIO;
    }
    kms_card.lit = lit;
    if (!nvos_thread_spawn(kms_serve, &kms_card))
    {
        kms_say("no thread to serve the card\n");
        return -ENOMEM;
    }
    kms_say("serving the card: %ux%u, frames copied to head %u\n", hello.width, hello.height,
            lit->head);
    return 0;
}

/*
 * A GPU none of whose displays was connected at start: look again every
 * two seconds, for a monitor switched on or plugged in later -- a TV in
 * standby drops its hot-plug line -- and offer it as the card once one is
 * lit.
 */
static void kms_watch(void *argument)
{
    struct kms_head *lit = argument;
    struct NvKmsKapiDeviceResourcesInfo *res = calloc(1, sizeof(*res));
    struct NvKmsKapiDynamicDisplayParams *dyn = calloc(1, sizeof(*dyn));
    NvKmsKapiDisplay displays[NVKMS_KAPI_MAX_CONNECTORS * 4];
    NvU32 count = NV_ARRAY_ELEMENTS(displays);
    int status = -ENODEV;

    if (res == NULL || dyn == NULL || !kapi.getDeviceResourcesInfo(lit->device, res) ||
        !kapi.getDisplays(lit->device, &count, displays))
    {
        kms_say("cannot watch for a display\n");
        goto out;
    }
    kms_say("watching for a display to be connected\n");
    for (NvU32 looks = 1; status == -ENODEV; looks++)
    {
        nvkms_usleep(2000000);
        status = kms_scan(lit, res, dyn, displays, count, NV_FALSE);
        /* A line a minute, so a wait is told from a hang. */
        if (status == -ENODEV && looks % 30 == 0)
            kms_say("still no display connected after %u s\n", looks * 2);
    }
    if (status == 0 && kms_card.lit == NULL)
        (void)kms_card_start(lit);
out:
    free(res);
    free(dyn);
}

/*
 * For os/glue/drm.c, nvidia-drm's render node over the same KAPI device
 * (N3b): the table, and the first GPU's device, id and generic page kind;
 * NULL before NVKMS has a device.
 */
const struct NvKmsKapiFunctionsTable *nvrm_kms_kapi(void)
{
    return &kapi;
}

struct NvKmsKapiDevice *nvrm_kms_device(NvU32 *gpu_id, NvU32 *page_kind)
{
    *gpu_id = kms_drm_gpu_id;
    *page_kind = kms_drm_page_kind;
    return kms_lit[0].device;
}

int nvrm_kms_show(void)
{
    NvU32 i;
    int status = -ENODEV;

    if (!nvKmsKapiGetFunctionsTableInternal(&kapi))
    {
        kms_say("KAPI refused version %s\n", NV_VERSION_STRING);
        return -EINVAL;
    }
    kapi.enumerateGpus(kms_gpu_found);
    kms_say("%u GPU(s)\n", kms_gpu_count);

    for (i = 0; i < kms_gpu_count; i++)
    {
        int shown = kms_show_gpu(i);
        if (shown == 0 || status != 0)
            status = shown;
        /* The first lit head becomes the card; one is all the core needs. */
        if (shown == 0 && kms_card.lit == NULL)
            status = kms_card_start(&kms_lit[i]);
        else if (shown == -ENODEV && kms_lit[i].device != NULL && i == 0)
            (void)nvos_thread_spawn(kms_watch, &kms_lit[i]);
    }
    return status;
}
