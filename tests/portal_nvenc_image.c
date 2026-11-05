#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include "../c_engine/portal_nvenc_image.h"
static int creates, destroys, fail_create, fail_destroy;
static EGLImageKHR EGLAPIENTRY create(EGLDisplay d, EGLContext c, EGLenum t, EGLClientBuffer b, const EGLint *a) {
    assert(d == (EGLDisplay)1 && c == EGL_NO_CONTEXT && t == EGL_LINUX_DMA_BUF_EXT && !b);
    assert(a[0] == EGL_WIDTH && a[1] == 2 && a[6] == EGL_DMA_BUF_PLANE0_FD_EXT && a[7] == 4);
    creates++; return fail_create ? EGL_NO_IMAGE_KHR : (EGLImageKHR)2;
}
static EGLBoolean EGLAPIENTRY destroy(EGLDisplay d, EGLImageKHR i) {
    assert(d == (EGLDisplay)1 && i == (EGLImageKHR)2); destroys++;
    return fail_destroy ? EGL_FALSE : EGL_TRUE;
}
int main(void) {
    struct pnspa_snapshot frame = { .format=SPA_VIDEO_FORMAT_BGRA, .width=2, .height=2,
        .stride=8, .pixel_fd=4, .modifier=0 };
    struct pnegl_image owner = {0};
    assert(!pnegl_image_create(&owner,(EGLDisplay)1,&frame,create,destroy));
    assert(pnegl_image_create(&owner,(EGLDisplay)1,&frame,create,destroy)==-EINVAL && creates==1);
    assert(pnegl_image_destroy_completed(&owner,0)==-EBUSY && !destroys);
    assert(!pnegl_image_destroy_completed(&owner,1) && destroys==1 && !owner.image);
    assert(pnegl_image_destroy_completed(&owner,1)==-EINVAL && destroys==1);
    frame.stride=1;
    assert(pnegl_image_create(&owner,(EGLDisplay)1,&frame,create,destroy)==-EINVAL && creates==1);
    frame.stride=8; fail_create=1;
    assert(pnegl_image_create(&owner,(EGLDisplay)1,&frame,create,destroy)==-EIO && !owner.image);
    fail_create=0;
    assert(!pnegl_image_create(&owner,(EGLDisplay)1,&frame,create,destroy));
    fail_destroy=1;
    assert(pnegl_image_destroy_completed(&owner,1)==-EIO && owner.image && destroys==2);
    assert(pnegl_image_destroy_completed(&owner,1)==-EINVAL && destroys==2);
    puts("PASS: EGL image creation, metadata rejection, completion gate and no retry after destroy failure");
    return 0;
}
