// SPDX-License-Identifier: GPL-3.0-or-later
// Narrow ABI for SPA's inline builders and PipeWire buffer ownership. No PCM memcpy.
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/param/buffers.h>

#define POOL 4
struct dl_pw {
    struct pw_thread_loop *loop;
    struct pw_stream *stream;
    struct pw_buffer *buffers[POOL];
    struct pw_buffer *capture_buffer;
    uint32_t capture_offset, capture_left;
    struct pw_buffer *playback_buffer;
    uint32_t playback_filled, playback_target;
    bool playback_borrowed, playback_invalidated, renegotiating;
    uint32_t playback_slot;
    void *samples[POOL];
    int fds[POOL];
    uint32_t bytes, channels, rate, direction;
    enum pw_stream_state state;
    int error;
    bool drained;
    uint64_t submitted, captured, presented, dropped, discontinuities;
    uint64_t latency_ns;
};

struct dl_region { uint32_t slot, frames, offset; };
struct dl_position { uint64_t frames, monotonic_ns, latency_ns, discontinuities; };

static void state_changed(void *opaque, enum pw_stream_state old,
        enum pw_stream_state state, const char *error) {
    (void)old;
    struct dl_pw *p = opaque;
    p->state = state;
    if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) {
        if (state == PW_STREAM_STATE_ERROR)
            pw_log_warn("Droidloom stream failed: %s", error ? error : "unknown error");
        p->error = EPIPE;
    }
    pw_thread_loop_signal(p->loop, false);
}

static void param_changed(void *opaque, uint32_t id, const struct spa_pod *param) {
    struct dl_pw *p = opaque;
    if (id != SPA_PARAM_Format || param == NULL) return;
    struct spa_audio_info_raw info = {0};
    if (spa_format_audio_raw_parse(param, &info) < 0 ||
        info.format != SPA_AUDIO_FORMAT_S16_LE || info.channels != p->channels || info.rate != p->rate) {
        p->error = ENOTSUP;
        pw_thread_loop_signal(p->loop, false);
        return;
    }
    uint8_t storage[512];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *params[] = {
        spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
            SPA_PARAM_BUFFERS_buffers, SPA_POD_Int(POOL),
            SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
            SPA_PARAM_BUFFERS_size, SPA_POD_Int(p->bytes),
            SPA_PARAM_BUFFERS_stride, SPA_POD_Int(p->channels * 2),
            SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_MemFd))
    };
    int result = pw_stream_update_params(p->stream, params, 1);
    if (result < 0) p->error = -result;
}

static void add_buffer(void *opaque, struct pw_buffer *buffer) {
    struct dl_pw *p = opaque;
    unsigned slot;
    for (slot = 0; slot < POOL && p->buffers[slot] != NULL; slot++);
    if (slot == POOL || buffer->buffer->n_datas != 1) {
        p->error = ENOTSUP;
        pw_thread_loop_signal(p->loop, false);
        return;
    }
    struct spa_data *d = &buffer->buffer->datas[0];
    d->type = SPA_DATA_MemFd;
    d->flags = SPA_DATA_FLAG_READWRITE;
    d->fd = p->fds[slot];
    d->mapoffset = 0;
    d->maxsize = p->bytes;
    d->data = p->samples[slot];
    p->buffers[slot] = buffer;
    buffer->user_data = (void *)(uintptr_t)(slot + 1);
    bool complete = true;
    for (unsigned i = 0; i < POOL; i++) complete &= p->buffers[i] != NULL;
    if (complete) p->renegotiating = false;
    pw_thread_loop_signal(p->loop, false);
}

static void remove_buffer(void *opaque, struct pw_buffer *buffer) {
    struct dl_pw *p = opaque;
    unsigned slot = (uintptr_t)buffer->user_data - 1;
    if (slot < POOL && p->buffers[slot] == buffer) p->buffers[slot] = NULL;
    pw_log_debug("Droidloom shared pool slot %u removed in state %s", slot, pw_stream_state_as_string(p->state));
    if (p->direction == 1) {
        // PipeWire recreates wrapper buffers when session policy changes ports.
        // The exported memfds stay immutable and alive. Playback buffers cannot
        // be consumed until we queue them, so an outstanding Android write may
        // finish safely; its invalidated credit is discarded on submit.
        if (!p->renegotiating) {
            p->renegotiating = true;
            p->dropped = p->submitted - p->presented;
            p->discontinuities++;
        }
        if (p->playback_buffer == buffer) {
            p->playback_buffer = NULL;
            p->playback_filled = p->playback_target = 0;
            p->playback_invalidated = p->playback_borrowed;
        }
    } else {
        // A capture producer could overwrite a region Android is still reading.
        // End that generation instead of silently reusing its live mappings.
        p->error = EPIPE;
    }
    pw_thread_loop_signal(p->loop, false);
}
static void process(void *opaque) {
    struct dl_pw *p = opaque;
    // No socket, allocation, blocking work, or sample copy in this callback.
    pw_thread_loop_signal(p->loop, false);
}
static void drained(void *opaque) {
    struct dl_pw *p = opaque;
    p->drained = true;
    pw_thread_loop_signal(p->loop, false);
}
static const struct pw_stream_events events = {
    PW_VERSION_STREAM_EVENTS,
    .state_changed = state_changed, .param_changed = param_changed,
    .add_buffer = add_buffer, .remove_buffer = remove_buffer,
    .process = process, .drained = drained,
};

void dl_pw_destroy(struct dl_pw *p) {
    if (!p) return;
    if (p->loop) pw_thread_loop_stop(p->loop);
    if (p->stream) pw_stream_destroy(p->stream);
    if (p->loop) pw_thread_loop_destroy(p->loop);
    free(p);
}

int dl_pw_open(uint32_t direction, uint32_t rate, uint32_t channels, uint32_t bytes,
        const int *fds, void *const *samples, const char *target, const char *remote,
        const char *name, uint32_t capture_sink, struct dl_pw **output) {
    *output = NULL;
    struct dl_pw *p = calloc(1, sizeof(*p));
    if (!p) return -ENOMEM;
    p->direction = direction; p->rate = rate; p->channels = channels; p->bytes = bytes;
    memcpy(p->fds, fds, sizeof(p->fds));
    memcpy(p->samples, samples, sizeof(p->samples));
    p->state = PW_STREAM_STATE_UNCONNECTED;
    p->loop = pw_thread_loop_new("droidloom-audio", NULL);
    if (!p->loop) { dl_pw_destroy(p); return -ENOMEM; }
    struct pw_properties *props = pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Audio", PW_KEY_MEDIA_CATEGORY, direction == 1 ? "Playback" : "Capture",
        PW_KEY_MEDIA_ROLE, "Music", PW_KEY_NODE_NAME, name,
        PW_KEY_NODE_DESCRIPTION, "Droidloom Android audio",
        "adapter.auto-port-config", "{ mode = dsp }",
        PW_KEY_NODE_LATENCY, "256/48000", PW_KEY_NODE_PAUSE_ON_IDLE, "true", NULL);
    // Playback must keep its clock when the host has no output device. Let
    // PipeWire schedule it on the dummy driver until a real sink is linked;
    // dl_pw_control() releases that clock while Android pauses or stands by.
    // Capture remains linked-device driven and does not synthesize microphone data.
    if (direction == 1) {
        pw_properties_set(props, PW_KEY_NODE_ALWAYS_PROCESS, "true");
        pw_properties_set(props, PW_KEY_NODE_WANT_DRIVER, "true");
        // WirePlumber must wait for a sink instead of sending a fatal no-target
        // error. This is policy node.linger, not PipeWire object.linger: the
        // stream still disappears when its client closes.
        pw_properties_set(props, "node.linger", "true");
    }
    if (target && *target) pw_properties_set(props, PW_KEY_TARGET_OBJECT, target);
    if (remote && *remote) pw_properties_set(props, PW_KEY_REMOTE_NAME, remote);
    if (capture_sink && direction == 2) pw_properties_set(props, PW_KEY_STREAM_CAPTURE_SINK, "true");
    p->stream = pw_stream_new_simple(pw_thread_loop_get_loop(p->loop), name, props, &events, p);
    if (!p->stream) { dl_pw_destroy(p); return -ENOMEM; }
    uint8_t storage[512];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_audio_info_raw info = { .format = SPA_AUDIO_FORMAT_S16_LE, .rate = rate, .channels = channels };
    info.position[0] = channels == 1 ? SPA_AUDIO_CHANNEL_MONO : SPA_AUDIO_CHANNEL_FL;
    if (channels == 2) info.position[1] = SPA_AUDIO_CHANNEL_FR;
    const struct spa_pod *params[] = { spa_format_audio_raw_build(&b, SPA_PARAM_EnumFormat, &info) };
    // Refill when a buffer is recycled, before the queue empties. The bounded
    // producer otherwise sleeps until an underrun asks for more data.
    int result = pw_stream_connect(p->stream, direction == 1 ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT,
        PW_ID_ANY, PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_ALLOC_BUFFERS |
        PW_STREAM_FLAG_ASYNC | PW_STREAM_FLAG_EARLY_PROCESS | PW_STREAM_FLAG_INACTIVE, params, 1);
    if (result < 0) { dl_pw_destroy(p); return result; }
    result = pw_thread_loop_start(p->loop);
    if (result < 0) { dl_pw_destroy(p); return result; }
    pw_thread_loop_lock(p->loop);
    struct timespec deadline;
    pw_thread_loop_get_time(p->loop, &deadline, 2000000000);
    while (p->state < PW_STREAM_STATE_PAUSED && !p->error) {
        result = pw_thread_loop_timed_wait_full(p->loop, &deadline);
        if (result < 0) { p->error = ETIMEDOUT; break; }
    }
    result = p->error;
    pw_thread_loop_unlock(p->loop);
    if (result) { dl_pw_destroy(p); return -result; }
    *output = p;
    return 0;
}

int dl_pw_acquire(struct dl_pw *p, uint32_t requested, struct dl_region *region) {
    int result = 0;
    pw_thread_loop_lock(p->loop);
    struct timespec deadline;
    pw_thread_loop_get_time(p->loop, &deadline, 500000000);
    struct pw_buffer *b = p->direction == 2 ? p->capture_buffer : p->playback_buffer;
    while (!p->error) {
        if (b) break;
        // Keep at most two bursts or graph periods in flight, including the
        // one being acquired. A coarse graph needs one complete period ready
        // ahead of its next cycle. Device delay never consumes pool credits.
        bool room = true;
        if (p->direction == 1) {
            struct pw_time time = {0};
            int status = pw_stream_get_time_n(p->stream, &time, sizeof(time));
            if (status == 0 || (status == -EIO && p->state == PW_STREAM_STATE_PAUSED)) {
                uint64_t period = time.size > requested ? time.size : requested;
                room = time.queued + time.buffered <= period;
            }
        }
        if (room && (b = pw_stream_dequeue_buffer(p->stream)) != NULL) break;
        if (pw_thread_loop_timed_wait_full(p->loop, &deadline) < 0) { result = ETIMEDOUT; break; }
    }
    if (p->error) result = p->error;
    if (b && !result) {
        unsigned slot = (uintptr_t)b->user_data - 1;
        if (slot >= POOL || p->buffers[slot] != b) result = EPROTO;
        else {
            const struct spa_data *d = &b->buffer->datas[0];
            uint32_t stride = p->channels * 2;
            uint32_t offset = p->direction == 2 ?
                (p->capture_buffer ? p->capture_offset : d->chunk->offset) :
                p->playback_filled * stride;
            uint32_t count = p->direction == 2 ?
                (p->capture_buffer ? p->capture_left : d->chunk->size / stride) : 0;
            if (p->direction == 1) {
                if (!p->playback_buffer) {
                    p->playback_buffer = b;
                    // A coarse graph needs a complete cycle of samples. Build
                    // it in the same memfd through successive Android credits;
                    // each credit writes directly at the accumulated offset.
                    uint64_t target = b->requested > requested ? b->requested : requested;
                    struct pw_time time = {0};
                    // Per-buffer suggestions can lag a graph-period change.
                    // size is the resampler's current demand in stream frames.
                    pw_stream_get_time_n(p->stream, &time, sizeof(time));
                    if (time.size > target) target = time.size;
                    uint32_t capacity = p->bytes / stride;
                    p->playback_target = target > capacity ? capacity : target;
                }
                count = p->playback_target - p->playback_filled;
            }
            if (p->direction == 2 && !p->capture_buffer) {
                if ((d->chunk->stride && d->chunk->stride != (int32_t)stride) ||
                        d->chunk->size % stride || offset % stride || offset > p->bytes ||
                        count > (p->bytes - offset) / stride) result = EPROTO;
                else {
                    p->capture_buffer = b;
                    p->capture_offset = offset;
                    p->capture_left = count;
                }
            }
            // requested is the graph/resampler's suggestion, not allocation
            // capacity. Android submits larger synchronous bursts; truncating
            // those to a graph quantum makes AudioFlinger repeatedly sleep
            // between partial writes. Return the caller's bounded pool credit.
            if (count > requested) count = requested;
            if (offset % stride || offset > p->bytes || count > (p->bytes - offset) / stride || !count)
                result = EPROTO;
            else if (!result) {
                *region = (struct dl_region){slot, count, offset};
                if (p->direction == 1) {
                    p->playback_borrowed = true;
                    p->playback_invalidated = false;
                    p->playback_slot = slot;
                }
            }
        }
        if (result) {
            pw_stream_return_buffer(p->stream, b);
            p->capture_buffer = NULL;
            p->playback_buffer = NULL;
            p->playback_filled = p->playback_target = 0;
        }
    }
    pw_thread_loop_unlock(p->loop);
    return -result;
}

// Called with the loop locked. A partial tail is queued only for drain; normal
// playback fills at least the graph request or one caller burst, whichever is larger.
static int queue_playback(struct dl_pw *p) {
    struct pw_buffer *b = p->playback_buffer;
    if (!b) return 0;
    int result;
    if (p->playback_filled) {
        struct spa_chunk *chunk = b->buffer->datas[0].chunk;
        chunk->offset = 0;
        chunk->stride = p->channels * 2;
        chunk->size = p->playback_filled * chunk->stride;
        b->size = p->playback_filled;
        result = pw_stream_queue_buffer(p->stream, b);
    } else result = pw_stream_return_buffer(p->stream, b);
    // Returning a buffer can report a positive queue count on success.
    if (result >= 0) {
        p->playback_buffer = NULL;
        p->playback_filled = p->playback_target = 0;
        result = 0;
    }
    return result;
}

int dl_pw_submit(struct dl_pw *p, uint32_t slot, uint32_t frames) {
    if (slot >= POOL || frames > p->bytes / (p->channels * 2)) return -EINVAL;
    pw_thread_loop_lock(p->loop);
    int result = p->error ? -p->error : 0;
    if (!result && p->direction == 1) {
        if (!p->playback_borrowed || slot != p->playback_slot) result = -EPROTO;
        else if (p->playback_invalidated) {
            p->submitted += frames;
            p->dropped += frames;
            p->playback_borrowed = p->playback_invalidated = false;
            pw_thread_loop_unlock(p->loop);
            return 0;
        }
        p->playback_borrowed = false;
    }
    struct pw_buffer *b = p->buffers[slot];
    if (!b && !result) result = -EPIPE;
    if (!result) {
        if (p->direction == 1) {
            if (b != p->playback_buffer || frames > p->playback_target - p->playback_filled)
                result = -EPROTO;
            else {
                p->submitted += frames;
                p->playback_filled += frames;
                if (p->playback_filled == p->playback_target) result = queue_playback(p);
            }
        } else if (b != p->capture_buffer || frames > p->capture_left) result = -EPROTO;
        else {
            p->captured += frames;
            p->capture_left -= frames;
            p->capture_offset += frames * p->channels * 2;
            if (!p->capture_left) {
                result = pw_stream_queue_buffer(p->stream, b);
                p->capture_buffer = NULL;
            }
        }
    }
    pw_thread_loop_unlock(p->loop);
    return result;
}

// PipeWire keeps a driver running for an always-process node even while the
// node is inactive, so a lingering stream in standby would tick the dummy
// driver forever. Change the property before pw_stream_set_active(), which
// recalculates the graph.
static int set_active(struct dl_pw *p, bool active) {
    if (p->direction == 1) {
        struct spa_dict_item item = SPA_DICT_ITEM_INIT(PW_KEY_NODE_ALWAYS_PROCESS,
            active ? "true" : "false");
        int result = pw_stream_update_properties(p->stream, &SPA_DICT_INIT(&item, 1));
        if (result < 0) return result;
    }
    return pw_stream_set_active(p->stream, active);
}

int dl_pw_control(struct dl_pw *p, uint64_t command) {
    pw_thread_loop_lock(p->loop);
    int result = p->error ? -p->error : 0;
    if (!result && (command == 3 || command == 5) && p->capture_buffer) {
        result = pw_stream_queue_buffer(p->stream, p->capture_buffer);
        p->capture_buffer = NULL;
        p->capture_left = 0;
    }
    uint64_t dropped = 0;
    if (!result && p->direction == 1 && (command == 3 || command == 5)) {
        struct pw_time time = {0};
        result = pw_stream_get_time_n(p->stream, &time, sizeof(time));
        // PipeWire still fills the queue counters while paused, but reports
        // -EIO until the graph is streaming. Flush/standby are valid here.
        if (result == -EIO && p->state == PW_STREAM_STATE_PAUSED) result = 0;
        if (!result) {
            dropped = time.queued + time.buffered + p->playback_filled;
            uint64_t remaining = p->submitted - p->dropped - p->presented;
            if (dropped > remaining) dropped = remaining;
        }
        if (!result && p->playback_buffer) {
            result = pw_stream_return_buffer(p->stream, p->playback_buffer);
            if (result >= 0) {
                p->playback_buffer = NULL;
                p->playback_filled = p->playback_target = 0;
                result = 0;
            }
        }
    }
    if (!result) switch (command) {
        case 1: result = set_active(p, true); break;
        case 2: result = set_active(p, false); break;
        case 3: result = pw_stream_flush(p->stream, false); break;
        case 4: {
            p->drained = false;
            if (p->direction == 1) result = queue_playback(p);
            if (!result) result = pw_stream_flush(p->stream, true);
            struct timespec deadline;
            pw_thread_loop_get_time(p->loop, &deadline, 2000000000);
            while (!result && !p->drained && !p->error)
                if (pw_thread_loop_timed_wait_full(p->loop, &deadline) < 0) result = -ETIMEDOUT;
            if (p->error) result = -p->error;
        } break;
        case 5: result = set_active(p, false);
            if (!result) result = pw_stream_flush(p->stream, false);
            break;
        default: result = -EINVAL;
    }
    if (!result && (command == 3 || command == 5)) {
        p->dropped += dropped;
        p->discontinuities++;
    }
    pw_thread_loop_unlock(p->loop);
    return result;
}

int dl_pw_position(struct dl_pw *p, struct dl_position *out) {
    pw_thread_loop_lock(p->loop);
    struct pw_time time = {0};
    int result = p->error ? -p->error : pw_stream_get_time_n(p->stream, &time, sizeof(time));
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    uint64_t ns = (uint64_t)now.tv_sec * 1000000000 + now.tv_nsec;
    if (!p->error && ((result == -EIO && p->state == PW_STREAM_STATE_PAUSED) ||
            (!result && !time.rate.denom))) {
        // Android queries position before the first burst, before the session
        // manager links the graph, and while paused. No graph progress exists
        // yet: retain the last observed frames instead of killing the stream.
        *out = (struct dl_position){p->direction == 1 ? p->presented : p->captured,
            ns, p->latency_ns, p->discontinuities};
        result = 0;
    } else
    if (!result && time.rate.denom) {
        uint64_t graph_delay_ns = time.delay > 0 ?
            (uint64_t)time.delay * time.rate.num * 1000000000 / time.rate.denom : 0;
        uint64_t pending = time.queued + time.buffered + p->playback_filled
            + graph_delay_ns * p->rate / 1000000000;
        uint64_t eligible = p->submitted - p->dropped;
        uint64_t frames = p->direction == 1 ? (eligible > pending ? eligible - pending : 0) : p->captured;
        if (frames < p->presented) frames = p->presented;
        p->presented = frames;
        p->latency_ns = (time.queued + time.buffered + p->playback_filled)
            * 1000000000 / p->rate + graph_delay_ns;
        *out = (struct dl_position){ frames, ns, p->latency_ns, p->discontinuities };
    } else if (!result) result = -EAGAIN;
    pw_thread_loop_unlock(p->loop);
    return result;
}

void dl_pw_init(void) { pw_init(NULL, NULL); }
