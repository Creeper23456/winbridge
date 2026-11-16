// SPDX-License-Identifier: GPL-3.0-or-later
//
// A D3D9 program for the Venus path: a clear and a rotating, Gouraud-shaded triangle (pre-transformed
// vertices, so no shaders are needed), presented once per frame. Frame i uses a fixed angle, so the
// frame written by --dump is the same on every path. Run with DXVK's d3d9.dll next to the exe.
// Usage: tri9 [--frames N] [--hold-ms N] [--dump PATH.ppm] [--size WxH]
// (--frames 0 runs until the window is closed; the window title shows the frame number and rate)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d9.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "hud.h"

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
    return DefWindowProcW(h, m, w, l);
}
#define CHECK(x) do { HRESULT hr_ = (x); if (FAILED(hr_)) { printf("RESULT: FAIL: %s = 0x%08lx\n", #x, (unsigned long)hr_); return 1; } } while (0)

struct Vtx { float x, y, z, rhw; DWORD color; };

int main(int argc, char **argv) {
    int frames = 120, hold = 0, W = 640, H = 360; const char *dump = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--hold-ms") && i + 1 < argc) hold = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dump") && i + 1 < argc) dump = argv[++i];
        else if (!strcmp(argv[i], "--size") && i + 1 < argc && sscanf(argv[++i], "%dx%d", &W, &H) != 2) { fprintf(stderr, "bad --size\n"); return 2; }
    }
    WNDCLASSW wc = {}; wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"tri9"; wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    RECT r = {0, 0, W, H}; AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, L"tri9", L"WinBridge D3D9 triangle", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                                40, 40, r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);

    IDirect3D9 *d3d = Direct3DCreate9(D3D_SDK_VERSION);
    if (!d3d) { printf("RESULT: FAIL: Direct3DCreate9\n"); return 1; }
    D3DPRESENT_PARAMETERS pp = {};
    pp.Windowed = TRUE; pp.SwapEffect = D3DSWAPEFFECT_DISCARD; pp.BackBufferFormat = D3DFMT_X8R8G8B8;
    pp.BackBufferWidth = W; pp.BackBufferHeight = H; pp.hDeviceWindow = hwnd; pp.PresentationInterval = D3DPRESENT_INTERVAL_ONE;
    IDirect3DDevice9 *dev = 0;
    CHECK(d3d->CreateDevice(D3DADAPTER_DEFAULT, D3DDEVTYPE_HAL, hwnd, D3DCREATE_HARDWARE_VERTEXPROCESSING, &pp, &dev));
    wchar_t adapter[128] = L"";
    D3DADAPTER_IDENTIFIER9 id;
    if (SUCCEEDED(d3d->GetAdapterIdentifier(0, 0, &id))) {
        printf("adapter: %s\n", id.Description);
        MultiByteToWideChar(CP_ACP, 0, id.Description, -1, adapter, 128);
    }
    Hud hud; hud.init(hwnd, L"tri9", adapter, W, H, frames);

    IDirect3DSurface9 *stage = 0;
    if (dump) CHECK(dev->CreateOffscreenPlainSurface(W, H, D3DFMT_X8R8G8B8, D3DPOOL_SYSTEMMEM, &stage, 0));

    LARGE_INTEGER f, t0, t1; QueryPerformanceFrequency(&f);
    double worst = 0, sum = 0;
    const float cx = W * 0.5f, cy = H * 0.5f, rad = (H < W ? H : W) * 0.4f;
    for (int i = 0; frames <= 0 || i < frames; i++) {
        if (!hud.pump()) break;
        QueryPerformanceCounter(&t0);
        Vtx v[3];
        const DWORD col[3] = {0xffff3030u, 0xff30ff30u, 0xff3030ffu};
        for (int k = 0; k < 3; k++) {
            float a = 0.05f * i + k * 2.0943951f;
            v[k] = { cx + rad * cosf(a), cy + rad * sinf(a), 0.5f, 1.0f, col[k] };
        }
        CHECK(dev->Clear(0, 0, D3DCLEAR_TARGET, D3DCOLOR_XRGB(12, 25, 50), 1.0f, 0));
        CHECK(dev->BeginScene());
        CHECK(dev->SetRenderState(D3DRS_LIGHTING, FALSE));
        CHECK(dev->SetRenderState(D3DRS_CULLMODE, D3DCULL_NONE));
        CHECK(dev->SetFVF(D3DFVF_XYZRHW | D3DFVF_DIFFUSE));
        CHECK(dev->DrawPrimitiveUP(D3DPT_TRIANGLELIST, 1, v, sizeof(Vtx)));
        CHECK(dev->EndScene());

        if (dump && i == frames - 1) {
            IDirect3DSurface9 *bb = 0;
            CHECK(dev->GetBackBuffer(0, 0, D3DBACKBUFFER_TYPE_MONO, &bb));
            CHECK(dev->GetRenderTargetData(bb, stage));
            bb->Release();
            D3DLOCKED_RECT lr; CHECK(stage->LockRect(&lr, 0, D3DLOCK_READONLY));
            FILE *fp = fopen(dump, "wb"); fprintf(fp, "P6\n%d %d\n255\n", W, H);
            for (int y = 0; y < H; y++) { const unsigned char *row = (const unsigned char *)lr.pBits + (size_t)y * lr.Pitch;
                for (int x = 0; x < W; x++) { unsigned char px[3] = {row[x*4+2], row[x*4+1], row[x*4]}; fwrite(px, 1, 3, fp); } }
            fclose(fp); stage->UnlockRect();
            printf("dumped: %s\n", dump);
        }
        CHECK(dev->Present(0, 0, 0, 0));
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
