#ifndef PORTAL_NVENC_EGL_ATTRS_H
#define PORTAL_NVENC_EGL_ATTRS_H
#include "portal_nvenc_spa.h"
#include <EGL/egl.h>

/* Pure metadata conversion only: NOT an authorization, bounds, provenance,
 * synchronization, modifier support or importability proof. The caller must
 * hold a trusted exclusive lease and keep the borrowed fd alive throughout
 * import and GPU use. No EGL/GPU operation or fd ownership transfer occurs.
 * On failure the output is unchanged. */
#define PNEGL_ATTR_COUNT 17
int pnegl_attrs(const struct pnspa_snapshot *s, EGLint out[PNEGL_ATTR_COUNT]);
#endif
