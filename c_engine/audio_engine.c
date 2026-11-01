#include <pulse/pulseaudio.h>
#include <opus/opus.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "i18n.h"

typedef struct {
    pa_mainloop *loop;
    pa_context *context;
    pa_stream *stream;
    OpusEncoder *encoder;
    char *source;
    int failed, connecting;
    int16_t pcm[960 * 2];
    size_t filled, offset;
} UuAudio;

void uu_audio_close(UuAudio *audio) {
    if (!audio) return;
    if (audio->stream) {
        pa_stream_disconnect(audio->stream);
        pa_stream_unref(audio->stream);
    }
    if (audio->context) {
        pa_context_disconnect(audio->context);
        pa_context_unref(audio->context);
    }
    if (audio->loop) pa_mainloop_free(audio->loop);
    if (audio->encoder) opus_encoder_destroy(audio->encoder);
    free(audio->source);
    free(audio);
}

static void connect_source(UuAudio *audio, const char *source) {
    if (!source || !*source) { audio->failed = 1; return; }
    pa_sample_spec spec = { PA_SAMPLE_S16NE, 48000, 2 };
    audio->stream = pa_stream_new(audio->context, L("UU 远程音频", "UU Remote audio"), &spec, NULL);
    pa_buffer_attr attr = { (uint32_t)-1, (uint32_t)-1, (uint32_t)-1, (uint32_t)-1, sizeof(audio->pcm) };
    if (!audio->stream || pa_stream_connect_record(audio->stream, source, &attr,
            PA_STREAM_ADJUST_LATENCY | PA_STREAM_DONT_MOVE) < 0) audio->failed = 1;
}

static void source_info(pa_context *context, const pa_source_info *info, int end, void *data) {
    (void)context;
    UuAudio *audio = data;
    if (info && !audio->stream) connect_source(audio, info->name);
    if (end && !audio->stream) audio->failed = 1;
}

static void sink_info(pa_context *context, const pa_sink_info *info, int end, void *data) {
    (void)context;
    UuAudio *audio = data;
    if (info && !audio->stream) connect_source(audio, info->monitor_source_name);
    if (end && !audio->stream) audio->failed = 1;
}

static void server_info(pa_context *context, const pa_server_info *info, void *data) {
    UuAudio *audio = data;
    if (!info || !info->default_sink_name) { audio->failed = 1; return; }
    pa_operation *op = pa_context_get_sink_info_by_name(context, info->default_sink_name, sink_info, audio);
    if (op) pa_operation_unref(op);
    else audio->failed = 1;
}

UuAudio *uu_audio_open(const char *source) {
    UuAudio *audio = calloc(1, sizeof(*audio));
    if (!audio) return NULL;
    int error;
    audio->source = strdup(source);
    audio->encoder = opus_encoder_create(48000, 2, OPUS_APPLICATION_RESTRICTED_LOWDELAY, &error);
    audio->loop = pa_mainloop_new();
    if (!audio->source || !audio->encoder || !audio->loop) goto fail;
    if (opus_encoder_ctl(audio->encoder, OPUS_SET_BITRATE(128000)) != OPUS_OK) goto fail;
    audio->context = pa_context_new(pa_mainloop_get_api(audio->loop), "UU Remote");
    if (!audio->context || pa_context_connect(audio->context, NULL, PA_CONTEXT_NOAUTOSPAWN, NULL) < 0) goto fail;
    return audio;
fail:
    uu_audio_close(audio);
    return NULL;
}

/* Nonblocking: the Rust worker checks cancellation between calls, including on a suspended sink. */
int uu_audio_packet(UuAudio *audio, uint8_t *packet, int capacity) {
    int result;
    if (pa_mainloop_iterate(audio->loop, 0, &result) < 0) return -1;
    pa_context_state_t state = pa_context_get_state(audio->context);
    if (!PA_CONTEXT_IS_GOOD(state) || audio->failed) return -2;
    if (state != PA_CONTEXT_READY) return 0;
    if (!audio->connecting) {
        audio->connecting = 1;
        if (strcmp(audio->source, "@DEFAULT_MONITOR@") == 0) {
            pa_operation *op = pa_context_get_server_info(audio->context, server_info, audio);
            if (op) pa_operation_unref(op);
            else return -3;
        } else {
            pa_operation *op = pa_context_get_source_info_by_name(audio->context, audio->source, source_info, audio);
            if (op) pa_operation_unref(op);
            else return -3;
        }
    }
    if (!audio->stream) return 0;
    pa_stream_state_t stream_state = pa_stream_get_state(audio->stream);
    if (!PA_STREAM_IS_GOOD(stream_state)) return -4;
    if (stream_state != PA_STREAM_READY) return 0;
    const void *buffer;
    size_t length;
    if (pa_stream_peek(audio->stream, &buffer, &length) < 0) return -5;
    if (!length) return 0;
    if (audio->offset > length) return -6;
    size_t copy = sizeof(audio->pcm) - audio->filled;
    if (copy > length - audio->offset) copy = length - audio->offset;
    if (buffer) memcpy((uint8_t *)audio->pcm + audio->filled, (const uint8_t *)buffer + audio->offset, copy);
    else memset((uint8_t *)audio->pcm + audio->filled, 0, copy);
    audio->filled += copy;
    audio->offset += copy;
    if (audio->offset == length) {
        if (pa_stream_drop(audio->stream) < 0) return -7;
        audio->offset = 0;
    }
    if (audio->filled < sizeof(audio->pcm)) return 0;
    audio->filled = 0;
    return opus_encode(audio->encoder, audio->pcm, 960, packet, capacity);
}
