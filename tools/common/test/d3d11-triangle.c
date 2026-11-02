/*
 * d3d11-triangle: the Direct3D 11 program `cargo xtask test-dxvk` runs in
 * Wine with DXVK on Mesa's lavapipe (stage 23, E2.4; docs/WINE.md §6).
 *
 *   d3d11-triangle.exe RRGGBB            offscreen
 *   d3d11-triangle.exe RRGGBB SECONDS    presented in a window
 *
 * It makes a device on the first adapter and renders frames 64 by 64, each
 * a clear to blue and a triangle in the colour given on the command line.
 * Offscreen, it renders 30 into a texture, copies the last to a staging
 * texture, reads it back, and prints one line the gate judges:
 *
 *   d3d11-triangle: frames=30 clear=3584 triangle=512 other=0
 *       centre=RRGGBBAA corner=RRGGBBAA fnv=XXXXXXXX
 *
 * (one line; fnv is FNV-1a over the frame's RGBA bytes, row by row). The
 * gate draws the same frame itself and compares. The colour comes from the
 * command line so that the gate can ask for one the program does not know
 * in advance, and a frame of the wrong colour fails it.
 *
 * Presented, it makes a window and a flip-model swap chain of two 64 by 64
 * buffers on it, and draws into the back buffer and presents for SECONDS,
 * DXVK scaling the buffer to the window. The first frame is read back
 * before it is presented and described as above with frames=1, and a line
 * every second counts the presents: Vulkan's presentation through Wine's X11
 * surface, which a game's frames take.
 *
 * Presented, it also behaves as a windowed game does when its window is
 * resized (maximized, say): each WM_SIZE is printed as
 *
 *   d3d11-triangle: client WIDTHxHEIGHT
 *
 * and, after the first frame, once the size differs from its buffers', the
 * swap chain's buffers are made that size with ResizeBuffers
 * (`d3d11-triangle: buffers WxH`). At any client size but the one the
 * window was shown at, its frames are cleared to green rather than blue,
 * so that a screen still showing blue after a resize shows a frame from
 * before it. Keys and left
 * button presses that reach the window are printed as
 *
 *   d3d11-triangle: key VK   /   d3d11-triangle: button X,Y
 *
 * (VK in hex, X and Y in the client area).
 *
 * Shaders are compiled at run time by Wine's own d3dcompiler_47, from the
 * HLSL below.
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

/* What a frame is drawn with. */
struct scene {
    ID3D11DeviceContext *context;
    ID3D11InputLayout *layout;
    ID3D11Buffer *vertices, *constants;
    ID3D11VertexShader *vs;
    ID3D11PixelShader *ps;
    UINT8 red, green, blue;
    /* The target's size, and whether the frames are the resized ones. */
    UINT width, height;
    BOOL resized;
};

/* One frame into `view`: a clear to blue (green once resized) and the
 * triangle. */
static void draw(const struct scene *scene, ID3D11RenderTargetView *view) {
    static const float blue[4] = {0.0f, 0.0f, 1.0f, 1.0f}, green[4] = {0.0f, 1.0f, 0.0f, 1.0f};
    const float *clear = scene->resized ? green : blue;
    D3D11_VIEWPORT viewport = {0, 0, (FLOAT)scene->width, (FLOAT)scene->height, 0, 1};
    UINT stride = 2 * sizeof(float), offset = 0;
    ID3D11DeviceContext *context = scene->context;
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, NULL);
    ID3D11DeviceContext_RSSetViewports(context, 1, &viewport);
    ID3D11DeviceContext_ClearRenderTargetView(context, view, clear);
    ID3D11DeviceContext_IASetInputLayout(context, scene->layout);
    ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ID3D11DeviceContext_IASetVertexBuffers(context, 0, 1, &scene->vertices, &stride, &offset);
    ID3D11DeviceContext_VSSetShader(context, scene->vs, NULL, 0);
    ID3D11DeviceContext_PSSetShader(context, scene->ps, NULL, 0);
    ID3D11DeviceContext_PSSetConstantBuffers(context, 0, 1, &scene->constants);
    ID3D11DeviceContext_Draw(context, 3, 0);
    ID3D11DeviceContext_Flush(context);
}

/* Copy `source` to `staging`, read it back, and print the line the gate
 * judges. */
static int describe(const struct scene *scene, ID3D11Texture2D *source,
                    ID3D11Texture2D *staging, int frames) {
    ID3D11DeviceContext *context = scene->context;
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging,
                                     (ID3D11Resource *)source);
    D3D11_MAPPED_SUBRESOURCE mapped;
    HRESULT hr = ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ,
                                         0, &mapped);
    if (FAILED(hr))
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
            else if (p[0] == scene->red && p[1] == scene->green && p[2] == scene->blue &&
                     p[3] == 0xff)
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
           frames, clear_pixels, triangle_pixels, other, centre, corner, hash);
    return 0;
}

/* The client size the last WM_SIZE gave; 0 before one came. */
static UINT client_width, client_height;

static LRESULT CALLBACK window_procedure(HWND window, UINT message, WPARAM w, LPARAM l) {
    switch (message) {
    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;
    case WM_SIZE:
        client_width = LOWORD(l);
        client_height = HIWORD(l);
        printf("d3d11-triangle: client %ux%u\n", client_width, client_height);
        return 0;
    case WM_KEYDOWN:
        printf("d3d11-triangle: key %02x\n", (unsigned)w);
        return 0;
    case WM_LBUTTONDOWN:
        printf("d3d11-triangle: button %d,%d\n", (short)LOWORD(l), (short)HIWORD(l));
        return 0;
    }
    return DefWindowProcW(window, message, w, l);
}

/* The frames presented in a window for `seconds` (see the top). */
static int present(ID3D11Device *device, IDXGIAdapter *adapter, struct scene *scene,
                   ID3D11Texture2D *staging, int seconds) {
    WNDCLASSW class = {.lpfnWndProc = window_procedure, .hInstance = GetModuleHandleW(NULL),
                       .lpszClassName = L"d3d11-triangle"};
    if (!RegisterClassW(&class))
        return fail("RegisterClassW", HRESULT_FROM_WIN32(GetLastError()));
    HWND window = CreateWindowW(L"d3d11-triangle", L"d3d11-triangle", WS_OVERLAPPEDWINDOW, 0, 0,
                                512, 512, NULL, NULL, class.hInstance, NULL);
    if (!window)
        return fail("CreateWindowW", HRESULT_FROM_WIN32(GetLastError()));
    ShowWindow(window, SW_SHOW);

    IDXGIFactory *factory = NULL;
    HRESULT hr = IDXGIAdapter_GetParent(adapter, &IID_IDXGIFactory, (void **)&factory);
    if (FAILED(hr))
        return fail("GetParent (factory)", hr);
    DXGI_SWAP_CHAIN_DESC description = {
        .BufferDesc = {.Width = SIZE, .Height = SIZE, .Format = DXGI_FORMAT_R8G8B8A8_UNORM},
        .SampleDesc = {1, 0},
        .BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT,
        .BufferCount = 2,
        .OutputWindow = window,
        .Windowed = TRUE,
        .SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD,
    };
    IDXGISwapChain *chain = NULL;
    if (FAILED(hr = IDXGIFactory_CreateSwapChain(factory, (IUnknown *)device, &description,
                                                 &chain)))
        return fail("CreateSwapChain", hr);
    ID3D11Texture2D *back = NULL;
    if (FAILED(hr = IDXGISwapChain_GetBuffer(chain, 0, &IID_ID3D11Texture2D, (void **)&back)))
        return fail("GetBuffer", hr);
    ID3D11RenderTargetView *view = NULL;
    if (FAILED(hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)back, NULL,
                                                        &view)))
        return fail("CreateRenderTargetView (back buffer)", hr);
    printf("d3d11-triangle: presenting for %d s\n", seconds);

    ULONGLONG start = GetTickCount64(), end = start + 1000ull * seconds, next = start + 1000;
    int presents = 0;
    /* The client size the window was shown at: frames are green only at
     * another, one the window was given later. */
    UINT first_width = 0, first_height = 0;
    while (GetTickCount64() < end) {
        MSG message;
        while (PeekMessageW(&message, NULL, 0, 0, PM_REMOVE)) {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        /* A windowed game's answer to a new client size: buffers of it. */
        if (presents > 0 && client_width && client_height &&
            (client_width != scene->width || client_height != scene->height)) {
            ID3D11RenderTargetView_Release(view);
            ID3D11Texture2D_Release(back);
            if (FAILED(hr = IDXGISwapChain_ResizeBuffers(chain, 0, client_width, client_height,
                                                         DXGI_FORMAT_UNKNOWN, 0)))
                return fail("ResizeBuffers", hr);
            if (FAILED(hr = IDXGISwapChain_GetBuffer(chain, 0, &IID_ID3D11Texture2D,
                                                     (void **)&back)))
                return fail("GetBuffer (resized)", hr);
            if (FAILED(hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)back,
                                                                NULL, &view)))
                return fail("CreateRenderTargetView (resized)", hr);
            scene->width = client_width;
            scene->height = client_height;
            if (!first_width) {
                first_width = client_width;
                first_height = client_height;
            }
            scene->resized = client_width != first_width || client_height != first_height;
            printf("d3d11-triangle: buffers %ux%u\n", scene->width, scene->height);
        }
        draw(scene, view);
        if (presents == 0) {
            int status = describe(scene, back, staging, 1);
            if (status)
                return status;
            first_width = client_width;
            first_height = client_height;
        }
        if (FAILED(hr = IDXGISwapChain_Present(chain, 1, 0)))
            return fail("Present", hr);
        presents++;
        if (GetTickCount64() >= next) {
            printf("d3d11-triangle: presented=%d in %llu ms\n", presents,
                   GetTickCount64() - start);
            next += 1000;
        }
    }
    printf("d3d11-triangle: presented=%d in %llu ms, done\n", presents, GetTickCount64() - start);
    return 0;
}

int main(int argc, char **argv) {
    unsigned long rgb = argc > 1 ? strtoul(argv[1], NULL, 16) : 0xffa000;
    int seconds = argc > 2 ? atoi(argv[2]) : 0;
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
    if (FAILED(hr = ID3D11Device_QueryInterface(device, &IID_IDXGIDevice, (void **)&dxgi)) ||
        FAILED(hr = IDXGIDevice_GetAdapter(dxgi, &adapter)))
        return fail("GetAdapter", hr);
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
    struct scene scene = {
        .context = context,
        .red = (rgb >> 16) & 0xff,
        .green = (rgb >> 8) & 0xff,
        .blue = rgb & 0xff,
        .width = SIZE,
        .height = SIZE,
    };
    if (FAILED(hr = ID3D11Device_CreateVertexShader(device, ID3D10Blob_GetBufferPointer(vs_code),
                                                    ID3D10Blob_GetBufferSize(vs_code), NULL,
                                                    &scene.vs)))
        return fail("CreateVertexShader", hr);
    if (FAILED(hr = ID3D11Device_CreatePixelShader(device, ID3D10Blob_GetBufferPointer(ps_code),
                                                   ID3D10Blob_GetBufferSize(ps_code), NULL,
                                                   &scene.ps)))
        return fail("CreatePixelShader", hr);
    D3D11_INPUT_ELEMENT_DESC element = {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0,
                                        D3D11_INPUT_PER_VERTEX_DATA, 0};
    if (FAILED(hr = ID3D11Device_CreateInputLayout(device, &element, 1,
                                                   ID3D10Blob_GetBufferPointer(vs_code),
                                                   ID3D10Blob_GetBufferSize(vs_code),
                                                   &scene.layout)))
        return fail("CreateInputLayout", hr);

    /* A triangle whose corners fall on pixel corners: (16,48), (32,16) and
     * (48,48) in the 64 by 64 target, 512 pixels. */
    static const float vertices[] = {-0.5f, -0.5f, 0.0f, 0.5f, 0.5f, -0.5f};
    D3D11_BUFFER_DESC buffer = {.ByteWidth = sizeof vertices, .Usage = D3D11_USAGE_IMMUTABLE,
                                .BindFlags = D3D11_BIND_VERTEX_BUFFER};
    D3D11_SUBRESOURCE_DATA data = {.pSysMem = vertices};
    if (FAILED(hr = ID3D11Device_CreateBuffer(device, &buffer, &data, &scene.vertices)))
        return fail("CreateBuffer (vertices)", hr);
    float tint[4] = {scene.red / 255.0f, scene.green / 255.0f, scene.blue / 255.0f, 1.0f};
    buffer.ByteWidth = sizeof tint;
    buffer.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    data.pSysMem = tint;
    if (FAILED(hr = ID3D11Device_CreateBuffer(device, &buffer, &data, &scene.constants)))
        return fail("CreateBuffer (constants)", hr);

    if (seconds > 0)
        return present(device, adapter, &scene, staging, seconds);
    for (int frame = 0; frame < FRAMES; frame++)
        draw(&scene, view);
    return describe(&scene, target, staging, FRAMES);
}
