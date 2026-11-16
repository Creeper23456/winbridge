// SPDX-License-Identifier: GPL-3.0-or-later
// D3D11 teapot-class scene for the Vulkan path tests: a lathe body, lid, spout and handle on a
// ground plane, drawn with a depth buffer, an indexed draw, a sampled texture and a constant buffer
// that is updated between two draws. The geometry is procedural; frame i uses a fixed angle, so
// the frame written by --dump is the same on every path. Run with DXVK's d3d11.dll and dxgi.dll
// next to the exe. Usage: teapot11 [--frames N] [--hold-ms N] [--dump PATH.ppm] [--size WxH] [--resize WxH]
// (--frames 0 runs until the window is closed; --resize changes the swapchain to that size half way
// through, which frees and creates its images). The window title shows the frame number and rate.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vector>
#include "hud.h"

struct Vec3 { float x, y, z; };
static Vec3 operator+(Vec3 a, Vec3 b) { return {a.x + b.x, a.y + b.y, a.z + b.z}; }
static Vec3 operator-(Vec3 a, Vec3 b) { return {a.x - b.x, a.y - b.y, a.z - b.z}; }
static Vec3 operator*(Vec3 a, float s) { return {a.x * s, a.y * s, a.z * s}; }
static float dot(Vec3 a, Vec3 b) { return a.x * b.x + a.y * b.y + a.z * b.z; }
static Vec3 cross(Vec3 a, Vec3 b) { return {a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x}; }
static Vec3 norm(Vec3 a) { float l = sqrtf(dot(a, a)); return l > 1e-12f ? a * (1.0f / l) : Vec3{0, 1, 0}; }

struct Mat { float m[4][4]; };  // row-major, row vectors: v * M
static Mat mul(const Mat &a, const Mat &b) {
    Mat r;
    for (int i = 0; i < 4; i++) for (int j = 0; j < 4; j++) {
        float s = 0; for (int k = 0; k < 4; k++) s += a.m[i][k] * b.m[k][j];
        r.m[i][j] = s;
    }
    return r;
}
static Mat rotY(float a) {
    float c = cosf(a), s = sinf(a);
    return {{{c, 0, -s, 0}, {0, 1, 0, 0}, {s, 0, c, 0}, {0, 0, 0, 1}}};
}
static Mat identity() { return {{{1, 0, 0, 0}, {0, 1, 0, 0}, {0, 0, 1, 0}, {0, 0, 0, 1}}}; }
static Mat lookAt(Vec3 eye, Vec3 at, Vec3 up) {
    Vec3 z = norm(at - eye), x = norm(cross(up, z)), y = cross(z, x);
    return {{{x.x, y.x, z.x, 0}, {x.y, y.y, z.y, 0}, {x.z, y.z, z.z, 0},
             {-dot(x, eye), -dot(y, eye), -dot(z, eye), 1}}};
}
static Mat perspective(float fov, float aspect, float zn, float zf) {
    float t = 1.0f / tanf(fov * 0.5f);
    return {{{t / aspect, 0, 0, 0}, {0, t, 0, 0}, {0, 0, zf / (zf - zn), 1}, {0, 0, -zn * zf / (zf - zn), 0}}};
}

struct Vtx { Vec3 p, n; float u, v; };

// Adds a (nu+1) x (nv+1) grid over the surface f(u, v) in [0,1]^2. Normals come from finite
// differences, with v kept off the poles where the surface degenerates.
template <class F>
static void addSurface(std::vector<Vtx> &vs, std::vector<uint32_t> &is, int nu, int nv, float uvScale, F f) {
    uint32_t base = (uint32_t)vs.size();
    const float e = 1e-3f;
    for (int j = 0; j <= nv; j++) for (int i = 0; i <= nu; i++) {
        float u = (float)i / nu, v = (float)j / nv;
        float vc = v < 0.01f ? 0.01f : (v > 0.99f ? 0.99f : v);
        Vec3 pu = f(u + e, vc) - f(u - e, vc), pv = f(u, vc + e) - f(u, vc - e);
        vs.push_back({f(u, v), norm(cross(pu, pv)), u * uvScale, v * uvScale});
    }
    for (int j = 0; j < nv; j++) for (int i = 0; i < nu; i++) {
        uint32_t a = base + j * (nu + 1) + i, b = a + 1, c = a + (nu + 1), d = c + 1;
        uint32_t t[6] = {a, b, c, b, d, c};
        is.insert(is.end(), t, t + 6);
    }
}

static Vec3 catmull(const Vec3 *pts, int n, float t) {
    float s = t * (n - 1); int i = (int)s; if (i >= n - 1) i = n - 2; float f = s - i;
    Vec3 p0 = pts[i > 0 ? i - 1 : 0], p1 = pts[i], p2 = pts[i + 1], p3 = pts[i + 2 < n ? i + 2 : n - 1];
    float f2 = f * f, f3 = f2 * f;
    return (p1 * 2.0f + (p2 - p0) * f + (p0 * 2.0f - p1 * 5.0f + p2 * 4.0f - p3) * f2 + (p1 * 3.0f - p0 - p2 * 3.0f + p3) * f3) * 0.5f;
}

static const char *kShader =
    "cbuffer CB : register(b0) { row_major float4x4 wvp; row_major float4x4 world; float4 light; float4 tint; };\n"
    "Texture2D tex : register(t0); SamplerState smp : register(s0);\n"
    "struct VO { float4 p : SV_Position; float3 n : NORMAL; float2 uv : TEXCOORD0; };\n"
    "VO vs(float3 p : POSITION, float3 n : NORMAL, float2 uv : TEXCOORD0) {\n"
    "  VO o; o.p = mul(float4(p, 1), wvp); o.n = mul(n, (float3x3)world); o.uv = uv; return o; }\n"
    "float4 ps(VO i) : SV_Target {\n"
    "  float3 n = normalize(i.n); float d = abs(dot(n, -light.xyz)) * 0.8 + 0.2;\n"
    "  float3 c = tex.Sample(smp, i.uv * tint.w).rgb * tint.rgb * d; return float4(c, 1); }\n";

struct CB { Mat wvp, world; float light[4], tint[4]; };

static LRESULT CALLBACK proc(HWND h, UINT m, WPARAM w, LPARAM l) {
    if (m == WM_DESTROY) PostQuitMessage(0);
    return DefWindowProcW(h, m, w, l);
}

#define CHECK(x) do { HRESULT hr_ = (x); if (FAILED(hr_)) { printf("RESULT: FAIL: %s = 0x%08lx\n", #x, (unsigned long)hr_); return 1; } } while (0)

int main(int argc, char **argv) {
    int frames = 120, hold = 0, W = 640, H = 360, W2 = 0, H2 = 0; const char *dump = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--frames") && i + 1 < argc) frames = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--hold-ms") && i + 1 < argc) hold = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--dump") && i + 1 < argc) dump = argv[++i];
        else if (!strcmp(argv[i], "--size") && i + 1 < argc && sscanf(argv[++i], "%dx%d", &W, &H) != 2) { fprintf(stderr, "bad --size\n"); return 2; }
        else if (!strcmp(argv[i], "--resize") && i + 1 < argc && sscanf(argv[++i], "%dx%d", &W2, &H2) != 2) { fprintf(stderr, "bad --resize\n"); return 2; }
    }
    WNDCLASSW wc = {0}; wc.lpfnWndProc = proc; wc.hInstance = GetModuleHandleW(0);
    wc.lpszClassName = L"teapot11"; wc.hCursor = LoadCursorW(0, (LPCWSTR)IDC_ARROW);
    RegisterClassW(&wc);
    RECT r = {0, 0, W, H}; AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
    HWND hwnd = CreateWindowExW(0, L"teapot11", L"WinBridge D3D11 teapot", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                                40, 40, r.right - r.left, r.bottom - r.top, 0, 0, wc.hInstance, 0);

    DXGI_SWAP_CHAIN_DESC sd = {};
    sd.BufferCount = 2; sd.BufferDesc.Width = W; sd.BufferDesc.Height = H;
    sd.BufferDesc.Format = DXGI_FORMAT_B8G8R8A8_UNORM; sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.OutputWindow = hwnd; sd.SampleDesc.Count = 1; sd.Windowed = TRUE; sd.SwapEffect = DXGI_SWAP_EFFECT_DISCARD;
    ID3D11Device *dev = 0; ID3D11DeviceContext *ctx = 0; IDXGISwapChain *sc = 0;
    D3D_FEATURE_LEVEL fl;
    CHECK(D3D11CreateDeviceAndSwapChain(0, D3D_DRIVER_TYPE_HARDWARE, 0, 0, 0, 0, D3D11_SDK_VERSION, &sd, &sc, &dev, &fl, &ctx));
    printf("feature_level: 0x%x\n", (unsigned)fl);
    IDXGIDevice *dd = 0; IDXGIAdapter *ad = 0; DXGI_ADAPTER_DESC adesc = {};
    if (SUCCEEDED(dev->QueryInterface(__uuidof(IDXGIDevice), (void **)&dd)) && SUCCEEDED(dd->GetAdapter(&ad)) && SUCCEEDED(ad->GetDesc(&adesc)))
        printf("adapter: %ls\n", adesc.Description);
    Hud hud; hud.init(hwnd, L"teapot11", adesc.Description, W, H, frames);

    // geometry: the pot first, the ground after it
    std::vector<Vtx> vs; std::vector<uint32_t> is;
    const float pi = 3.14159265f;
    static const Vec3 prof[] = {{0.0f, 0.0f, 0}, {1.0f, 0.0f, 0}, {1.25f, 0.4f, 0}, {1.35f, 0.85f, 0}, {1.1f, 1.3f, 0}, {0.8f, 1.45f, 0}};
    addSurface(vs, is, 64, 24, 4.0f, [&](float u, float v) {
        Vec3 q = catmull(prof, 6, v); float a = u * 2 * pi;
        return Vec3{q.x * cosf(a), q.y, q.x * sinf(a)}; });
    addSurface(vs, is, 48, 12, 2.0f, [&](float u, float v) {      // lid
        float a = u * 2 * pi, b = v * 0.5f * pi;
        return Vec3{0.8f * cosf(b) * cosf(a), 1.45f + 0.5f * sinf(b), 0.8f * cosf(b) * sinf(a)}; });
    addSurface(vs, is, 24, 10, 1.0f, [&](float u, float v) {      // knob
        float a = u * 2 * pi, b = (v - 0.5f) * pi;
        return Vec3{0.18f * cosf(b) * cosf(a), 2.0f + 0.18f * sinf(b), 0.18f * cosf(b) * sinf(a)}; });
    addSurface(vs, is, 24, 12, 2.0f, [&](float u, float v) {      // spout: a tapered tube
        Vec3 a0 = {-1.15f, 0.55f, 0}, a1 = {-2.0f, 1.35f, 0};
        Vec3 axis = norm(a1 - a0), side = norm(cross(axis, Vec3{0, 0, 1})), up = cross(axis, side);
        float rad = 0.32f + (0.14f - 0.32f) * v, a = u * 2 * pi;
        return a0 + (a1 - a0) * v + side * (rad * cosf(a)) + up * (rad * sinf(a)); });
    addSurface(vs, is, 24, 16, 1.0f, [&](float u, float v) {      // handle: half a torus
        float phi = (u - 0.5f) * pi, psi = v * 2 * pi, rr = 0.6f + 0.12f * cosf(psi);
        return Vec3{1.0f + rr * cosf(phi), 0.8f + rr * sinf(phi), 0.12f * sinf(psi)}; });
    uint32_t potIndices = (uint32_t)is.size();
    addSurface(vs, is, 1, 1, 1.0f, [&](float u, float v) { return Vec3{(u - 0.5f) * 8.0f, -0.02f, (v - 0.5f) * 8.0f}; });
    uint32_t groundIndices = (uint32_t)is.size() - potIndices;

    ID3DBlob *vsb = 0, *psb = 0, *err = 0;
    if (FAILED(D3DCompile(kShader, strlen(kShader), 0, 0, 0, "vs", "vs_5_0", 0, 0, &vsb, &err))) { printf("RESULT: FAIL: vs compile: %s\n", err ? (char *)err->GetBufferPointer() : "?"); return 1; }
    if (FAILED(D3DCompile(kShader, strlen(kShader), 0, 0, 0, "ps", "ps_5_0", 0, 0, &psb, &err))) { printf("RESULT: FAIL: ps compile: %s\n", err ? (char *)err->GetBufferPointer() : "?"); return 1; }
    ID3D11VertexShader *vsh = 0; ID3D11PixelShader *psh = 0;
    CHECK(dev->CreateVertexShader(vsb->GetBufferPointer(), vsb->GetBufferSize(), 0, &vsh));
    CHECK(dev->CreatePixelShader(psb->GetBufferPointer(), psb->GetBufferSize(), 0, &psh));
    D3D11_INPUT_ELEMENT_DESC il[] = {
        {"POSITION", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0},
        {"NORMAL", 0, DXGI_FORMAT_R32G32B32_FLOAT, 0, 12, D3D11_INPUT_PER_VERTEX_DATA, 0},
        {"TEXCOORD", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 24, D3D11_INPUT_PER_VERTEX_DATA, 0}};
    ID3D11InputLayout *layout = 0;
    CHECK(dev->CreateInputLayout(il, 3, vsb->GetBufferPointer(), vsb->GetBufferSize(), &layout));

    ID3D11Buffer *vb = 0, *ib = 0, *cb = 0;
    D3D11_BUFFER_DESC bd = {0}; D3D11_SUBRESOURCE_DATA sr = {0};
    bd.ByteWidth = (UINT)(vs.size() * sizeof(Vtx)); bd.BindFlags = D3D11_BIND_VERTEX_BUFFER; sr.pSysMem = vs.data();
    CHECK(dev->CreateBuffer(&bd, &sr, &vb));
    bd.ByteWidth = (UINT)(is.size() * sizeof(uint32_t)); bd.BindFlags = D3D11_BIND_INDEX_BUFFER; sr.pSysMem = is.data();
    CHECK(dev->CreateBuffer(&bd, &sr, &ib));
    bd.ByteWidth = sizeof(CB); bd.BindFlags = D3D11_BIND_CONSTANT_BUFFER; bd.Usage = D3D11_USAGE_DEFAULT;
    CHECK(dev->CreateBuffer(&bd, 0, &cb));

    // a 64x64 checker texture
    static uint32_t texels[64 * 64];
    for (int y = 0; y < 64; y++) for (int x = 0; x < 64; x++) {
        bool a = ((x / 8) + (y / 8)) & 1;
        texels[y * 64 + x] = a ? 0xff464038u : 0xffe6e6e6u;
    }
    D3D11_TEXTURE2D_DESC td = {0};
    td.Width = 64; td.Height = 64; td.MipLevels = 1; td.ArraySize = 1; td.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    td.SampleDesc.Count = 1; td.Usage = D3D11_USAGE_DEFAULT; td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    D3D11_SUBRESOURCE_DATA tsr = {texels, 64 * 4, 0};
    ID3D11Texture2D *tex = 0; ID3D11ShaderResourceView *srv = 0;
    CHECK(dev->CreateTexture2D(&td, &tsr, &tex));
    CHECK(dev->CreateShaderResourceView(tex, 0, &srv));
    D3D11_SAMPLER_DESC sm = {};
    sm.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR; sm.AddressU = sm.AddressV = sm.AddressW = D3D11_TEXTURE_ADDRESS_WRAP;
    sm.MaxLOD = D3D11_FLOAT32_MAX; sm.ComparisonFunc = D3D11_COMPARISON_NEVER;
    ID3D11SamplerState *samp = 0; CHECK(dev->CreateSamplerState(&sm, &samp));

    ID3D11Texture2D *bb = 0, *depth = 0, *stage = 0;
    ID3D11RenderTargetView *rtv = 0; ID3D11DepthStencilView *dsv = 0;
    D3D11_DEPTH_STENCIL_DESC dsd = {0}; dsd.DepthEnable = TRUE; dsd.DepthWriteMask = D3D11_DEPTH_WRITE_MASK_ALL; dsd.DepthFunc = D3D11_COMPARISON_LESS;
    ID3D11DepthStencilState *dss = 0; CHECK(dev->CreateDepthStencilState(&dsd, &dss));
    D3D11_RASTERIZER_DESC rd = {}; rd.FillMode = D3D11_FILL_SOLID; rd.CullMode = D3D11_CULL_NONE; rd.DepthClipEnable = TRUE;
    ID3D11RasterizerState *rs = 0; CHECK(dev->CreateRasterizerState(&rd, &rs));

    D3D11_VIEWPORT vp = {}; Mat proj;
    // everything that depends on the size of the back buffer
    auto makeTargets = [&]() -> HRESULT {
        HRESULT hr;
        if (FAILED(hr = sc->GetBuffer(0, __uuidof(ID3D11Texture2D), (void **)&bb))) return hr;
        if (FAILED(hr = dev->CreateRenderTargetView(bb, 0, &rtv))) return hr;
        D3D11_TEXTURE2D_DESC dd2 = {0};
        dd2.Width = W; dd2.Height = H; dd2.MipLevels = 1; dd2.ArraySize = 1; dd2.Format = DXGI_FORMAT_D32_FLOAT;
        dd2.SampleDesc.Count = 1; dd2.Usage = D3D11_USAGE_DEFAULT; dd2.BindFlags = D3D11_BIND_DEPTH_STENCIL;
        if (FAILED(hr = dev->CreateTexture2D(&dd2, 0, &depth))) return hr;
        if (FAILED(hr = dev->CreateDepthStencilView(depth, 0, &dsv))) return hr;
        if (dump) {
            D3D11_TEXTURE2D_DESC st; bb->GetDesc(&st); st.Usage = D3D11_USAGE_STAGING; st.BindFlags = 0;
            st.CPUAccessFlags = D3D11_CPU_ACCESS_READ; st.MiscFlags = 0;
            if (FAILED(hr = dev->CreateTexture2D(&st, 0, &stage))) return hr;
        }
        vp = {0, 0, (float)W, (float)H, 0, 1};
        proj = perspective(0.8f, (float)W / H, 0.1f, 50.0f);
        return S_OK;
    };
    CHECK(makeTargets());
    Mat view = lookAt({0, 2.4f, -6.5f}, {0, 0.9f, 0}, {0, 1, 0});
    Vec3 ld = norm({-0.4f, -0.8f, 0.5f});
    LARGE_INTEGER f, t0, t1; QueryPerformanceFrequency(&f);
    double worst = 0, sum = 0;
    for (int i = 0; frames <= 0 || i < frames; i++) {
        if (W2 && frames > 0 && i == frames / 2) {
            ctx->OMSetRenderTargets(0, 0, 0); ctx->ClearState();
            rtv->Release(); dsv->Release(); depth->Release(); bb->Release(); rtv = 0; dsv = 0; depth = 0; bb = 0;
            if (stage) { stage->Release(); stage = 0; }
            W = W2; H = H2;
            CHECK(sc->ResizeBuffers(0, W, H, DXGI_FORMAT_UNKNOWN, 0));
            CHECK(makeTargets());
            printf("resized to %dx%d\n", W, H); hud.w = W; hud.h = H;
        }
        if (!hud.pump()) break;
        QueryPerformanceCounter(&t0);
        float clear[4] = {0.05f, 0.10f, 0.20f, 1.0f};
        ctx->OMSetRenderTargets(1, &rtv, dsv); ctx->OMSetDepthStencilState(dss, 0); ctx->RSSetState(rs); ctx->RSSetViewports(1, &vp);
        ctx->ClearRenderTargetView(rtv, clear); ctx->ClearDepthStencilView(dsv, D3D11_CLEAR_DEPTH, 1.0f, 0);
        UINT stride = sizeof(Vtx), off = 0;
        ctx->IASetInputLayout(layout); ctx->IASetVertexBuffers(0, 1, &vb, &stride, &off);
        ctx->IASetIndexBuffer(ib, DXGI_FORMAT_R32_UINT, 0);
        ctx->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ctx->VSSetShader(vsh, 0, 0); ctx->PSSetShader(psh, 0, 0);
        ctx->VSSetConstantBuffers(0, 1, &cb); ctx->PSSetConstantBuffers(0, 1, &cb);
        ctx->PSSetShaderResources(0, 1, &srv); ctx->PSSetSamplers(0, 1, &samp);

        CB c; c.world = rotY(0.04f * i);
        c.wvp = mul(mul(c.world, view), proj);
        c.light[0] = ld.x; c.light[1] = ld.y; c.light[2] = ld.z; c.light[3] = 0;
        c.tint[0] = 0.35f; c.tint[1] = 0.85f; c.tint[2] = 0.8f; c.tint[3] = 1.0f;
        ctx->UpdateSubresource(cb, 0, 0, &c, 0, 0);
        ctx->DrawIndexed(potIndices, 0, 0);
        c.world = identity(); c.wvp = mul(view, proj);
        c.tint[0] = 0.9f; c.tint[1] = 0.9f; c.tint[2] = 0.9f; c.tint[3] = 6.0f;
        ctx->UpdateSubresource(cb, 0, 0, &c, 0, 0);
        ctx->DrawIndexed(groundIndices, potIndices, 0);

        if (dump && i == frames - 1) {
            ctx->CopyResource(stage, bb);
            D3D11_MAPPED_SUBRESOURCE ms;
            CHECK(ctx->Map(stage, 0, D3D11_MAP_READ, 0, &ms));
            FILE *fp = fopen(dump, "wb"); fprintf(fp, "P6\n%d %d\n255\n", W, H);
            for (int y = 0; y < H; y++) { const unsigned char *row = (const unsigned char *)ms.pData + (size_t)y * ms.RowPitch;
                for (int x = 0; x < W; x++) { unsigned char px[3] = {row[x*4+2], row[x*4+1], row[x*4]}; fwrite(px, 1, 3, fp); } }
            fclose(fp); ctx->Unmap(stage, 0);
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
