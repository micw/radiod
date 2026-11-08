/* SPDX-License-Identifier: MIT
 * Original radiod UPnP shim. All callbacks run synchronously on the run thread.
 */
#ifndef RADIOD_UPNP_H
#define RADIOD_UPNP_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
#define RADIOD_UPNP_URI_CAP 4097
#define RADIOD_UPNP_METADATA_CAP 8193
typedef struct {
    const char *interface_name; /* optional; NULL chooses interface by address */
    const char *bind_address;   /* required numeric IPv4 */
    const char *friendly_name;
    const char *uuid;           /* stable UUID, with or without uuid: prefix */
    const char *asset_dir;      /* directory containing the four XML assets */
    uint16_t http_port;         /* required nonzero; production caller default 8091 */
} radiod_upnp_config;
typedef struct {
    uint64_t revision;
    int transport_state; /* 0 STOPPED, 1 TRANSITIONING, 2 PLAYING, 3 PAUSED_PLAYBACK */
    int transport_error; /* 0 OK, nonzero ERROR_OCCURRED */
    int volume;         /* 0..100 */
    int mute;           /* boolean */
    uint32_t duration_seconds;
    uint32_t position_seconds;
    char uri[RADIOD_UPNP_URI_CAP];
    char metadata[RADIOD_UPNP_METADATA_CAP];
} radiod_upnp_snapshot;
typedef struct {
    /* Called exactly once, including startup failure. message valid during call. */
    void (*ready)(void *userdata, int success, const char *message);
    /* action is the standard UPnP action name; value carries volume/mute.
     * owner_ip is the actual HTTP peer; uri/metadata empty for other actions.
     * Return 0 on success, otherwise an UPnP error code; write a safe description.
     * No pointers may be retained. Calls must not block the GLib loop for long. */
    int (*action)(void *userdata, const char *action, const char *owner_ip,
                  const char *uri, const char *metadata, int value,
                  char *error_buffer, size_t error_buffer_length);
    /* Fill the entire zero-initialized snapshot; strings must be NUL terminated.
     * Called initially, on queries/actions, and every 100ms. */
    void (*snapshot)(void *userdata, radiod_upnp_snapshot *snapshot);
    /* Optional: nonzero requests clean exit; polled every 100ms. */
    int (*should_stop)(void *userdata);
} radiod_upnp_callbacks;
/* Blocking: call on a dedicated thread. Own private GMainContext/GMainLoop.
 * HTTP uses config.http_port (caller default 8091). Returns 0 after clean stop,
 * -1 on startup failure.
 * Config/callbacks/userdata must remain valid until return. No signal handlers. */
int radiod_upnp_run(const radiod_upnp_config *config,
                   const radiod_upnp_callbacks *callbacks, void *userdata,
                   char *error_buffer, size_t error_buffer_length);
#ifdef __cplusplus
}
#endif
#endif
