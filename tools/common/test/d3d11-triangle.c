/*
 * d3d11-triangle: the Direct3D 11 program `cargo xtask test-dxvk` runs in
 * Wine with DXVK on Mesa's lavapipe (stage 23, E2.4; docs/WINE.md §6).
 *
 * It makes a device on the first adapter, renders frames into a 64 by 64
 * RGBA texture -- each a clear to blue and a triangle drawn in the colour
 * given on the command line -- copies the last frame to a staging texture,
 * reads it back, and prints one line the gate judges:
 *
 *   d3d11-triangle: frames=30 clear=3584 triangle=512 other=0
 *       centre=RRGGBBAA corner=RRGGBBAA fnv=XXXXXXXX
 *
 * (one line; fnv is FNV-1a over the frame's RGBA bytes, row by row). The
 * gate draws the same frame itself and compares. The triangle's colour
 * comes from the command line, not from here, so the gate can ask for a
 * colour the program does not know in advance and a frame of the wrong
 * colour fails it.
 *
 * Shaders are compiled at run time by Wine's own d3dcompiler_47, from the
 * HLSL below. No window and no swap chain: nothing is presented, so this
 * holds DXVK, lavapipe and the threads under them, not Vulkan's WSI.
 *
 * Built by tools/common/fetch/fetch-dxvk.sh with mingw-w64:
 *   x86_64-w64-mingw32-gcc -O2 -o d3d11-triangle.exe d3d11-triangle.c \
 *       -ld3d11 -ldxgi -ld3dcompiler_47 -luuid
 */

#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <stdio.h>
#include <stdlib.h>

#define SIZE 64
#define FRAMES 30

static const char shaders[] =
    "cbuffer colour : register(b0) { float4 tint; };\n"
    "float4 vs(float2 position : POSITION) : SV_Position {\n"
    "    return float4(position, 0.0, 1.0);\n"
    "}\n"
    "float4 ps(float4 position : SV_Position) : SV_Target {\n"
    "    return tint;\n"
    "}\n";

static int fail(const char *what, HRESULT hr) {
    printf("d3d11-triangle: %s failed: 0x%08lx\n", what, (unsigned long)hr);
    fflush(stdout);
    return 1;
}

static ID3DBlob *compile(const char *entry, const char *target) {
    ID3DBlob *code = NULL, *errors = NULL;
    HRESULT hr = D3DCompile(shaders, sizeof shaders - 1, "triangle.hlsl", NULL, NULL, entry,
                            target, 0, 0, &code, &errors);
    if (FAILED(hr)) {
        printf("d3d11-triangle: D3DCompile(%s) failed: 0x%08lx %s\n", entry, (unsigned long)hr,
               errors ? (const char *)ID3D10Blob_GetBufferPointer(errors) : "");
        return NULL;
    }
    return code;
}

int main(int argc, char **argv) {
    unsigned long rgb = argc > 1 ? strtoul(argv[1], NULL, 16) : 0xffa000;
    UINT8 red = (rgb >> 16) & 0xff, green = (rgb >> 8) & 0xff, blue = rgb & 0xff;
    setvbuf(stdout, NULL, _IONBF, 0);

    static const D3D_FEATURE_LEVEL levels[] = {
        D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    };
    ID3D11Device *device = NULL;
    ID3D11DeviceContext *context = NULL;
    D3D_FEATURE_LEVEL level = 0;
    HRESULT hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, levels,
                                   sizeof levels / sizeof levels[0], D3D11_SDK_VERSION, &device,
                                   &level, &context);
    if (FAILED(hr))
        return fail("D3D11CreateDevice", hr);

    IDXGIDevice *dxgi = NULL;
    IDXGIAdapter *adapter = NULL;
    DXGI_ADAPTER_DESC description = {0};
    if (SUCCEEDED(ID3D11Device_QueryInterface(device, &IID_IDXGIDevice, (void **)&dxgi)) &&
        SUCCEEDED(IDXGIDevice_GetAdapter(dxgi, &adapter)))
        IDXGIAdapter_GetDesc(adapter, &description);
    printf("d3d11-triangle: device on \"%ls\" (vendor %04x), feature level %d.%d\n",
           description.Description, description.VendorId, level >> 12, (level >> 8) & 0xf);

    D3D11_TEXTURE2D_DESC texture = {
        .Width = SIZE, .Height = SIZE, .MipLevels = 1, .ArraySize = 1,
        .Format = DXGI_FORMAT_R8G8B8A8_UNORM, .SampleDesc = {1, 0},
        .Usage = D3D11_USAGE_DEFAULT, .BindFlags = D3D11_BIND_RENDER_TARGET,
    };
    ID3D11Texture2D *target = NULL, *staging = NULL;
    if (FAILED(hr = ID3D11Device_CreateTexture2D(device, &texture, NULL, &target)))
        return fail("CreateTexture2D (target)", hr);
    texture.Usage = D3D11_USAGE_STAGING;
    texture.BindFlags = 0;
    texture.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    if (FAILED(hr = ID3D11Device_CreateTexture2D(device, &texture, NULL, &staging)))
        return fail("CreateTexture2D (staging)", hr);
    ID3D11RenderTargetView *view = NULL;
    if (FAILED(hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)target, NULL,
                                                        &view)))
        return fail("CreateRenderTargetView", hr);

    ID3DBlob *vs_code = compile("vs", "vs_4_0"), *ps_code = compile("ps", "ps_4_0");
    if (!vs_code || !ps_code)
        return 1;
    ID3D11VertexShader *vs = NULL;
    ID3D11PixelShader *ps = NULL;
    if (FAILED(hr = ID3D11Device_CreateVertexShader(device, ID3D10Blob_GetBufferPointer(vs_code),
                                                    ID3D10Blob_GetBufferSize(vs_code), NULL, &vs)))
        return fail("CreateVertexShader", hr);
    if (FAILED(hr = ID3D11Device_CreatePixelShader(device, ID3D10Blob_GetBufferPointer(ps_code),
                                                   ID3D10Blob_GetBufferSize(ps_code), NULL, &ps)))
        return fail("CreatePixelShader", hr);
    D3D11_INPUT_ELEMENT_DESC element = {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0,
                                        D3D11_INPUT_PER_VERTEX_DATA, 0};
    ID3D11InputLayout *layout = NULL;
    if (FAILED(hr = ID3D11Device_CreateInputLayout(device, &element, 1,
                                                   ID3D10Blob_GetBufferPointer(vs_code),
                                                   ID3D10Blob_GetBufferSize(vs_code), &layout)))
        return fail("CreateInputLayout", hr);

    /* A triangle whose corners fall on pixel corners: (16,48), (32,16) and
     * (48,48) in the 64 by 64 target, 512 pixels by the top-left rule. */
    static const float vertices[] = {-0.5f, -0.5f, 0.0f, 0.5f, 0.5f, -0.5f};
    D3D11_BUFFER_DESC buffer = {.ByteWidth = sizeof vertices, .Usage = D3D11_USAGE_IMMUTABLE,
                                .BindFlags = D3D11_BIND_VERTEX_BUFFER};
    D3D11_SUBRESOURCE_DATA data = {.pSysMem = vertices};
    ID3D11Buffer *vertex_buffer = NULL, *constants = NULL;
    if (FAILED(hr = ID3D11Device_CreateBuffer(device, &buffer, &data, &vertex_buffer)))
        return fail("CreateBuffer (vertices)", hr);
    float tint[4] = {red / 255.0f, green / 255.0f, blue / 255.0f, 1.0f};
    buffer.ByteWidth = sizeof tint;
    buffer.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    data.pSysMem = tint;
    if (FAILED(hr = ID3D11Device_CreateBuffer(device, &buffer, &data, &constants)))
        return fail("CreateBuffer (constants)", hr);

    D3D11_VIEWPORT viewport = {0, 0, SIZE, SIZE, 0, 1};
    UINT stride = 2 * sizeof(float), offset = 0;
    static const float clear[4] = {0.0f, 0.0f, 1.0f, 1.0f};
    for (int frame = 0; frame < FRAMES; frame++) {
        ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, NULL);
        ID3D11DeviceContext_RSSetViewports(context, 1, &viewport);
        ID3D11DeviceContext_ClearRenderTargetView(context, view, clear);
        ID3D11DeviceContext_IASetInputLayout(context, layout);
        ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ID3D11DeviceContext_IASetVertexBuffers(context, 0, 1, &vertex_buffer, &stride, &offset);
        ID3D11DeviceContext_VSSetShader(context, vs, NULL, 0);
        ID3D11DeviceContext_PSSetShader(context, ps, NULL, 0);
        ID3D11DeviceContext_PSSetConstantBuffers(context, 0, 1, &constants);
        ID3D11DeviceContext_Draw(context, 3, 0);
        ID3D11DeviceContext_Flush(context);
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging,
                                     (ID3D11Resource *)target);
    D3D11_MAPPED_SUBRESOURCE mapped;
    if (FAILED(hr = ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ,
                                            0, &mapped)))
        return fail("Map", hr);

    unsigned clear_pixels = 0, triangle_pixels = 0, other = 0;
    UINT32 hash = 2166136261u;
    UINT32 centre = 0, corner = 0;
    for (int y = 0; y < SIZE; y++) {
        const UINT8 *row = (const UINT8 *)mapped.pData + (size_t)y * mapped.RowPitch;
        for (int x = 0; x < SIZE; x++) {
            const UINT8 *p = row + 4 * x;
            UINT32 rgba = (UINT32)p[0] << 24 | (UINT32)p[1] << 16 | (UINT32)p[2] << 8 | p[3];
            for (int i = 0; i < 4; i++)
                hash = (hash ^ p[i]) * 16777619u;
            if (rgba == 0x0000ffffu)
                clear_pixels++;
            else if (p[0] == red && p[1] == green && p[2] == blue && p[3] == 0xff)
                triangle_pixels++;
            else
                other++;
            if (x == SIZE / 2 && y == SIZE / 2)
                centre = rgba;
            if (x == 1 && y == 1)
                corner = rgba;
        }
    }
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    printf("d3d11-triangle: frames=%d clear=%u triangle=%u other=%u centre=%08x corner=%08x "
           "fnv=%08x\n",
           FRAMES, clear_pixels, triangle_pixels, other, centre, corner, hash);
    return 0;
}
