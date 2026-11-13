// SPDX-License-Identifier: GPL-2.0-or-later
/* The rules for pointer and file fields. */
#include "rmargs.h"

#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define RM_CMD(nr, size) (0xc0000000u | (uint32_t)(size) << 16 | 'F' << 8 | (nr))
#define CTRL_FB_GET_INFO 0x20801301u /* one embedded list: pointer at 8, count at 0, 8 bytes each */
#define CTRL_EXPORT_FD 0x3d05u       /* fd at 16 of its parameters */
#define CTRL_NO_PUBLIC_SOURCE 0x20808163u /* forwarded at exactly 4 bytes and no other size */
#define CTRL_NO_PUBLIC_SOURCE_BYTES 4
#define CLASS_CONTEXT_SHARE 0x9067 /* parameters of 12 bytes in branch 615, 16 in 620 */
#define FUZZ_ROUNDS 3000000

static uint8_t data[4096];
static RmField fields[RMARGS_MAX_FIELDS];
static int matched[RMARGS_MAX_FIELDS];

static void put32(uint32_t at, uint32_t v)
{
    memcpy(data + at, &v, sizeof(v));
}

static int walk(uint32_t cmd, uint32_t len, const struct nvfwd_fix *fix, uint32_t nfix)
{
    RmCall call;

    if (!rmargs_parse(&call, cmd, data, len, fix, nfix)) {
        return -EBADMSG;
    }
    return rmargs_fields(&call, fields, matched);
}

static void test_parse(void)
{
    RmCall c;

    memset(data, 0, sizeof(data));
    assert(rmargs_parse(&c, RM_CMD(ESC_RM_CONTROL, 32), data, 32, NULL, 0));
    assert(c.nr == ESC_RM_CONTROL && c.arg_at == 0 && c.size == 32);

    /* Another ioctl type, and an ioctl longer than its call. */
    assert(!rmargs_parse(&c, 0xc020462a ^ 0x0100, data, 32, NULL, 0));
    assert(!rmargs_parse(&c, 0xc0204b2a, data, 32, NULL, 0));
    assert(!rmargs_parse(&c, RM_CMD(ESC_RM_CONTROL, 64), data, 32, NULL, 0)); /* longer than the call */

    put32(RM_XFER_CMD, ESC_RM_ALLOC);
    put32(RM_XFER_SIZE, 48);
    assert(rmargs_parse(&c, RM_CMD(ESC_XFER, 16), data, 64, NULL, 0));
    assert(c.nr == ESC_RM_ALLOC && c.arg_at == 16 && c.size == 48);
    assert(!rmargs_parse(&c, RM_CMD(ESC_XFER, 16), data, 63, NULL, 0));
    assert(!rmargs_parse(&c, RM_CMD(ESC_XFER, 8), data, 64, NULL, 0));
    put32(RM_XFER_CMD, 0x100 | ESC_RM_ALLOC); /* the driver reads the whole word */
    assert(!rmargs_parse(&c, RM_CMD(ESC_XFER, 16), data, 64, NULL, 0));
}

static int control_of_size(uint32_t cmd, uint32_t size)
{
    struct nvfwd_fix fix = { NVFWD_FIX_PTR, RM_CONTROL_PARAMS, 64, 0 };

    memset(data, 0, sizeof(data));
    put32(RM_CONTROL_CMD, cmd);
    put32(RM_CONTROL_PARAMS_SIZE, size);
    return walk(RM_CMD(ESC_RM_CONTROL, 32), 192, &fix, 1);
}

/* Nobody can read what this control does, so its size is the one thing known about it. */
static void test_a_pinned_control_goes_at_its_size_only(void)
{
    static const uint32_t wrong[] = { 0, 1, 3, 5, 8, 64, 100, UINT32_MAX };

    assert(control_of_size(CTRL_NO_PUBLIC_SOURCE, CTRL_NO_PUBLIC_SOURCE_BYTES) == 1);
    for (size_t i = 0; i < sizeof(wrong) / sizeof(wrong[0]); i++) {
        assert(control_of_size(CTRL_NO_PUBLIC_SOURCE, wrong[i]) == -EINVAL);
    }
    /* Others aren't held to it. */
    assert(control_of_size(0x20800110u, 100) == 1);
}

static void test_xfer_points_at_what_was_read(void)
{
    struct nvfwd_fix good = { NVFWD_FIX_PTR, RM_XFER_POINTER, RM_XFER_BYTES, 0 };
    struct nvfwd_fix elsewhere = { NVFWD_FIX_PTR, RM_XFER_POINTER, 1024, 0 };

    memset(data, 0, sizeof(data));
    put32(RM_XFER_CMD, ESC_RM_FREE);
    put32(RM_XFER_SIZE, 16);
    assert(walk(RM_CMD(ESC_XFER, 16), 2048, &good, 1) == 1);
    assert(walk(RM_CMD(ESC_XFER, 16), 2048, &elsewhere, 1) == -EINVAL);
    assert(walk(RM_CMD(ESC_XFER, 16), 2048, NULL, 0) == -EINVAL);
}

static void test_control(void)
{
    struct nvfwd_fix fix[3] = {
        { NVFWD_FIX_PTR, RM_CONTROL_PARAMS, 64, 0 },
        { NVFWD_FIX_PTR, 64 + 8, 128, 0 },
    };

    memset(data, 0, sizeof(data));
    put32(RM_CONTROL_CMD, CTRL_FB_GET_INFO);
    put32(RM_CONTROL_PARAMS_SIZE, 16);
    put32(64, 4); /* four list entries */

    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == 2);
    assert(matched[0] == 0 && matched[1] == 1);

    /* A field with no fix is still listed: it is cleared, never passed on. */
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 1) == 2);
    assert(matched[1] == -1 && fields[1].kind == NVFWD_FIX_PTR && fields[1].at == 72);
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, NULL, 0) == 1 && matched[0] == -1);

    /* The list must fit behind its pointer, and the count times the size must not wrap. */
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 160, fix, 2) == 2);
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 159, fix, 2) == -EINVAL);
    put32(64, 0x20000001); /* times 8 this is 8 in 32 bits */
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == -EINVAL);
    put32(64, 4);

    /* Parameters too short to hold the list's own fields. */
    put32(RM_CONTROL_PARAMS_SIZE, 8);
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == -EINVAL);
    put32(RM_CONTROL_PARAMS_SIZE, 16);

    /* A size past the end of the call. */
    put32(RM_CONTROL_PARAMS_SIZE, 4096);
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == -EINVAL);
    put32(RM_CONTROL_PARAMS_SIZE, 16);

    /* A fix anywhere but a field, and two on one field. */
    fix[1].at = 40;
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == -EINVAL);
    fix[1].at = 72;
    fix[2] = fix[1];
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 3) == -EINVAL);

    /* The wrong kind of fix on a field. */
    fix[1].kind = NVFWD_FIX_FD;
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == -EINVAL);
    fix[1].kind = NVFWD_FIX_PAGES;
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 192, fix, 2) == -EINVAL);

    /* An fd in the parameters. */
    memset(data, 0, sizeof(data));
    put32(RM_CONTROL_CMD, CTRL_EXPORT_FD);
    put32(RM_CONTROL_PARAMS_SIZE, 32);
    fix[0] = (struct nvfwd_fix){ NVFWD_FIX_PTR, RM_CONTROL_PARAMS, 64, 0 };
    fix[1] = (struct nvfwd_fix){ NVFWD_FIX_FD, 64 + 16, 3, 0 };
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 96, fix, 2) == 2 && fields[1].kind == NVFWD_FIX_FD);
    assert(walk(RM_CMD(ESC_RM_CONTROL, 32), 96, fix, 1) == 2 && matched[1] == -1);
}

static void test_alloc_and_memory(void)
{
    struct nvfwd_fix params = { NVFWD_FIX_PTR, RM_ALLOC_PARAMS, 64, 0 };
    struct nvfwd_fix pages = { NVFWD_FIX_PAGES, RM_MEMORY_POINTER, 64, 0 };
    struct nvfwd_fix event[2] = { params, { NVFWD_FIX_FD, 64 + RM_EVENT_DATA, 3, 0 } };

    memset(data, 0, sizeof(data));
    put32(RM_ALLOC_CLASS, 0x80);
    put32(RM_ALLOC_LONG_PARAMS_SIZE, 0); /* size from the class table: 56 for a device */
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 120, &params, 1) == 2);
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 119, &params, 1) == -EINVAL);
    assert(walk(RM_CMD(ESC_RM_ALLOC, 40), 120, &params, 1) == -EINVAL);

    memset(data, 0, sizeof(data));
    put32(RM_ALLOC_CLASS, CLASS_EVENT);
    put32(RM_ALLOC_LONG_PARAMS_SIZE, 24);
    put32(64 + RM_EVENT_KIND, CLASS_OS_EVENT);
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 128, event, 2) == 3);
    put32(64 + RM_EVENT_KIND, 0x78); /* any kind but an OS event */
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 128, event, 1) == -EPERM);

    memset(data, 0, sizeof(data));
    put32(RM_MEMORY_CLASS, CLASS_OS_DESCRIPTOR);
    assert(walk(RM_CMD(ESC_RM_ALLOC_MEMORY, 56), 128, &pages, 1) == 2);
    assert(walk(RM_CMD(ESC_RM_ALLOC_MEMORY, 56), 128, NULL, 0) == 2 && matched[0] == -1);
    put32(RM_MEMORY_CLASS, CLASS_MEMORY_SYSTEM); /* guest pages are for OS descriptors only */
    assert(walk(RM_CMD(ESC_RM_ALLOC_MEMORY, 56), 128, &pages, 1) == -EINVAL);

    memset(data, 0, sizeof(data));
    put32(8, 27); /* pins an address, and guest memory has its own way in */
    assert(walk(RM_CMD(ESC_RM_VID_HEAP_CONTROL, 184), 184, NULL, 0) == -EPERM);
    put32(8, 2);
    put32(12, 27); /* a handle, which may be anything */
    assert(walk(RM_CMD(ESC_RM_VID_HEAP_CONTROL, 184), 184, NULL, 0) == 0);
    assert(walk(RM_CMD(ESC_RM_VID_HEAP_CONTROL, 176), 184, NULL, 0) == -EINVAL);

    /* No fields, so no fix has anywhere to land. */
    assert(walk(RM_CMD(ESC_RM_FREE, 16), 128, &params, 1) == -EINVAL);
}

static void test_a_branch_picks_its_class_sizes(void)
{
    struct nvfwd_fix params = { NVFWD_FIX_PTR, RM_ALLOC_PARAMS, 64, 0 };

    memset(data, 0, sizeof(data));
    put32(RM_ALLOC_CLASS, CLASS_CONTEXT_SHARE);
    put32(RM_ALLOC_LONG_PARAMS_SIZE, 0);
    assert(!rmargs_use_driver("610.1.2") && !rmargs_use_driver("6150.1") && !rmargs_use_driver(""));
    assert(rmargs_use_driver("615.78.08"));
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 76, &params, 1) == 2);
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 80, &params, 1) == 2);
    assert(rmargs_use_driver("620.70.06"));
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 76, &params, 1) == -EINVAL);
    assert(walk(RM_CMD(ESC_RM_ALLOC, 48), 80, &params, 1) == 2);
    assert(rmargs_use_driver("615.78.08"));
}

static void test_uvm(void)
{
    struct nvfwd_fix fd = { NVFWD_FIX_FD, 24, 3, 0 };

    assert(rmargs_uvm_fields(37, 40, &fd, 1, fields, matched) == 1); /* UVM_REGISTER_GPU */
    assert(rmargs_uvm_fields(37, 40, NULL, 0, fields, matched) == 1 && matched[0] == -1);
    assert(rmargs_uvm_fields(37, 39, &fd, 1, fields, matched) == -EINVAL);
    assert(rmargs_uvm_fields(39, 8, &fd, 1, fields, matched) == -EINVAL);
    assert(rmargs_uvm_fields(0xdead, 8, NULL, 0, fields, matched) == -EPERM);
}

/* Anything the walk accepts stays inside the call, whatever the call holds. */
static void fuzz(void)
{
    static const uint32_t escapes[] = {
        ESC_RM_CONTROL, ESC_RM_ALLOC, ESC_RM_ALLOC_MEMORY, ESC_RM_MAP_MEMORY, ESC_REGISTER_FD,
        ESC_ALLOC_OS_EVENT, ESC_RM_IDLE_CHANNELS, ESC_RM_VID_HEAP_CONTROL, ESC_XFER, ESC_RM_FREE,
    };
    static const uint32_t controls[] = { CTRL_FB_GET_INFO, CTRL_EXPORT_FD, 0x101, 0x80170d, 0x3d06 };
    uint64_t accepted = 0;

    for (int round = 0; round < FUZZ_ROUNDS; round++) {
        uint32_t len = random() % 512, nfix = random() % 6;
        uint8_t *call = malloc(len ? len : 1); /* exact, so ASan sees any read past it */
        struct nvfwd_fix fix[6];
        uint32_t nr = escapes[random() % (sizeof(escapes) / sizeof(escapes[0]))];
        uint32_t size = random() % 3 ? (random() % 4 ? 32 : 48) : random() % 256;
        RmCall c;
        int n;

        for (uint32_t i = 0; i < len; i++) {
            call[i] = random() % 4 ? 0 : random();
        }
        if (len >= 32) {
            uint32_t ctl = controls[random() % 5], small = random() % 64;

            memcpy(call + RM_CONTROL_CMD, &ctl, 4);
            memcpy(call + RM_CONTROL_PARAMS_SIZE, &small, 4);
        }
        for (uint32_t i = 0; i < nfix; i++) {
            fix[i] = (struct nvfwd_fix){ random() % 4, random() % 4 ? random() % 96 : random(),
                                         random() % 4 ? random() % 512 : random(), 0 };
        }
        if (rmargs_parse(&c, RM_CMD(nr, size), call, len, fix, nfix)) {
            assert((uint64_t)c.arg_at + c.size <= len);
            n = rmargs_fields(&c, fields, matched);
            for (int i = 0; i < n; i++) {
                assert((uint64_t)fields[i].at + (fields[i].kind == NVFWD_FIX_FD ? 4 : 8) <= len);
                if (matched[i] >= 0 && fields[i].kind == NVFWD_FIX_PTR) {
                    assert((uint64_t)fix[matched[i]].target + fields[i].size <= len);
                }
                accepted++;
            }
        }
        free(call);
    }
    assert(accepted > 1000);
    printf("fuzz: %d calls, %llu fields accepted, all inside their call\n", FUZZ_ROUNDS,
           (unsigned long long)accepted);
}

int main(void)
{
    test_parse();
    test_a_pinned_control_goes_at_its_size_only();
    test_xfer_points_at_what_was_read();
    test_control();
    test_alloc_and_memory();
    test_a_branch_picks_its_class_sizes();
    test_uvm();
    fuzz();
    puts("ok");
    return 0;
}
