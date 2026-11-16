/* SPDX-License-Identifier: GPL-3.0-or-later */

/*
 * The guest end of `crates/winbridge-venus/tests/ring_interop.rs`: claims the slot in the file
 * named by argv[1] with the guest's ring code (`wbvn_ring.h`) and exchanges records with the
 * Rust host in both directions at once. Each side checks every byte it receives. Exit 0 and a
 * line "ok" mean that it all matched.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>

#include "../guest/wbvn_proto.h"
#include "../guest/wbvn_ring.h"

#define RECORDS 10000u
#define MAX_LEN 70000u

static struct wbvn_ring tx, rx;
static volatile int failed;

/* The generators the Rust side has too. */
static uint32_t
rec_len(uint32_t seed, uint32_t i)
{
   return 1 + (uint32_t)(((uint64_t)(i + seed) * 2654435761u) % MAX_LEN);
}

static uint8_t
rec_byte(uint32_t seed, uint32_t i, uint32_t j)
{
   return (uint8_t)((i + seed) * 31 + j);
}

static void
fail(const char *what, uint32_t i)
{
   fprintf(stderr, "ring_peer: %s (record %u)\n", what, i);
   failed = 1;
}

static void *
sender(void *arg)
{
   (void)arg;
   uint8_t *buf = malloc(MAX_LEN);
   for (uint32_t i = 0; i < RECORDS && !failed; i++) {
      const uint32_t len = rec_len(1, i);
      for (uint32_t j = 0; j < len; j++)
         buf[j] = rec_byte(1, i, j);
      bool kick;
      int r;
      while ((r = wbvn_ring_push(&tx, buf, len, &kick)) == WBVN_RING_FULL && !failed)
         sched_yield();
      if (r != WBVN_RING_OK) {
         fail("push failed", i);
         break;
      }
   }
   free(buf);
   return NULL;
}

static void *
receiver(void *arg)
{
   (void)arg;
   for (uint32_t i = 0; i < RECORDS && !failed; i++) {
      const uint8_t *p;
      uint32_t len;
      int r;
      while ((r = wbvn_ring_peek(&rx, &p, &len)) == WBVN_RING_FULL && !failed)
         sched_yield();
      if (r != WBVN_RING_OK) {
         fail("peek failed", i);
         break;
      }
      if (len != rec_len(2, i)) {
         fail("wrong length", i);
         break;
      }
      for (uint32_t j = 0; j < len; j++) {
         if (p[j] != rec_byte(2, i, j)) {
            fail("wrong byte", i);
            return NULL;
         }
      }
      wbvn_ring_consume(&rx);
   }
   return NULL;
}

int
main(int argc, char **argv)
{
   if (argc != 2) {
      fprintf(stderr, "usage: ring_peer FILE\n");
      return 2;
   }
   const int fd = open(argv[1], O_RDWR);
   if (fd < 0) {
      perror("open");
      return 2;
   }
   uint8_t *bar = mmap(NULL, WBVN_SLOT_LEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
   if (bar == MAP_FAILED) {
      perror("mmap");
      return 2;
   }

   /* The host sets the region up; wait for it like the driver does. */
   for (int i = 0; i < 1000 && !*(volatile uint64_t *)bar; i++)
      usleep(1000);
   if (*(uint64_t *)bar != WBVN_SHM_MAGIC || *(uint32_t *)(bar + 0x24) != WBVN_SHM_ROLE_CLIENT ||
       !(*(uint64_t *)(bar + 0x10) & WBVN_SHM_CAP_VENUS) || *(uint32_t *)(bar + 0x28) != 2) {
      fprintf(stderr, "ring_peer: not a Venus region\n");
      return 1;
   }
   uint64_t off[2] = { 0 }, size[2] = { 0 };
   for (uint32_t i = 0; i < 2; i++) {
      const uint8_t *e = bar + WBVN_SHM_DIR + i * WBVN_SHM_DIR_ENTRY;
      const uint16_t id = *(const uint16_t *)e;
      if (id == WBVN_LANE_G2H && e[2] == 1) {
         off[0] = *(const uint64_t *)(e + 8);
         size[0] = *(const uint64_t *)(e + 16);
      } else if (id == WBVN_LANE_H2G && e[2] == 0) {
         off[1] = *(const uint64_t *)(e + 8);
         size[1] = *(const uint64_t *)(e + 16);
      }
   }
   if (!size[0] || !size[1] || size[0] != WBVN_LANE_SIZE) {
      fprintf(stderr, "ring_peer: lanes are missing\n");
      return 1;
   }
   if (*(uint64_t *)(bar + WBVN_SLOT_BLOCK + WBVN_SLOT_OFF_MAGIC) != WBVN_SLOT_MAGIC) {
      fprintf(stderr, "ring_peer: no slot block\n");
      return 1;
   }
   uint32_t expect = WBVN_SLOT_FREE;
   uint32_t *state = (uint32_t *)(bar + WBVN_SLOT_BLOCK + WBVN_SLOT_OFF_STATE);
   if (!__atomic_compare_exchange_n(state, &expect, WBVN_SLOT_CLAIMED, false, __ATOMIC_ACQ_REL,
                                    __ATOMIC_ACQUIRE)) {
      fprintf(stderr, "ring_peer: the slot is not free (%u)\n", expect);
      return 1;
   }
   *(uint64_t *)(bar + WBVN_SLOT_BLOCK + WBVN_SLOT_OFF_NONCE) = 0x1234;
   if (!wbvn_ring_attach(&tx, bar + off[0], size[0], true) ||
       !wbvn_ring_attach(&rx, bar + off[1], size[1], false)) {
      fprintf(stderr, "ring_peer: cannot attach the lanes\n");
      return 1;
   }

   pthread_t s, r;
   pthread_create(&s, NULL, sender, NULL);
   pthread_create(&r, NULL, receiver, NULL);
   pthread_join(s, NULL);
   pthread_join(r, NULL);
   __atomic_store_n(state, WBVN_SLOT_CLOSED, __ATOMIC_RELEASE);
   if (failed)
      return 1;
   printf("ok\n");
   return 0;
}
