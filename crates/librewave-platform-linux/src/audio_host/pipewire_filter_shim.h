#ifndef LIBREWAVE_PIPEWIRE_FILTER_SHIM_H
#define LIBREWAVE_PIPEWIRE_FILTER_SHIM_H

#include <stdbool.h>
#include <stdint.h>

struct lw_pw_filter;
struct lw_pw_test_peer;
struct pw_core;
struct pw_properties;

typedef int (*lw_pw_process_fn)(void *data,
        uint32_t clock_id,
        uint32_t rate_num,
        uint32_t rate_denom,
        uint64_t position,
        uint32_t duration,
        float *buffers[8],
        uint32_t missing_mask);

typedef void (*lw_pw_state_fn)(void *data, int old_state, int state);

enum lw_pw_filter_state {
    LW_PW_FILTER_STATE_OTHER = 0,
    LW_PW_FILTER_STATE_PAUSED = 1,
    LW_PW_FILTER_STATE_STREAMING = 2,
    LW_PW_FILTER_STATE_ERROR = 3,
};

struct lw_pw_filter *lw_pw_filter_new(
        struct pw_core *core,
        const char *name,
        struct pw_properties *properties,
        lw_pw_process_fn process,
        lw_pw_state_fn state_changed,
        void *data);

int lw_pw_filter_add_port(
        struct lw_pw_filter *filter,
        uint32_t index,
        bool output,
        struct pw_properties *properties);

int lw_pw_filter_connect_inactive_rt(struct lw_pw_filter *filter);
uint32_t lw_pw_filter_node_id(const struct lw_pw_filter *filter);
int lw_pw_filter_set_active(struct lw_pw_filter *filter, bool active);
int lw_pw_filter_disconnect(struct lw_pw_filter *filter);
void lw_pw_filter_destroy(struct lw_pw_filter *filter);
int lw_pw_filter_semantic_paused(void);
int lw_pw_filter_semantic_streaming(void);
int lw_pw_filter_semantic_error(void);

typedef void (*lw_pw_test_process_fn)(void *data,
        uint32_t clock_id,
        uint32_t rate_num,
        uint32_t rate_denom,
        uint64_t position,
        uint32_t duration,
        float *buffers[6],
        uint32_t missing_mask);

struct lw_pw_test_peer *lw_pw_test_peer_new(
        struct pw_core *core,
        const char *name,
        struct pw_properties *properties,
        uint32_t port_count,
        lw_pw_test_process_fn process,
        void *data);
int lw_pw_test_peer_add_port(
        struct lw_pw_test_peer *peer,
        uint32_t index,
        bool output,
        struct pw_properties *properties);
int lw_pw_test_peer_connect_inactive_rt(struct lw_pw_test_peer *peer);
uint32_t lw_pw_test_peer_node_id(const struct lw_pw_test_peer *peer);
int lw_pw_test_peer_set_active(struct lw_pw_test_peer *peer, bool active);
void lw_pw_test_peer_destroy(struct lw_pw_test_peer *peer);

#endif
