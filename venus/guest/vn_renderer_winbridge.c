/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * WinBridge `vn_renderer` backend for the Mesa Venus guest ICD on Windows.
 *
 * Replaces the POSIX-only virtgpu and vtest backends. The renderer is a host process that
 * owns virglrenderer's Venus context; this file talks to it over the WBVN channel
 * (`wbvn_proto.h`) on a virtio-serial port. There is no mapping shared with the host, so every
 * venus shmem is a local allocation that is mirrored in both directions (see the protocol
 * header for the ordering rules). Timeline syncs live in the guest; the host only reports
 * the points that its fences retired.
 *
 * Device memory is not implemented yet: `create_from_device_memory` fails.
 */

#include <windows.h>

#include "vn_renderer.h"
#include "virtio/virtio-gpu/venus_hw.h"
#include "wbvn_proto.h"

#define WBVN_PORT L"\\\\.\\Global\\org.winbridge.venus.0"

/* A single read or write on the port moves at most this much. */
#define WBVN_MAX_IO (1u << 20)
/* A mirror message for non-ring shmems is cut at about this size. */
#define WBVN_PUSH_CHUNK (4u << 20)
/* How long a request waits for the host's answer. */
#define WBVN_REPLY_TIMEOUT_MS 30000
/* Runs closer than this many words are sent as one. */
#define WBVN_RUN_GAP_WORDS 8

#define WBVN_PAGE 4096u

/* Offset of the status word in a ring shmem (`struct layout` in `vn_ring_get_layout`). */
#define WBVN_RING_STATUS 128u

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

   HANDLE port;
   HANDLE read_event;
   HANDLE write_event;
   HANDLE reader;
   HANDLE pump;
   volatile LONG quit;
   volatile LONG lost;

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

/* ---------- port I/O ----------------------------------------------------------------- */

static bool
port_xfer(struct wbvn *w, HANDLE event, bool write, void *buf, size_t len)
{
   uint8_t *p = buf;
   while (len) {
      DWORD want = len > WBVN_MAX_IO ? WBVN_MAX_IO : (DWORD)len;
      DWORD got = 0;
      OVERLAPPED ov = { .hEvent = event };
      BOOL ok = write ? WriteFile(w->port, p, want, &got, &ov)
                      : ReadFile(w->port, p, want, &got, &ov);
      if (!ok) {
         if (GetLastError() != ERROR_IO_PENDING)
            return false;
         if (!GetOverlappedResult(w->port, &ov, &got, TRUE))
            return false;
      }
      if (!got && !write)
         return false;
      p += got;
      len -= got;
   }
   return true;
}

static void
wbvn_set_lost(struct wbvn *w)
{
   InterlockedExchange(&w->lost, 1);
   AcquireSRWLockExclusive(&w->mu);
   WakeAllConditionVariable(&w->cv);
   ReleaseSRWLockExclusive(&w->mu);
}

static bool
wbvn_send(struct wbvn *w, uint32_t kind, uint32_t req, uint32_t arg,
          const void *payload, size_t len)
{
   struct wbvn_hdr h = { .len = (uint32_t)len, .kind = kind, .req = req, .arg = arg };
   bool ok;
   AcquireSRWLockExclusive(&w->write_lock);
   ok = !w->lost && port_xfer(w, w->write_event, true, &h, sizeof(h)) &&
        (!len || port_xfer(w, w->write_event, true, (void *)payload, len));
   ReleaseSRWLockExclusive(&w->write_lock);
   if (!ok)
      wbvn_set_lost(w);
   return ok;
}

/* Sends a request and waits for its reply. `out`, if given, receives the reply payload
 * (malloc'd, caller frees). Returns the reply arg, or -1 on a lost channel. */
static int64_t
wbvn_request(struct wbvn *w, uint32_t kind, uint32_t arg, const void *payload,
             size_t len, uint8_t **out, uint32_t *out_len)
{
   int64_t result = -1;
   AcquireSRWLockExclusive(&w->req_lock);
   AcquireSRWLockExclusive(&w->mu);
   uint32_t req = ++w->req_seq;
   w->reply.done = false;
   w->reply.req = req;
   ReleaseSRWLockExclusive(&w->mu);

   if (wbvn_send(w, kind, req, arg, payload, len)) {
      AcquireSRWLockExclusive(&w->mu);
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
   return result;
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
   ULONG_PTR count = s->size / WBVN_PAGE;
   DWORD gran = 0;
   if (GetWriteWatch(WRITE_WATCH_FLAG_RESET, s->ptr, s->size, s->dirty,
                     &count, &gran) != 0)
      return -1;

   int ranges = 0;
   uint64_t page[WBVN_PAGE / 8];
   for (ULONG_PTR i = 0; i < count; i++) {
      const uint8_t *src = s->dirty[i];
      uint32_t base_off = (uint32_t)(src - s->ptr);
      uint64_t *snap = s->snap + base_off / 8;
      for (uint32_t k = 0; k < WBVN_PAGE / 8; k++)
         page[k] = load64(src + k * 8);

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

static bool
send_push(struct wbvn *w, struct wbvn_buf *body, uint32_t count)
{
   struct wbvn_push_hdr h = { .count = count };
   struct wbvn_buf msg = { 0 };
   bool ok = buf_put(&msg, &h, sizeof(h)) && buf_put(&msg, body->data, body->len) &&
             wbvn_send(w, WBVN_PUSH, 0, 0, msg.data, msg.len);
   free(msg.data);
   return ok;
}

/* Sends the words the guest changed to the host. The ring goes last, and was read first,
 * so the tail never arrives ahead of the commands it covers. Returns true if anything was
 * sent. The caller holds push_lock. */
static bool
wbvn_flush_push(struct wbvn *w)
{
   struct wbvn_buf ring = { 0 };
   struct wbvn_buf *body = &w->scratch;
   uint32_t ring_count = 0, count = 0;
   bool sent = false;
   body->len = 0;

   AcquireSRWLockExclusive(&w->mu);
   /* roles first, so the host never sees ring data without knowing it is a ring */
   for (uint32_t id = 1; id < w->regions.cap; id++) {
      struct wbvn_region *s = w->regions.v[id];
      if (s && !s->role_sent && region_ring_id(s)) {
         s->role_sent = true;
         const uint64_t ring_id = region_ring_id(s);
         ReleaseSRWLockExclusive(&w->mu);
         wbvn_send(w, WBVN_SHM_ROLE, 0, s->res_id, &ring_id, sizeof(ring_id));
         AcquireSRWLockExclusive(&w->mu);
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
            wbvn_send(w, WBVN_STATUS_CLEAR, 0, s->res_id, op, sizeof(op));
            sent = true;
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
         ReleaseSRWLockExclusive(&w->mu);
         send_push(w, body, count);
         sent = true;
         body->len = 0;
         count = 0;
         AcquireSRWLockExclusive(&w->mu);
      }
   }
   ReleaseSRWLockExclusive(&w->mu);

   if (count) {
      send_push(w, body, count);
      sent = true;
   }
   if (ring_count) {
      send_push(w, &ring, ring_count);
      sent = true;
   }
   free(ring.data);
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

   AcquireSRWLockExclusive(&w->mu);
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
   AcquireSRWLockExclusive(&w->mu);
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
   WakeAllConditionVariable(&w->cv);
   ReleaseSRWLockExclusive(&w->mu);
}

static DWORD WINAPI
wbvn_reader(LPVOID arg)
{
   struct wbvn *w = arg;
   for (;;) {
      struct wbvn_hdr h;
      if (!port_xfer(w, w->read_event, false, &h, sizeof(h)))
         break;
      if (h.len > WBVN_MAX_PAYLOAD)
         break;
      uint8_t *payload = h.len ? malloc(h.len) : NULL;
      if (h.len && !payload)
         break;
      if (h.len && !port_xfer(w, w->read_event, false, payload, h.len)) {
         free(payload);
         break;
      }

      if (h.kind & WBVN_REPLY) {
         AcquireSRWLockExclusive(&w->mu);
         if (!w->reply.done && h.req == w->reply.req) {
            w->reply.done = true;
            w->reply.arg = h.arg;
            w->reply.payload = payload;
            w->reply.len = h.len;
            payload = NULL;
         }
         WakeAllConditionVariable(&w->cv);
         ReleaseSRWLockExclusive(&w->mu);
      } else if (h.kind == WBVN_PUSH) {
         wbvn_apply_push(w, payload, h.len);
      } else if (h.kind == WBVN_SYNC_SIGNAL) {
         wbvn_apply_signals(w, payload, h.len);
      } else if (h.kind == WBVN_LOST) {
         free(payload);
         break;
      }
      free(payload);
   }
   if (!w->quit)
      wbvn_set_lost(w);
   return 0;
}

/* Scans the guest side continuously: the ring may be running and take new commands without a
 * submit to announce them. Spins briefly after activity, then backs off to the timer tick. */
static DWORD WINAPI
wbvn_pump(LPVOID arg)
{
   struct wbvn *w = arg;
   uint32_t idle = 0;
   while (!w->quit && !w->lost) {
      AcquireSRWLockExclusive(&w->push_lock);
      bool sent = wbvn_flush_push(w);
      ReleaseSRWLockExclusive(&w->push_lock);
      if (sent) {
         idle = 0;
      } else if (idle < 4000) {
         idle++;
         if (idle % 16 == 0)
            SwitchToThread();
         else
            YieldProcessor();
      } else {
         Sleep(1);
      }
   }
   return 0;
}

/* A Sleep(1) lasts a whole 15.6 ms timer tick by default, and every wait for the host (the relax
 * loop in vn_ring, the pump) pays that once per round trip. Ask for 0.5 ms while a renderer lives. */
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

   InterlockedExchange(&w->quit, 1);
   if (w->pump) {
      WaitForSingleObject(w->pump, INFINITE);
      CloseHandle(w->pump);
      wbvn_timer_resolution(false);
   }
   if (w->port != INVALID_HANDLE_VALUE) {
      /* unblocks the reader */
      CancelIoEx(w->port, NULL);
   }
   if (w->reader) {
      WaitForSingleObject(w->reader, 5000);
      CloseHandle(w->reader);
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
   if (w->lost)
      return VK_ERROR_DEVICE_LOST;

   struct wbvn_buf msg = { 0 };
   bool ok = batch_encode(&msg, batch);
   if (!ok) {
      free(msg.data);
      return VK_ERROR_OUT_OF_HOST_MEMORY;
   }

   AcquireSRWLockExclusive(&w->push_lock);
   wbvn_flush_push(w);
   ok = wbvn_send(w, WBVN_SUBMIT, 0, 0, msg.data, msg.len);
   ReleaseSRWLockExclusive(&w->push_lock);
   free(msg.data);
   return ok ? VK_SUCCESS : VK_ERROR_DEVICE_LOST;
}

static VkResult
wbvn_wait(struct vn_renderer *renderer, const struct vn_renderer_wait *wait)
{
   struct wbvn *w = (struct wbvn *)renderer;
   const DWORD64 start = GetTickCount64();
   VkResult result = VK_SUCCESS;

   AcquireSRWLockExclusive(&w->mu);
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
   AcquireSRWLockExclusive(&w->mu);
   r->res_id = tab_add(&w->regions, r);
   ReleaseSRWLockExclusive(&w->mu);
   if (!r->res_id)
      return false;
   if (wbvn_request(w, kind, r->res_id, payload, len, NULL, NULL) == 0)
      return true;
   AcquireSRWLockExclusive(&w->mu);
   tab_del(&w->regions, r->res_id);
   ReleaseSRWLockExclusive(&w->mu);
   return false;
}

static void
region_detach(struct wbvn *w, struct wbvn_region *r)
{
   /* the host may still have to see the last words before the resource goes away */
   AcquireSRWLockExclusive(&w->push_lock);
   wbvn_flush_push(w);
   AcquireSRWLockExclusive(&w->mu);
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

   AcquireSRWLockExclusive(&w->push_lock);
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
   AcquireSRWLockExclusive(&w->push_lock);
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
   AcquireSRWLockExclusive(&w->mu);
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
   AcquireSRWLockExclusive(&w->mu);
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
   AcquireSRWLockExclusive(&w->mu);
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
   AcquireSRWLockShared(&w->mu);
   *val = s->value;
   ReleaseSRWLockShared(&w->mu);
   return VK_SUCCESS;
}

/* ---------- start-up ----------------------------------------------------------------- */

static VkResult
wbvn_init(struct wbvn *w)
{
   InitializeSRWLock(&w->write_lock);
   InitializeSRWLock(&w->push_lock);
   InitializeSRWLock(&w->mu);
   InitializeSRWLock(&w->req_lock);
   InitializeConditionVariable(&w->cv);

   w->port = CreateFileW(WBVN_PORT, GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING,
                         FILE_FLAG_OVERLAPPED, NULL);
   if (w->port == INVALID_HANDLE_VALUE) {
      vn_log(w->instance, "cannot open the venus port (error %lu)", GetLastError());
      return VK_ERROR_INITIALIZATION_FAILED;
   }
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

   *renderer = &w->base;
   if (VN_DEBUG(INIT))
      vn_log(instance, "winbridge backend initialized");
   return VK_SUCCESS;
}
