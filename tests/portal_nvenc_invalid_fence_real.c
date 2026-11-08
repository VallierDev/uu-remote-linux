/* Private kernel syncobjs only: no GPU work, Portal, screen or service.
 * cc -std=gnu11 -Wall -Wextra -Werror -g tests/portal_nvenc_invalid_fence_real.c $(pkg-config --cflags --libs libdrm) -o /tmp/invalid-fence
 * Add -fsanitize=undefined for UBSan. Run: /tmp/invalid-fence [/dev/dri/renderD128]
 * Exit 77: node unavailable or identity/capability unconfirmed; 1: setup,
 * required observation, control validation or resource cleanup failure.
 * Observations are not cross-driver contracts or evidence of GPU completion.
 * Requires trusted local /dev/dri and sysfs; privileged replacement is out of scope. */
#define _GNU_SOURCE
#include <xf86drm.h>
#include <drm.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

static void log_result(const char *label, int r, int e) {
    printf("%s: return=%d errno=%d (%s)\n", label, r, e, e ? strerror(e) : "none");
}
static int pin_render(const char *path, struct stat *st) {
    const char prefix[]="/dev/dri/renderD";
    if (strncmp(path,prefix,sizeof(prefix)-1)) return -1;
    const char *digits=path+sizeof(prefix)-1;
    if (!*digits || strspn(digits,"0123456789")!=strlen(digits)) return -1;
    int fd=open(path,O_PATH|O_NOFOLLOW|O_CLOEXEC);
    if (fd<0) return -1;
    char sys[128], resolved[PATH_MAX], subsystem[PATH_MAX];
    if (fstat(fd,st) || !S_ISCHR(st->st_mode)) goto fail;
    snprintf(sys,sizeof(sys),"/sys/dev/char/%u:%u",major(st->st_rdev),minor(st->st_rdev));
    if (!realpath(sys,resolved)) goto fail;
    const char *base=strrchr(resolved,'/');
    if (!base || strcmp(base+1,path+sizeof("/dev/dri/")-1)) goto fail;
    size_t len=strlen(sys);
    if (len+sizeof("/subsystem")>sizeof(sys)) goto fail;
    memcpy(sys+len,"/subsystem",sizeof("/subsystem"));
    if (!realpath(sys,subsystem) || strcmp(subsystem,"/sys/class/drm")) goto fail;
    return fd;
fail:
    close(fd); return -1;
}
static int observe_target(int fd, uint32_t target, int control) {
    uint64_t point=1, value=UINT64_MAX;
    int r,e,ok=1;
    errno=0; r=drmSyncobjQuery(fd,&target,&value,1); e=errno;
    log_result("target query completed",r,e);
    if (!r) printf("target completed value=%" PRIu64 "\n",value);
    if (r || (control && value<point)) {
        fprintf(stderr,"%s: completed query failed or control point 1 not completed\n",control ? "CONTROL FAILURE" : "OBSERVATION ERROR");
        ok=0;
    }
    value=UINT64_MAX;
    errno=0; r=drmSyncobjQuery2(fd,&target,&value,1,DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED); e=errno;
    log_result("target query LAST_SUBMITTED",r,e);
    if (!r) printf("target last submitted value=%" PRIu64 "\n",value);
    if (r || (control && value<point)) {
        fprintf(stderr,"%s: LAST_SUBMITTED query failed or control point 1 not submitted\n",control ? "CONTROL FAILURE" : "OBSERVATION ERROR");
        ok=0;
    }
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC,&now)) { perror("OBSERVATION ERROR: clock_gettime"); return 0; }
    int64_t until=(int64_t)now.tv_sec*1000000000LL+now.tv_nsec+100000000LL;
    errno=0; r=drmSyncobjTimelineWait(fd,&target,&point,1,until,0,NULL); e=errno;
    log_result("target point 1 finite wait no flags",r,e);
    /* All return codes in the invalid-fence case are observations, not an
     * assertion that a missing point must wait or return a particular errno. */
    if (control && r) {
        fprintf(stderr,"CONTROL FAILURE: point 1 wait failed\n");
        ok=0;
    }
    if (clock_gettime(CLOCK_MONOTONIC,&now)) { perror("OBSERVATION ERROR: clock_gettime"); return 0; }
    until=(int64_t)now.tv_sec*1000000000LL+now.tv_nsec+100000000LL;
    errno=0; r=drmSyncobjTimelineWait(fd,&target,&point,1,until,
        DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT|DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE,NULL); e=errno;
    log_result("target point 1 finite WAIT_AVAILABLE|WAIT_FOR_SUBMIT",r,e);
    if (control && r) {
        fprintf(stderr,"CONTROL FAILURE: point 1 availability wait failed\n");
        ok=0;
    }
    return ok;
}
static int run_case(int fd, int control) {
    uint32_t source=0, imported=0, target=0;
    int have_source=0,have_imported=0,have_target=0,fence=-1,status=1,r,e,import_ok,transfer_ok,observation_ok;
    printf("\ncase: %s\n",control ? "valid sync_file from already CPU-signaled private syncobj" : "invalid sync_file fd -1");
    if (drmSyncobjCreate(fd,0,&imported)) { perror("create imported"); goto done; }
    have_imported=1;
    if (drmSyncobjCreate(fd,0,&target)) { perror("create target"); goto done; }
    have_target=1;
    if (control) {
        if (drmSyncobjCreate(fd,0,&source)) { perror("create source"); goto done; }
        have_source=1;
        errno=0; r=drmSyncobjSignal(fd,&source,1); e=errno;
        log_result("CPU signal source (not GPU)",r,e);
        if (r) goto done;
        errno=0; r=drmSyncobjExportSyncFile(fd,source,&fence); e=errno;
        log_result("export already signaled source sync_file",r,e);
        if (r || fence<0) goto done;
    }
    errno=0; r=drmSyncobjImportSyncFile(fd,imported,fence); e=errno;
    log_result("import sync_file",r,e);
    import_ok=!r;
    /* Even a failed import can mutate its object. Probe that exact private
     * object via transfer, never a shared or pre-existing object. */
    errno=0; r=drmSyncobjTransfer(fd,target,1,imported,0,0); e=errno;
    log_result("transfer imported binary point 0 to target point 1",r,e);
    transfer_ok=!r;
    observation_ok=observe_target(fd,target,control);
    if (control && (!import_ok || !transfer_ok))
        fprintf(stderr,"CONTROL FAILURE: valid sync_file import and transfer must both succeed\n");
    status=!(observation_ok && (!control || (import_ok && transfer_ok)));
done:
    if (fence>=0 && close(fence)) { perror("close sync_file"); status=1; }
    if (have_source && drmSyncobjDestroy(fd,source)) status=1;
    if (have_imported && drmSyncobjDestroy(fd,imported)) status=1;
    if (have_target && drmSyncobjDestroy(fd,target)) status=1;
    return status;
}
int main(int argc, char **argv) {
    if (argc>2) { fprintf(stderr,"usage: %s [render node]\n",argv[0]); return 1; }
    const char *path=argc==2 ? argv[1] : "/dev/dri/renderD128";
    struct stat pinned_stat, opened;
    int pin=pin_render(path,&pinned_stat);
    if (pin<0) { fprintf(stderr,"UNSUPPORTED: cannot pin and verify render node %s\n",path); return 77; }
    char proc[64]; snprintf(proc,sizeof(proc),"/proc/self/fd/%d",pin);
    int fd=open(proc,O_RDWR|O_NONBLOCK|O_CLOEXEC);
    if (fd<0) { perror("UNSUPPORTED: open pinned node"); close(pin); return 77; }
    if (fstat(fd,&opened) || !S_ISCHR(opened.st_mode) || opened.st_dev!=pinned_stat.st_dev ||
        opened.st_ino!=pinned_stat.st_ino || opened.st_rdev!=pinned_stat.st_rdev ||
        drmGetNodeTypeFromFd(fd)!=DRM_NODE_RENDER) {
        fprintf(stderr,"UNSUPPORTED: render node identity/type unconfirmed\n"); close(fd); close(pin); return 77;
    }
    drmVersionPtr version=drmGetVersion(fd);
    if (!version) { fprintf(stderr,"UNSUPPORTED: DRM driver identity unavailable\n"); close(fd); close(pin); return 77; }
    printf("verified render node %s driver=%.*s version=%d.%d.%d device=%u:%u\n",path,
        (int)version->name_len,version->name,version->version_major,version->version_minor,version->version_patchlevel,
        major(opened.st_rdev),minor(opened.st_rdev));
    drmFreeVersion(version);
    uint64_t cap=0;
    if (drmGetCap(fd,DRM_CAP_SYNCOBJ_TIMELINE,&cap) || !cap) {
        fprintf(stderr,"UNSUPPORTED: timeline syncobj unavailable\n"); close(fd); close(pin); return 77;
    }
    puts("Private synthetic syncobjs only; CPU signal is not GPU/frame completion. errno shown is raw errno after clearing it.");
    int a=run_case(fd,0), b=run_case(fd,1);
    int close_error = 0;
    if (close(fd)) { perror("close DRM node"); close_error = 1; }
    if (close(pin)) { perror("close pinned node"); close_error = 1; }
    return a || b || close_error;
}
