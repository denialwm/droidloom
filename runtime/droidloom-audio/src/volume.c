// SPDX-License-Identifier: GPL-3.0-or-later
// The host's default output volume, read and set the way pipewire-pulse does:
// the default sink's node Props report it; a sink on a card is set through the
// card's active Route, so the session manager keeps it, any other sink through
// its node Props. Policy stays in Rust.
#include <errno.h>
#include <math.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <pipewire/pipewire.h>
#include <pipewire/extensions/metadata.h>
#include <spa/param/audio/raw.h>
#include <spa/param/props.h>
#include <spa/param/route.h>
#include <spa/pod/builder.h>
#include <spa/pod/iter.h>
#include <spa/pod/parser.h>
#include <spa/utils/json.h>

#define NAME 256

// available: 1 with a level, 0 without a default output, -1 when PipeWire is gone.
typedef void (*dl_volume_report)(void *user, int available, float linear, int muted);

struct dl_node {
    struct spa_list link;
    uint32_t id, device;
    int32_t profile_device;
    bool sink;
    char name[NAME];
};

struct dl_volume {
    struct pw_thread_loop *loop;
    struct pw_context *context;
    struct pw_core *core;
    struct pw_registry *registry;
    struct spa_hook core_listener, registry_listener;
    struct pw_metadata *metadata;
    struct spa_hook metadata_listener;
    struct spa_list nodes;
    char default_name[NAME];
    // The default sink, bound while there is one.
    uint32_t target;
    struct pw_node *node;
    struct spa_hook node_listener;
    struct pw_device *device;
    struct spa_hook device_listener;
    int32_t profile_device, route_index, route_device;
    uint32_t channels;
    float volumes[SPA_AUDIO_MAX_CHANNELS];
    bool muted, known, gone;
    // Whether the first listing and the objects it bound have arrived;
    // before then, no default output is not yet news.
    int syncs, pending;
    dl_volume_report report;
    void *user;
};

static void unbind(struct dl_volume *v) {
    if (v->node) {
        spa_hook_remove(&v->node_listener);
        pw_proxy_destroy((struct pw_proxy *)v->node);
        v->node = NULL;
    }
    if (v->device) {
        spa_hook_remove(&v->device_listener);
        pw_proxy_destroy((struct pw_proxy *)v->device);
        v->device = NULL;
    }
    v->target = SPA_ID_INVALID;
    v->route_index = v->route_device = -1;
    v->known = false;
}

static void tell(struct dl_volume *v) {
    float loudest = 0;
    for (uint32_t i = 0; i < v->channels; i++) loudest = fmaxf(loudest, v->volumes[i]);
    v->report(v->user, 1, loudest, v->muted);
}

static void node_param(void *data, int seq SPA_UNUSED, uint32_t id, uint32_t index SPA_UNUSED,
        uint32_t next SPA_UNUSED,
        const struct spa_pod *param) {
    struct dl_volume *v = data;
    if (id != SPA_PARAM_Props || !spa_pod_is_object_type(param, SPA_TYPE_OBJECT_Props)) return;
    const struct spa_pod_prop *prop;
    bool changed = false;
    SPA_POD_OBJECT_FOREACH((const struct spa_pod_object *)param, prop) {
        if (prop->key == SPA_PROP_channelVolumes) {
            uint32_t count = spa_pod_copy_array(&prop->value, SPA_TYPE_Float, v->volumes,
                SPA_AUDIO_MAX_CHANNELS);
            if (count > 0) { v->channels = count; changed = true; }
        } else if (prop->key == SPA_PROP_mute) {
            bool muted;
            if (spa_pod_get_bool(&prop->value, &muted) == 0) { v->muted = muted; changed = true; }
        }
    }
    if (changed && v->channels > 0) { v->known = true; tell(v); }
}

static const struct pw_node_events node_events = {
    PW_VERSION_NODE_EVENTS,
    .param = node_param,
};

static void device_param(void *data, int seq SPA_UNUSED, uint32_t id, uint32_t index SPA_UNUSED,
        uint32_t next SPA_UNUSED,
        const struct spa_pod *param) {
    struct dl_volume *v = data;
    if (id != SPA_PARAM_Route) return;
    int32_t route_index, route_device;
    uint32_t direction;
    if (spa_pod_parse_object(param, SPA_TYPE_OBJECT_ParamRoute, NULL,
            SPA_PARAM_ROUTE_index, SPA_POD_Int(&route_index),
            SPA_PARAM_ROUTE_direction, SPA_POD_Id(&direction),
            SPA_PARAM_ROUTE_device, SPA_POD_Int(&route_device)) < 0) return;
    if (direction == SPA_DIRECTION_OUTPUT && route_device == v->profile_device) {
        v->route_index = route_index;
        v->route_device = route_device;
    }
}

static const struct pw_device_events device_events = {
    PW_VERSION_DEVICE_EVENTS,
    .param = device_param,
};

static void resolve(struct dl_volume *v) {
    struct dl_node *found = NULL, *node;
    spa_list_for_each(node, &v->nodes, link)
        if (node->sink && v->default_name[0] && strcmp(node->name, v->default_name) == 0) found = node;
    if (found && found->id == v->target) return;
    unbind(v);
    if (!found) { if (v->syncs == 2) v->report(v->user, 0, 0, 0); return; }
    v->target = found->id;
    v->profile_device = found->profile_device;
    v->node = pw_registry_bind(v->registry, found->id, PW_TYPE_INTERFACE_Node, PW_VERSION_NODE, 0);
    if (v->node) {
        pw_node_add_listener(v->node, &v->node_listener, &node_events, v);
        uint32_t ids[] = { SPA_PARAM_Props };
        pw_node_subscribe_params(v->node, ids, 1);
    }
    if (found->device != SPA_ID_INVALID && found->profile_device >= 0) {
        v->device = pw_registry_bind(v->registry, found->device, PW_TYPE_INTERFACE_Device,
            PW_VERSION_DEVICE, 0);
        if (v->device) {
            pw_device_add_listener(v->device, &v->device_listener, &device_events, v);
            uint32_t ids[] = { SPA_PARAM_Route };
            pw_device_subscribe_params(v->device, ids, 1);
        }
    }
}

static int metadata_property(void *data, uint32_t subject, const char *key,
        const char *type SPA_UNUSED,
        const char *value) {
    struct dl_volume *v = data;
    if (subject != PW_ID_CORE || (key && strcmp(key, "default.audio.sink") != 0)) return 0;
    v->default_name[0] = 0;
    if (value && spa_json_str_object_find(value, strlen(value), "name", v->default_name,
            sizeof(v->default_name)) < 0)
        v->default_name[0] = 0;
    resolve(v);
    return 0;
}

static const struct pw_metadata_events metadata_events = {
    PW_VERSION_METADATA_EVENTS,
    .property = metadata_property,
};

static void registry_global(void *data, uint32_t id, uint32_t permissions SPA_UNUSED,
        const char *type, uint32_t version SPA_UNUSED, const struct spa_dict *props) {
    struct dl_volume *v = data;
    if (!props) return;
    if (strcmp(type, PW_TYPE_INTERFACE_Metadata) == 0) {
        const char *name = spa_dict_lookup(props, PW_KEY_METADATA_NAME);
        if (v->metadata || !name || strcmp(name, "default") != 0) return;
        v->metadata = pw_registry_bind(v->registry, id, PW_TYPE_INTERFACE_Metadata,
            PW_VERSION_METADATA, 0);
        if (v->metadata) pw_metadata_add_listener(v->metadata, &v->metadata_listener,
            &metadata_events, v);
        return;
    }
    if (strcmp(type, PW_TYPE_INTERFACE_Node) != 0) return;
    const char *name = spa_dict_lookup(props, PW_KEY_NODE_NAME);
    const char *class = spa_dict_lookup(props, PW_KEY_MEDIA_CLASS);
    if (!name || strlen(name) >= NAME) return;
    struct dl_node *node = calloc(1, sizeof(*node));
    if (!node) return;
    node->id = id;
    node->sink = class && strcmp(class, "Audio/Sink") == 0;
    strcpy(node->name, name);
    const char *device = spa_dict_lookup(props, PW_KEY_DEVICE_ID);
    const char *profile_device = spa_dict_lookup(props, "card.profile.device");
    if (!device || !spa_atou32(device, &node->device, 10)) node->device = SPA_ID_INVALID;
    if (!profile_device || !spa_atoi32(profile_device, &node->profile_device, 10))
        node->profile_device = -1;
    spa_list_append(&v->nodes, &node->link);
    if (node->sink) resolve(v);
}

static void registry_global_remove(void *data, uint32_t id) {
    struct dl_volume *v = data;
    struct dl_node *node, *next;
    spa_list_for_each_safe(node, next, &v->nodes, link) {
        if (node->id != id) continue;
        spa_list_remove(&node->link);
        free(node);
        if (id == v->target) { unbind(v); resolve(v); }
    }
}

static const struct pw_registry_events registry_events = {
    PW_VERSION_REGISTRY_EVENTS,
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static void core_error(void *data, uint32_t id, int seq SPA_UNUSED, int res,
        const char *message SPA_UNUSED) {
    struct dl_volume *v = data;
    if (id != PW_ID_CORE || res != -EPIPE || v->gone) return;
    v->gone = true;
    unbind(v);
    v->report(v->user, -1, 0, 0);
}

static void core_done(void *data, uint32_t id, int seq) {
    struct dl_volume *v = data;
    if (id != PW_ID_CORE || seq != v->pending || v->syncs == 2) return;
    // The first sync ends the listing; the second, the defaults it bound.
    if (++v->syncs == 1) { v->pending = pw_core_sync(v->core, PW_ID_CORE, 0); return; }
    if (!v->known && !v->node) v->report(v->user, 0, 0, 0);
}

static const struct pw_core_events core_events = {
    PW_VERSION_CORE_EVENTS,
    .done = core_done,
    .error = core_error,
};

void dl_volume_close(struct dl_volume *v) {
    if (!v) return;
    if (v->loop) pw_thread_loop_stop(v->loop);
    unbind(v);
    if (v->metadata) {
        spa_hook_remove(&v->metadata_listener);
        pw_proxy_destroy((struct pw_proxy *)v->metadata);
    }
    if (v->registry) {
        spa_hook_remove(&v->registry_listener);
        pw_proxy_destroy((struct pw_proxy *)v->registry);
    }
    if (v->core) {
        spa_hook_remove(&v->core_listener);
        pw_core_disconnect(v->core);
    }
    if (v->context) pw_context_destroy(v->context);
    if (v->loop) pw_thread_loop_destroy(v->loop);
    struct dl_node *node, *next;
    spa_list_for_each_safe(node, next, &v->nodes, link) free(node);
    free(v);
}

int dl_volume_open(const char *remote, dl_volume_report report, void *user,
        struct dl_volume **output) {
    *output = NULL;
    struct dl_volume *v = calloc(1, sizeof(*v));
    if (!v) return -ENOMEM;
    spa_list_init(&v->nodes);
    v->report = report;
    v->user = user;
    v->target = SPA_ID_INVALID;
    v->route_index = v->route_device = v->profile_device = -1;
    v->loop = pw_thread_loop_new("droidloom-volume", NULL);
    if (!v->loop) { dl_volume_close(v); return -ENOMEM; }
    v->context = pw_context_new(pw_thread_loop_get_loop(v->loop), NULL, 0);
    if (!v->context) { dl_volume_close(v); return -errno; }
    struct pw_properties *props = pw_properties_new(PW_KEY_APP_NAME, "Droidloom volume", NULL);
    if (remote && *remote) pw_properties_set(props, PW_KEY_REMOTE_NAME, remote);
    v->core = pw_context_connect(v->context, props, 0);
    if (!v->core) { int error = -errno; dl_volume_close(v); return error; }
    pw_core_add_listener(v->core, &v->core_listener, &core_events, v);
    v->registry = pw_core_get_registry(v->core, PW_VERSION_REGISTRY, 0);
    if (!v->registry) { dl_volume_close(v); return -ENOMEM; }
    pw_registry_add_listener(v->registry, &v->registry_listener, &registry_events, v);
    v->pending = pw_core_sync(v->core, PW_ID_CORE, 0);
    int result = pw_thread_loop_start(v->loop);
    if (result < 0) { dl_volume_close(v); return result; }
    *output = v;
    return 0;
}

int dl_volume_set(struct dl_volume *v, float linear, int muted) {
    pw_thread_loop_lock(v->loop);
    int result = -ENODEV;
    if (v->known && v->node) {
        // Keep the channels' balance: scale them so the loudest is `linear`.
        float loudest = 0, volumes[SPA_AUDIO_MAX_CHANNELS];
        for (uint32_t i = 0; i < v->channels; i++) loudest = fmaxf(loudest, v->volumes[i]);
        for (uint32_t i = 0; i < v->channels; i++)
            volumes[i] = loudest > 0 ? v->volumes[i] * linear / loudest : linear;
        uint8_t storage[1024];
        struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
        struct spa_pod_frame outer, inner;
        if (v->device && v->route_index >= 0) {
            spa_pod_builder_push_object(&b, &outer, SPA_TYPE_OBJECT_ParamRoute, SPA_PARAM_Route);
            spa_pod_builder_add(&b, SPA_PARAM_ROUTE_index, SPA_POD_Int(v->route_index),
                SPA_PARAM_ROUTE_device, SPA_POD_Int(v->route_device), 0);
            spa_pod_builder_prop(&b, SPA_PARAM_ROUTE_props, 0);
            spa_pod_builder_push_object(&b, &inner, SPA_TYPE_OBJECT_Props, SPA_PARAM_Route);
            spa_pod_builder_add(&b, SPA_PROP_mute, SPA_POD_Bool(muted != 0),
                SPA_PROP_channelVolumes, SPA_POD_Array(sizeof(float), SPA_TYPE_Float,
                    v->channels, volumes), 0);
            spa_pod_builder_pop(&b, &inner);
            spa_pod_builder_prop(&b, SPA_PARAM_ROUTE_save, 0);
            spa_pod_builder_bool(&b, true);
            const struct spa_pod *param = spa_pod_builder_pop(&b, &outer);
            result = pw_device_set_param(v->device, SPA_PARAM_Route, 0, param);
        } else {
            spa_pod_builder_push_object(&b, &outer, SPA_TYPE_OBJECT_Props, SPA_PARAM_Props);
            spa_pod_builder_add(&b, SPA_PROP_mute, SPA_POD_Bool(muted != 0),
                SPA_PROP_channelVolumes, SPA_POD_Array(sizeof(float), SPA_TYPE_Float,
                    v->channels, volumes), 0);
            const struct spa_pod *param = spa_pod_builder_pop(&b, &outer);
            result = pw_node_set_param(v->node, SPA_PARAM_Props, 0, param);
        }
        if (result > 0) result = 0;
    }
    pw_thread_loop_unlock(v->loop);
    return result;
}
