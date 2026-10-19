/*
 * d3d11-shaders: many distinct pixel shaders through DXVK on lavapipe, for
 * the shader caches' gate, `cargo xtask test-shader-cache` (docs/WINE.md
 * §6a).
 *
 *   d3d11-shaders.exe COUNT
 *
 * It compiles COUNT pixel shaders that differ in a constant (Wine's own
 * d3dcompiler_47), then creates each and draws one triangle with it into a
 * 64 by 64 target, so DXVK builds a pipeline for each and lavapipe compiles
 * each with LLVM. A read-back at the end waits for the draws. It prints
 *
 *   d3d11-shaders: shaders=COUNT hlsl_ms=A draw_ms=B centre=RRGGBBAA
 *
 * draw_ms covers creating the shaders and drawing until the read-back: the
 * part a warm shader cache shortens. Everything is released before it
 * returns, so that DXVK and Mesa close their caches.
 *
 * Built by xtask with mingw-w64:
 *   x86_64-w64-mingw32-gcc -O2 -Wall -Wextra -Werror -s -o d3d11-shaders.exe \
 *       d3d11-shaders.c -ld3d11 -ldxgi -ld3dcompiler_47 -luuid
 */

#define COBJMACROS
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define SIZE 64
#define MAX_SHADERS 4096

static const char vs_source[] =
    "float4 vs(float2 position : POSITION) : SV_Position {\n"
    "    return float4(position, 0.0, 1.0);\n"
    "}\n";

/* %d is the shader's number: each one is its own program. */
static const char ps_format[] =
    "float4 ps(float4 position : SV_Position) : SV_Target {\n"
    "    float k = %d.0;\n"
    "    float2 p = position.xy * (0.01 + k * 0.0001);\n"
    "    float a = 0.0;\n"
    "    [unroll] for (int i = 0; i < 12; i++) {\n"
    "        a += sin(p.x * (i + 1) + k) * cos(p.y * (i + 2) - k);\n"
    "        p = float2(p.y + a * 0.5, p.x - a * 0.25);\n"
    "    }\n"
    "    return float4(frac(a), frac(a * 2.0), frac(k / 7.0), 1.0);\n"
    "}\n";

static double now_ms(void) {
    LARGE_INTEGER count, frequency;
    QueryPerformanceCounter(&count);
    QueryPerformanceFrequency(&frequency);
    return (double)count.QuadPart * 1000.0 / (double)frequency.QuadPart;
}

static int fail(const char *what, HRESULT hr) {
    printf("d3d11-shaders: %s failed: 0x%08lx\n", what, (unsigned long)hr);
    return 1;
}

static ID3DBlob *compile(const char *source, const char *entry, const char *target) {
    ID3DBlob *code = NULL, *errors = NULL;
    HRESULT hr = D3DCompile(source, strlen(source), "shaders.hlsl", NULL, NULL, entry, target, 0,
                            0, &code, &errors);
    if (FAILED(hr)) {
        printf("d3d11-shaders: D3DCompile(%s) failed: 0x%08lx %s\n", entry, (unsigned long)hr,
               errors ? (const char *)ID3D10Blob_GetBufferPointer(errors) : "");
        return NULL;
    }
    return code;
}

int main(int argc, char **argv) {
    int count = argc > 1 ? atoi(argv[1]) : 64;
    if (count < 1 || count > MAX_SHADERS)
        count = 64;
    setvbuf(stdout, NULL, _IONBF, 0);

    static ID3DBlob *ps_code[MAX_SHADERS];
    static ID3D11PixelShader *ps[MAX_SHADERS];
    double start = now_ms();
    ID3DBlob *vs_code = compile(vs_source, "vs", "vs_4_0");
    if (!vs_code)
        return 1;
    char source[2048];
    for (int i = 0; i < count; i++) {
        snprintf(source, sizeof source, ps_format, i + 1);
        if (!(ps_code[i] = compile(source, "ps", "ps_4_0")))
            return 1;
    }
    double hlsl = now_ms() - start;

    static const D3D_FEATURE_LEVEL levels[] = {D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0};
    ID3D11Device *device = NULL;
    ID3D11DeviceContext *context = NULL;
    D3D_FEATURE_LEVEL level = 0;
    HRESULT hr = D3D11CreateDevice(NULL, D3D_DRIVER_TYPE_HARDWARE, NULL, 0, levels, 2,
                                   D3D11_SDK_VERSION, &device, &level, &context);
    if (FAILED(hr))
        return fail("D3D11CreateDevice", hr);

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
    ID3D11VertexShader *vs = NULL;
    if (FAILED(hr = ID3D11Device_CreateVertexShader(device, ID3D10Blob_GetBufferPointer(vs_code),
                                                    ID3D10Blob_GetBufferSize(vs_code), NULL,
                                                    &vs)))
        return fail("CreateVertexShader", hr);
    D3D11_INPUT_ELEMENT_DESC element = {"POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0,
                                        D3D11_INPUT_PER_VERTEX_DATA, 0};
    ID3D11InputLayout *layout = NULL;
    if (FAILED(hr = ID3D11Device_CreateInputLayout(device, &element, 1,
                                                   ID3D10Blob_GetBufferPointer(vs_code),
                                                   ID3D10Blob_GetBufferSize(vs_code), &layout)))
        return fail("CreateInputLayout", hr);
    static const float vertices[] = {-1.0f, -1.0f, -1.0f, 3.0f, 3.0f, -1.0f};
    D3D11_BUFFER_DESC buffer = {.ByteWidth = sizeof vertices, .Usage = D3D11_USAGE_IMMUTABLE,
                                .BindFlags = D3D11_BIND_VERTEX_BUFFER};
    D3D11_SUBRESOURCE_DATA data = {.pSysMem = vertices};
    ID3D11Buffer *vertex_buffer = NULL;
    if (FAILED(hr = ID3D11Device_CreateBuffer(device, &buffer, &data, &vertex_buffer)))
        return fail("CreateBuffer", hr);

    start = now_ms();
    D3D11_VIEWPORT viewport = {0, 0, SIZE, SIZE, 0, 1};
    UINT stride = 2 * sizeof(float), offset = 0;
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, NULL);
    ID3D11DeviceContext_RSSetViewports(context, 1, &viewport);
    ID3D11DeviceContext_IASetInputLayout(context, layout);
    ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ID3D11DeviceContext_IASetVertexBuffers(context, 0, 1, &vertex_buffer, &stride, &offset);
    ID3D11DeviceContext_VSSetShader(context, vs, NULL, 0);
    for (int i = 0; i < count; i++) {
        if (FAILED(hr = ID3D11Device_CreatePixelShader(device,
                                                       ID3D10Blob_GetBufferPointer(ps_code[i]),
                                                       ID3D10Blob_GetBufferSize(ps_code[i]), NULL,
                                                       &ps[i])))
            return fail("CreatePixelShader", hr);
        ID3D11DeviceContext_PSSetShader(context, ps[i], NULL, 0);
        ID3D11DeviceContext_Draw(context, 3, 0);
        ID3D11DeviceContext_Flush(context);
    }
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);
    D3D11_MAPPED_SUBRESOURCE mapped;
    if (FAILED(hr = ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ,
                                            0, &mapped)))
        return fail("Map", hr);
    const UINT8 *p = (const UINT8 *)mapped.pData + (size_t)(SIZE / 2) * mapped.RowPitch +
                     4 * (SIZE / 2);
    UINT32 centre = (UINT32)p[0] << 24 | (UINT32)p[1] << 16 | (UINT32)p[2] << 8 | p[3];
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    double draw = now_ms() - start;
    printf("d3d11-shaders: shaders=%d hlsl_ms=%.0f draw_ms=%.0f centre=%08x\n", count, hlsl, draw,
           centre);

    ID3D11DeviceContext_ClearState(context);
    ID3D11DeviceContext_Flush(context);
    for (int i = 0; i < count; i++) {
        ID3D11PixelShader_Release(ps[i]);
        ID3D10Blob_Release(ps_code[i]);
    }
    ID3D11Buffer_Release(vertex_buffer);
    ID3D11InputLayout_Release(layout);
    ID3D11VertexShader_Release(vs);
    ID3D10Blob_Release(vs_code);
    ID3D11RenderTargetView_Release(view);
    ID3D11Texture2D_Release(staging);
    ID3D11Texture2D_Release(target);
    ID3D11DeviceContext_Release(context);
    ID3D11Device_Release(device);
    printf("d3d11-shaders: released\n");
    return 0;
}
