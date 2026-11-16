/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * Single-producer single-consumer record ring of the WinBridge shared-memory transport, as the
 * guest uses it. The layout is the one `crates/winbridge-shm` implements (`ring.rs`) and must
 * stay identical; `crates/winbridge-venus/tests/ring_interop.rs` runs this file against the Rust
 * side. It has no dependency on Windows, so that test builds it with a plain `cc`.
 *
 * Ring (at the lane's `offset`): a 128-byte header, then `size` data bytes (a power of two).
 *   +0   u64 head   free-running byte count, written by the producer
 *   +8   u32 flags  producer: bit 0 closed
 *   +64  u64 tail   free-running byte count, written by the consumer
 *   +72  u32 flags  consumer: bit 0 sleeping, bit 1 closed
 * Record: u32 payload length, u16 tag, u16 flags, payload, zero padding to a multiple of 8.
 * A record never wraps: a skip marker (flag 0x8000) covers what is left before the end.
 *
 * The producer writes the record, issues a store fence, then stores `head` with release order.
 * The consumer loads `head` with acquire order, reads the record, then stores `tail`.
 * Nothing the peer writes is trusted: indices are validated and a violation closes the ring.
 */

#ifndef WBVN_RING_H
#define WBVN_RING_H

#include <stdbool.h>
#include <stdint.h>
#include <string.h>

#if defined(__x86_64__) || defined(_M_X64)
#include <emmintrin.h>
#endif

#define WBVN_RING_HEADER 128u
#define WBVN_REC_HEADER 8u
#define WBVN_REC_SKIP 0x8000u
/* Same cap as `DEFAULT_MAX_RECORD` of the Rust ring. */
#define WBVN_RING_MAX_RECORD (4u << 20)

#define WBVN_PROD_CLOSED 1u
#define WBVN_CONS_SLEEPING 1u
#define WBVN_CONS_CLOSED 2u

/* A mask that keeps `index + record` free of overflow: honest peers never get near 2^62. */
#define WBVN_RING_MAX_INDEX (1ull << 62)

enum wbvn_ring_status {
   WBVN_RING_OK = 0,
   WBVN_RING_FULL = 1,   /* push: no room now; peek: nothing to read */
   WBVN_RING_CLOSED = -1 /* the peer closed it or broke the protocol */
};

struct wbvn_ring {
   uint8_t *hdr;
   uint8_t *data;
   uint64_t size;
   /* producer: its head; consumer: the last head it saw */
   uint64_t head;
   /* consumer: its tail; producer: the last tail it saw */
   uint64_t tail;
   uint64_t pending; /* consumer: bytes of the record `peek` returned */
   uint32_t max_record;
   bool closed;
};

#define wbvn_ring_ld64(p, order) __atomic_load_n((const uint64_t *)(p), order)
#define wbvn_ring_st64(p, v, order) __atomic_store_n((uint64_t *)(p), (v), order)
#define wbvn_ring_ld32(p, order) __atomic_load_n((const uint32_t *)(p), order)

static inline void
wbvn_ring_publish_fence(void)
{
   __atomic_signal_fence(__ATOMIC_SEQ_CST);
#if defined(__x86_64__) || defined(_M_X64)
   _mm_sfence();
#else
   __atomic_thread_fence(__ATOMIC_RELEASE);
#endif
}

static inline uint64_t
wbvn_ring_round8(uint64_t n)
{
   return (n + 7) & ~7ull;
}

/* The ring header and data are `hdr` and `hdr + WBVN_RING_HEADER`; `size` is the data size. */
static inline bool
wbvn_ring_attach(struct wbvn_ring *r, uint8_t *hdr, uint64_t size, bool producer)
{
   if (size < 64 || (size & (size - 1)))
      return false;
   memset(r, 0, sizeof(*r));
   r->hdr = hdr;
   r->data = hdr + WBVN_RING_HEADER;
   r->size = size;
   r->head = wbvn_ring_ld64(hdr, __ATOMIC_ACQUIRE);
   r->tail = wbvn_ring_ld64(hdr + 64, __ATOMIC_ACQUIRE);
   r->max_record = WBVN_RING_MAX_RECORD < size - WBVN_REC_HEADER
                      ? WBVN_RING_MAX_RECORD
                      : (uint32_t)(size - WBVN_REC_HEADER);
   if (r->tail > r->head || r->head > WBVN_RING_MAX_INDEX || r->head - r->tail > size ||
       (r->head | r->tail) & 7)
      return false;
   /* our own flags word starts clean */
   __atomic_store_n((uint32_t *)(hdr + (producer ? 8 : 72)), 0, __ATOMIC_RELEASE);
   return true;
}

/* ---------- producer ---------------------------------------------------------------- */

static inline bool
wbvn_ring_has_room(struct wbvn_ring *r, uint64_t need, bool *corrupt)
{
   if (r->size - (r->head - r->tail) >= need)
      return true;
   const uint64_t tail = wbvn_ring_ld64(r->hdr + 64, __ATOMIC_ACQUIRE);
   if (tail < r->tail || tail > r->head || (tail & 7)) {
      *corrupt = true;
      return false;
   }
   r->tail = tail;
   return r->size - (r->head - r->tail) >= need;
}

static inline void
wbvn_ring_publish(struct wbvn_ring *r, uint64_t head)
{
   wbvn_ring_publish_fence();
   wbvn_ring_st64(r->hdr, head, __ATOMIC_RELEASE);
   r->head = head;
}

/* Appends a record. On OK, `*kick` tells whether the consumer sleeps and the ring was empty:
 * the caller then wakes it over the serial port. */
static inline int
wbvn_ring_push(struct wbvn_ring *r, const void *payload, uint32_t len, bool *kick)
{
   *kick = false;
   if (r->closed)
      return WBVN_RING_CLOSED;
   if (len > r->max_record)
      return WBVN_RING_CLOSED;
   if (wbvn_ring_ld32(r->hdr + 72, __ATOMIC_ACQUIRE) & WBVN_CONS_CLOSED) {
      r->closed = true;
      return WBVN_RING_CLOSED;
   }
   bool corrupt = false;
   const uint64_t total = WBVN_REC_HEADER + wbvn_ring_round8(len);
   const uint64_t start_head = r->head;
   uint64_t pos = r->head & (r->size - 1);
   if (total > r->size - pos) {
      const uint64_t skip = r->size - pos;
      if (!wbvn_ring_has_room(r, skip, &corrupt))
         goto blocked;
      wbvn_ring_st64(r->data + pos, (uint64_t)WBVN_REC_SKIP << 48, __ATOMIC_RELAXED);
      wbvn_ring_publish(r, r->head + skip);
   }
   if (!wbvn_ring_has_room(r, total, &corrupt))
      goto blocked;
   pos = r->head & (r->size - 1);
   uint8_t *dst = r->data + pos;
   wbvn_ring_st64(dst, len, __ATOMIC_RELAXED);
   memcpy(dst + WBVN_REC_HEADER, payload, len);
   memset(dst + WBVN_REC_HEADER + len, 0, total - WBVN_REC_HEADER - len);
   wbvn_ring_publish(r, r->head + total);
   /* pairs with the consumer's set-sleeping fence: either it sees our head, or we see its flag */
   __atomic_thread_fence(__ATOMIC_SEQ_CST);
   if (wbvn_ring_ld32(r->hdr + 72, __ATOMIC_SEQ_CST) & WBVN_CONS_SLEEPING)
      *kick = wbvn_ring_ld64(r->hdr + 64, __ATOMIC_ACQUIRE) == start_head;
   return WBVN_RING_OK;

blocked:
   if (corrupt) {
      r->closed = true;
      __atomic_fetch_or((uint32_t *)(r->hdr + 8), WBVN_PROD_CLOSED, __ATOMIC_RELEASE);
      return WBVN_RING_CLOSED;
   }
   return WBVN_RING_FULL;
}

static inline void
wbvn_ring_close_producer(struct wbvn_ring *r)
{
   r->closed = true;
   __atomic_fetch_or((uint32_t *)(r->hdr + 8), WBVN_PROD_CLOSED, __ATOMIC_RELEASE);
}

/* ---------- consumer ---------------------------------------------------------------- */

static inline int
wbvn_ring_corrupt(struct wbvn_ring *r)
{
   r->closed = true;
   __atomic_fetch_or((uint32_t *)(r->hdr + 72), WBVN_CONS_CLOSED, __ATOMIC_SEQ_CST);
   return WBVN_RING_CLOSED;
}

/* Whether a record may be waiting (cheap; the head is validated by `peek`). */
static inline bool
wbvn_ring_ready(const struct wbvn_ring *r)
{
   return wbvn_ring_ld64(r->hdr, __ATOMIC_ACQUIRE) != r->tail;
}

/* Finds the next record. OK: `*payload` and `*len` describe it inside the ring and stay valid
 * until `consume`. FULL: nothing to read. CLOSED: the ring is finished. */
static inline int
wbvn_ring_peek(struct wbvn_ring *r, const uint8_t **payload, uint32_t *len)
{
   if (r->closed)
      return WBVN_RING_CLOSED;
   for (;;) {
      const uint64_t head = wbvn_ring_ld64(r->hdr, __ATOMIC_ACQUIRE);
      if (head < r->head || head > WBVN_RING_MAX_INDEX || head - r->tail > r->size || (head & 7))
         return wbvn_ring_corrupt(r);
      r->head = head;
      const uint64_t avail = head - r->tail;
      if (!avail) {
         if (wbvn_ring_ld32(r->hdr + 8, __ATOMIC_ACQUIRE) & WBVN_PROD_CLOSED) {
            r->closed = true;
            return WBVN_RING_CLOSED;
         }
         return WBVN_RING_FULL;
      }
      const uint64_t pos = r->tail & (r->size - 1);
      const uint64_t w = wbvn_ring_ld64(r->data + pos, __ATOMIC_RELAXED);
      const uint64_t n = w & 0xffffffffull;
      const uint32_t flags = (uint32_t)(w >> 48);
      if (flags & WBVN_REC_SKIP) {
         const uint64_t skip = r->size - pos;
         if (pos == 0 || skip > avail)
            return wbvn_ring_corrupt(r);
         r->tail += skip;
         wbvn_ring_st64(r->hdr + 64, r->tail, __ATOMIC_RELEASE);
         continue;
      }
      const uint64_t total = WBVN_REC_HEADER + wbvn_ring_round8(n);
      if (n > r->max_record || total > avail || pos + total > r->size)
         return wbvn_ring_corrupt(r);
      *payload = r->data + pos + WBVN_REC_HEADER;
      *len = (uint32_t)n;
      r->pending = total;
      return WBVN_RING_OK;
   }
}

/* Releases the record `peek` returned. */
static inline void
wbvn_ring_consume(struct wbvn_ring *r)
{
   r->tail += r->pending;
   r->pending = 0;
   wbvn_ring_st64(r->hdr + 64, r->tail, __ATOMIC_RELEASE);
}

/* Publishes or clears the sleeping flag. Protocol: set, check `ready` again, sleep only if the
 * ring is still empty, clear on wake. */
static inline void
wbvn_ring_set_sleeping(struct wbvn_ring *r, bool on)
{
   if (on)
      __atomic_fetch_or((uint32_t *)(r->hdr + 72), WBVN_CONS_SLEEPING, __ATOMIC_SEQ_CST);
   else
      __atomic_fetch_and((uint32_t *)(r->hdr + 72), ~WBVN_CONS_SLEEPING, __ATOMIC_SEQ_CST);
   __atomic_thread_fence(__ATOMIC_SEQ_CST);
}

#endif
