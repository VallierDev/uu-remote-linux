#ifndef PORTAL_NVENC_SPA_H
#define PORTAL_NVENC_SPA_H
#include <stdint.h>
#include <spa/buffer/buffer.h>
#include <spa/param/video/raw.h>

/* Standalone metadata snapshot only, not an importer or access authorization.
 * Caller must hold the PipeWire buffer and format locks and an upper-layer lease
 * throughout the call, and guarantee all pointers/arrays are valid, readable,
 * correctly sized and stable. C cannot validate pointer provenance or allocation
 * lengths from SPA counts/sizes. The fixed raw format must be genuinely negotiated
 * (including its modifier); expected_modifier is an independently selected exact
 * match, not a modifier capability or importability test.
 *
 * Caller remains responsible for fd lifetime, validity and object identity,
 * allocation bounds, synchronization ownership and timeline identity, lease
 * authenticity, waits, signals and all access. FD numbers can be reused or alias;
 * even equal numbers/ordered points prove no ownership. No mapping, dup, wait,
 * signal or GPU operation occurs here. Snapshot contains no borrowed pointers;
 * fd integers are borrowed, not pinned. On failure output is unchanged.
 * Supports only one packed RGB plane followed by acquire/release SyncObj slots.
 * Metadata count is bounded; exactly one correctly sized timeline is required.
 * This is a restricted SPA subset: rather than applying SPA's offset modulo
 * maxsize and size clamp, out-of-range or wrapping chunks are rejected.
 */
struct pnspa_snapshot {
    uint32_t format, width, height;
    uint64_t modifier;
    int pixel_fd, acquire_fd, release_fd;
    uint32_t mapoffset, maxsize, offset, size;
    int32_t stride, chunk_flags;
    uint64_t acquire_point, release_point;
    uint32_t sync_flags;
};
int pnspa_snapshot(const struct spa_buffer *buffer,
                   const struct spa_video_info_raw *fixed_format,
                   uint64_t expected_modifier, struct pnspa_snapshot *out);
#endif
