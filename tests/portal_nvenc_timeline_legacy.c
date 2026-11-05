/* Compile the no-explicit-sync branch without accessing SPA metadata. */
#include <spa/buffer/meta.h>
#undef SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE
#include "../c_engine/portal_nvenc_timeline.c"
#include <assert.h>
int main(void) { assert(pnt_release(NULL, NULL) == -ENOTSUP); return 0; }
