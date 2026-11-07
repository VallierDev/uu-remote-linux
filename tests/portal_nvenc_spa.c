/* Standalone: cc -std=gnu11 -Wall -Wextra -Werror -fsanitize=address,undefined
 * -fno-omit-frame-pointer $(pkg-config --cflags libspa-0.2)
 * tests/portal_nvenc_spa.c c_engine/portal_nvenc_spa.c -o /tmp/pnspa-test && /tmp/pnspa-test */
#include "../c_engine/portal_nvenc_spa.h"
#include <assert.h>
#include <limits.h>
#include <string.h>

int main(void)
{
    struct spa_chunk c = {.offset=8, .size=32, .stride=16};
    struct spa_data d[3] = {{.type=SPA_DATA_DmaBuf,.fd=3,.mapoffset=16,.maxsize=64,.chunk=&c},
        {.type=SPA_DATA_SyncObj,.fd=4}, {.type=SPA_DATA_SyncObj,.fd=5}};
    struct spa_meta_sync_timeline t = {.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,.acquire_point=9,.release_point=10};
    struct spa_meta m[2] = {{.type=SPA_META_SyncTimeline,.size=sizeof(t),.data=&t}, {0}};
    struct spa_buffer b = {.n_datas=3,.datas=d,.n_metas=1,.metas=m};
    struct spa_video_info_raw f = {.format=SPA_VIDEO_FORMAT_BGRA,.flags=SPA_VIDEO_FLAG_MODIFIER,.size={4,2},.modifier=0};
    struct pnspa_snapshot s, saved;
#define OK() pnspa_snapshot(&b,&f,f.modifier,&s)
#define BAD(expr) do { saved=s; expr; assert(!OK()); assert(!memcmp(&s,&saved,sizeof(s))); } while (0)
    assert(OK()); assert(s.pixel_fd==3 && s.acquire_fd==4 && s.release_fd==5 && s.offset==8 && s.acquire_point==9);
    saved=s; c.offset=9; d[0].fd=7; t.acquire_point=8; f.size.width=3;
    assert(s.offset==saved.offset && s.pixel_fd==saved.pixel_fd && s.acquire_point==saved.acquire_point && s.width==saved.width);
    c.offset=8; d[0].fd=3; t.acquire_point=9; f.size.width=4;
    assert(!pnspa_snapshot(NULL,&f,0,&s)); assert(!pnspa_snapshot(&b,NULL,0,&s)); assert(!pnspa_snapshot(&b,&f,0,NULL));
    assert(!pnspa_snapshot(&b,&f,1,&s));
    BAD(b.n_datas=2); b.n_datas=3;
    BAD(b.datas=NULL); b.datas=d;
    BAD(b.n_metas=65); b.n_metas=1;
    BAD(b.metas=NULL); b.metas=m;
    BAD(d[0].type=SPA_DATA_MemFd); d[0].type=SPA_DATA_DmaBuf;
    BAD(d[1].type=SPA_DATA_DmaBuf); d[1].type=SPA_DATA_SyncObj;
    BAD(d[2].fd=-1); d[2].fd=5;
    BAD(d[0].fd=(int64_t)INT_MAX+1); d[0].fd=3;
    BAD(d[0].chunk=NULL); d[0].chunk=&c;
    BAD(c.flags=SPA_CHUNK_FLAG_EMPTY); c.flags=0;
    BAD(c.flags=SPA_CHUNK_FLAG_CORRUPTED); c.flags=0;
    BAD(c.offset=64); c.offset=8;
    BAD(c.size=57); c.size=32;
    BAD(c.size=0); c.size=32;
    BAD(c.stride=-1); c.stride=16;
    BAD(d[0].mapoffset=UINT32_MAX); d[0].mapoffset=16;
    BAD(f.size.width=UINT32_MAX); f.size.width=4;
    BAD(f.size.height=UINT32_MAX); f.size.height=2;
    BAD(f.size.height=0); f.size.height=2;
    BAD(f.format=SPA_VIDEO_FORMAT_NV12); f.format=SPA_VIDEO_FORMAT_BGRA;
    BAD(f.flags=0); f.flags=SPA_VIDEO_FLAG_MODIFIER;
    BAD(f.modifier=UINT64_C(0x00ffffffffffffff)); f.modifier=0;
    BAD(m[0].size--); m[0].size++;
    BAD(m[0].size++); m[0].size--;
    BAD(m[0].data=NULL); m[0].data=&t;
    m[1]=m[0]; BAD(b.n_metas=2); b.n_metas=1;
    BAD(t.acquire_point=0); t.acquire_point=9;
    BAD(t.release_point=9); t.release_point=10;
    BAD(t.flags=2); t.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
    /* Non-linear exact modifier is accepted without pretending stride*height
     * describes its tile footprint; chunk wrap is still rejected. */
    f.modifier=0x1234; f.size.height=UINT32_MAX; c.size=1; c.stride=1;
    assert(OK()); assert(!pnspa_snapshot(&b,&f,0x1235,&s));
    BAD(c.offset=64);
    return 0;
}
