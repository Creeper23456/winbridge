/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * Talks to the ivshmem driver the way the Venus driver does, from whatever bitness this is built
 * for, and shows what each request answers. Built for x86 it tells how the driver treats a
 * 32-bit caller. The ivshmem devices are found through their device interface; the one with the
 * size of a Venus slot (64 MiB) is asked for its mapping with output buffers of several sizes
 * (the driver's IVSHMEM_MMAP is 32 bytes in 64-bit code). A mapping that succeeds is released.
 * Usage: ivshmem-probe.exe
 */

#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <setupapi.h>
#include <winioctl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const GUID guid = { 0xdf576976, 0x569d, 0x4672, { 0x95, 0xa0, 0xf5, 0x7e, 0x4e, 0xa0, 0xb2, 0x10 } };

#define IOCTL(fn) CTL_CODE(FILE_DEVICE_UNKNOWN, fn, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_SIZE IOCTL(0x801)
#define IOCTL_MMAP IOCTL(0x802)
#define IOCTL_RELEASE IOCTL(0x803)

#define SLOT_LEN (64ull << 20)

static void
hexdump(const uint8_t *p, size_t n)
{
   for (size_t i = 0; i < n; i++)
      printf("%02x%s", p[i], (i & 7) == 7 ? " " : "");
   printf("\n");
}

int
main(void)
{
   printf("built for %zu-bit\n", sizeof(void *) * 8);
   HDEVINFO set = SetupDiGetClassDevsW(&guid, NULL, NULL, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
   if (set == INVALID_HANDLE_VALUE) {
      printf("RESULT: FAIL: SetupDiGetClassDevs error %lu\n", GetLastError());
      return 1;
   }
   int slots_mapped = 0;
   for (DWORD i = 0;; i++) {
      SP_DEVICE_INTERFACE_DATA did = { .cbSize = sizeof(did) };
      if (!SetupDiEnumDeviceInterfaces(set, NULL, &guid, i, &did))
         break;
      DWORD need = 0;
      SetupDiGetDeviceInterfaceDetailW(set, &did, NULL, 0, &need, NULL);
      SP_DEVICE_INTERFACE_DETAIL_DATA_W *det = need ? malloc(need) : NULL;
      if (!det)
         continue;
      det->cbSize = sizeof(*det);
      if (!SetupDiGetDeviceInterfaceDetailW(set, &did, det, need, NULL, NULL)) {
         printf("device %lu: detail error %lu (cbSize %lu, need %lu)\n", i, GetLastError(),
                (unsigned long)det->cbSize, need);
         free(det);
         continue;
      }
      printf("device %lu: %ls\n", i, det->DevicePath);
      HANDLE h = CreateFileW(det->DevicePath, GENERIC_READ | GENERIC_WRITE,
                             FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING,
                             FILE_ATTRIBUTE_NORMAL, NULL);
      if (h == INVALID_HANDLE_VALUE) {
         printf("  CreateFile error %lu\n", GetLastError());
         free(det);
         continue;
      }
      uint64_t size = 0;
      DWORD got = 0;
      if (!DeviceIoControl(h, IOCTL_SIZE, NULL, 0, &size, sizeof(size), &got, NULL)) {
         printf("  IOCTL_SIZE error %lu\n", GetLastError());
         CloseHandle(h);
         free(det);
         continue;
      }
      printf("  size %llu MiB\n", (unsigned long long)(size >> 20));
      if (size == SLOT_LEN) {
         static const DWORD lengths[] = { 16, 24, 28, 32, 40, 64 };
         for (size_t k = 0; k < sizeof(lengths) / sizeof(lengths[0]) && !slots_mapped; k++) {
            uint8_t out[64];
            memset(out, 0xcc, sizeof(out));
            uint8_t mode = 1;
            got = 0;
            const BOOL ok = DeviceIoControl(h, IOCTL_MMAP, &mode, sizeof(mode), out, lengths[k], &got, NULL);
            printf("  MMAP with a %lu-byte output buffer: %s, error %lu, %lu bytes returned\n", lengths[k],
                   ok ? "ok" : "failed", ok ? 0ul : GetLastError(), got);
            if (ok) {
               printf("  answer: ");
               hexdump(out, got ? got : lengths[k]);
               DeviceIoControl(h, IOCTL_RELEASE, NULL, 0, NULL, 0, &got, NULL);
               slots_mapped++;
            }
         }
      }
      CloseHandle(h);
      free(det);
   }
   SetupDiDestroyDeviceInfoList(set);
   printf("RESULT: %s\n", slots_mapped ? "PASS" : "FAIL");
   return slots_mapped ? 0 : 1;
}
