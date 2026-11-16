// SPDX-License-Identifier: GPL-3.0-or-later
//
// What a test program shows while it runs: its window title carries the frame number, the frame
// rate, the time Present and the recording of a frame took (average and worst since the last
// update), the size and the adapter the driver reports. A viewer therefore sees at a glance
// whether the frames move and through which driver. Standard output is unbuffered, so a log file
// stays current.
#pragma once
#include <windows.h>
#include <stdio.h>
#include <wchar.h>

struct Hud {
    HWND hwnd = 0;
    wchar_t name[32] = L"", adapter[128] = L"";
    int w = 0, h = 0, limit = 0; // limit: frames to run, 0 = until the window closes
    LARGE_INTEGER freq = {}, mark = {};
    long long frames = 0, since = 0;
    double sum = 0, worst = 0; // since the last update

    void init(HWND hw, const wchar_t *n, const wchar_t *a, int W, int H, int total) {
        setvbuf(stdout, 0, _IONBF, 0);
        hwnd = hw; w = W; h = H; limit = total;
        wcsncpy(name, n, 31); wcsncpy(adapter, a, 127);
        QueryPerformanceFrequency(&freq); QueryPerformanceCounter(&mark);
        title(0, 0, 0);
    }

    // Dispatches the window's messages; false once the window was closed.
    bool pump() {
        MSG m;
        while (PeekMessageW(&m, 0, 0, 0, PM_REMOVE)) {
            if (m.message == WM_QUIT) return false;
            TranslateMessage(&m); DispatchMessageW(&m);
        }
        return true;
    }

    // Counts one frame that took `ms`; refreshes the title twice a second.
    void frame(double ms) {
        frames++; since++; sum += ms; if (ms > worst) worst = ms;
        LARGE_INTEGER now; QueryPerformanceCounter(&now);
        double secs = (double)(now.QuadPart - mark.QuadPart) / (double)freq.QuadPart;
        if (secs < 0.5) return;
        title(since / secs, sum / since, worst);
        mark = now; since = 0; sum = 0; worst = 0;
    }

    void title(double fps, double avg, double max) {
        wchar_t of[24] = L"", t[320];
        if (limit > 0) swprintf(of, 24, L"/%d", limit);
        swprintf(t, 320, L"%ls | frame %lld%ls | %.1f fps | %.2f ms avg %.2f max | %dx%d | %ls",
                 name, frames, of, fps, avg, max, w, h, adapter);
        SetWindowTextW(hwnd, t);
    }
};
