/*
 * C ABI smoke test: compiled against include/vds.h only (docs/spec/c-abi.md).
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "vds.h"

#define CHECK(cond)                                                        \
    do {                                                                   \
        if (!(cond)) {                                                     \
            fprintf(stderr, "smoke: check failed: %s (line %d)\n", #cond,  \
                    __LINE__);                                             \
            return 1;                                                      \
        }                                                                  \
    } while (0)

static int wakeups;

static void on_wakeup(void *opaque)
{
    (void)opaque;
    wakeups++;
}

int main(void)
{
    VdsConfig cfg;
    VdsCallbacks cb;
    VdsServer *server;
    VdsHead *head;
    VdsRect rect = { 0, 0, 64, 64 };
    VdsClientInfo info[4];
    uint32_t pixels[64 * 64];
    uint32_t cursor[16 * 16];
    char *err = NULL;
    uint16_t port = 0;
    int16_t pcm[960] = { 0 };

    CHECK((vds_abi_version() >> 16) == VDS_ABI_MAJOR);

    vds_config_init(&cfg);
    CHECK(cfg.struct_size == sizeof(cfg));
    cfg.codecs = 0;
    CHECK(vds_server_new(&cfg, NULL, NULL, &err) == NULL);
    CHECK(err != NULL);
    vds_free_string(err);
    err = NULL;

    vds_config_init(&cfg);
    memset(&cb, 0, sizeof(cb));
    cb.wakeup = on_wakeup;
    server = vds_server_new(&cfg, &cb, NULL, &err);
    CHECK(server != NULL);

    head = vds_head_add(server, 0, 64, 64, &err);
    CHECK(head != NULL);
    memset(pixels, 0x80, sizeof(pixels));
    CHECK(vds_head_submit(head, (const uint8_t *)pixels, sizeof(pixels),
                          64 * 4, &rect, 1) == VDS_OK);
    CHECK(vds_head_submit(head, (const uint8_t *)pixels, 16, 64 * 4, &rect,
                          1) == VDS_E_INVALID_ARG);
    CHECK(vds_head_resize(head, 32, 32) == VDS_OK);
    CHECK(!vds_server_wants_frame(server));

    memset(cursor, 0xff, sizeof(cursor));
    CHECK(vds_cursor_define(server, 16, 16, 1, 1, cursor) == VDS_OK);
    vds_cursor_set_visible(server, false);
    vds_audio_out(server, pcm, 960);

    CHECK(vds_set_password(server, "user", "secret", NULL, VDS_CONNECTED_KEEP,
                           true, &err) == VDS_OK);
    CHECK(vds_set_password_expiry(server, VDS_EXPIRE_NEVER) == VDS_OK);

    CHECK(vds_server_listen(server, "127.0.0.1", 0, &port, &err) == VDS_OK);
    CHECK(port != 0);
    CHECK(vds_server_clients(server, info, 4) == 0);

    vds_server_dispatch(server);
    vds_head_remove(head);
    vds_server_free(server);
    printf("smoke: ok (listened on port %u)\n", (unsigned)port);
    return 0;
}
