// SPDX-License-Identifier: GPL-3.0-or-later
// OpenGL triangle: the same picture as tri11/tri9/tri10, drawn through WGL. With Mesa's opengl32.dll
// (Zink) next to the exe, the frames go GL -> Zink -> Vulkan loader -> the WinBridge Venus driver.
// Usage: gltri [--legacy] [--frames N] [--hold-ms N] [--dump PATH.ppm] [--interval N]
//   --interval sets wglSwapIntervalEXT (0 = no wait for the display, 1 = one per refresh).
//   default: a 3.3 core profile context and a GLSL program; --legacy: the compatibility profile
//   with glBegin/glEnd and no shaders. --frames 0 runs until the window is closed; the window title
//   shows the frame number and rate.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <GL/gl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../d3d/hud.h"

typedef char GLchar;
typedef ptrdiff_t GLsizeiptr;
#define GL_FRAGMENT_SHADER 0x8B30
#define GL_VERTEX_SHADER 0x8B31
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_INFO_LOG_LENGTH 0x8B84
#define GL_ARRAY_BUFFER 0x8892
#define GL_STATIC_DRAW 0x88E4
#define WGL_CONTEXT_MAJOR_VERSION_ARB 0x2091
#define WGL_CONTEXT_MINOR_VERSION_ARB 0x2092
#define WGL_CONTEXT_PROFILE_MASK_ARB 0x9126
#define WGL_CONTEXT_CORE_PROFILE_BIT_ARB 1

#define GL_FUNCS(X) \
    X(GLuint, CreateShader, (GLenum)) \
    X(void, ShaderSource, (GLuint, GLsizei, const GLchar *const *, const GLint *)) \
    X(void, CompileShader, (GLuint)) \
    X(void, GetShaderiv, (GLuint, GLenum, GLint *)) \
    X(void, GetShaderInfoLog, (GLuint, GLsizei, GLsizei *, GLchar *)) \
    X(GLuint, CreateProgram, (void)) \
    X(void, AttachShader, (GLuint, GLuint)) \
    X(void, LinkProgram, (GLuint)) \
    X(void, GetProgramiv, (GLuint, GLenum, GLint *)) \
    X(void, GetProgramInfoLog, (GLuint, GLsizei, GLsizei *, GLchar *)) \
    X(void, UseProgram, (GLuint)) \
    X(void, GenBuffers, (GLsizei, GLuint *)) \
    X(void, BindBuffer, (GLenum, GLuint)) \
    X(void, BufferData, (GLenum, GLsizeiptr, const void *, GLenum)) \
    X(void, GenVertexArrays, (GLsizei, GLuint *)) \
    X(void, BindVertexArray, (GLuint)) \
    X(void, VertexAttribPointer, (GLuint, GLint, GLenum, GLboolean, GLsizei, const void *)) \
    X(void, EnableVertexAttribArray, (GLuint))
#define X(ret, name, args) static ret(APIENTRY *gl##name) args;
GL_FUNCS(X)
#undef X

static const char *kVs =
    "#version 330 core\n"
    "layout(location = 0) in vec2 pos;\n"
    "layout(location = 1) in vec3 col;\n"
    "out vec3 c;\n"
    "void main() { gl_Position = vec4(pos, 0.0, 1.0); c = col; }\n";
static const char *kFs =
    "#version 330 core\n"
    "in vec3 c;\n"
    "out vec4 o;\n"
    "void main() { o = vec4(c, 1.0); }\n";

// x, y, r, g, b: the triangle of the Direct3D tests
static const float kVerts[3][5] = {
    {0.0f, 0.6f, 1.0f, 0.4f, 0.1f},
    {0.6f, -0.6f, 0.2f, 0.9f, 0.3f},
    {-0.6f, -0.6f, 0.2f, 0.3f, 1.0f},
};

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) PostQuitMessage(0);
    return DefWindowProcW(h, m, w, l);
}

static bool load_gl() {
    bool ok = true;
#define X(ret, name, args) \
    gl##name = (ret(APIENTRY *) args)wglGetProcAddress("gl" #name); \
    if (!gl##name) { printf("missing gl" #name "\n"); ok = false; }
    GL_FUNCS(X)
#undef X
    return ok;
}

static GLuint compile(GLenum type, const char *src) {
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, 0);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[2048] = "";
        glGetShaderInfoLog(s, sizeof log, 0, log);
        printf("RESULT: FAIL: shader compile: %s\n", log);
        return 0;
    }
    return s;
}

int main(int argc, char **argv) {
    int frames = 120, hold = 0, interval = -1;
    bool legacy = false;
    const char *dump = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--hold-ms") && i + 1 < argc) hold = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dump") && i + 1 < argc) dump = argv[++i];
        else if (!strcmp(argv[i], "--legacy")) legacy = true;
        else if (!strcmp(argv[i], "--interval") && i + 1 < argc) interval = atoi(argv[++i]);
    }
    const int W = 640, H = 360;
    WNDCLASSW wc = {0};
    wc.style = CS_OWNDC; wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"gltri"; wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    RECT r = {0, 0, W, H}; AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, L"gltri", L"WinBridge OpenGL triangle", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                                40, 40, r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);
    HDC dc = GetDC(hwnd);
    PIXELFORMATDESCRIPTOR pfd = {};
    pfd.nSize = sizeof pfd; pfd.nVersion = 1;
    pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    pfd.iPixelType = PFD_TYPE_RGBA; pfd.cColorBits = 32; pfd.cDepthBits = 24; pfd.cStencilBits = 8;
    int pf = ChoosePixelFormat(dc, &pfd);
    if (!pf || !SetPixelFormat(dc, pf, &pfd)) { printf("RESULT: FAIL: pixel format (%d)\n", pf); return 1; }
    HGLRC rc = wglCreateContext(dc);
    if (!rc || !wglMakeCurrent(dc, rc)) { printf("RESULT: FAIL: wglCreateContext/MakeCurrent: %lu\n", GetLastError()); return 1; }
    if (!legacy) {
        typedef HGLRC(WINAPI * CreateAttribs)(HDC, HGLRC, const int *);
        CreateAttribs create = (CreateAttribs)wglGetProcAddress("wglCreateContextAttribsARB");
        const int attribs[] = {WGL_CONTEXT_MAJOR_VERSION_ARB, 3, WGL_CONTEXT_MINOR_VERSION_ARB, 3,
                               WGL_CONTEXT_PROFILE_MASK_ARB, WGL_CONTEXT_CORE_PROFILE_BIT_ARB, 0};
        HGLRC core = create ? create(dc, 0, attribs) : 0;
        if (!core) { printf("RESULT: FAIL: no 3.3 core context (wglCreateContextAttribsARB %s)\n", create ? "failed" : "missing"); return 1; }
        wglMakeCurrent(0, 0); wglDeleteContext(rc);
        rc = core;
        if (!wglMakeCurrent(dc, rc)) { printf("RESULT: FAIL: MakeCurrent (core): %lu\n", GetLastError()); return 1; }
    }
    const char *vendor = (const char *)glGetString(GL_VENDOR);
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *version = (const char *)glGetString(GL_VERSION);
    printf("vendor: %s\nrenderer: %s\nversion: %s\n", vendor ? vendor : "?", renderer ? renderer : "?", version ? version : "?");
    wchar_t wrenderer[128] = L"";
    if (renderer) MultiByteToWideChar(CP_UTF8, 0, renderer, -1, wrenderer, 127);
    if (interval >= 0) {
        typedef BOOL(WINAPI * SwapInterval)(int);
        SwapInterval swap = (SwapInterval)wglGetProcAddress("wglSwapIntervalEXT");
        printf("swap interval %d: %s\n", interval, swap && swap(interval) ? "set" : "not available");
    }
    Hud hud; hud.init(hwnd, legacy ? L"gltri legacy" : L"gltri", wrenderer, W, H, frames);

    GLuint prog = 0, vao = 0, vbo = 0;
    if (!legacy) {
        if (!load_gl()) { printf("RESULT: FAIL: missing GL 3.3 entry points\n"); return 1; }
        GLuint vs = compile(GL_VERTEX_SHADER, kVs), fs = compile(GL_FRAGMENT_SHADER, kFs);
        if (!vs || !fs) return 1;
        prog = glCreateProgram();
        glAttachShader(prog, vs); glAttachShader(prog, fs); glLinkProgram(prog);
        GLint ok = 0; glGetProgramiv(prog, GL_LINK_STATUS, &ok);
        if (!ok) { char log[2048] = ""; glGetProgramInfoLog(prog, sizeof log, 0, log); printf("RESULT: FAIL: link: %s\n", log); return 1; }
        glGenVertexArrays(1, &vao); glBindVertexArray(vao);
        glGenBuffers(1, &vbo); glBindBuffer(GL_ARRAY_BUFFER, vbo);
        glBufferData(GL_ARRAY_BUFFER, sizeof kVerts, kVerts, GL_STATIC_DRAW);
        glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 5 * sizeof(float), (const void *)0);
        glVertexAttribPointer(1, 3, GL_FLOAT, GL_FALSE, 5 * sizeof(float), (const void *)(2 * sizeof(float)));
        glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);
    }

    LARGE_INTEGER f, t0, t1; QueryPerformanceFrequency(&f);
    double worst = 0, sum = 0;
    for (int i = 0; frames <= 0 || i < frames; i++) {
        if (!hud.pump()) break;
        QueryPerformanceCounter(&t0);
        glViewport(0, 0, W, H);
        glClearColor(0.05f, 0.10f, 0.20f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        if (legacy) {
            glBegin(GL_TRIANGLES);
            for (int v = 0; v < 3; v++) { glColor3f(kVerts[v][2], kVerts[v][3], kVerts[v][4]); glVertex2f(kVerts[v][0], kVerts[v][1]); }
            glEnd();
        } else {
            glUseProgram(prog);
            glDrawArrays(GL_TRIANGLES, 0, 3);
        }
        if (dump && i == frames - 1) {
            unsigned char *px = (unsigned char *)malloc((size_t)W * H * 3);
            glPixelStorei(GL_PACK_ALIGNMENT, 1);
            glReadBuffer(GL_BACK);
            glReadPixels(0, 0, W, H, GL_RGB, GL_UNSIGNED_BYTE, px);
            FILE *fp = fopen(dump, "wb");
            if (!fp) { printf("RESULT: FAIL: cannot open %s\n", dump); return 1; }
            fprintf(fp, "P6\n%d %d\n255\n", W, H);
            for (int y = H - 1; y >= 0; y--) fwrite(px + (size_t)y * W * 3, 1, (size_t)W * 3, fp);
            fclose(fp); free(px);
            printf("dumped: %s\n", dump);
        }
        if (!SwapBuffers(dc)) { printf("RESULT: FAIL: SwapBuffers: %lu\n", GetLastError()); return 1; }
        QueryPerformanceCounter(&t1);
        double ms_ = (double)(t1.QuadPart - t0.QuadPart) * 1000.0 / (double)f.QuadPart;
        sum += ms_; if (ms_ > worst) worst = ms_; hud.frame(ms_);
    }
    GLenum err = glGetError();
    printf("frames: %lld avg_ms: %.2f max_ms: %.2f glError: 0x%x\n", hud.frames, sum / (hud.frames ? hud.frames : 1), worst, (unsigned)err);
    DWORD end = GetTickCount() + hold;
    while ((int)(end - GetTickCount()) > 0) { MSG m; while (PeekMessageW(&m, 0, 0, 0, PM_REMOVE)) { TranslateMessage(&m); DispatchMessageW(&m); } Sleep(10); }
    wglMakeCurrent(0, 0); wglDeleteContext(rc);
    printf("RESULT: %s\n", err == GL_NO_ERROR ? "PASS" : "FAIL");
    return err == GL_NO_ERROR ? 0 : 1;
}
