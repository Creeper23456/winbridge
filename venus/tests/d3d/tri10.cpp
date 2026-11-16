// SPDX-License-Identifier: GPL-3.0-or-later
// D3D10 triangle: window, swapchain, HLSL compiled at run time with D3DCompile (shader model 4.0).
// With DXVK's d3d10core.dll and dxgi.dll next to the exe, the frames go through DXVK and the Venus
// driver. Usage: tri10 [--frames N] [--hold-ms N] [--dump PATH.ppm]
// (--frames 0 runs until the window is closed; the window title shows the frame number and rate)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d10.h>
#include <d3dcompiler.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "hud.h"

static const char *kShader =
    "struct V { float4 p : SV_Position; float4 c : COLOR; };\n"
    "V vs(uint id : SV_VertexID) {\n"
    "  float2 pos[3] = { float2(0.0, 0.6), float2(0.6, -0.6), float2(-0.6, -0.6) };\n"
    "  float4 col[3] = { float4(1,0.4,0.1,1), float4(0.2,0.9,0.3,1), float4(0.2,0.3,1,1) };\n"
    "  V o; o.p = float4(pos[id], 0, 1); o.c = col[id]; return o; }\n"
    "float4 ps(V i) : SV_Target { return i.c; }\n";

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) PostQuitMessage(0);
    return DefWindowProcW(h, m, w, l);
}

#define CHECK(x) do { HRESULT hr_ = (x); if (FAILED(hr_)) { printf("RESULT: FAIL: %s = 0x%08lx\n", #x, (unsigned long)hr_); return 1; } } while (0)

int main(int argc, char **argv) {
    int frames = 120, hold = 0; const char *dump = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--hold-ms") && i + 1 < argc) hold = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dump") && i + 1 < argc) dump = argv[++i];
    }
    const int W = 640, H = 360;
    WNDCLASSW wc = {0}; wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"tri10"; wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    RECT r = {0, 0, W, H}; AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, L"tri10", L"WinBridge D3D10 triangle", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                                40, 40, r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);

    DXGI_SWAP_CHAIN_DESC sd = {};
    sd.BufferCount = 2; sd.BufferDesc.Width = W; sd.BufferDesc.Height = H;
    sd.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM; sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow = hwnd; sd.SampleDesc.Count = 1; sd.Windowed = TRUE; sd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
    ID3D10Device *dev = 0; IDXGISwapChain *sc = 0;
    CHECK(D3D10CreateDeviceAndSwapChain(0, D3D10_DRIVER_TYPE_HARDWARE, 0, 0, D3D10_SDK_VERSION, &sd, &sc, &dev));
    IDXGIDevice *dd = 0; IDXGIAdapter *ad = 0; DXGI_ADAPTER_DESC adesc = {};
    if (SUCCEEDED(dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dd)) && SUCCEEDED(dd->GetAdapter(&ad)) && SUCCEEDED(ad->GetDesc(&adesc)))
        printf("adapter: %ls\n", adesc.Description);
    Hud hud; hud.init(hwnd, L"tri10", adesc.Description, W, H, frames);

    ID3DBlob *vsb = 0, *psb = 0, *err = 0;
    if (FAILED(D3DCompile(kShader, strlen(kShader), 0, 0, 0, "vs", "vs_4_0", 0, 0, &vsb, &err))) { printf("RESULT: FAIL: vs compile: %s\n", err ? (char *)err->GetBufferPointer() : "?"); return 1; }
    if (FAILED(D3DCompile(kShader, strlen(kShader), 0, 0, 0, "ps", "ps_4_0", 0, 0, &psb, &err))) { printf("RESULT: FAIL: ps compile: %s\n", err ? (char *)err->GetBufferPointer() : "?"); return 1; }
    ID3D10VertexShader *vs = 0; ID3D10PixelShader *ps = 0;
    CHECK(dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), &vs));
    CHECK(dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), &ps));
    ID3D10Texture2D *bb = 0; CHECK(sc->GetBuffer(0, __uuidof(ID3D10Texture2D), (void **)&bb));
    ID3D10RenderTargetView *rtv = 0; CHECK(dev->CreateRenderTargetView(bb, 0, &rtv));

    ID3D10Texture2D *stage = 0;
    if (dump) {
        D3D10_TEXTURE2D_DESC td; bb->GetDesc(&td); td.Usage = D3D10_USAGE_STAGING; td.BindFlags = 0;
        td.CPUAccessFlags = D3D10_CPU_ACCESS_READ; td.MiscFlags = 0;
        CHECK(dev->CreateTexture2D(&td, 0, &stage));
    }

    D3D10_VIEWPORT vp = {0, 0, (UINT)W, (UINT)H, 0.0f, 1.0f};
    LARGE_INTEGER f, t0, t1; QueryPerformanceFrequency(&f);
    double worst = 0, sum = 0;
    for (int i = 0; frames <= 0 || i < frames; i++) {
        if (!hud.pump()) break;
        QueryPerformanceCounter(&t0);
        float clear[4] = {0.05f, 0.10f, 0.20f, 1.0f};
        dev->OMSetRenderTargets(1, &rtv, 0); dev->RSSetViewports(1, &vp);
        dev->ClearRenderTargetView(rtv, clear);
        dev->IASetPrimitiveTopology(D3D10_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        dev->VSSetShader(vs); dev->PSSetShader(ps);
        dev->Draw(3, 0);
        if (dump && i == frames - 1) {
            dev->CopyResource(stage, bb);
            D3D10_MAPPED_TEXTURE2D ms;
            CHECK(stage->Map(0, D3D10_MAP_READ, 0, &ms));
            FILE *fp = fopen(dump, "wb");
            if (!fp) { printf("RESULT: FAIL: cannot open %s\n", dump); return 1; }
            fprintf(fp, "P6\n%d %d\n255\n", W, H);
            for (int y = 0; y < H; y++) { const unsigned char *row = (const unsigned char *)ms.pData + (size_t)y * ms.RowPitch;
                for (int x = 0; x < W; x++) { unsigned char px[3] = {row[x*4+2], row[x*4+1], row[x*4]}; fwrite(px, 1, 3, fp); } }
            fclose(fp); stage->Unmap(0);
            printf("dumped: %s\n", dump);
        }
        CHECK(sc->Present(1, 0));
        QueryPerformanceCounter(&t1);
        double ms_ = (double)(t1.QuadPart - t0.QuadPart) * 1000.0 / (double)f.QuadPart;
        sum += ms_; if (ms_ > worst) worst = ms_; hud.frame(ms_);
    }
    printf("frames: %lld avg_ms: %.2f max_ms: %.2f\n", hud.frames, sum / (hud.frames ? hud.frames : 1), worst);
    DWORD end = GetTickCount() + hold;
    while ((int)(end - GetTickCount()) > 0) { MSG m; while (PeekMessageW(&m, 0, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageW(&m); } Sleep(10); }
    printf("RESULT: PASS\n");
    return 0;
}
