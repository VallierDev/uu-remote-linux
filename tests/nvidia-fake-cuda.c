/* Fake CUDA driver for status tests, never accesses hardware. */
#include <stdlib.h>
#include <string.h>
static int fail(const char *s) { const char *v = getenv("FAKE_CUDA_FAIL"); return v && !strcmp(v, s); }
int cuInit(unsigned x) { (void)x; return 0; }
int cuGLGetDevices_v2(unsigned *n, int *d, unsigned max, unsigned list) { (void)max; (void)list; *n = 1; *d = 0; return 0; }
int cuCtxCreate_v2(void **ctx, unsigned flags, int dev) { (void)flags; (void)dev; *ctx = (void *)1; return 0; }
int cuCtxDestroy_v2(void *ctx) { (void)ctx; return fail("destroy"); }
int cuGraphicsGLRegisterImage(void **r, unsigned tex, unsigned target, unsigned flags) { (void)tex; (void)target; (void)flags; *r = (void *)1; return 0; }
int cuGraphicsEGLRegisterImage(void **r, void *image, unsigned flags) { (void)image; (void)flags; *r = (void *)1; return 0; }
int cuGraphicsUnregisterResource(void *r) { (void)r; return fail("unregister"); }
int cuGraphicsMapResources(unsigned n, void **r, void *stream) { (void)n; (void)r; (void)stream; return fail("map"); }
int cuGraphicsUnmapResources(unsigned n, void **r, void *stream) { (void)n; (void)r; (void)stream; return fail("unmap"); }
