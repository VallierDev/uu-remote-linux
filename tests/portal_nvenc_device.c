#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include "../c_engine/portal_nvenc_device.h"
static int calls;
static EGLBoolean EGLAPIENTRY fail_terminate(EGLDisplay display) {
    assert(display == (EGLDisplay)1); calls++; return EGL_FALSE;
}
int main(int argc, char **argv) {
    struct pnegl_device *d = NULL;
    assert(pnegl_device_open(&d, -1) == -EINVAL && !d);
    int fd = open("/dev/null", O_RDONLY | O_CLOEXEC); assert(fd >= 0);
    assert(pnegl_device_open(&d, fd) == -ENODEV && d && d->fd != fd);
    assert(fcntl(d->fd, F_GETFD) & FD_CLOEXEC);
    assert(pnegl_device_close_idle(&d, 0) == -EBUSY && d);
    assert(!pnegl_device_close_idle(&d, 1) && !d && fcntl(fd,F_GETFD) >= 0);
    close(fd);
    d=calloc(1,sizeof(*d)); assert(d); d->fd=-1; d->initialized=1;
    d->display=(EGLDisplay)1; d->terminate=fail_terminate;
    assert(pnegl_device_close_idle(&d,1)==-EIO && d && calls==1);
    assert(pnegl_device_close_idle(&d,1)==-EINVAL && calls==1);
    free(d); d=NULL; /* mock display only */
    puts("PASS: render-node rejection, owned CLOEXEC fd, idle gate and terminate no-retry");
    if (argc == 2) {
        fd=open(argv[1],O_RDWR | O_CLOEXEC); assert(fd>=0);
        for (int i=0;i<3;i++) {
            int result=pnegl_device_open(&d,fd);
            if(result) { fprintf(stderr,"device initialization result=%d\n",result); return 1; }
            assert(d->initialized && d->display && d->config && d->pci[0]);
            assert(!pnegl_device_close_idle(&d,1) && !d && fcntl(fd,F_GETFD)>=0);
        }
        close(fd); puts("PASS: NVIDIA EGL display initialize/terminate repeated three times");
    }
    return 0;
}
