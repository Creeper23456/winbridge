// SPDX-License-Identifier: GPL-3.0-or-later
//
// How often does Windows hand a window's pixels to the display driver when a program draws them the
// way Mesa's Windows software WSI presents a frame: copy the frame into a DIB section, then StretchBlt
// it onto the window's device context? No GPU is involved. Every frame is different (a moving bar),
// so every present changes the window.
// Usage: gdiblit [--size WxH] [--fps N] [--mode blit|paint] [--dwmflush] [--frames N]
//   --fps N      pace the program to N frames a second (0 = as fast as it goes)
//   --mode paint draw in WM_PAINT (InvalidateRect + BeginPaint) instead of on GetDC
//   --dwmflush   call DwmFlush() after each frame
// (--frames 0 runs until the window is closed; the window title shows the frame number and rate)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <dwmapi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "hud.h"

static HDC g_mem;
static void *g_bits;
static int g_w, g_h, g_frame;
static bool g_paint_mode;

static void draw_frame() {
    unsigned *px = (unsigned *)g_bits;
    int bar = (g_frame * 7) % g_w;
    for (int y = 0; y < g_h; y++) {
        for (int x = 0; x < g_w; x++) {
            unsigned c = 0x00203050u + (unsigned)((x * 255 / g_w) << 8);
            px[y * g_w + x] = (x >= bar && x < bar + 24) ? 0x00ffd040u : c;
        }
    }
}

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) { PostQuitMessage(0); return 0; }
    if (m == WM_PAINT && g_paint_mode) {
        PAINTSTRUCT ps;
        HDC dc = BeginPaint(h, &ps);
        StretchBlt(dc, 0, 0, g_w, g_h, g_mem, 0, 0, g_w, g_h, SRCCOPY);
        EndPaint(h, &ps);
        return 0;
    }
    return DefWindowProcW(h, m, w, l);
}

int main(int argc, char **argv) {
    int frames = 600, W = 640, H = 360, fps = 0;
    bool dwmflush = false;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--fps") && i + 1 < argc) fps = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--mode") && i + 1 < argc) g_paint_mode = !strcmp(argv[++i], "paint");
        else if (!strcmp(argv[i], "--dwmflush")) dwmflush = true;
        else if (!strcmp(argv[i], "--size") && i + 1 < argc && sscanf(argv[++i], "%dx%d", &W, &H) != 2) { fprintf(stderr, "bad --size\n"); return 2; }
    }
    g_w = W; g_h = H;
    WNDCLASSW wc = {}; wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gdiblit"; wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    RECT r = {0, 0, W, H}; AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, L"gdiblit", L"WinBridge GDI present", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                                40, 40, r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);
    HDC wdc = GetDC(hwnd);
    g_mem = CreateCompatibleDC(wdc);
    BITMAPINFO bi = {}; bi.bmiHeader.biSize = sizeof bi.bmiHeader; bi.bmiHeader.biWidth = W; bi.bmiHeader.biHeight = -H;
    bi.bmiHeader.biPlanes = 1; bi.bmiHeader.biBitCount = 32; bi.bmiHeader.biCompression = BI_RGB;
    HBITMAP bmp = CreateDIBSection(g_mem, &bi, DIB_RGB_COLORS, &g_bits, 0, 0);
    if (!bmp || !g_bits) { printf("RESULT: FAIL: CreateDIBSection\n"); return 1; }
    SelectObject(g_mem, bmp);

    Hud hud; hud.init(hwnd, L"gdiblit", g_paint_mode ? L"WM_PAINT" : L"GetDC", W, H, frames);
    LARGE_INTEGER f, t0, t1, next; QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&next);
    double sum = 0, worst = 0;
    for (g_frame = 0; frames <= 0 || g_frame < frames; g_frame++) {
        if (!hud.pump()) break;
        if (fps > 0) {
            next.QuadPart += f.QuadPart / fps;
            for (;;) {
                QueryPerformanceCounter(&t0);
                LONGLONG left = next.QuadPart - t0.QuadPart;
                if (left <= 0) break;
                if (left > f.QuadPart / 500) Sleep(1);
            }
        }
        QueryPerformanceCounter(&t0);
        draw_frame();
        if (g_paint_mode) { InvalidateRect(hwnd, 0, FALSE); UpdateWindow(hwnd); }
        else StretchBlt(wdc, 0, 0, W, H, g_mem, 0, 0, W, H, SRCCOPY);
        if (dwmflush) DwmFlush();
        QueryPerformanceCounter(&t1);
        double ms_ = (double)(t1.QuadPart - t0.QuadPart) * 1000.0 / (double)f.QuadPart;
        sum += ms_; if (ms_ > worst) worst = ms_; hud.frame(ms_);
    }
    printf("frames: %lld avg_ms: %.2f max_ms: %.2f\n", hud.frames, sum / (hud.frames ? hud.frames : 1), worst);
    printf("RESULT: PASS\n");
    return 0;
}
