/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * WinBridge Venus channel (WBVN), version 2.
 * A byte stream, little endian, carried by the records of two lanes in an ivshmem region (the
 * guest to host lane and the host to guest lane, see `wbvn_ring.h`); a message may span several
 * records and a record may hold several messages. A virtio-serial port `org.winbridge.venus.N`
 * carries one wake-up byte whenever the peer has said that it sleeps. Every message is a
 * 16-byte header followed by `len` payload bytes. The Rust host side
 * (`crates/winbridge-venus/src/proto.rs`, `slot.rs`) must stay identical to this file.
 *
 * Venus shared memory is mirrored, not shared: each side keeps a snapshot of what the other
 * side last saw and sends the 8-byte words it changed (`WBVN_PUSH`). Ranges are applied in
 * reverse order of arrival, and both sides scan the ring first, so a ring tail never becomes
 * visible before the commands it covers, and a ring head never before the reply it covers.
 */

#ifndef WBVN_PROTO_H
#define WBVN_PROTO_H

/* ---- the ivshmem slot (`crates/winbridge-venus/src/slot.rs`) ----
 *
 * A guest Vulkan process maps one 64 MiB ivshmem device, however many Vulkan instances it creates;
 * they share the region, its lanes and the serial port, and each one is a venus context on the
 * host (see WBVN_TAG). The region is a `winbridge-shm` region
 * (header page, lane directory at 0x40, rings after the header page): lane 1 carries guest to
 * host, lane 2 host to guest, 16 MiB of data each. The host lays the region out and publishes
 * the slot block last. */
#define WBVN_SLOT_LEN (64ull << 20)
#define WBVN_LANE_G2H 1u
#define WBVN_LANE_H2G 2u
#define WBVN_LANE_SIZE (16ull << 20)
/* region header (`winbridge-shm`) */
#define WBVN_SHM_MAGIC 0x004d454d48534257ull /* "WBSHMEM\0" */
#define WBVN_SHM_VERSION 1u
#define WBVN_SHM_HEADER_LEN 4096u
#define WBVN_SHM_ROLE_CLIENT 3u
#define WBVN_SHM_CAP_VENUS 1ull
#define WBVN_SHM_DIR 0x40u
#define WBVN_SHM_DIR_ENTRY 32u
/* slot block in the header page */
#define WBVN_SLOT_BLOCK 0x800u
#define WBVN_SLOT_MAGIC 0x544f4c534e564257ull /* "WBVNSLOT" */
#define WBVN_SLOT_VERSION 1u
#define WBVN_SLOT_OFF_MAGIC 0x00u
#define WBVN_SLOT_OFF_VERSION 0x08u
#define WBVN_SLOT_OFF_ID 0x0Cu
#define WBVN_SLOT_OFF_STATE 0x10u
#define WBVN_SLOT_OFF_NONCE 0x18u
#define WBVN_SLOT_OFF_HEARTBEAT 0x20u
/* states: the host frees the slot, a guest claims it and closes it, the host resets it */
#define WBVN_SLOT_FREE 0u
#define WBVN_SLOT_CLAIMED 1u
#define WBVN_SLOT_CLOSED 2u

#include <stdint.h>

#define WBVN_VERSION 2u

/* The `kind` word of a guest message carries a context id in its high half. Every Vulkan instance
 * of a process is a venus context on the host, and the messages that act on one (HELLO,
 * SHM_CREATE, BO_CREATE, SUBMIT, CTX_DESTROY) name it. Shmem, bo and sync ids are unique across
 * the slot, so the other messages need no context. Replies and host messages carry none. */
#define WBVN_KIND_MASK 0xFFFFu
#define WBVN_CTX_SHIFT 16
#define WBVN_CTX_MAX 0xFFFFu
#define WBVN_TAG(kind, ctx) ((uint32_t)(kind) | ((uint32_t)(ctx) << WBVN_CTX_SHIFT))

/* Message kinds. Guest -> host unless noted. A reply has the request kind | WBVN_REPLY. */
enum wbvn_kind {
   /* The context id is in the kind word (nonzero, chosen by the guest); the host creates the
    * context. payload: u32 version. reply payload: u32 version, u32 max_timeline_count,
    * then the venus capset bytes (struct virgl_renderer_capset_venus). reply arg: 0 = ok */
   WBVN_HELLO = 1,
   /* arg: res_id chosen by the guest (nonzero, unique across the slot). The kind word names the
    * context. payload: u64 size. reply arg: 0 = ok.
    * The host has the renderer create a shmem of that size (blob id 0), maps it, and attaches it;
    * the renderer knows it when the reply arrives, so the guest may name it in a command at once. */
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
   /* The kind word names the context. payload: u32 ring_idx, u32 sync_count, u32 cs_size,
    * u32 reserved, then sync_count * { u32 sync_id, u32 reserved, u64 value }, then cs_size
    * bytes. The cs runs on that venus context; when sync_count > 0 the host creates a fence on
    * ring_idx and answers with WBVN_SYNC_SIGNAL once it retires. No reply. */
   WBVN_SUBMIT = 6,
   /* host -> guest. payload: u32 count, u32 reserved, count * { u32 sync_id, u32 reserved,
    * u64 value }. No reply. */
   WBVN_SYNC_SIGNAL = 7,
   /* host -> guest. arg: the context the host stopped serving (the instance is lost); the
    * session goes on for the others. No payload. */
   WBVN_LOST = 8,
   /* arg: res_id. payload: u32 offset, u32 mask. The guest cleared `mask` in the 32-bit word
    * at `offset` of a ring shmem (the status word, whose other bits the host owns). The
    * host does `word &= ~mask` on its memory and on its snapshot. No reply. */
   WBVN_STATUS_CLEAR = 9,
   /* arg: res_id chosen by the guest (unique across the slot); the kind word names the context.
    * payload: struct wbvn_bo_create, then batch_len bytes
    * laid out like a WBVN_SUBMIT payload (the vkWaitRingSeqnoMESA that orders the resource
    * after its vkAllocateMemory; may be empty). The host runs the batch, creates a mappable
    * host3d blob for the device memory with id blob_id, maps it, and mirrors it like a shmem.
    * The host also scans it for GPU writes whenever a fence retires. reply arg: 0 = ok.
    * Destroyed with WBVN_SHM_DESTROY. */
   WBVN_BO_CREATE = 10,
   /* guest -> host. Payload of any size; the reply echoes payload and arg unchanged. Measures the
    * channel alone, the renderer is not involved. */
   WBVN_PING = 11,
   /* The kind word names the context; the guest destroyed its Vulkan instance. The host releases
    * the context and what it still holds. reply arg: 0 = ok. */
   WBVN_CTX_DESTROY = 12,

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
