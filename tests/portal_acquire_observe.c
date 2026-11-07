/* Hardware check with self-created syncobjs only; no Portal or pixel access. */
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>
#include <glib.h>
#include <xf86drm.h>
#include "../contrib/portal-acquire-observe.inc"
int main(int argc,char **argv) {
    assert(argc==2);
    int fd=open(argv[1],O_RDWR|O_CLOEXEC); assert(fd>=0);
    uint32_t handle=0; int sync=-1;
    assert(!drmSyncobjCreate(fd,0,&handle)); assert(!drmSyncobjHandleToFD(fd,handle,&sync));
    gint64 start=g_get_monotonic_time();
    int result=observe_acquire(argv[1],sync,1,sync,2,30);
    assert(result==-ETIME || result==-ETIMEDOUT);
    assert(g_get_monotonic_time()-start<500000);
    uint64_t point=0;
    assert(!drmSyncobjQuery(fd,&handle,&point,1) && point==0);
    point=1; assert(!drmSyncobjTimelineSignal(fd,&handle,&point,1));
    assert(!observe_acquire(argv[1],sync,1,sync,2,30));
    assert(!drmSyncobjQuery(fd,&handle,&point,1) && point==1);
    assert(observe_acquire(argv[1],sync,2,sync,2,30)==-EINVAL);
    point=2; assert(!drmSyncobjTimelineSignal(fd,&handle,&point,1));
    assert(observe_acquire(argv[1],sync,1,sync,2,30)==-ESTALE);
    int invalid=open("/dev/null",O_RDONLY|O_CLOEXEC); assert(invalid>=0);
    assert(observe_acquire(argv[1],sync,1,invalid,3,30)==-EIO);
    close(invalid); close(sync); assert(!drmSyncobjDestroy(fd,handle)); close(fd);
    puts("PASS: bounded unsignaled wait, completed acquire, already released rejection, import failure; no observer signal");
}
