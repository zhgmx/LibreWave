#define _GNU_SOURCE

#include "pipewire_filter_shim.h"

#include <errno.h>
#include <stddef.h>
#include <stdlib.h>

#include <pipewire/filter.h>

_Static_assert(PW_VERSION_FILTER_EVENTS == 1,
        "review the pw_filter event ABI before updating the shim");

struct lw_pw_filter {
    struct pw_filter *filter;
    struct spa_hook listener;
    void *ports[8];
    lw_pw_process_fn process;
    lw_pw_state_fn state_changed;
    void *data;
    uint32_t added_mask;
};

struct lw_pw_test_peer {
    struct pw_filter *filter;
    struct spa_hook listener;
    void *ports[6];
    lw_pw_test_process_fn process;
    void *data;
    uint32_t port_count;
    uint32_t added_mask;
};

static void check_pipewire_signatures(void)
{
    struct pw_filter *(*new_filter)(struct pw_core *, const char *,
            struct pw_properties *) = pw_filter_new;
    void (*add_listener)(struct pw_filter *, struct spa_hook *,
            const struct pw_filter_events *, void *) = pw_filter_add_listener;
    void *(*add_port)(struct pw_filter *, enum pw_direction,
            enum pw_filter_port_flags, size_t, struct pw_properties *,
            const struct spa_pod **, uint32_t) = pw_filter_add_port;
    int (*connect_filter)(struct pw_filter *, enum pw_filter_flags,
            const struct spa_pod **, uint32_t) = pw_filter_connect;
    void *(*get_buffer)(void *, uint32_t) = pw_filter_get_dsp_buffer;
    int (*flush_filter)(struct pw_filter *, bool) = pw_filter_flush;
    int (*set_active)(struct pw_filter *, bool) = pw_filter_set_active;
    int (*disconnect_filter)(struct pw_filter *) = pw_filter_disconnect;
    uint32_t (*get_node_id)(struct pw_filter *) = pw_filter_get_node_id;
    void (*destroy_filter)(struct pw_filter *) = pw_filter_destroy;

    (void)new_filter;
    (void)add_listener;
    (void)add_port;
    (void)connect_filter;
    (void)get_buffer;
    (void)flush_filter;
    (void)set_active;
    (void)disconnect_filter;
    (void)get_node_id;
    (void)destroy_filter;
}

static int semantic_filter_state(enum pw_filter_state state)
{
    switch (state) {
    case PW_FILTER_STATE_PAUSED:
        return LW_PW_FILTER_STATE_PAUSED;
    case PW_FILTER_STATE_STREAMING:
        return LW_PW_FILTER_STATE_STREAMING;
    case PW_FILTER_STATE_ERROR:
        return LW_PW_FILTER_STATE_ERROR;
    default:
        return LW_PW_FILTER_STATE_OTHER;
    }
}

static void on_state_changed(void *data, enum pw_filter_state old_state,
        enum pw_filter_state state, const char *error)
{
    struct lw_pw_filter *bridge = data;
    (void)error;
    bridge->state_changed(bridge->data,
            semantic_filter_state(old_state),
            semantic_filter_state(state));
}

static void on_process(void *data, struct spa_io_position *position)
{
    struct lw_pw_filter *bridge = data;
    float *buffers[8];
    uint32_t missing_mask = 0;
    uint32_t index;
    int faulted;

    for (index = 0; index < 8; index++) {
        buffers[index] = pw_filter_get_dsp_buffer(
                bridge->ports[index], position->clock.duration);
        if (buffers[index] == NULL)
            missing_mask |= (1u << index);
    }
    faulted = bridge->process(bridge->data,
            position->clock.id,
            position->clock.rate.num,
            position->clock.rate.denom,
            position->clock.position,
            position->clock.duration,
            buffers,
            missing_mask);
    if (faulted != 0)
        (void)pw_filter_flush(bridge->filter, false);
}

static const struct pw_filter_events filter_events = {
    PW_VERSION_FILTER_EVENTS,
    .state_changed = on_state_changed,
    .process = on_process,
};

static void on_test_process(void *data, struct spa_io_position *position)
{
    struct lw_pw_test_peer *peer = data;
    float *buffers[6] = { NULL };
    uint32_t missing_mask = 0;
    uint32_t index;

    for (index = 0; index < peer->port_count; index++) {
        buffers[index] = pw_filter_get_dsp_buffer(
                peer->ports[index], position->clock.duration);
        if (buffers[index] == NULL)
            missing_mask |= 1u << index;
    }
    peer->process(peer->data,
            position->clock.id,
            position->clock.rate.num,
            position->clock.rate.denom,
            position->clock.position,
            position->clock.duration,
            buffers,
            missing_mask);
}

static const struct pw_filter_events test_events = {
    PW_VERSION_FILTER_EVENTS,
    .process = on_test_process,
};

struct lw_pw_filter *lw_pw_filter_new(struct pw_core *core,
        const char *name,
        struct pw_properties *properties,
        lw_pw_process_fn process,
        lw_pw_state_fn state_changed,
        void *data)
{
    struct lw_pw_filter *bridge;

    check_pipewire_signatures();
    if (core == NULL || name == NULL || properties == NULL ||
            process == NULL || state_changed == NULL || data == NULL) {
        if (properties != NULL)
            pw_properties_free(properties);
        errno = EINVAL;
        return NULL;
    }
    bridge = calloc(1, sizeof(*bridge));
    if (bridge == NULL) {
        pw_properties_free(properties);
        return NULL;
    }
    bridge->process = process;
    bridge->state_changed = state_changed;
    bridge->data = data;
    bridge->filter = pw_filter_new(core, name, properties);
    if (bridge->filter == NULL) {
        free(bridge);
        return NULL;
    }
    pw_filter_add_listener(bridge->filter, &bridge->listener,
            &filter_events, bridge);
    return bridge;
}

int lw_pw_filter_add_port(struct lw_pw_filter *bridge, uint32_t index,
        bool output, struct pw_properties *properties)
{
    enum pw_direction direction = output ?
            PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT;
    if (bridge == NULL || properties == NULL || index >= 8 ||
            bridge->ports[index] != NULL) {
        if (properties != NULL)
            pw_properties_free(properties);
        return -EINVAL;
    }
    bridge->ports[index] = pw_filter_add_port(bridge->filter,
            direction, PW_FILTER_PORT_FLAG_MAP_BUFFERS, 0,
            properties, NULL, 0);
    if (bridge->ports[index] == NULL)
        return -errno;
    bridge->added_mask |= 1u << index;
    return 0;
}

int lw_pw_filter_connect_inactive_rt(struct lw_pw_filter *bridge)
{
    if (bridge == NULL || bridge->added_mask != UINT32_C(0xff))
        return -EINVAL;
    return pw_filter_connect(bridge->filter,
            PW_FILTER_FLAG_INACTIVE | PW_FILTER_FLAG_RT_PROCESS, NULL, 0);
}

uint32_t lw_pw_filter_node_id(const struct lw_pw_filter *bridge)
{
    return bridge == NULL ? PW_ID_ANY : pw_filter_get_node_id(bridge->filter);
}

int lw_pw_filter_set_active(struct lw_pw_filter *bridge, bool active)
{
    return bridge == NULL ? -EINVAL :
            pw_filter_set_active(bridge->filter, active);
}

int lw_pw_filter_disconnect(struct lw_pw_filter *bridge)
{
    return bridge == NULL ? 0 : pw_filter_disconnect(bridge->filter);
}

void lw_pw_filter_destroy(struct lw_pw_filter *bridge)
{
    if (bridge == NULL)
        return;
    spa_hook_remove(&bridge->listener);
    pw_filter_destroy(bridge->filter);
    free(bridge);
}

int lw_pw_filter_semantic_paused(void)
{
    return semantic_filter_state(PW_FILTER_STATE_PAUSED);
}

int lw_pw_filter_semantic_streaming(void)
{
    return semantic_filter_state(PW_FILTER_STATE_STREAMING);
}

int lw_pw_filter_semantic_error(void)
{
    return semantic_filter_state(PW_FILTER_STATE_ERROR);
}

struct lw_pw_test_peer *lw_pw_test_peer_new(struct pw_core *core,
        const char *name,
        struct pw_properties *properties,
        uint32_t port_count,
        lw_pw_test_process_fn process,
        void *data)
{
    struct lw_pw_test_peer *peer;

    if (core == NULL || name == NULL || properties == NULL ||
            port_count == 0 || port_count > 6 || process == NULL ||
            data == NULL) {
        if (properties != NULL)
            pw_properties_free(properties);
        errno = EINVAL;
        return NULL;
    }
    peer = calloc(1, sizeof(*peer));
    if (peer == NULL) {
        pw_properties_free(properties);
        return NULL;
    }
    peer->process = process;
    peer->data = data;
    peer->port_count = port_count;
    peer->filter = pw_filter_new(core, name, properties);
    if (peer->filter == NULL) {
        free(peer);
        return NULL;
    }
    pw_filter_add_listener(peer->filter, &peer->listener, &test_events, peer);
    return peer;
}

int lw_pw_test_peer_add_port(struct lw_pw_test_peer *peer, uint32_t index,
        bool output, struct pw_properties *properties)
{
    enum pw_direction direction = output ?
            PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT;

    if (peer == NULL || properties == NULL || index >= peer->port_count ||
            peer->ports[index] != NULL) {
        if (properties != NULL)
            pw_properties_free(properties);
        return -EINVAL;
    }
    peer->ports[index] = pw_filter_add_port(peer->filter,
            direction, PW_FILTER_PORT_FLAG_MAP_BUFFERS, 0,
            properties, NULL, 0);
    if (peer->ports[index] == NULL)
        return -errno;
    peer->added_mask |= 1u << index;
    return 0;
}

int lw_pw_test_peer_connect_inactive_rt(struct lw_pw_test_peer *peer)
{
    uint32_t expected_mask;

    if (peer == NULL)
        return -EINVAL;
    expected_mask = (1u << peer->port_count) - 1u;
    if (peer->added_mask != expected_mask)
        return -EINVAL;
    return pw_filter_connect(peer->filter,
            PW_FILTER_FLAG_INACTIVE | PW_FILTER_FLAG_RT_PROCESS, NULL, 0);
}

uint32_t lw_pw_test_peer_node_id(const struct lw_pw_test_peer *peer)
{
    return peer == NULL ? PW_ID_ANY : pw_filter_get_node_id(peer->filter);
}

int lw_pw_test_peer_set_active(struct lw_pw_test_peer *peer, bool active)
{
    return peer == NULL ? -EINVAL :
            pw_filter_set_active(peer->filter, active);
}

void lw_pw_test_peer_destroy(struct lw_pw_test_peer *peer)
{
    if (peer == NULL)
        return;
    spa_hook_remove(&peer->listener);
    pw_filter_destroy(peer->filter);
    free(peer);
}
