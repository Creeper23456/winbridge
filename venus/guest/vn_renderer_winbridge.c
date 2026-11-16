/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * WinBridge `vn_renderer` backend for the Mesa Venus guest ICD on Windows.
 *
 * Replaces the POSIX-only virtgpu and vtest backends. The renderer is a host process that
 * owns virglrenderer's Venus context; this file talks to it over the WBVN channel
 * (`wbvn_proto.h`). The channel is the two lanes of an ivshmem region that this process maps
 * for itself (a "slot", `wbvn_ring.h`); a virtio-serial port carries a wake-up byte when the
 * peer sleeps. The guest cannot map the memory the renderer uses, so every venus shmem is a
 * local allocation that is mirrored in both directions (see the protocol header for the
 * ordering rules). Timeline syncs live in the guest; the host only reports the points that
 * its fences retired.
 *
 * Host-visible device memory is a bo whose host copy is a mappable blob; it is mirrored by
 * the same scan-and-push scheme and brought up to date when a fence retires.
 */

#include <windows.h>
#include <winioctl.h>
#include <setupapi.h>
#include <stdio.h>

#include "vn_renderer.h"
#include "virtio/virtio-gpu/venus_hw.h"
#include "wbvn_proto.h"
#include "wbvn_ring.h"

#pragma comment(lib, "setupapi.lib")

/* The serial port of slot N; it carries wake-up bytes only. */
#define WBVN_PORT_FORMAT L"\\\\.\\Global\\org.winbridge.venus.%u"
/* How long a claim waits for the host to free or set up a slot (WBVN_CONNECT_MS overrides). */
#define WBVN_CONNECT_MS 5000
/* How long the reader polls for the host before it sleeps (WBVN_SPIN_US overrides). */
#define WBVN_SPIN_US 150
/* A sleeping reader wakes at least this often, in case a wake-up byte was lost. */
#define WBVN_KICK_TIMEOUT_MS 20
/* A message goes into the ring in records of at most this size. */
#define WBVN_RECORD_MAX (1u << 20)
/* A mirror message for non-ring shmems is cut at about this size. */
#define WBVN_PUSH_CHUNK (4u << 20)
/* How long a request waits for the host's answer. */
#define WBVN_REPLY_TIMEOUT_MS 30000
/* Runs closer than this many words are sent as one. */
#define WBVN_RUN_GAP_WORDS 8

#define WBVN_PAGE 4096u

/* Offset of the status word in a ring shmem (`struct layout` in `vn_ring_get_layout`). */
#define WBVN_RING_STATUS 128u
/* Offset of the tail word. The guest is its only writer. */
#define WBVN_RING_TAIL 64u

/* A table of pointers indexed by a small nonzero id. */
struct wbvn_tab {
   void **v;
   uint32_t cap;
   uint32_t next;
};

/* Memory that is mirrored with the host: a venus shmem or a device memory bo. */
struct wbvn_region {
   uint8_t *ptr; /* the guest copy, write tracked */
   size_t size; /* a multiple of the page size */
   uint64_t *snap; /* what the host last saw (or sent us) */
   void **dirty; /* scratch for GetWriteWatch, size / page entries */
   uint32_t res_id;
   bool role_sent;
   /* the shmem's `mirror_ring_id`, which Mesa sets after creation; NULL for a bo */
   const uint64_t *ring_src;
};

static inline uint64_t
region_ring_id(const struct wbvn_region *r)
{
   return r->ring_src ? *r->ring_src : 0;
}

struct wbvn_shmem {
   struct vn_renderer_shmem base;
   struct wbvn_region r;
};

struct wbvn_bo {
   struct vn_renderer_bo base;
   struct wbvn_region r;
};

struct wbvn_sync {
   struct vn_renderer_sync base;
   uint64_t value;
};

struct wbvn_buf {
   uint8_t *data;
   size_t len;
   size_t cap;
};

struct wbvn {
   struct vn_renderer base;
   struct vn_instance *instance;

   /* the serial port of the slot; it carries wake-up bytes only */
   HANDLE port;
   HANDLE read_event;
   HANDLE write_event;
   HANDLE reader;
   HANDLE pump;
   volatile LONG quit;
   volatile LONG lost;

   /* the ivshmem device this process owns; the driver maps a device for one process only */
   HANDLE bar_dev;
   uint8_t *bar;
   uint32_t slot_id;
   uint64_t nonce;
   struct wbvn_ring tx; /* guest to host */
   struct wbvn_ring rx; /* host to guest */
   int64_t spin_ticks;  /* QueryPerformanceCounter ticks the reader polls for */
   /* the reader has a one-byte read pending on the port, to be woken by a wake-up byte */
   bool kick_armed;
   uint8_t kick_byte;
   OVERLAPPED kick_ov;
   uint64_t beat_at; /* GetTickCount64 of the last heartbeat */
   /* counts the host updates (pushes and signals) applied so far; guarded by mu */
   uint64_t push_gen;

   SRWLOCK write_lock;
   /* serialises mirror scans with the messages that carry them */
   SRWLOCK push_lock;
   /* guards the tables, sync values and the reply slot */
   SRWLOCK mu;
   CONDITION_VARIABLE cv;

   struct wbvn_tab regions;
   struct wbvn_tab syncs;

   /* one outstanding request at a time */
   SRWLOCK req_lock;
   uint32_t req_seq;
   struct {
      bool done;
      uint32_t req;
      uint32_t arg;
      uint8_t *payload;
      uint32_t len;
   } reply;

   struct wbvn_buf scratch;
   /* threads waiting for push_lock or mu; the pump stands aside while there are any */
   volatile LONG contenders;
   /* frames a flush has built and not written yet; guarded by push_lock */
   struct wbvn_buf out;
   uint32_t max_timeline_count;
   struct virgl_renderer_capset_venus capset;
};

/* ---------- small helpers ------------------------------------------------------------ */

static uint32_t
tab_add(struct wbvn_tab *t, void *p)
{
   for (uint32_t pass = 0; pass < 2; pass++) {
      for (uint32_t n = 0; n < t->cap; n++) {
         uint32_t id = 1 + (t->next + n) % (t->cap ? t->cap : 1);
         if (id < t->cap && !t->v[id]) {
            t->v[id] = p;
            t->next = id;
            return id;
         }
      }
      uint32_t cap = t->cap ? t->cap * 2 : 64;
      void **v = realloc(t->v, cap * sizeof(*v));
      if (!v)
         return 0;
      memset(v + t->cap, 0, (cap - t->cap) * sizeof(*v));
      t->v = v;
      t->cap = cap;
   }
   return 0;
}

static void *
tab_get(const struct wbvn_tab *t, uint32_t id)
{
   return id && id < t->cap ? t->v[id] : NULL;
}

static void
tab_del(struct wbvn_tab *t, uint32_t id)
{
   if (id && id < t->cap)
      t->v[id] = NULL;
}

static bool
buf_reserve(struct wbvn_buf *b, size_t extra)
{
   if (b->len + extra <= b->cap)
      return true;
   size_t cap = b->cap ? b->cap : 4096;
   while (cap < b->len + extra)
      cap *= 2;
   uint8_t *d = realloc(b->data, cap);
   if (!d)
      return false;
   b->data = d;
   b->cap = cap;
   return true;
}

static bool
buf_put(struct wbvn_buf *b, const void *p, size_t n)
{
   if (!buf_reserve(b, n))
      return false;
   memcpy(b->data + b->len, p, n);
   b->len += n;
   return true;
}

static inline uint64_t
load64(const void *p)
{
   return __atomic_load_n((const uint64_t *)p, __ATOMIC_RELAXED);
}

static inline void
store64(void *p, uint64_t v)
{
   __atomic_store_n((uint64_t *)p, v, __ATOMIC_RELAXED);
}

/* ---------- statistics --------------------------------------------------------------- */

/* With WBVN_STATS=<file> the backend appends a line of counter deltas to the file once a second
 * and a total when the renderer is destroyed, and measures the channel's round trip when it
 * starts. When the variable is unset every counter costs one branch. Names ending in _ns print
 * as milliseconds; names ending in _max_ns are maxima over the interval. The counters before
 * flush_n measure work the application asked for; the rest is the pump's background activity,
 * which a line only reports together with the first kind. */
#define WBVN_STAT_LIST(X)                                                                      \
   X(submit_n) X(submit_ns) X(submit_bytes)                                                    \
   X(wait_n) X(wait_ns) X(wait_max_ns)                                                         \
   X(wait_h0) X(wait_h1) X(wait_h2) X(wait_h3) X(wait_h4)                                      \
   X(req_n) X(req_ns)                                                                          \
   X(relax_ring_n) X(relax_ring_iters) X(relax_ring_ns)                                        \
   X(relax_tls_n) X(relax_tls_iters) X(relax_tls_ns)                                           \
   X(relax_space_n) X(relax_space_iters) X(relax_space_ns)                                     \
   X(relax_sem_n) X(relax_sem_iters) X(relax_sem_ns)                                           \
   X(relax_query_n) X(relax_query_iters) X(relax_query_ns)                                     \
   X(rd_msgs) X(rd_bytes) X(rdpush_n) X(rdpush_bytes) X(rdpush_ns) X(rdsignal_n)               \
   X(push_msgs) X(push_bytes) X(push_ranges)                                                   \
   X(send_n) X(send_ns) X(send_bytes) X(send_max_ns)                                           \
   X(lock_n) X(lock_ns) X(lock_max_ns)                                                         \
   X(flush_n) X(flush_ns) X(flush_sent_n)                                                      \
   X(pump_loops) X(pump_sleeps) X(pump_sleep_ns)                                               \
   X(kick_tx_n) X(kick_rx_n) X(rd_sleep_n) X(rd_sleep_ns) X(ringfull_n)

enum wbvn_stat {
#define X(name) S_##name,
   WBVN_STAT_LIST(X)
#undef X
   S_COUNT
};

static const char *const stat_names[S_COUNT] = {
#define X(name) #name,
   WBVN_STAT_LIST(X)
#undef X
};

/* The renderer of this process, for DllMain: when the process ends without destroying its
 * instance, the slot is closed at once instead of when the host notices a stopped heartbeat. */
struct wbvn;
static struct wbvn *volatile g_renderer;

static bool g_stats_on;
static FILE *g_stats_file;
static int64_t g_qpc_freq;
static int64_t g_stats_t0, g_stats_last;
static volatile LONG64 g_stat[S_COUNT];
static int64_t g_stat_prev[S_COUNT];
static int64_t g_stat_peak[S_COUNT];

/* Nanoseconds on a monotonic clock; 0 when statistics are off, so callers test the result. */
static inline int64_t
stat_now(void)
{
   if (!g_stats_on)
      return 0;
   LARGE_INTEGER c;
   QueryPerformanceCounter(&c);
   return (c.QuadPart / g_qpc_freq) * 1000000000 + (c.QuadPart % g_qpc_freq) * 1000000000 / g_qpc_freq;
}

static inline void
stat_add(enum wbvn_stat s, int64_t v)
{
   if (g_stats_on)
      InterlockedExchangeAdd64(&g_stat[s], v);
}

static inline void
stat_max(enum wbvn_stat s, int64_t v)
{
   if (!g_stats_on)
      return;
   LONG64 cur = g_stat[s];
   while (cur < v) {
      const LONG64 seen = InterlockedCompareExchange64(&g_stat[s], v, cur);
      if (seen == cur)
         break;
      cur = seen;
   }
}

static bool
stat_is_max(enum wbvn_stat s)
{
   const size_t n = strlen(stat_names[s]);
   return n > 7 && !strcmp(stat_names[s] + n - 7, "_max_ns");
}

static void
stats_init(void)
{
   char path[MAX_PATH];
   if (g_stats_on || !GetEnvironmentVariableA("WBVN_STATS", path, sizeof(path)))
      return;
   LARGE_INTEGER f;
   QueryPerformanceFrequency(&f);
   g_qpc_freq = f.QuadPart;
   g_stats_file = fopen(path, "a");
   if (!g_stats_file)
      return;
   g_stats_on = true;
   g_stats_t0 = g_stats_last = stat_now();
}

/* Writes the counters: once a second the change since the last line, at the end the totals.
 * Only the pump thread calls it while the pump runs, and the destroy path after it ended. */
static void
stats_tick(bool final)
{
   if (!g_stats_on)
      return;
   const int64_t now = stat_now();
   if (!final && now - g_stats_last < 1000000000)
      return;
   g_stats_last = now;

   char line[4096];
   int n = snprintf(line, sizeof(line), "%s t=%.1fs", final ? "TOTAL" : "interval",
                    (double)(now - g_stats_t0) / 1e9);
   bool active = final;
   for (int i = 0; i < S_COUNT; i++) {
      int64_t v;
      if (stat_is_max(i)) {
         v = InterlockedExchange64(&g_stat[i], 0);
         if (v > g_stat_peak[i])
            g_stat_peak[i] = v;
         if (final)
            v = g_stat_peak[i];
      } else {
         const int64_t total = g_stat[i];
         v = final ? total : total - g_stat_prev[i];
         g_stat_prev[i] = total;
      }
      if (!v)
         continue;
      if (i < S_flush_n && !stat_is_max(i))
         active = true;
      const char *name = stat_names[i];
      const size_t len = strlen(name);
      if (len > 3 && !strcmp(name + len - 3, "_ns"))
         n += snprintf(line + n, sizeof(line) - n, " %.*s_ms=%.2f", (int)(len - 3), name,
                       (double)v / 1e6);
      else
         n += snprintf(line + n, sizeof(line) - n, " %s=%lld", name, (long long)v);
      if (n >= (int)sizeof(line) - 64)
         break;
   }
   if (active) {
      fprintf(g_stats_file, "%s\n", line);
      fflush(g_stats_file);
   }
}

/* Called by Mesa when a wait in vn_ring.c / vn_query_pool.c / vn_sync.c ends (see the
 * `vn_relax` patch): `iters` polls, the first 255 only yield, the rest sleep. */
void
wbvn_stats_relax(unsigned reason, uint32_t iters, int64_t ns)
{
   if (!g_stats_on || reason > 4)
      return;
   const enum wbvn_stat base = S_relax_ring_n + reason * 3;
   stat_add(base, 1);
   stat_add(base + 1, iters);
   stat_add(base + 2, ns);
}

/* The pump holds push_lock and mu for most of every iteration (flush_ns was 86% of the time in the
 * first trace, and a submit took 3 ms on average). A thread that wants either may keep losing the
 * race, because an SRW lock does not necessarily hand over to a waiter that has not run yet when
 * the holder takes it again at once [INFERENCE; the A/B run is the evidence]. Threads announce
 * themselves here and the pump yields until they hold the lock. lock_n counts only the waits
 * that took 20 us or more. */
static void
lock_excl(struct wbvn *w, SRWLOCK *l)
{
   InterlockedIncrement(&w->contenders);
   const int64_t t0 = stat_now();
   AcquireSRWLockExclusive(l);
   InterlockedDecrement(&w->contenders);
   if (t0) {
      const int64_t d = stat_now() - t0;
      stat_add(S_lock_ns, d);
      if (d >= 20000) {
         stat_add(S_lock_n, 1);
         stat_max(S_lock_max_ns, d);
      }
   }
}

static void
lock_shared(struct wbvn *w, SRWLOCK *l)
{
   InterlockedIncrement(&w->contenders);
   AcquireSRWLockShared(l);
   InterlockedDecrement(&w->contenders);
}

/* ---------- the slot ----------------------------------------------------------------- */

/* GUID_DEVINTERFACE_IVSHMEM and the IOCTLs of the ivshmem driver (its Public.h) */
static const GUID wbvn_ivshmem_guid = { 0xdf576976, 0x569d, 0x4672,
                                        { 0x95, 0xa0, 0xf5, 0x7e, 0x4e, 0xa0, 0xb2, 0x10 } };
#define WBVN_IOCTL(fn) CTL_CODE(FILE_DEVICE_UNKNOWN, fn, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define WBVN_IOCTL_SIZE WBVN_IOCTL(0x801)
#define WBVN_IOCTL_MMAP WBVN_IOCTL(0x802)
#define WBVN_IOCTL_RELEASE WBVN_IOCTL(0x803)
#define WBVN_CACHE_CACHED 1

/* IVSHMEM_MMAP as the driver fills it. The driver checks the output buffer length against the size
 * of its own struct for the caller's bitness: 32 bytes for 64-bit code, 24 for a 32-bit process
 * (measured with tests/ivshmem-probe.c), which is what a native struct gives in both. */
struct wbvn_ivshmem_mmap {
   uint16_t peer_id;
   uint64_t size;
   void *ptr;
   uint16_t vectors;
};
_Static_assert(sizeof(struct wbvn_ivshmem_mmap) == (sizeof(void *) == 8 ? 32 : 24),
               "IVSHMEM_MMAP has the size the driver expects");

#define SLOT_LD32(bar, off) \
   __atomic_load_n((const uint32_t *)((bar) + WBVN_SLOT_BLOCK + (off)), __ATOMIC_ACQUIRE)
#define SLOT_ST32(bar, off, v) \
   __atomic_store_n((uint32_t *)((bar) + WBVN_SLOT_BLOCK + (off)), (v), __ATOMIC_RELEASE)
#define SLOT_LD64(bar, off) \
   __atomic_load_n((const uint64_t *)((bar) + WBVN_SLOT_BLOCK + (off)), __ATOMIC_ACQUIRE)
#define SLOT_ST64(bar, off, v) \
   __atomic_store_n((uint64_t *)((bar) + WBVN_SLOT_BLOCK + (off)), (v), __ATOMIC_RELEASE)

static inline int64_t
qpc_ticks(void)
{
   LARGE_INTEGER c;
   QueryPerformanceCounter(&c);
   return c.QuadPart;
}

static DWORD
env_uint(const char *name, DWORD fallback)
{
   char v[32];
   const DWORD n = GetEnvironmentVariableA(name, v, sizeof(v));
   return n && n < sizeof(v) ? (DWORD)strtoul(v, NULL, 10) : fallback;
}

static void
wbvn_unmap(struct wbvn *w)
{
   DWORD got;
   if (w->bar)
      DeviceIoControl(w->bar_dev, WBVN_IOCTL_RELEASE, NULL, 0, NULL, 0, &got, NULL);
   if (w->bar_dev)
      CloseHandle(w->bar_dev);
   w->bar = NULL;
   w->bar_dev = NULL;
}

/* Checks the region the host laid out and finds its two lanes. Returns NULL, or what is wrong. */
static const char *
slot_check(const uint8_t *bar, uint64_t len, uint64_t lane[2][2])
{
   if (len < WBVN_SLOT_LEN)
      return "the device is smaller than a slot";
   const uint64_t magic = __atomic_load_n((const uint64_t *)bar, __ATOMIC_ACQUIRE);
   if (!magic)
      return "the host has not set the slot up";
   if (magic != WBVN_SHM_MAGIC || *(const uint32_t *)(bar + 8) != WBVN_SHM_VERSION ||
       *(const uint32_t *)(bar + 0xC) != WBVN_SHM_HEADER_LEN)
      return "the region is not a WinBridge region";
   const uint64_t declared = *(const uint64_t *)(bar + 0x18);
   if (declared < WBVN_SHM_HEADER_LEN || declared > len)
      return "the region length is wrong";
   if (*(const uint32_t *)(bar + 0x24) != WBVN_SHM_ROLE_CLIENT ||
       !(*(const uint64_t *)(bar + 0x10) & WBVN_SHM_CAP_VENUS))
      return "the region is not a Venus slot";
   const uint32_t count = *(const uint32_t *)(bar + 0x28);
   if (count > 64)
      return "the region has too many lanes";
   bool g2h = false, h2g = false;
   for (uint32_t i = 0; i < count; i++) {
      const uint8_t *e = bar + WBVN_SHM_DIR + i * WBVN_SHM_DIR_ENTRY;
      const uint16_t id = *(const uint16_t *)e;
      const uint8_t dir = e[2];
      const uint64_t off = *(const uint64_t *)(e + 8), size = *(const uint64_t *)(e + 16);
      if (off < WBVN_SHM_HEADER_LEN || (off & 63) || size < 64 || (size & (size - 1)) ||
          off > declared || size > declared - off - WBVN_RING_HEADER)
         return "a lane lies outside the region";
      if (id == WBVN_LANE_G2H && dir == 1) {
         lane[0][0] = off;
         lane[0][1] = size;
         g2h = true;
      } else if (id == WBVN_LANE_H2G && dir == 0) {
         lane[1][0] = off;
         lane[1][1] = size;
         h2g = true;
      }
   }
   if (!g2h || !h2g)
      return "the region lacks the Venus lanes";
   if (SLOT_LD64(bar, WBVN_SLOT_OFF_MAGIC) != WBVN_SLOT_MAGIC ||
       SLOT_LD32(bar, WBVN_SLOT_OFF_VERSION) != WBVN_SLOT_VERSION)
      return "the slot block is missing";
   return NULL;
}

/* One attempt to claim the slot behind a device interface. Returns NULL on success, else why not;
 * `*retry` says whether waiting may help (the host is not ready, or has not freed the slot). */
static const char *
wbvn_try_device(struct wbvn *w, const wchar_t *path, bool *sized, bool *retry)
{
   DWORD got;
   const HANDLE h = CreateFileW(path, GENERIC_READ | GENERIC_WRITE,
                                FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING,
                                FILE_ATTRIBUTE_NORMAL, NULL);
   if (h == INVALID_HANDLE_VALUE) {
      vn_log(w->instance, "cannot open %ls (error %lu)", path, GetLastError());
      return "cannot open an ivshmem device";
   }
   uint64_t size = 0;
   if (!DeviceIoControl(h, WBVN_IOCTL_SIZE, NULL, 0, &size, sizeof(size), &got, NULL) ||
       size != WBVN_SLOT_LEN) {
      vn_log(w->instance, "ivshmem size query of %ls: error %lu, size %llu", path,
             GetLastError(), (unsigned long long)size);
      CloseHandle(h);
      return "no ivshmem device has the size of a Venus slot";
   }
   *sized = true;
   struct wbvn_ivshmem_mmap m = { 0 };
   uint8_t mode = WBVN_CACHE_CACHED;
   if (!DeviceIoControl(h, WBVN_IOCTL_MMAP, &mode, sizeof(mode), &m, sizeof(m), &got, NULL) ||
       !m.ptr) {
      vn_log(w->instance, "ivshmem mapping of %ls: error %lu, ptr %p", path, GetLastError(), m.ptr);
      CloseHandle(h);
      return "every Venus slot is in use by another process";
   }
   w->bar_dev = h;
   w->bar = (uint8_t *)m.ptr;

   uint64_t lane[2][2];
   const char *why = slot_check(w->bar, m.size, lane);
   if (why) {
      *retry = true;
      goto fail;
   }
   /* The driver maps a device for one process, and this process holds the mapping: the previous
    * owner of a claimed slot is gone. Closing the slot makes the host reset it. */
   if (SLOT_LD32(w->bar, WBVN_SLOT_OFF_STATE) == WBVN_SLOT_CLAIMED)
      SLOT_ST32(w->bar, WBVN_SLOT_OFF_STATE, WBVN_SLOT_CLOSED);
   uint32_t expect = WBVN_SLOT_FREE;
   if (!__atomic_compare_exchange_n((uint32_t *)(w->bar + WBVN_SLOT_BLOCK + WBVN_SLOT_OFF_STATE),
                                    &expect, WBVN_SLOT_CLAIMED, false, __ATOMIC_ACQ_REL,
                                    __ATOMIC_ACQUIRE)) {
      why = "the host has not freed the slot";
      *retry = true;
      goto fail;
   }
   w->slot_id = SLOT_LD32(w->bar, WBVN_SLOT_OFF_ID);
   w->nonce = ((uint64_t)GetCurrentProcessId() << 32) ^ (uint64_t)qpc_ticks();
   SLOT_ST64(w->bar, WBVN_SLOT_OFF_NONCE, w->nonce);
   if (!wbvn_ring_attach(&w->tx, w->bar + lane[0][0], lane[0][1], true) ||
       !wbvn_ring_attach(&w->rx, w->bar + lane[1][0], lane[1][1], false)) {
      SLOT_ST32(w->bar, WBVN_SLOT_OFF_STATE, WBVN_SLOT_CLOSED);
      why = "a lane is not empty";
      goto fail;
   }
   return NULL;

fail:
   wbvn_unmap(w);
   return why;
}

/* Finds a free slot among the ivshmem devices and claims it. */
static bool
wbvn_attach(struct wbvn *w)
{
   const HDEVINFO set = SetupDiGetClassDevsW(&wbvn_ivshmem_guid, NULL, NULL,
                                             DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
   if (set == INVALID_HANDLE_VALUE) {
      vn_log(w->instance, "cannot list the ivshmem devices (error %lu)", GetLastError());
      return false;
   }
   const char *why = "no ivshmem device is present";
   const DWORD64 deadline = GetTickCount64() + env_uint("WBVN_CONNECT_MS", WBVN_CONNECT_MS);
   bool done = false;
   for (;;) {
      bool sized = false, retry = false;
      for (DWORD i = 0; !done; i++) {
         SP_DEVICE_INTERFACE_DATA did = { .cbSize = sizeof(did) };
         if (!SetupDiEnumDeviceInterfaces(set, NULL, &wbvn_ivshmem_guid, i, &did))
            break;
         DWORD need = 0;
         SetupDiGetDeviceInterfaceDetailW(set, &did, NULL, 0, &need, NULL);
         SP_DEVICE_INTERFACE_DETAIL_DATA_W *det = need ? malloc(need) : NULL;
         if (!det)
            continue;
         det->cbSize = sizeof(*det);
         if (SetupDiGetDeviceInterfaceDetailW(set, &did, det, need, NULL, NULL)) {
            const char *r = wbvn_try_device(w, det->DevicePath, &sized, &retry);
            if (!r)
               done = true;
            else
               why = r;
         }
         free(det);
      }
      if (done || !sized || !retry || GetTickCount64() >= deadline)
         break;
      Sleep(10);
   }
   SetupDiDestroyDeviceInfoList(set);
   if (!done)
      vn_log(w->instance, "cannot claim a Venus slot: %s", why);
   return done;
}

/* ---------- wake-ups and the heartbeat ------------------------------------------------ */

/* Sends the host a wake-up byte. The caller holds write_lock. */
static void
wbvn_kick(struct wbvn *w)
{
   const uint8_t b = 1;
   DWORD got = 0;
   OVERLAPPED ov = { .hEvent = w->write_event };
   if (!WriteFile(w->port, &b, 1, &got, &ov)) {
      if (GetLastError() != ERROR_IO_PENDING || !GetOverlappedResult(w->port, &ov, &got, TRUE))
         return;
   }
   stat_add(S_kick_tx_n, 1);
}

static void
wbvn_set_lost(struct wbvn *w)
{
   InterlockedExchange(&w->lost, 1);
   lock_excl(w, &w->mu);
   WakeAllConditionVariable(&w->cv);
   ReleaseSRWLockExclusive(&w->mu);
}

/* Appends one message, header and payload, to `b`. */
static bool
frame_put(struct wbvn_buf *b, uint32_t kind, uint32_t req, uint32_t arg, const void *payload,
          size_t len)
{
   const struct wbvn_hdr h = { .len = (uint32_t)len, .kind = kind, .req = req, .arg = arg };
   const size_t keep = b->len;
   if (buf_put(b, &h, sizeof(h)) && (!len || buf_put(b, payload, len)))
      return true;
   b->len = keep;
   return false;
}

/* Writes whole messages into the guest to host lane, in records of WBVN_RECORD_MAX at most.
 * The caller sends header and payload of a message, and the messages one flush produces, in one
 * call, so the host finds them together. */
static bool
wire_write(struct wbvn *w, const void *data, size_t len)
{
   bool ok = true;
   const int64_t t0 = stat_now();
   const uint8_t *p = data;
   uint32_t spins = 0;
   DWORD64 stalled = 0;
   AcquireSRWLockExclusive(&w->write_lock);
   while (len && ok) {
      if (w->lost) {
         ok = false;
         break;
      }
      const uint32_t n = len > WBVN_RECORD_MAX ? WBVN_RECORD_MAX : (uint32_t)len;
      bool kick;
      const int r = wbvn_ring_push(&w->tx, p, n, &kick);
      if (r == WBVN_RING_OK) {
         if (kick)
            wbvn_kick(w);
         p += n;
         len -= n;
         spins = 0;
         stalled = 0;
      } else if (r == WBVN_RING_CLOSED) {
         ok = false;
      } else {
         /* the host has not drained the lane yet */
         if (!stalled) {
            stalled = GetTickCount64();
            stat_add(S_ringfull_n, 1);
         } else if (GetTickCount64() - stalled > WBVN_REPLY_TIMEOUT_MS) {
            ok = false;
         }
         if (++spins < 2000)
            YieldProcessor();
         else
            SwitchToThread();
      }
   }
   ReleaseSRWLockExclusive(&w->write_lock);
   if (t0) {
      const int64_t d = stat_now() - t0;
      stat_add(S_send_n, 1);
      stat_add(S_send_ns, d);
      stat_add(S_send_bytes, (int64_t)(p - (const uint8_t *)data));
      stat_max(S_send_max_ns, d);
   }
   if (!ok)
      wbvn_set_lost(w);
   return ok;
}

static bool
wbvn_send(struct wbvn *w, uint32_t kind, uint32_t req, uint32_t arg,
          const void *payload, size_t len)
{
   struct wbvn_buf b = { 0 };
   const bool ok = frame_put(&b, kind, req, arg, payload, len) && wire_write(w, b.data, b.len);
   free(b.data);
   return ok;
}

/* Writes the frames a flush built. The caller holds push_lock. */
static bool
wbvn_out_write(struct wbvn *w)
{
   const bool ok = !w->out.len || wire_write(w, w->out.data, w->out.len);
   w->out.len = 0;
   return ok;
}

/* Sends a request and waits for its reply. `out`, if given, receives the reply payload
 * (malloc'd, caller frees). Returns the reply arg, or -1 on a lost channel. */
static int64_t
wbvn_request(struct wbvn *w, uint32_t kind, uint32_t arg, const void *payload,
             size_t len, uint8_t **out, uint32_t *out_len)
{
   int64_t result = -1;
   const int64_t t0 = stat_now();
   AcquireSRWLockExclusive(&w->req_lock);
   lock_excl(w, &w->mu);
   uint32_t req = ++w->req_seq;
   w->reply.done = false;
   w->reply.req = req;
   ReleaseSRWLockExclusive(&w->mu);

   if (wbvn_send(w, kind, req, arg, payload, len)) {
      lock_excl(w, &w->mu);
      DWORD64 start = GetTickCount64();
      while (!w->reply.done && !w->lost) {
         DWORD64 spent = GetTickCount64() - start;
         if (spent >= WBVN_REPLY_TIMEOUT_MS)
            break;
         SleepConditionVariableSRW(&w->cv, &w->mu,
                                   (DWORD)(WBVN_REPLY_TIMEOUT_MS - spent), 0);
      }
      if (w->reply.done) {
         result = w->reply.arg;
         if (out) {
            *out = w->reply.payload;
            *out_len = w->reply.len;
         } else {
            free(w->reply.payload);
         }
         w->reply.payload = NULL;
      }
      ReleaseSRWLockExclusive(&w->mu);
   }
   ReleaseSRWLockExclusive(&w->req_lock);
   if (t0) {
      stat_add(S_req_n, 1);
      stat_add(S_req_ns, stat_now() - t0);
   }
   return result;
}

/* Measures the round trip of the channel with nothing else running: the host echoes the
 * payload, so the large size also gives the throughput of a write and a read in series. */
static void
wbvn_stats_ping(struct wbvn *w)
{
   if (!g_stats_on)
      return;
   static const uint32_t sizes[] = { 16, 4096, 65536, 1048576 };
   uint8_t *buf = calloc(1, sizes[3]);
   if (!buf)
      return;
   for (unsigned s = 0; s < ARRAY_SIZE(sizes); s++) {
      const int rounds = sizes[s] >= 65536 ? 16 : 64;
      int64_t min = INT64_MAX, total = 0;
      int done = 0;
      for (; done < rounds; done++) {
         const int64_t t0 = stat_now();
         if (wbvn_request(w, WBVN_PING, 0, buf, sizes[s], NULL, NULL) != 0)
            break;
         const int64_t d = stat_now() - t0;
         total += d;
         if (d < min)
            min = d;
      }
      if (done)
         fprintf(g_stats_file, "ping %u B: min %.0f us avg %.0f us over %d\n", sizes[s],
                 (double)min / 1e3, (double)total / done / 1e3, done);
   }
   int64_t smin = INT64_MAX, stotal = 0;
   for (int i = 0; i < 32; i++) {
      const int64_t t0 = stat_now();
      Sleep(1);
      const int64_t d = stat_now() - t0;
      stotal += d;
      if (d < smin)
         smin = d;
   }
   fprintf(g_stats_file, "Sleep(1): min %.0f us avg %.0f us over 32\n", (double)smin / 1e3,
           (double)stotal / 32 / 1e3);
   fflush(g_stats_file);
   free(buf);
}

/* ---------- mirror ------------------------------------------------------------------- */

/* Appends one range to a push message body. */
static bool
push_range(struct wbvn_buf *b, uint32_t res_id, uint32_t off, const uint64_t *words,
           uint32_t nwords)
{
   struct wbvn_push_range r = {
      .res_id = res_id, .offset = off, .len = nwords * 8u,
   };
   return buf_put(b, &r, sizeof(r)) && buf_put(b, words, nwords * 8u);
}

/* Collects the words the guest changed in `s` since the last scan. Returns the number of
 * ranges appended to `b`, or -1 if memory ran out. */
static int
scan_region(struct wbvn_region *s, struct wbvn_buf *b, uint32_t *status_clear)
{
   const bool ring = region_ring_id(s) != 0;
   /* The tail is read before the write watch. Every command it covers was stored before the
    * tail, so its page is flagged by the time the watch is read below, and the host never gets a
    * tail whose commands are missing. The host applies a push's ranges last to first, so the
    * tail goes first in the message. */
   const uint64_t tail = ring ? load64(s->ptr + WBVN_RING_TAIL) : 0;
   ULONG_PTR count = s->size / WBVN_PAGE;
   DWORD gran = 0;
   if (GetWriteWatch(WRITE_WATCH_FLAG_RESET, s->ptr, s->size, s->dirty,
                     &count, &gran) != 0)
      return -1;

   int ranges = 0;
   if (ring && tail != s->snap[WBVN_RING_TAIL / 8]) {
      if (!push_range(b, s->res_id, WBVN_RING_TAIL, &tail, 1))
         return -1;
      s->snap[WBVN_RING_TAIL / 8] = tail;
      ranges++;
   }

   uint64_t page[WBVN_PAGE / 8];
   for (ULONG_PTR i = 0; i < count; i++) {
      const uint8_t *src = s->dirty[i];
      uint32_t base_off = (uint32_t)(src - s->ptr);
      uint64_t *snap = s->snap + base_off / 8;
      for (uint32_t k = 0; k < WBVN_PAGE / 8; k++)
         page[k] = load64(src + k * 8);
      /* only the tail value read above goes out; a newer one waits for the next scan */
      if (ring && base_off == 0)
         page[WBVN_RING_TAIL / 8] = snap[WBVN_RING_TAIL / 8];

      if (region_ring_id(s) && base_off == 0) {
         /* The status word has two writers. The host sets and clears IDLE and FATAL and the
          * ALIVE bit; the guest only ever clears ALIVE. Do not mirror the word, tell the host
          * which bits the guest took away. */
         const uint32_t was = (uint32_t)snap[WBVN_RING_STATUS / 8];
         const uint32_t now = (uint32_t)page[WBVN_RING_STATUS / 8];
         *status_clear |= was & ~now;
         snap[WBVN_RING_STATUS / 8] = page[WBVN_RING_STATUS / 8];
      }

      uint32_t k = 0;
      while (k < WBVN_PAGE / 8) {
         if (page[k] == snap[k]) {
            k++;
            continue;
         }
         uint32_t start = k, end = k + 1, gap = 0;
         for (uint32_t j = k + 1; j < WBVN_PAGE / 8 && gap < WBVN_RUN_GAP_WORDS; j++) {
            if (page[j] != snap[j]) {
               end = j + 1;
               gap = 0;
            } else {
               gap++;
            }
         }
         if (!push_range(b, s->res_id, base_off + start * 8, page + start, end - start))
            return -1;
         memcpy(snap + start, page + start, (end - start) * 8u);
         ranges++;
         k = end;
      }
   }
   return ranges;
}

/* Appends a PUSH message made of the ranges in `body` to `out`. */
static bool
frame_put_push(struct wbvn_buf *out, const struct wbvn_buf *body, uint32_t count)
{
   stat_add(S_push_msgs, 1);
   stat_add(S_push_bytes, body->len);
   stat_add(S_push_ranges, count);
   const struct wbvn_push_hdr ph = { .count = count };
   const struct wbvn_hdr h = { .len = (uint32_t)(sizeof(ph) + body->len), .kind = WBVN_PUSH };
   const size_t keep = out->len;
   if (buf_put(out, &h, sizeof(h)) && buf_put(out, &ph, sizeof(ph)) &&
       buf_put(out, body->data, body->len))
      return true;
   out->len = keep;
   return false;
}

/* Builds the messages that bring the host up to date into w->out: the words the guest changed.
 * The ring goes last, and was read first, so the tail never arrives ahead of the commands it
 * covers. Returns true if there is anything to send. The caller holds push_lock and writes
 * w->out with wbvn_out_write, after adding messages of its own if it has any. */
static bool
wbvn_flush_build(struct wbvn *w)
{
   struct wbvn_buf ring = { 0 };
   struct wbvn_buf *body = &w->scratch;
   struct wbvn_buf *out = &w->out;
   uint32_t ring_count = 0, count = 0;
   bool sent = false;
   const int64_t flush_start = stat_now();
   body->len = 0;

   lock_excl(w, &w->mu);
   /* roles first, so the host never sees ring data without knowing it is a ring */
   for (uint32_t id = 1; id < w->regions.cap; id++) {
      struct wbvn_region *s = w->regions.v[id];
      if (s && !s->role_sent && region_ring_id(s)) {
         s->role_sent = true;
         const uint64_t ring_id = region_ring_id(s);
         sent |= frame_put(out, WBVN_SHM_ROLE, 0, s->res_id, &ring_id, sizeof(ring_id));
      }
   }
   for (uint32_t id = 1; id < w->regions.cap; id++) {
      struct wbvn_region *s = w->regions.v[id];
      if (s && region_ring_id(s)) {
         uint32_t clear = 0;
         int n = scan_region(s, &ring, &clear);
         if (n > 0)
            ring_count += n;
         if (clear) {
            const uint32_t op[2] = { WBVN_RING_STATUS, clear };
            sent |= frame_put(out, WBVN_STATUS_CLEAR, 0, s->res_id, op, sizeof(op));
         }
      }
   }
   for (uint32_t id = 1; id < w->regions.cap; id++) {
      struct wbvn_region *s = w->regions.v[id];
      if (!s || region_ring_id(s))
         continue;
      int n = scan_region(s, body, &(uint32_t){ 0 });
      if (n > 0)
         count += n;
      if (body->len >= WBVN_PUSH_CHUNK) {
         sent |= frame_put_push(out, body, count);
         body->len = 0;
         count = 0;
         if (out->len >= WBVN_PUSH_CHUNK) {
            ReleaseSRWLockExclusive(&w->mu);
            wbvn_out_write(w);
            lock_excl(w, &w->mu);
         }
      }
   }
   ReleaseSRWLockExclusive(&w->mu);

   if (count)
      sent |= frame_put_push(out, body, count);
   if (ring_count)
      sent |= frame_put_push(out, &ring, ring_count);
   free(ring.data);
   if (flush_start) {
      stat_add(S_flush_n, 1);
      stat_add(S_flush_ns, stat_now() - flush_start);
      if (sent)
         stat_add(S_flush_sent_n, 1);
   }
   return sent;
}

/* Sends the words the guest changed to the host. Returns true if anything was sent. The caller
 * holds push_lock. */
static bool
wbvn_flush_push(struct wbvn *w)
{
   const bool sent = wbvn_flush_build(w);
   wbvn_out_write(w);
   return sent;
}

/* Applies a host push. Ranges arrive ascending and are applied from the last to the first. */
static void
wbvn_apply_push(struct wbvn *w, const uint8_t *p, uint32_t len)
{
   struct wbvn_push_hdr h;
   if (len < sizeof(h))
      return;
   memcpy(&h, p, sizeof(h));

   const struct wbvn_push_range **ranges = malloc((h.count ? h.count : 1) * sizeof(*ranges));
   if (!ranges)
      return;
   uint32_t pos = sizeof(h), n = 0;
   struct wbvn_push_range r;
   while (n < h.count && pos + sizeof(r) <= len) {
      memcpy(&r, p + pos, sizeof(r));
      if (r.len > len - pos - sizeof(r))
         break;
      ranges[n++] = (const struct wbvn_push_range *)(p + pos);
      pos += sizeof(r) + r.len;
   }

   lock_excl(w, &w->mu);
   for (uint32_t i = n; i-- > 0;) {
      memcpy(&r, ranges[i], sizeof(r));
      struct wbvn_region *s = tab_get(&w->regions, r.res_id);
      if (!s || (r.offset & 7) || (r.len & 7) || r.offset > s->size ||
          r.len > s->size - r.offset)
         continue;
      const uint8_t *data = (const uint8_t *)ranges[i] + sizeof(r);
      for (uint32_t o = 0; o < r.len; o += 8) {
         uint64_t v;
         memcpy(&v, data + o, 8);
         s->snap[(r.offset + o) / 8] = v;
         store64(s->ptr + r.offset + o, v);
      }
   }
   w->push_gen++;
   WakeAllConditionVariable(&w->cv);
   ReleaseSRWLockExclusive(&w->mu);
   free(ranges);
}

static void
wbvn_apply_signals(struct wbvn *w, const uint8_t *p, uint32_t len)
{
   struct wbvn_push_hdr h;
   if (len < sizeof(h))
      return;
   memcpy(&h, p, sizeof(h));
   lock_excl(w, &w->mu);
   for (uint32_t i = 0; i < h.count; i++) {
      size_t at = sizeof(h) + (size_t)i * sizeof(struct wbvn_sync_point);
      struct wbvn_sync_point sp;
      if (at + sizeof(sp) > len)
         break;
      memcpy(&sp, p + at, sizeof(sp));
      struct wbvn_sync *s = tab_get(&w->syncs, sp.sync_id);
      if (s && s->value < sp.value)
         s->value = sp.value;
   }
   w->push_gen++;
   WakeAllConditionVariable(&w->cv);
   ReleaseSRWLockExclusive(&w->mu);
}

/* For Mesa's `vn_relax` (patch 0010): its waits end with something the host sent, so they sleep
 * until a host update has been applied. The count is read before the caller looks at the state
 * and compared under the lock, so an update that lands in between is not missed. */
uint64_t
wbvn_push_gen(void)
{
   struct wbvn *w = g_renderer;
   if (!w)
      return 0;
   lock_shared(w, &w->mu);
   const uint64_t gen = w->push_gen;
   ReleaseSRWLockShared(&w->mu);
   return gen;
}

/* For Mesa's `vn_ring_submit_internal` (patch 0011): the ring's tail has just been published, so
 * the host gets the ring and what it covers now instead of when the pump next looks. */
void
wbvn_ring_published(void)
{
   struct wbvn *w = g_renderer;
   if (!w || w->lost)
      return;
   lock_excl(w, &w->push_lock);
   wbvn_flush_push(w);
   ReleaseSRWLockExclusive(&w->push_lock);
}

uint64_t
wbvn_relax_wait(uint64_t seen, uint32_t us)
{
   struct wbvn *w = g_renderer;
   if (!w) {
      Sleep((us + 999) / 1000);
      return seen;
   }
   lock_excl(w, &w->mu);
   if (w->push_gen == seen && !w->lost)
      SleepConditionVariableSRW(&w->cv, &w->mu, (us + 999) / 1000, 0);
   const uint64_t gen = w->push_gen;
   ReleaseSRWLockExclusive(&w->mu);
   return gen;
}

/* Handles one message from the host; `payload` points into the reader's buffer. Returns false
 * when the session is over. */
static bool
wbvn_handle_message(struct wbvn *w, const struct wbvn_hdr *h, const uint8_t *payload)
{
   stat_add(S_rd_msgs, 1);
   stat_add(S_rd_bytes, (int64_t)(sizeof(*h) + h->len));
   if (h->kind & WBVN_REPLY) {
      uint8_t *copy = h->len ? malloc(h->len) : NULL;
      if (h->len && !copy)
         return false;
      if (copy)
         memcpy(copy, payload, h->len);
      lock_excl(w, &w->mu);
      if (!w->reply.done && h->req == w->reply.req) {
         w->reply.done = true;
         w->reply.arg = h->arg;
         w->reply.payload = copy;
         w->reply.len = h->len;
         copy = NULL;
      }
      WakeAllConditionVariable(&w->cv);
      ReleaseSRWLockExclusive(&w->mu);
      free(copy);
   } else if (h->kind == WBVN_PUSH) {
      const int64_t t0 = stat_now();
      wbvn_apply_push(w, payload, h->len);
      if (t0) {
         stat_add(S_rdpush_n, 1);
         stat_add(S_rdpush_bytes, h->len);
         stat_add(S_rdpush_ns, stat_now() - t0);
      }
   } else if (h->kind == WBVN_SYNC_SIGNAL) {
      stat_add(S_rdsignal_n, 1);
      wbvn_apply_signals(w, payload, h->len);
   } else if (h->kind == WBVN_LOST) {
      return false;
   }
   return true;
}

/* Tells the host this process is alive, and notices when the host took the slot away. */
static void
wbvn_heartbeat(struct wbvn *w)
{
   const uint64_t now = GetTickCount64();
   if (now - w->beat_at < 100)
      return;
   w->beat_at = now;
   if (SLOT_LD64(w->bar, WBVN_SLOT_OFF_MAGIC) != WBVN_SLOT_MAGIC ||
       SLOT_LD32(w->bar, WBVN_SLOT_OFF_STATE) != WBVN_SLOT_CLAIMED ||
       SLOT_LD64(w->bar, WBVN_SLOT_OFF_NONCE) != w->nonce) {
      if (!w->quit)
         wbvn_set_lost(w);
      return;
   }
   __atomic_fetch_add((uint64_t *)(w->bar + WBVN_SLOT_BLOCK + WBVN_SLOT_OFF_HEARTBEAT), 1,
                      __ATOMIC_ACQ_REL);
}

/* Waits for the host to write: polls for a short while, then sleeps until a wake-up byte comes.
 * `partial` means a message is half in; the rest is on its way, so it only polls. */
static void
wbvn_reader_idle(struct wbvn *w, bool partial)
{
   const int64_t t0 = qpc_ticks();
   const int64_t limit = partial ? w->spin_ticks * 300 + 1 : w->spin_ticks;
   do {
      if (wbvn_ring_ready(&w->rx) || w->quit || w->lost)
         return;
      for (int i = 0; i < 32; i++)
         YieldProcessor();
   } while (qpc_ticks() - t0 < limit);

   /* Flag, look again, then wait: the host either sees the flag and sends a byte, or we see
    * its record. */
   wbvn_ring_set_sleeping(&w->rx, true);
   if (!wbvn_ring_ready(&w->rx) && !w->quit && !w->lost) {
      const int64_t s0 = stat_now();
      if (!w->kick_armed) {
         memset(&w->kick_ov, 0, sizeof(w->kick_ov));
         w->kick_ov.hEvent = w->read_event;
         DWORD got;
         if (ReadFile(w->port, &w->kick_byte, 1, &got, &w->kick_ov) ||
             GetLastError() == ERROR_IO_PENDING)
            w->kick_armed = true;
      }
      if (w->kick_armed) {
         if (WaitForSingleObject(w->read_event, WBVN_KICK_TIMEOUT_MS) == WAIT_OBJECT_0) {
            DWORD got;
            GetOverlappedResult(w->port, &w->kick_ov, &got, FALSE);
            w->kick_armed = false;
            stat_add(S_kick_rx_n, 1);
         }
      } else {
         /* no wake-up port: fall back to the timer */
         Sleep(1);
      }
      if (s0) {
         stat_add(S_rd_sleep_n, 1);
         stat_add(S_rd_sleep_ns, stat_now() - s0);
      }
   }
   wbvn_ring_set_sleeping(&w->rx, false);
}

/* Reads the host to guest lane, a byte stream of messages in records, and handles each message. */
static DWORD WINAPI
wbvn_reader(LPVOID arg)
{
   struct wbvn *w = arg;
   struct wbvn_buf acc = { 0 };
   size_t pos = 0;
   for (;;) {
      wbvn_heartbeat(w);
      bool got = false;
      for (;;) {
         const uint8_t *p;
         uint32_t n;
         const int r = wbvn_ring_peek(&w->rx, &p, &n);
         if (r == WBVN_RING_CLOSED || (r == WBVN_RING_OK && !buf_put(&acc, p, n)))
            goto done;
         if (r != WBVN_RING_OK)
            break;
         wbvn_ring_consume(&w->rx);
         got = true;
      }
      bool partial = false;
      for (;;) {
         struct wbvn_hdr h;
         if (acc.len - pos < sizeof(h)) {
            partial = acc.len > pos;
            break;
         }
         memcpy(&h, acc.data + pos, sizeof(h));
         if (h.len > WBVN_MAX_PAYLOAD)
            goto done;
         if (acc.len - pos < sizeof(h) + h.len) {
            partial = true;
            break;
         }
         if (!wbvn_handle_message(w, &h, acc.data + pos + sizeof(h)))
            goto done;
         pos += sizeof(h) + h.len;
      }
      if (pos == acc.len) {
         acc.len = 0;
         pos = 0;
      } else if (pos >= (1u << 20)) {
         memmove(acc.data, acc.data + pos, acc.len - pos);
         acc.len -= pos;
         pos = 0;
      }
      if (got)
         continue;
      if (w->quit || w->lost)
         break;
      wbvn_reader_idle(w, partial);
   }
done:
   free(acc.data);
   if (!w->quit)
      wbvn_set_lost(w);
   return 0;
}

/* Sweeps up what nothing announced. The ring's tail is pushed by wbvn_ring_published, and a
 * submit, a wait and a bo flush push what they need, so this runs at the timer's rate and finds
 * little. A pump that polled for 4000 rounds after every push kept a virtual CPU busy: the teapot
 * took 5.3 and 6.3 ms a frame with it and 2.2 and 2.3 ms without (600 frames, build VM, 4 vCPUs). */
static DWORD WINAPI
wbvn_pump(LPVOID arg)
{
   struct wbvn *w = arg;
   while (!w->quit && !w->lost) {
      if (w->contenders) {
         SwitchToThread();
         continue;
      }
      AcquireSRWLockExclusive(&w->push_lock);
      wbvn_flush_push(w);
      ReleaseSRWLockExclusive(&w->push_lock);
      stat_add(S_pump_loops, 1);
      const int64_t t0 = stat_now();
      Sleep(1);
      if (t0) {
         stat_add(S_pump_sleeps, 1);
         stat_add(S_pump_sleep_ns, stat_now() - t0);
      }
      stats_tick(false);
   }
   return 0;
}

/* Asks for a 0.5 ms timer resolution while a renderer lives. In the Build VM guest Sleep(1) still
 * lasted 4.5 to 6 ms (WBVN_STATS measures it at start-up; the default tick is 15.6 ms) and so did
 * a high-resolution waitable timer, so nothing on the hot path may depend on a timed sleep. */
static void
wbvn_timer_resolution(bool fine)
{
   typedef LONG(NTAPI * set_resolution_fn)(ULONG, BOOLEAN, PULONG);
   const HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
   const set_resolution_fn set =
      ntdll ? (set_resolution_fn)(void (*)(void))GetProcAddress(ntdll, "NtSetTimerResolution") : NULL;
   ULONG actual;
   if (set)
      set(5000, fine, &actual);
}

/* ---------- vn_renderer ops ---------------------------------------------------------- */

static void
wbvn_destroy(struct vn_renderer *renderer, const VkAllocationCallbacks *alloc)
{
   struct wbvn *w = (struct wbvn *)renderer;

   if (g_renderer == w)
      g_renderer = NULL;
   InterlockedExchange(&w->quit, 1);
   if (w->pump) {
      WaitForSingleObject(w->pump, INFINITE);
      CloseHandle(w->pump);
      wbvn_timer_resolution(false);
   }
   stats_tick(true);
   if (w->port != INVALID_HANDLE_VALUE) {
      /* wakes a reader that sleeps on the port */
      CancelIoEx(w->port, NULL);
   }
   if (w->reader) {
      WaitForSingleObject(w->reader, 5000);
      CloseHandle(w->reader);
   }
   if (w->bar) {
      /* All we sent is in the lane; the host reads it out before it honours the close. */
      if (SLOT_LD64(w->bar, WBVN_SLOT_OFF_NONCE) == w->nonce)
         SLOT_ST32(w->bar, WBVN_SLOT_OFF_STATE, WBVN_SLOT_CLOSED);
      wbvn_unmap(w);
   }
   if (w->port != INVALID_HANDLE_VALUE)
      CloseHandle(w->port);
   if (w->read_event)
      CloseHandle(w->read_event);
   if (w->write_event)
      CloseHandle(w->write_event);
   free(w->regions.v);
   free(w->syncs.v);
   free(w->scratch.data);
   free(w->out.data);
   free(w->reply.payload);
   vk_free(alloc, w);
}

/* Appends a batch in the layout of a WBVN_SUBMIT payload. */
static bool
batch_encode(struct wbvn_buf *msg, const struct vn_renderer_submit_batch *batch)
{
   const struct wbvn_submit_hdr h = {
      .ring_idx = batch->ring_idx,
      .sync_count = batch->sync_count,
      .cs_size = (uint32_t)batch->cs_size,
   };
   if (!buf_reserve(msg, sizeof(h) + (size_t)batch->sync_count * sizeof(struct wbvn_sync_point) +
                            batch->cs_size) ||
       !buf_put(msg, &h, sizeof(h)))
      return false;
   for (uint32_t i = 0; i < batch->sync_count; i++) {
      const struct wbvn_sync_point sp = {
         .sync_id = ((struct wbvn_sync *)batch->syncs[i])->base.sync_id,
         .value = batch->sync_values[i],
      };
      if (!buf_put(msg, &sp, sizeof(sp)))
         return false;
   }
   return buf_put(msg, batch->cs_data, batch->cs_size);
}

static VkResult
wbvn_submit(struct vn_renderer *renderer,
            const struct vn_renderer_submit_batch *batch)
{
   struct wbvn *w = (struct wbvn *)renderer;
   const int64_t submit_start = stat_now();
   if (w->lost)
      return VK_ERROR_DEVICE_LOST;

   struct wbvn_buf msg = { 0 };
   bool ok = batch_encode(&msg, batch);
   if (!ok) {
      free(msg.data);
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   }

   lock_excl(w, &w->push_lock);
   wbvn_flush_build(w);
   ok = frame_put(&w->out, WBVN_SUBMIT, 0, 0, msg.data, msg.len) && wbvn_out_write(w);
   ReleaseSRWLockExclusive(&w->push_lock);
   if (submit_start) {
      stat_add(S_submit_n, 1);
      stat_add(S_submit_ns, stat_now() - submit_start);
      stat_add(S_submit_bytes, (int64_t)msg.len);
   }
   free(msg.data);
   return ok ? VK_SUCCESS : VK_ERROR_DEVICE_LOST;
}

static VkResult
wbvn_wait(struct vn_renderer *renderer, const struct vn_renderer_wait *wait)
{
   struct wbvn *w = (struct wbvn *)renderer;
   const DWORD64 start = GetTickCount64();
   const int64_t wait_start = stat_now();
   VkResult result = VK_SUCCESS;

   /* What the sync waits for may still sit in the ring or in a mirrored buffer, because the pump
    * has not pushed it yet. The host has to see it first. */
   lock_excl(w, &w->push_lock);
   wbvn_flush_push(w);
   ReleaseSRWLockExclusive(&w->push_lock);

   lock_excl(w, &w->mu);
   for (;;) {
      uint32_t reached = 0;
      for (uint32_t i = 0; i < wait->sync_count; i++) {
         if (((struct wbvn_sync *)wait->syncs[i])->value >= wait->sync_values[i])
            reached++;
      }
      if (wait->wait_any ? reached > 0 : reached == wait->sync_count)
         break;
      if (w->lost) {
         result = VK_ERROR_DEVICE_LOST;
         break;
      }

      DWORD ms = INFINITE;
      if (wait->timeout != UINT64_MAX) {
         const uint64_t total_ms = (wait->timeout + 999999) / 1000000;
         const DWORD64 spent = GetTickCount64() - start;
         if (spent >= total_ms) {
            result = VK_TIMEOUT;
            break;
         }
         ms = (DWORD)(total_ms - spent);
      }
      SleepConditionVariableSRW(&w->cv, &w->mu, ms, 0);
   }
   ReleaseSRWLockExclusive(&w->mu);
   if (wait_start) {
      const int64_t d = stat_now() - wait_start;
      stat_add(S_wait_n, 1);
      stat_add(S_wait_ns, d);
      stat_max(S_wait_max_ns, d);
      stat_add(d < 100000 ? S_wait_h0 : d < 500000 ? S_wait_h1 : d < 2000000 ? S_wait_h2
               : d < 10000000 ? S_wait_h3 : S_wait_h4, 1);
   }
   return result;
}

static void
region_fini(struct wbvn_region *r)
{
   if (r->ptr)
      VirtualFree(r->ptr, 0, MEM_RELEASE);
   free(r->snap);
   free(r->dirty);
   memset(r, 0, sizeof(*r));
}

/* Allocates the guest copy: page aligned, with write tracking so the mirror only looks at
 * the pages the guest touched. */
static bool
region_init(struct wbvn_region *r, size_t size)
{
   r->size = (size + WBVN_PAGE - 1) & ~(size_t)(WBVN_PAGE - 1);
   r->snap = calloc(1, r->size);
   r->dirty = malloc(r->size / WBVN_PAGE * sizeof(void *));
   r->ptr = VirtualAlloc(NULL, r->size, MEM_COMMIT | MEM_RESERVE | MEM_WRITE_WATCH,
                         PAGE_READWRITE);
   if (r->snap && r->dirty && r->ptr)
      return true;
   region_fini(r);
   return false;
}

/* Gives the region its id and tells the host about it. */
static bool
region_attach(struct wbvn *w, struct wbvn_region *r, uint32_t kind, const void *payload,
              size_t len)
{
   lock_excl(w, &w->mu);
   r->res_id = tab_add(&w->regions, r);
   ReleaseSRWLockExclusive(&w->mu);
   if (!r->res_id)
      return false;
   if (wbvn_request(w, kind, r->res_id, payload, len, NULL, NULL) == 0)
      return true;
   lock_excl(w, &w->mu);
   tab_del(&w->regions, r->res_id);
   ReleaseSRWLockExclusive(&w->mu);
   return false;
}

static void
region_detach(struct wbvn *w, struct wbvn_region *r)
{
   /* the host may still have to see the last words before the resource goes away */
   lock_excl(w, &w->push_lock);
   wbvn_flush_push(w);
   lock_excl(w, &w->mu);
   tab_del(&w->regions, r->res_id);
   ReleaseSRWLockExclusive(&w->mu);
   ReleaseSRWLockExclusive(&w->push_lock);

   wbvn_request(w, WBVN_SHM_DESTROY, r->res_id, NULL, 0, NULL, NULL);
}

static struct vn_renderer_shmem *
wbvn_shmem_create(struct vn_renderer *renderer, size_t size)
{
   struct wbvn *w = (struct wbvn *)renderer;
   if (w->lost)
      return NULL;

   struct wbvn_shmem *s = calloc(1, sizeof(*s));
   if (!s)
      return NULL;
   if (!region_init(&s->r, size))
      goto fail;
   s->r.ring_src = &s->base.mirror_ring_id;

   const uint64_t size64 = s->r.size;
   if (!region_attach(w, &s->r, WBVN_SHM_CREATE, &size64, sizeof(size64))) {
      region_fini(&s->r);
      goto fail;
   }

   s->base.refcount = VN_REFCOUNT_INIT(1);
   s->base.res_id = s->r.res_id;
   s->base.mmap_size = s->r.size;
   s->base.mmap_ptr = s->r.ptr;
   return &s->base;

fail:
   free(s);
   return NULL;
}

static void
wbvn_shmem_destroy(struct vn_renderer *renderer, struct vn_renderer_shmem *_shmem)
{
   struct wbvn *w = (struct wbvn *)renderer;
   struct wbvn_shmem *s = (struct wbvn_shmem *)_shmem;
   region_detach(w, &s->r);
   region_fini(&s->r);
   free(s);
}

static VkResult
wbvn_bo_create_from_device_memory(struct vn_renderer *renderer,
                                  struct vn_renderer_submit_batch *batch,
                                  VkDeviceSize size,
                                  vn_object_id mem_id,
                                  VkMemoryPropertyFlags flags,
                                  VkExternalMemoryHandleTypeFlags external_handles,
                                  struct vn_renderer_bo **out_bo)
{
   struct wbvn *w = (struct wbvn *)renderer;
   if (w->lost)
      return VK_ERROR_DEVICE_LOST;

   struct wbvn_bo *bo = calloc(1, sizeof(*bo));
   if (!bo)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   if (!region_init(&bo->r, size)) {
      free(bo);
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   }

   /* The batch waits for the ring to reach the vkAllocateMemory that made this memory. It
    * has to be at the host before the host creates the resource, as does the ring itself. */
   struct wbvn_buf msg = { 0 };
   const struct wbvn_bo_create hdr = {
      .size = size,
      .blob_id = mem_id,
      .blob_flags = (flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) ? WBVN_BLOB_MAPPABLE : 0,
   };
   bool ok = buf_put(&msg, &hdr, sizeof(hdr)) && (!batch || batch_encode(&msg, batch));
   if (ok && batch) {
      const uint32_t batch_len = (uint32_t)(msg.len - sizeof(hdr));
      memcpy(msg.data + offsetof(struct wbvn_bo_create, batch_len), &batch_len, 4);
   }

   lock_excl(w, &w->push_lock);
   wbvn_flush_push(w);
   ok = ok && region_attach(w, &bo->r, WBVN_BO_CREATE, msg.data, msg.len);
   ReleaseSRWLockExclusive(&w->push_lock);
   free(msg.data);
   if (!ok) {
      region_fini(&bo->r);
      free(bo);
      return w->lost ? VK_ERROR_DEVICE_LOST : VK_ERROR_OUT_OF_DEVICE_MEMORY;
   }

   bo->base.refcount = VN_REFCOUNT_INIT(1);
   bo->base.res_id = bo->r.res_id;
   bo->base.mmap_size = size;
   bo->base.mmap_ptr = bo->r.ptr;
   *out_bo = &bo->base;
   return VK_SUCCESS;
}

static bool
wbvn_bo_destroy(struct vn_renderer *renderer, struct vn_renderer_bo *_bo)
{
   struct wbvn *w = (struct wbvn *)renderer;
   struct wbvn_bo *bo = (struct wbvn_bo *)_bo;
   region_detach(w, &bo->r);
   region_fini(&bo->r);
   free(bo);
   return true;
}

static void *
wbvn_bo_map(struct vn_renderer *renderer, struct vn_renderer_bo *_bo, void *placed_addr)
{
   /* the guest copy is allocated up front; a placed mapping cannot be honoured */
   return placed_addr ? NULL : ((struct wbvn_bo *)_bo)->r.ptr;
}

/* The memory types the guest sees are coherent, so applications flush nothing. The guest
 * copy goes to the host at every submit; this makes an explicit flush do the same. The host
 * sends GPU writes back when a fence retires, so there is nothing to do on invalidate. */
static void
wbvn_bo_flush(struct vn_renderer *renderer, struct vn_renderer_bo *bo,
              VkDeviceSize offset, VkDeviceSize size)
{
   struct wbvn *w = (struct wbvn *)renderer;
   lock_excl(w, &w->push_lock);
   wbvn_flush_push(w);
   ReleaseSRWLockExclusive(&w->push_lock);
}

static void
wbvn_bo_invalidate(struct vn_renderer *renderer, struct vn_renderer_bo *bo,
                   VkDeviceSize offset, VkDeviceSize size)
{
}

static VkResult
wbvn_sync_create(struct vn_renderer *renderer, uint64_t initial_val,
                 struct vn_renderer_sync **out_sync)
{
   struct wbvn *w = (struct wbvn *)renderer;
   struct wbvn_sync *s = calloc(1, sizeof(*s));
   if (!s)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   s->value = initial_val;
   lock_excl(w, &w->mu);
   s->base.sync_id = tab_add(&w->syncs, s);
   ReleaseSRWLockExclusive(&w->mu);
   if (!s->base.sync_id) {
      free(s);
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   }
   *out_sync = &s->base;
   return VK_SUCCESS;
}

static void
wbvn_sync_destroy(struct vn_renderer *renderer, struct vn_renderer_sync *_sync)
{
   struct wbvn *w = (struct wbvn *)renderer;
   struct wbvn_sync *s = (struct wbvn_sync *)_sync;
   lock_excl(w, &w->mu);
   tab_del(&w->syncs, s->base.sync_id);
   ReleaseSRWLockExclusive(&w->mu);
   free(s);
}

static VkResult
wbvn_sync_write(struct vn_renderer *renderer, struct vn_renderer_sync *_sync,
                uint64_t val)
{
   struct wbvn *w = (struct wbvn *)renderer;
   struct wbvn_sync *s = (struct wbvn_sync *)_sync;
   lock_excl(w, &w->mu);
   s->value = val;
   WakeAllConditionVariable(&w->cv);
   ReleaseSRWLockExclusive(&w->mu);
   return VK_SUCCESS;
}

static VkResult
wbvn_sync_reset(struct vn_renderer *renderer, struct vn_renderer_sync *sync)
{
   return wbvn_sync_write(renderer, sync, 0);
}

static VkResult
wbvn_sync_read(struct vn_renderer *renderer, struct vn_renderer_sync *_sync,
               uint64_t *val)
{
   struct wbvn *w = (struct wbvn *)renderer;
   struct wbvn_sync *s = (struct wbvn_sync *)_sync;
   lock_shared(w, &w->mu);
   *val = s->value;
   ReleaseSRWLockShared(&w->mu);
   return VK_SUCCESS;
}

/* ---------- start-up ----------------------------------------------------------------- */

static VkResult
wbvn_init(struct wbvn *w)
{
   stats_init();
   InitializeSRWLock(&w->write_lock);
   InitializeSRWLock(&w->push_lock);
   InitializeSRWLock(&w->mu);
   InitializeSRWLock(&w->req_lock);
   InitializeConditionVariable(&w->cv);

   if (!wbvn_attach(w))
      return VK_ERROR_INITIALIZATION_FAILED;

   WCHAR port_name[96];
   swprintf(port_name, ARRAYSIZE(port_name), WBVN_PORT_FORMAT, w->slot_id);
   w->port = CreateFileW(port_name, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
                         FILE_FLAG_OVERLAPPED, NULL);
   if (w->port == INVALID_HANDLE_VALUE) {
      vn_log(w->instance, "cannot open the wake-up port of slot %u (error %lu)", w->slot_id,
             GetLastError());
      return VK_ERROR_INITIALIZATION_FAILED;
   }
   LARGE_INTEGER qpc_freq;
   QueryPerformanceFrequency(&qpc_freq);
   w->spin_ticks = (int64_t)env_uint("WBVN_SPIN_US", WBVN_SPIN_US) * qpc_freq.QuadPart / 1000000;
   w->beat_at = GetTickCount64();
   w->read_event = CreateEventW(NULL, TRUE, FALSE, NULL);
   w->write_event = CreateEventW(NULL, TRUE, FALSE, NULL);
   if (!w->read_event || !w->write_event)
      return VK_ERROR_OUT_OF_HOST_MEMORY;

   w->reader = CreateThread(NULL, 0, wbvn_reader, w, 0, NULL);
   if (!w->reader)
      return VK_ERROR_OUT_OF_HOST_MEMORY;

   const uint32_t version = WBVN_VERSION;
   uint8_t *hello = NULL;
   uint32_t hello_len = 0;
   int64_t r = wbvn_request(w, WBVN_HELLO, 0, &version, sizeof(version), &hello,
                            &hello_len);
   if (r != 0 || hello_len < 8 + sizeof(w->capset)) {
      vn_log(w->instance, "the venus host refused the session (%lld)", (long long)r);
      free(hello);
      return VK_ERROR_INITIALIZATION_FAILED;
   }
   uint32_t host_version;
   memcpy(&host_version, hello, 4);
   memcpy(&w->max_timeline_count, hello + 4, 4);
   memcpy(&w->capset, hello + 8, sizeof(w->capset));
   free(hello);
   if (host_version != WBVN_VERSION || !w->max_timeline_count ||
       !w->capset.supports_blob_id_0 || !w->capset.allow_vk_wait_syncs ||
       !w->capset.supports_multiple_timelines) {
      vn_log(w->instance, "the venus host does not offer what the guest needs");
      return VK_ERROR_INITIALIZATION_FAILED;
   }

   struct vn_renderer_info *info = &w->base.info;
   info->pci.vendor_id = 0x1af4;
   info->pci.device_id = 0x1050;
   info->has_dma_buf_import = false;
   info->has_external_sync = false;
   info->has_timeline_sync = true;
   /* There are no sync files or dma-bufs. The software WSI path waits on a fence before it reads
    * an image, and a bo is brought up to date when that fence retires. */
   info->has_implicit_fencing = true;
   info->has_guest_vram = false;
   info->wire_format_version = w->capset.wire_format_version;
   info->vk_xml_version = w->capset.vk_xml_version;
   info->vk_ext_command_serialization_spec_version =
      w->capset.vk_ext_command_serialization_spec_version;
   info->vk_mesa_venus_protocol_spec_version =
      w->capset.vk_mesa_venus_protocol_spec_version;
   STATIC_ASSERT(sizeof(info->vk_extension_mask) >=
                 sizeof(w->capset.vk_extension_mask1));
   memcpy(info->vk_extension_mask, w->capset.vk_extension_mask1,
          sizeof(w->capset.vk_extension_mask1));
   info->max_timeline_count = w->max_timeline_count;

   w->base.ops.destroy = wbvn_destroy;
   w->base.ops.submit = wbvn_submit;
   w->base.ops.wait = wbvn_wait;

   w->base.shmem_ops.create = wbvn_shmem_create;
   w->base.shmem_ops.destroy = wbvn_shmem_destroy;

   w->base.bo_ops.create_from_device_memory = wbvn_bo_create_from_device_memory;
   w->base.bo_ops.create_from_dma_buf = NULL;
   w->base.bo_ops.destroy = wbvn_bo_destroy;
   w->base.bo_ops.export_dma_buf = NULL;
   w->base.bo_ops.export_sync_file = NULL;
   w->base.bo_ops.map = wbvn_bo_map;
   w->base.bo_ops.flush = wbvn_bo_flush;
   w->base.bo_ops.invalidate = wbvn_bo_invalidate;

   w->base.sync_ops.create = wbvn_sync_create;
   w->base.sync_ops.create_from_syncobj = NULL;
   w->base.sync_ops.destroy = wbvn_sync_destroy;
   w->base.sync_ops.export_syncobj = NULL;
   w->base.sync_ops.reset = wbvn_sync_reset;
   w->base.sync_ops.read = wbvn_sync_read;
   w->base.sync_ops.write = wbvn_sync_write;

   wbvn_timer_resolution(true);
   wbvn_stats_ping(w);
   w->pump = CreateThread(NULL, 0, wbvn_pump, w, 0, NULL);
   if (!w->pump)
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   return VK_SUCCESS;
}

/* Feedback slots (events, semaphores, queries) are device memory the guest would poll
 * through a mapping. Without device memory support they are off; the checks become
 * synchronous commands. */
BOOL WINAPI
DllMain(HINSTANCE inst, DWORD reason, LPVOID reserved)
{
   if (reason == DLL_PROCESS_DETACH && reserved) {
      /* the process is ending: the other threads are gone, and the host must free the slot */
      struct wbvn *w = g_renderer;
      if (w && w->bar && SLOT_LD64(w->bar, WBVN_SLOT_OFF_NONCE) == w->nonce)
         SLOT_ST32(w->bar, WBVN_SLOT_OFF_STATE, WBVN_SLOT_CLOSED);
   }
   if (reason == DLL_PROCESS_ATTACH) {
      char cur[256];
      DWORD n = GetEnvironmentVariableA("VN_PERF", cur, sizeof(cur));
      if (!n) {
         _putenv_s("VN_PERF", "no_event_feedback,no_semaphore_feedback,no_query_feedback");
      }
   }
   return TRUE;
}

VkResult
vn_renderer_create_winbridge(struct vn_instance *instance,
                             const VkAllocationCallbacks *alloc,
                             struct vn_renderer **renderer)
{
   struct wbvn *w = vk_zalloc(alloc, sizeof(*w), VN_DEFAULT_ALIGN,
                              VK_SYSTEM_ALLOCATION_SCOPE_INSTANCE);
   if (!w)
      return VK_ERROR_OUT_OF_HOST_MEMORY;

   w->instance = instance;
   w->port = INVALID_HANDLE_VALUE;

   VkResult result = wbvn_init(w);
   if (result != VK_SUCCESS) {
      wbvn_destroy(&w->base, alloc);
      return result;
   }

   g_renderer = w;
   *renderer = &w->base;
   if (VN_DEBUG(INIT))
      vn_log(instance, "winbridge backend initialized");
   return VK_SUCCESS;
}
