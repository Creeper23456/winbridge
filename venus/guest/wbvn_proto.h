/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * WinBridge Venus channel (WBVN), version 1.
 *
 * Byte stream over the virtio-serial port `org.winbridge.venus.0`, little endian.
 * Every message is a 16-byte header followed by `len` payload bytes. The Rust host side
 * (`crates/winbridge-venus/src/proto.rs`) must stay identical to this file.
 *
 * Venus shared memory is mirrored, not shared: each side keeps a snapshot of what the other
 * side last saw and sends the 8-byte words it changed (`WBVN_PUSH`). Ranges are applied in
 * reverse order of arrival, and both sides scan the ring first, so a ring tail never becomes
 * visible before the commands it covers, and a ring head never before the reply it covers.
 */

#ifndef WBVN_PROTO_H
#define WBVN_PROTO_H

#include <stdint.h>

#define WBVN_VERSION 1u

/* Message kinds. Guest -> host unless noted. A reply has the request kind | WBVN_REPLY. */
enum wbvn_kind {
   /* payload: u32 version. reply payload: u32 version, u32 max_timeline_count,
    * then the venus capset bytes (struct virgl_renderer_capset_venus). reply arg: 0 = ok */
   WBVN_HELLO = 1,
   /* arg: res_id chosen by the guest (nonzero). payload: u64 size. reply arg: 0 = ok.
    * The host creates a memfd of that size, imports it as a venus shmem and attaches it. */
   WBVN_SHM_CREATE = 2,
   /* arg: res_id. reply arg: 0 = ok. */
   WBVN_SHM_DESTROY = 3,
   /* Both directions, no reply. payload: u32 count, u32 reserved, then count ranges of
    * { u32 res_id, u32 offset, u32 len, u32 reserved, data[len] }; offset and len are
    * multiples of 8. The sender lists the ring first; the receiver applies in reverse. */
   WBVN_PUSH = 4,
   /* arg: res_id. payload: u64 ring id (the id vkCreateRingMESA names). No reply. Marks the
    * shmem as a ring (bidirectional, scanned first). The host wakes the ring thread itself
    * with vkNotifyRingMESA when a pushed tail finds the ring idle, because mirrored memory
    * lets the ring go idle after the guest has already looked at the status word. */
   WBVN_SHM_ROLE = 5,
   /* payload: u32 ring_idx, u32 sync_count, u32 cs_size, u32 reserved,
    * then sync_count * { u32 sync_id, u32 reserved, u64 value }, then cs_size bytes.
    * The cs runs on the venus context; when sync_count > 0 the host creates a fence on
    * ring_idx and answers with WBVN_SYNC_SIGNAL once it retires. No reply. */
   WBVN_SUBMIT = 6,
   /* host -> guest. payload: u32 count, u32 reserved, count * { u32 sync_id, u32 reserved,
    * u64 value }. No reply. */
   WBVN_SYNC_SIGNAL = 7,
   /* host -> guest. The host stopped serving the context (device lost). No payload. */
   WBVN_LOST = 8,
   /* arg: res_id. payload: u32 offset, u32 mask. The guest cleared `mask` in the 32-bit word
    * at `offset` of a ring shmem (the status word, whose other bits the host owns). The
    * host does `word &= ~mask` on its memory and on its snapshot. No reply. */
   WBVN_STATUS_CLEAR = 9,
   /* arg: res_id chosen by the guest. payload: struct wbvn_bo_create, then batch_len bytes
    * laid out like a WBVN_SUBMIT payload (the vkWaitRingSeqnoMESA that orders the resource
    * after its vkAllocateMemory; may be empty). The host runs the batch, creates a mappable
    * host3d blob for the device memory with id blob_id, maps it, and mirrors it like a shmem.
    * The host also scans it for GPU writes whenever a fence retires. reply arg: 0 = ok.
    * Destroyed with WBVN_SHM_DESTROY. */
   WBVN_BO_CREATE = 10,
   /* guest -> host. Payload of any size; the reply echoes payload and arg unchanged. Measures the
    * channel alone, the renderer is not involved. */
   WBVN_PING = 11,

   WBVN_REPLY = 0x8000,
};

struct wbvn_hdr {
   uint32_t len;  /* payload bytes after the header */
   uint32_t kind; /* enum wbvn_kind */
   uint32_t req;  /* request id, echoed in the reply */
   uint32_t arg;
};

struct wbvn_push_hdr {
   uint32_t count;
   uint32_t reserved;
};

struct wbvn_push_range {
   uint32_t res_id;
   uint32_t offset;
   uint32_t len;
   uint32_t reserved;
};

struct wbvn_submit_hdr {
   uint32_t ring_idx;
   uint32_t sync_count;
   uint32_t cs_size;
   uint32_t reserved;
};

struct wbvn_sync_point {
   uint32_t sync_id;
   uint32_t reserved;
   uint64_t value;
};

/* WBVN_BO_CREATE payload header. */
struct wbvn_bo_create {
   uint64_t size;
   uint64_t blob_id; /* the vn_object_id of the VkDeviceMemory */
   uint32_t blob_flags; /* WBVN_BLOB_* */
   uint32_t batch_len;
};

#define WBVN_BLOB_MAPPABLE 1u

/* Largest payload either side accepts. */
#define WBVN_MAX_PAYLOAD (64u << 20)

#endif
