/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * WinBridge global graphics hook: an AppInit_DLLs module that gives every program that is not part
 * of Windows the accelerated OpenGL (Mesa Zink: opengl32.dll, libgallium_wgl.dll) and Direct3D 9,
 * 10 and 11 (DXVK: d3d9.dll, d3d10core.dll, d3d11.dll, dxgi.dll) without touching the program's
 * folder. Measured on the Windows 11 LTSC guest (docs/adr/0001-venus-as-serialization-layer.md):
 *
 *  - user32 loads this module after the program's static imports are bound, so a program that
 *    imports opengl32.dll or d3d11.dll already points at System32. The hook therefore loads its
 *    replacements by full path (the loader then hands them out for every later request of the
 *    same file name, LoadLibraryEx with LOAD_LIBRARY_SEARCH_SYSTEM32 included) and rewrites the
 *    import tables of the modules that are already loaded.
 *  - Chromium's sandbox forbids AppInit modules in its child processes; browsers use ANGLE's
 *    Vulkan backend instead (--use-angle=vulkan), which needs no hook.
 *
 * Programs under the Windows directory are never touched, nor those on the deny list, nor ones
 * that already carry their own opengl32.dll or DirectX DLLs next to them. Nothing here may stop a
 * program from starting: every failure leaves the process as it was. No C runtime, kernel32 only.
 *
 * Settings, HKLM\SOFTWARE\WinBridge\Hook (64-bit view, written by scripts/install-venus-icd.ps1):
 *   Components  REG_DWORD      1 = OpenGL, 2 = Direct3D; absent means both
 *   Deny        REG_MULTI_SZ   file names of programs to leave alone
 * Debugging: set the environment variable WBHOOK_LOG to a file name to get a line per decision.
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <winternl.h>

#define WBH_COMP_GL 1u
#define WBH_COMP_D3D 2u

#ifndef RRF_RT_REG_DWORD
#define RRF_RT_REG_DWORD 0x00000010
#endif
#ifndef RRF_RT_REG_MULTI_SZ
#define RRF_RT_REG_MULTI_SZ 0x00000020
#endif
#ifndef RRF_SUBKEY_WOW6464KEY
#define RRF_SUBKEY_WOW6464KEY 0x00010000
#endif

/* The files to load, in this order (libgallium_wgl.dll before the opengl32.dll that imports it,
 * dxgi.dll before the d3d11.dll that imports it). The files sit in x64\ or x86\ next to the hook. */
static const struct {
   const WCHAR *file;
   const char *import; /* the name programs import it by, NULL when none does */
   DWORD comp;
} wbh_files[] = {
   { L"libgallium_wgl.dll", NULL, WBH_COMP_GL },
   { L"opengl32.dll", "opengl32.dll", WBH_COMP_GL },
   { L"dxgi.dll", "dxgi.dll", WBH_COMP_D3D },
   { L"d3d11.dll", "d3d11.dll", WBH_COMP_D3D },
   { L"d3d10core.dll", "d3d10core.dll", WBH_COMP_D3D },
   { L"d3d9.dll", "d3d9.dll", WBH_COMP_D3D },
};
#define WBH_NFILES (sizeof(wbh_files) / sizeof(wbh_files[0]))

/* Programs that must never get the hook, whatever the registry says. */
static const WCHAR *const wbh_never[] = { L"winbridge-agent.exe", NULL };

static HMODULE wbh_loaded[WBH_NFILES];
static HMODULE wbh_self;

typedef LONG(WINAPI *wbh_reg_get_value)(HKEY, LPCWSTR, LPCWSTR, DWORD, LPDWORD, PVOID, LPDWORD);

/* ---------- small helpers (no C runtime) ------------------------------------------------- */

static WCHAR
wbh_lower(WCHAR c)
{
   return (c >= L'A' && c <= L'Z') ? (WCHAR)(c + (L'a' - L'A')) : c;
}

static int
wbh_ieq(const WCHAR *a, const WCHAR *b)
{
   while (*a && *b) {
      if (wbh_lower(*a++) != wbh_lower(*b++))
         return 0;
   }
   return *a == *b;
}

/* a (alen characters, not terminated) starts with the terminated b */
static int
wbh_istarts(const WCHAR *a, size_t alen, const WCHAR *b)
{
   for (size_t i = 0; b[i]; i++) {
      if (i >= alen || wbh_lower(a[i]) != wbh_lower(b[i]))
         return 0;
   }
   return 1;
}

static int
wbh_ieq_ascii(const char *a, const char *b)
{
   while (*a && *b) {
      char x = *a++, y = *b++;
      if (x >= 'A' && x <= 'Z')
         x = (char)(x + ('a' - 'A'));
      if (y >= 'A' && y <= 'Z')
         y = (char)(y + ('a' - 'A'));
      if (x != y)
         return 0;
   }
   return *a == *b;
}

static const WCHAR *
wbh_base_name(const WCHAR *path, size_t len, size_t *out_len)
{
   size_t start = 0;
   for (size_t i = 0; i < len; i++) {
      if (path[i] == L'\\' || path[i] == L'/')
         start = i + 1;
   }
   *out_len = len - start;
   return path + start;
}

static size_t
wbh_len(const WCHAR *s)
{
   size_t n = 0;
   while (s[n])
      n++;
   return n;
}

static void
wbh_log(const char *what, const WCHAR *subject)
{
   WCHAR file[MAX_PATH];
   DWORD n = GetEnvironmentVariableW(L"WBHOOK_LOG", file, MAX_PATH);
   if (!n || n >= MAX_PATH)
      return;
   HANDLE h = CreateFileW(file, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_ALWAYS,
                          FILE_ATTRIBUTE_NORMAL, NULL);
   if (h == INVALID_HANDLE_VALUE)
      return;
   char line[640];
   size_t at = 0;
   char digits[12];
   int nd = 0;
   DWORD pid = GetCurrentProcessId();
   do {
      digits[nd++] = (char)('0' + pid % 10);
      pid /= 10;
   } while (pid);
   while (nd)
      line[at++] = digits[--nd];
   line[at++] = ' ';
   while (*what && at < 100)
      line[at++] = *what++;
   if (subject) {
      line[at++] = ' ';
      for (size_t i = 0; subject[i] && at < sizeof(line) - 4; i++)
         line[at++] = subject[i] < 128 ? (char)subject[i] : '?';
   }
   line[at++] = '\r';
   line[at++] = '\n';
   DWORD written;
   WriteFile(h, line, (DWORD)at, &written, NULL);
   CloseHandle(h);
}

/* ---------- decisions --------------------------------------------------------------------- */

static wbh_reg_get_value
wbh_registry(void)
{
   /* advapi32 may not be loaded yet; KernelBase always is and has the same function. */
   HMODULE kb = GetModuleHandleW(L"kernelbase.dll");
   return kb ? (wbh_reg_get_value)(void *)GetProcAddress(kb, "RegGetValueW") : NULL;
}

static DWORD
wbh_components(wbh_reg_get_value get)
{
   DWORD value = 0, size = sizeof(value);
   if (get && get(HKEY_LOCAL_MACHINE, L"SOFTWARE\\WinBridge\\Hook", L"Components",
                  RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, NULL, &value, &size) == ERROR_SUCCESS)
      return value & (WBH_COMP_GL | WBH_COMP_D3D);
   return WBH_COMP_GL | WBH_COMP_D3D;
}

static int
wbh_denied(wbh_reg_get_value get, const WCHAR *name)
{
   for (int i = 0; wbh_never[i]; i++) {
      if (wbh_ieq(name, wbh_never[i]))
         return 1;
   }
   WCHAR list[1024]; /* under a page: no stack probe, so no C runtime is needed */
   DWORD size = sizeof(list) - 2 * sizeof(WCHAR);
   if (!get || get(HKEY_LOCAL_MACHINE, L"SOFTWARE\\WinBridge\\Hook", L"Deny",
                   RRF_RT_REG_MULTI_SZ | RRF_SUBKEY_WOW6464KEY, NULL, list, &size) != ERROR_SUCCESS)
      return 0;
   list[size / sizeof(WCHAR)] = 0;
   list[size / sizeof(WCHAR) + 1] = 0;
   for (const WCHAR *p = list; *p; p += wbh_len(p) + 1) {
      if (wbh_ieq(name, p))
         return 1;
   }
   return 0;
}

/* Calls f for every loaded module with its base address and full path. */
static void
wbh_each_module(void (*f)(HMODULE, const WCHAR *, size_t, void *), void *arg)
{
   PEB *peb = NtCurrentTeb()->ProcessEnvironmentBlock;
   if (!peb || !peb->Ldr)
      return;
   LIST_ENTRY *head = &peb->Ldr->InMemoryOrderModuleList;
   for (LIST_ENTRY *e = head->Flink; e && e != head; e = e->Flink) {
      LDR_DATA_TABLE_ENTRY *m = CONTAINING_RECORD(e, LDR_DATA_TABLE_ENTRY, InMemoryOrderLinks);
      if (m->DllBase)
         f((HMODULE)m->DllBase, m->FullDllName.Buffer, m->FullDllName.Length / sizeof(WCHAR), arg);
   }
}

struct wbh_scan {
   const WCHAR *windir;
   size_t windir_len;
   DWORD own_files; /* components whose DLL a program already carries itself */
};

static void
wbh_scan_module(HMODULE m, const WCHAR *path, size_t len, void *arg)
{
   struct wbh_scan *s = arg;
   (void)m;
   if (!path || wbh_istarts(path, len, s->windir))
      return;
   size_t n;
   const WCHAR *name = wbh_base_name(path, len, &n);
   for (size_t i = 0; i < WBH_NFILES; i++) {
      if (!wbh_files[i].import)
         continue;
      WCHAR want[32];
      size_t k = 0;
      for (; wbh_files[i].file[k]; k++)
         want[k] = wbh_files[i].file[k];
      if (k == n) {
         size_t j = 0;
         while (j < n && wbh_lower(name[j]) == wbh_lower(want[j]))
            j++;
         if (j == n)
            s->own_files |= wbh_files[i].comp;
      }
   }
}

/* ---------- import tables ------------------------------------------------------------------ */

static HMODULE
wbh_replacement(const char *import)
{
   for (size_t i = 0; i < WBH_NFILES; i++) {
      if (wbh_files[i].import && wbh_loaded[i] && wbh_ieq_ascii(import, wbh_files[i].import))
         return wbh_loaded[i];
   }
   return NULL;
}

static void
wbh_patch(FARPROC *slot, FARPROC target)
{
   DWORD old;
   if (!VirtualProtect(slot, sizeof(*slot), PAGE_READWRITE, &old))
      return;
   *slot = target;
   VirtualProtect(slot, sizeof(*slot), old, &old);
}

static void
wbh_rebind(HMODULE m, const WCHAR *path, size_t len, void *arg)
{
   (void)path;
   (void)len;
   (void)arg;
   if (m == wbh_self)
      return;
   const BYTE *base = (const BYTE *)m;
   const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)base;
   if (dos->e_magic != IMAGE_DOS_SIGNATURE)
      return;
   const IMAGE_NT_HEADERS *nt = (const IMAGE_NT_HEADERS *)(base + dos->e_lfanew);
   if (nt->Signature != IMAGE_NT_SIGNATURE)
      return;
   const IMAGE_DATA_DIRECTORY dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
   if (!dir.VirtualAddress)
      return;
   for (const IMAGE_IMPORT_DESCRIPTOR *d = (const IMAGE_IMPORT_DESCRIPTOR *)(base + dir.VirtualAddress);
        d->Name; d++) {
      HMODULE repl = wbh_replacement((const char *)(base + d->Name));
      /* Without the name table the names are gone once the table is bound. */
      if (!repl || repl == m || !d->OriginalFirstThunk)
         continue;
      const IMAGE_THUNK_DATA *names = (const IMAGE_THUNK_DATA *)(base + d->OriginalFirstThunk);
      FARPROC *iat = (FARPROC *)(base + d->FirstThunk);
      for (; names->u1.AddressOfData; names++, iat++) {
         FARPROC target;
         if (IMAGE_SNAP_BY_ORDINAL(names->u1.Ordinal))
            target = GetProcAddress(repl, (LPCSTR)IMAGE_ORDINAL(names->u1.Ordinal));
         else
            target = GetProcAddress(repl, (LPCSTR)((const IMAGE_IMPORT_BY_NAME *)(base + names->u1.AddressOfData))->Name);
         if (target && *iat != target)
            wbh_patch(iat, target);
      }
   }
}

/* ---------- entry ------------------------------------------------------------------------- */

BOOL WINAPI
DllMain(HINSTANCE inst, DWORD reason, LPVOID reserved)
{
   (void)reserved;
   if (reason != DLL_PROCESS_ATTACH)
      return TRUE;
   DisableThreadLibraryCalls(inst);
   wbh_self = (HMODULE)inst;

   WCHAR exe[MAX_PATH], windir[MAX_PATH], self[MAX_PATH];
   DWORD exe_len = GetModuleFileNameW(NULL, exe, MAX_PATH);
   DWORD win_len = GetSystemWindowsDirectoryW(windir, MAX_PATH);
   DWORD self_len = GetModuleFileNameW(inst, self, MAX_PATH);
   if (!exe_len || exe_len >= MAX_PATH || !win_len || win_len >= MAX_PATH || !self_len || self_len >= MAX_PATH)
      return TRUE;

   /* Programs of Windows, whatever their name. */
   if (wbh_istarts(exe, exe_len, windir)) {
      wbh_log("skip: system program", exe);
      return TRUE;
   }
   size_t name_len;
   const WCHAR *name = wbh_base_name(exe, exe_len, &name_len);
   wbh_reg_get_value get = wbh_registry();
   if (wbh_denied(get, name)) {
      wbh_log("skip: deny list", name);
      return TRUE;
   }
   DWORD comps = wbh_components(get);

   /* A program with its own copies keeps them. */
   struct wbh_scan scan = { windir, win_len, 0 };
   wbh_each_module(wbh_scan_module, &scan);
   comps &= ~scan.own_files;
   if (!comps) {
      wbh_log("skip: nothing to add", name);
      return TRUE;
   }

   /* Our files: <dir of this hook>\x64 or x86\ . */
   size_t dir = self_len;
   while (dir && self[dir - 1] != L'\\')
      dir--;
   WCHAR path[MAX_PATH];
   for (size_t i = 0; i < dir; i++)
      path[i] = self[i];
#ifdef _WIN64
   static const WCHAR arch[] = L"x64\\";
#else
   static const WCHAR arch[] = L"x86\\";
#endif
   size_t at = dir;
   for (size_t i = 0; arch[i]; i++)
      path[at++] = arch[i];

   int loaded = 0;
   for (size_t i = 0; i < WBH_NFILES; i++) {
      if (!(comps & wbh_files[i].comp))
         continue;
      size_t n = 0;
      while (wbh_files[i].file[n] && at + n < MAX_PATH - 1) {
         path[at + n] = wbh_files[i].file[n];
         n++;
      }
      path[at + n] = 0;
      wbh_loaded[i] = LoadLibraryW(path);
      if (wbh_loaded[i])
         loaded++;
      else
         wbh_log("cannot load", path);
   }
   if (!loaded)
      return TRUE;

   wbh_each_module(wbh_rebind, NULL);
   wbh_log("hooked", name);
   return TRUE;
}
