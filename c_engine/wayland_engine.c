/* Native Hyprland/wlroots desktop bridge. Protocol XMLs retain upstream licenses. */
#define _GNU_SOURCE
#include "wayland_transform.h"
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include <pthread.h>
#include <poll.h>
#include <sys/mman.h>
#include <unistd.h>
#include <time.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <fcntl.h>
#include <dlfcn.h>
#include "linux-dmabuf-unstable-v1-client-protocol.h"
#include "wlr-screencopy-unstable-v1-client-protocol.h"
#include "wlr-virtual-pointer-unstable-v1-client-protocol.h"
#include "virtual-keyboard-unstable-v1-client-protocol.h"
#include "wlr-output-power-management-unstable-v1-client-protocol.h"

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static struct wl_display *display;
static struct wl_registry *registry;
static struct wl_shm *shm;
static struct wl_seat *seat;
static struct zwlr_screencopy_manager_v1 *copy_manager;
static struct zwp_linux_dmabuf_v1 *dmabuf;
static int dmabuf_requested,dmabuf_disabled,dmabuf_reported;
static unsigned dmabuf_linear_formats;
/* GBM is loaded only for explicit opt-in; default builds need no GBM headers. */
struct gbm_device;
struct gbm_bo;
static struct {
    void *library;
    int fd;
    struct gbm_device *device;
    struct gbm_device *(*create_device)(int);
    void (*device_destroy)(struct gbm_device*);
    struct gbm_bo *(*bo_create)(struct gbm_device*,uint32_t,uint32_t,uint32_t,uint32_t);
    void (*bo_destroy)(struct gbm_bo*);
    int (*bo_get_fd)(struct gbm_bo*);
    int (*bo_get_plane_count)(struct gbm_bo*);
    uint32_t (*bo_get_stride)(struct gbm_bo*);
    uint32_t (*bo_get_offset)(struct gbm_bo*,int);
    uint64_t (*bo_get_modifier)(struct gbm_bo*);
    void *(*bo_map)(struct gbm_bo*,uint32_t,uint32_t,uint32_t,uint32_t,uint32_t,uint32_t*,void**);
    void (*bo_unmap)(struct gbm_bo*,void*);
} gbm={.fd=-1};
static struct zwlr_virtual_pointer_manager_v1 *pointer_manager;
static struct zwp_virtual_keyboard_manager_v1 *keyboard_manager;
static struct zwlr_virtual_pointer_v1 *pointer;
static struct zwp_virtual_keyboard_v1 *keyboard;
static struct zwlr_output_power_manager_v1 *power_manager;
static struct zwlr_output_power_v1 *power;
static int power_failed;
static struct xkb_context *xkb;
static struct xkb_keymap *keymap;
static struct xkb_state *key_state;
static unsigned char held_keys[768];
/* Mobile input-method text (#13/#46). The protocol has no per-key remap request, but
   zwp_virtual_keyboard_v1_keymap may be re-sent at any time, so the pristine keymap text is kept
   and printable codes on a separate text keyboard carry each batch. Never logs the text. */
#define UU_TEXT_SPARE 32
static char *keymap_text;
static size_t keymap_text_len, keymap_insert;
static xkb_keycode_t spare_codes[UU_TEXT_SPARE];
static unsigned spare_count;
struct output { struct wl_output *handle; char name[128]; int transform,scale; uint32_t width,height; };
static struct output outputs[32];
static unsigned output_count, selected;
static uint32_t width,height;
static int initialized;
/* Independent slots let the compositor fill the next frame during delivery. */
struct frame {
    struct zwlr_screencopy_frame_v1 *handle;
    struct wl_buffer *buffer,*shm_buffer;
    struct wl_shm_pool *shm_pool;
    void *pixels,*shm_pixels;
    size_t shm_length;
    int shm_fd,shm_busy;
    uint32_t shm_width,shm_height,shm_stride,shm_format;
    uint32_t width,height,stride,format,flags;
    int done,failed,submitted,released;
    struct zwp_linux_buffer_params_v1 *params;
    struct gbm_bo *bo;
    int bo_fd,dmabuf_used;
    void *map_data;
    uint32_t dma_format,dma_width,dma_height;
    uint32_t bo_format,bo_width,bo_height;
};
static struct frame frames[2]={{.shm_fd=-1,.bo_fd=-1},{.shm_fd=-1,.bo_fd=-1}};
static unsigned pending;

static uint32_t timestamp(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t);
    return (uint32_t)((uint64_t)t.tv_sec*1000+t.tv_nsec/1000000);
}
/* Never let a disappearing compositor block a worker indefinitely. */
static int dispatch_until(int *condition) {
    uint32_t start=timestamp();
    while (!*condition) {
        while (wl_display_prepare_read(display)!=0) {
            if (wl_display_dispatch_pending(display)<0) return -1;
            if (*condition) return 0;
        }
        if (wl_display_flush(display)<0) { wl_display_cancel_read(display); return -1; }
        struct pollfd fd={wl_display_get_fd(display),POLLIN,0};
        int wait=poll(&fd,1,100);
        if (wait>0 && (fd.revents&POLLIN)) {
            if (wl_display_read_events(display)<0) return -1;
            if (wl_display_dispatch_pending(display)<0) return -1;
        } else { wl_display_cancel_read(display); if (wait<0 || fd.revents&(POLLERR|POLLHUP)) return -1; }
        if ((uint32_t)(timestamp()-start)>3000) return -1;
    }
    return 0;
}
static void sync_done(void *data,struct wl_callback *cb,uint32_t serial) { *(int*)data=1; wl_callback_destroy(cb); }
static const struct wl_callback_listener sync_listener={sync_done};
static int sync_display(void) {
    int ready=0; struct wl_callback *cb=wl_display_sync(display);
    wl_callback_add_listener(cb,&sync_listener,&ready);
    int result=dispatch_until(&ready);
    if (!ready) wl_callback_destroy(cb);
    return result;
}
/* Hand a keymap to the virtual keyboard. The compositor dups the fd, so it is closed here. */
static int upload_keymap(const char *text,size_t length) {
    int fd=memfd_create("uuremote-keymap",MFD_CLOEXEC);
    if(fd<0) return -1;
    if(ftruncate(fd,(off_t)length)!=0 || write(fd,text,length)!=(ssize_t)length) { close(fd); return -1; }
    zwp_virtual_keyboard_v1_keymap(keyboard,WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1,fd,(uint32_t)length);
    close(fd);
    return 0;
}
/* A keymap uploaded to a keyboard the seat already has is never handed to the focused client again,
   so pressing the remapped keycodes would still be translated with the keymap it has - which is how
   every message came out as the one before it. Re-attaching the keyboard is what makes the
   compositor send the keymap to the client (measured 2026-10-18 on Hyprland 0.56.2). */
static int rekey_keyboard(void) {
    if(!keyboard_manager || !seat) return -1;
    if(keyboard) zwp_virtual_keyboard_v1_destroy(keyboard);
    keyboard=zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(keyboard_manager,seat);
    return keyboard?0:-1;
}
/* Offset of the '}' that closes the xkb_symbols section, so extra keys can be declared there.
   Pairing the braces is enough because the bodies are balanced; looking for the last "};" is not,
   the keymap's own brace follows, and an optional xkb_geometry section may too. */
static size_t symbols_close(const char *text) {
    const char *section=strstr(text,"xkb_symbols");
    if(!section) return 0;
    const char *open=strchr(section,'{');
    if(!open) return 0;
    int depth=0;
    for(const char *p=open;*p;p++) {
        if(*p=='{') depth++;
        else if(*p=='}' && --depth==0) return (size_t)(p-text);
    }
    return 0;
}
/* Named keycodes the keymap gives no symbol to. Reading the compositor's own list is the only
   portable way: pressing an unmapped keycode types nothing, and uu_wl_key never matches one
   because it searches by keysym. Highest first, since uu_wl_key takes the lowest match. */
static unsigned find_spare_codes(xkb_keycode_t *out,unsigned limit) {
    unsigned count=0;
    xkb_keycode_t min=xkb_keymap_min_keycode(keymap);
    for(xkb_keycode_t code=xkb_keymap_max_keycode(keymap);code>min && count<limit;code--) {
        if(code<8 || code>=sizeof(held_keys) || !xkb_keymap_key_get_name(keymap,code)) continue;
        int mapped=0;
        for(xkb_level_index_t level=0;level<xkb_keymap_num_levels_for_key(keymap,code,0) && !mapped;level++) {
            const xkb_keysym_t *syms;
            if(xkb_keymap_key_get_syms_by_level(keymap,code,0,level,&syms)>0) mapped=1;
        }
        if(!mapped) out[count++]=code;
    }
    return count;
}
/* A keycode the session keymap already produces this symbol with, unshifted. Typing through it
   needs no upload, so no client can mistranslate it - and it is how plain latin text and spaces
   arrive without touching the keymap at all. Spares never match: the pristine keymap leaves them
   unmapped. */
static xkb_keycode_t existing_keycode(xkb_keysym_t symbol) {
    for(xkb_keycode_t code=xkb_keymap_min_keycode(keymap);code<=xkb_keymap_max_keycode(keymap);code++) {
        if(code<8 || code>=sizeof(held_keys)) continue;
        const xkb_keysym_t *syms;
        if(xkb_keymap_key_get_syms_by_level(keymap,code,0,0,&syms)==1 && syms[0]==symbol) return code;
    }
    return 0;
}
static unsigned find_niri_text_codes(xkb_keycode_t *out,unsigned limit) {
    /* Printable physical keys work in Chromium; reserved evdev codes can be discarded. */
    static const xkb_keycode_t text_codes[]={24,25,26,27,28,29,30,31,32,33,38,39,40,41,42,43,44,45,46,52,53,54,55,56,57,58};
    unsigned count=0;
    for(unsigned i=0;i<sizeof(text_codes)/sizeof(text_codes[0]) && count<limit;i++)
        if(xkb_keymap_key_get_name(keymap,text_codes[i])) out[count++]=text_codes[i];
    return count;
}
static void press_keycode(xkb_keycode_t code) {
    zwp_virtual_keyboard_v1_key(keyboard,timestamp(),code-8,WL_KEYBOARD_KEY_STATE_PRESSED);
    zwp_virtual_keyboard_v1_key(keyboard,timestamp(),code-8,WL_KEYBOARD_KEY_STATE_RELEASED);
    held_keys[code]=0;
}
/* The replaced keyboard starts with nothing held, and a keycode held_keys still calls held would be
   skipped by uu_wl_key's next press of it. */
static void release_held_keys_locked(void) {
    for(xkb_keycode_t code=xkb_keymap_min_keycode(keymap);code<=xkb_keymap_max_keycode(keymap);code++) {
        if(code<8 || code>=sizeof(held_keys) || !held_keys[code]) continue;
        zwp_virtual_keyboard_v1_key(keyboard,timestamp(),code-8,WL_KEYBOARD_KEY_STATE_RELEASED);
        xkb_state_update_key(key_state,code,XKB_KEY_UP);
    }
    memset(held_keys,0,sizeof(held_keys));
}
/* Upload one batch: re-attach the keyboard so the client takes the keymap, point spare keycodes at
   these symbols, wait for the compositor to install it, then press them. Spares are left out of
   key_state and held_keys: pressed and released here. */
static int flush_batch(char *buffer,size_t capacity,const char names[][64],const xkb_keycode_t *codes,unsigned count,int *typed) {
    release_held_keys_locked();
    if(getenv("NIRI_SOCKET")) {if(sync_display()!=0) return -1;usleep(8000);}
    else if(rekey_keyboard()!=0) return -1;
    size_t at=keymap_insert;
    memcpy(buffer,keymap_text,at);
    for(unsigned i=0;i<count;i++) {
        int wrote=snprintf(buffer+at,capacity-at,"\tkey <%s> { [ %s ] };\n",xkb_keymap_key_get_name(keymap,codes[i]),names[i]);
        if(wrote<0 || (size_t)wrote>=capacity-at) return -1;
        at+=(size_t)wrote;
    }
    memcpy(buffer+at,keymap_text+keymap_insert,keymap_text_len-keymap_insert);
    if(upload_keymap(buffer,at+(keymap_text_len-keymap_insert))!=0) return -1;
    if(getenv("NIRI_SOCKET")) zwp_virtual_keyboard_v1_modifiers(keyboard,0,0,0,0);
    if(sync_display()!=0) return -1;
    if(getenv("NIRI_SOCKET")) usleep(8000);
    for(unsigned i=0;i<count;i++) { press_keycode(codes[i]); (*typed)++; }
    if(getenv("NIRI_SOCKET")) {if(sync_display()!=0) return -1;usleep(8000);}
    return 0;
}
/* Type UTF-8 like xdotool type does on X11, but on Wayland: characters the session keymap already
   has go out as ordinary keycodes, the rest share the spares of one keymap upload (~70 KB, so one
   batch per upload), in text order. Never logs the text. */
static int type_utf8_locked(const char *text) {
    size_t capacity=keymap_text_len+(size_t)UU_TEXT_SPARE*96+64;
    char *buffer=malloc(capacity);
    if(!buffer) return -1;
    char names[UU_TEXT_SPARE][64];
    xkb_keycode_t codes[UU_TEXT_SPARE];
    unsigned count=0;
    int typed=0, failed=0;
    const unsigned char *p=(const unsigned char *)text;
    while(*p && !failed) {
        unsigned cp=0; int n=0;
        if(*p<0x80) { cp=*p; n=1; }
        else if((*p&0xE0)==0xC0 && (p[1]&0xC0)==0x80) { cp=((p[0]&0x1F)<<6)|(p[1]&0x3F); n=2; }
        else if((*p&0xF0)==0xE0 && (p[1]&0xC0)==0x80 && (p[2]&0xC0)==0x80) { cp=((p[0]&0x0F)<<12)|((p[1]&0x3F)<<6)|(p[2]&0x3F); n=3; }
        else if((*p&0xF8)==0xF0 && (p[1]&0xC0)==0x80 && (p[2]&0xC0)==0x80 && (p[3]&0xC0)==0x80) { cp=((p[0]&0x07)<<18)|((p[1]&0x3F)<<12)|((p[2]&0x3F)<<6)|(p[3]&0x3F); n=4; }
        else { p++; continue; }
        p+=n;
        xkb_keysym_t sym;
        if(cp=='\n') sym=0xff0d;                                       /* Return */
        else if(cp=='\t') sym=0xff09;                                  /* Tab */
        else if(cp<0x20 || cp==0x7f || (cp>=0x80 && cp<0xa0)) continue; /* controls carry no text */
        else if(cp<0x100) sym=cp;                                      /* Latin-1 keysyms equal the code point */
        else sym=(xkb_keysym_t)(0x1000000|cp);                         /* Unicode keysym */
        xkb_keycode_t existing=getenv("NIRI_SOCKET")?0:existing_keycode(sym);
        if(existing) {
            if(count && flush_batch(buffer,capacity,names,codes,count,&typed)!=0) { failed=1; break; }
            count=0;
            press_keycode(existing); typed++;
            continue;
        }
        if(count==(getenv("NIRI_SOCKET")?spare_count:UU_TEXT_SPARE)) {
            if(flush_batch(buffer,capacity,names,codes,count,&typed)!=0) { failed=1; break; }
            count=0;
        }
        /* The name is what the keymap parser reads back; an unnamed keysym cannot be spelled. */
        if(xkb_keysym_get_name(sym,names[count],sizeof(names[count]))<=0) continue;
        codes[count]=spare_codes[count];
        count++;
    }
    if(!failed && count) failed=flush_batch(buffer,capacity,names,codes,count,&typed)!=0;
    free(buffer);
    /* Restore the pristine keymap whatever happened above, so uu_wl_key keeps its own symbol map. */
    if(getenv("NIRI_SOCKET")) {if(sync_display()!=0) return -1;usleep(8000);}
    else if(upload_keymap(keymap_text,keymap_text_len)!=0) return -1;
    return failed && !typed?-1:typed;
}
int uu_wl_type_utf8(const char *text) {
    pthread_mutex_lock(&lock);
    int typed=-1;
    if(initialized && text && keymap_text && keymap_insert && spare_count) {
        if(getenv("NIRI_SOCKET")) {
            /* Only niri uses printable codes on a separate text device; preserve the Hyprland path. */
            release_held_keys_locked();
            struct zwp_virtual_keyboard_v1 *saved=keyboard;
            keyboard=zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(keyboard_manager,seat);
            if(keyboard) {typed=type_utf8_locked(text);sync_display();usleep(8000);if(keyboard) zwp_virtual_keyboard_v1_destroy(keyboard);}
            keyboard=saved;
        } else typed=type_utf8_locked(text);
    }
    if(typed>=0 && wl_display_flush(display)<0) typed=-1;
    pthread_mutex_unlock(&lock); return typed;
}
/* Diagnostic: how many characters one keymap upload can carry, 0 when this keymap has no spare. */
int uu_wl_text_ready(void) {
    pthread_mutex_lock(&lock);
    int ready=initialized && keymap_text && keymap_insert ? (int)spare_count : 0;
    pthread_mutex_unlock(&lock); return ready;
}
static void output_geometry(void*d,struct wl_output*o,int32_t x,int32_t y,int32_t w,int32_t h,int32_t sub,const char*m,const char*model,int32_t transform) { ((struct output*)d)->transform=transform; }
static void output_mode(void*d,struct wl_output*o,uint32_t f,int32_t w,int32_t h,int32_t rate) {
    if(f&WL_OUTPUT_MODE_CURRENT) { ((struct output*)d)->width=w; ((struct output*)d)->height=h; }
}
static void output_done(void*d,struct wl_output*o) {}
static void output_scale(void*d,struct wl_output*o,int32_t scale) { if(scale>0 && scale<=16) ((struct output*)d)->scale=scale; }
static void output_name(void*d,struct wl_output*o,const char*name) { snprintf(((struct output*)d)->name,128,"%s",name); }
static void output_description(void*d,struct wl_output*o,const char*desc) {}
static const struct wl_output_listener output_listener={output_geometry,output_mode,output_done,output_scale,output_name,output_description};
static unsigned dma_format_bit(uint32_t format) {
    switch(format) {
        case 0x34325258: return 1; /* DRM_FORMAT_XRGB8888 */
        case 0x34325241: return 2; /* DRM_FORMAT_ARGB8888 */
        case 0x34324258: return 4; /* DRM_FORMAT_XBGR8888 */
        case 0x34324241: return 8; /* DRM_FORMAT_ABGR8888 */
        default: return 0;
    }
}
static void dma_format(void*d,struct zwp_linux_dmabuf_v1*b,uint32_t fmt) {}
static void dma_modifier(void*d,struct zwp_linux_dmabuf_v1*b,uint32_t fmt,uint32_t hi,uint32_t lo) {
    if(!hi && !lo) dmabuf_linear_formats|=dma_format_bit(fmt);
}
static const struct zwp_linux_dmabuf_v1_listener dma_listener={dma_format,dma_modifier};
static void global(void*d,struct wl_registry*r,uint32_t id,const char*interface,uint32_t version) {
    if (!strcmp(interface,"wl_shm")) shm=wl_registry_bind(r,id,&wl_shm_interface,1);
    else if (!strcmp(interface,"wl_seat") && !seat) seat=wl_registry_bind(r,id,&wl_seat_interface,1);
    else if (!strcmp(interface,"wl_output") && output_count<32) {
        struct output *o=&outputs[output_count++];
        o->scale=1;
        o->handle=wl_registry_bind(r,id,&wl_output_interface,version<4?version:4);
        wl_output_add_listener(o->handle,&output_listener,o);
    } else if (!strcmp(interface,"zwp_linux_dmabuf_v1") && dmabuf_requested && version>=3) {
        dmabuf=wl_registry_bind(r,id,&zwp_linux_dmabuf_v1_interface,3);
        zwp_linux_dmabuf_v1_add_listener(dmabuf,&dma_listener,NULL);
    } else if (!strcmp(interface,"zwlr_screencopy_manager_v1")) copy_manager=wl_registry_bind(r,id,&zwlr_screencopy_manager_v1_interface,version<3?version:3);
    else if (!strcmp(interface,"zwlr_virtual_pointer_manager_v1") && version>=2) pointer_manager=wl_registry_bind(r,id,&zwlr_virtual_pointer_manager_v1_interface,2);
    else if (!strcmp(interface,"zwp_virtual_keyboard_manager_v1")) keyboard_manager=wl_registry_bind(r,id,&zwp_virtual_keyboard_manager_v1_interface,1);
    else if (!strcmp(interface,"zwlr_output_power_manager_v1")) power_manager=wl_registry_bind(r,id,&zwlr_output_power_manager_v1_interface,1);
}
static void global_remove(void*d,struct wl_registry*r,uint32_t id) { /* failed captures are returned to caller */ }
static const struct wl_registry_listener registry_listener={global,global_remove};
static void power_mode(void*d,struct zwlr_output_power_v1*p,uint32_t mode) {}
static void power_failure(void*d,struct zwlr_output_power_v1*p) { power_failed=1; }
static const struct zwlr_output_power_v1_listener power_listener={power_mode,power_failure};
static void dma_disable(const char *reason) {
    /* reason is always a fixed diagnostic tag, never driver/user text. */
    if(!dmabuf_disabled) fprintf(stderr,"Wayland dma-buf unavailable; falling back to SHM (%s)\n",reason);
    dmabuf_disabled=1;
}
static void dma_init(void) {
    if(!dmabuf_requested) return;
    if(!dmabuf) {dma_disable("protocol"); return;}
    if(!dmabuf_linear_formats) {dma_disable("linear-format"); return;}
    gbm.library=dlopen("libgbm.so.1",RTLD_NOW|RTLD_LOCAL);
    if(!gbm.library) {dma_disable("gbm-library"); return;}
#define GBM_LOAD(field,name) do { *(void**)(&gbm.field)=dlsym(gbm.library,name); if(!gbm.field) {dma_disable("gbm-symbol"); return;} } while(0)
    GBM_LOAD(create_device,"gbm_create_device"); GBM_LOAD(device_destroy,"gbm_device_destroy");
    GBM_LOAD(bo_create,"gbm_bo_create"); GBM_LOAD(bo_destroy,"gbm_bo_destroy");
    GBM_LOAD(bo_get_fd,"gbm_bo_get_fd"); GBM_LOAD(bo_get_plane_count,"gbm_bo_get_plane_count");
    GBM_LOAD(bo_get_stride,"gbm_bo_get_stride"); GBM_LOAD(bo_get_offset,"gbm_bo_get_offset");
    GBM_LOAD(bo_get_modifier,"gbm_bo_get_modifier"); GBM_LOAD(bo_map,"gbm_bo_map"); GBM_LOAD(bo_unmap,"gbm_bo_unmap");
#undef GBM_LOAD
    const char *device=getenv("UU_WAYLAND_DMABUF_DEVICE");
    gbm.fd=open(device && *device ? device:"/dev/dri/renderD128",O_RDWR|O_CLOEXEC);
    if(gbm.fd<0 || !(gbm.device=gbm.create_device(gbm.fd))) dma_disable("render-device");
}
static void shm_clear(struct frame *frame) {
    if(frame->shm_buffer) wl_buffer_destroy(frame->shm_buffer);
    if(frame->shm_pool) wl_shm_pool_destroy(frame->shm_pool);
    if(frame->shm_pixels) munmap(frame->shm_pixels,frame->shm_length);
    if(frame->shm_fd>=0) close(frame->shm_fd);
    frame->shm_buffer=NULL; frame->shm_pool=NULL; frame->shm_pixels=NULL;
    frame->shm_length=0; frame->shm_fd=-1; frame->shm_busy=0;
    frame->shm_width=frame->shm_height=frame->shm_stride=frame->shm_format=0;
}
static void dma_clear(struct frame *frame) {
    if(frame->buffer) wl_buffer_destroy(frame->buffer);
    if(frame->dmabuf_used && frame->pixels) {
        gbm.bo_unmap(frame->bo,frame->map_data); frame->pixels=NULL; frame->map_data=NULL;
    }
    if(frame->bo) { if(frame->bo_fd>=0)close(frame->bo_fd); gbm.bo_destroy(frame->bo); }
    frame->buffer=NULL; frame->bo=NULL; frame->bo_fd=-1; frame->dmabuf_used=frame->released=0;
    frame->bo_format=frame->bo_width=frame->bo_height=0;
}
static void frame_clear(struct frame *frame) {
    if(frame->handle) zwlr_screencopy_frame_v1_destroy(frame->handle);
    if(frame->params) zwp_linux_buffer_params_v1_destroy(frame->params);
    if(frame->dmabuf_used && frame->pixels) gbm.bo_unmap(frame->bo,frame->map_data);
    if(frame->shm_busy) shm_clear(frame);
    frame->handle=NULL; frame->params=NULL;
    frame->pixels=NULL; frame->map_data=NULL; frame->dmabuf_used=0;
    frame->width=frame->height=frame->stride=frame->format=frame->flags=0;
    frame->done=frame->failed=frame->submitted=0;
    frame->dma_format=frame->dma_width=frame->dma_height=0;
}
static void buffer_release(void*d,struct wl_buffer*b) { ((struct frame*)d)->released=1; }
static const struct wl_buffer_listener buffer_listener={buffer_release};
static void frame_fail(struct frame *frame) { frame->failed=frame->done=frame->submitted=1; }
static void copy_buffer(void*d,struct zwlr_screencopy_frame_v1*f,uint32_t fmt,uint32_t w,uint32_t h,uint32_t s) {
    struct frame *frame=d;
    frame->format=fmt; frame->width=w; frame->height=h; frame->stride=s;
    if (w==0 || h==0 || w>16384 || h>16384 || s<w*4 || s>131072 || (uint64_t)s*h>256*1024*1024 ||
        (fmt!=WL_SHM_FORMAT_XRGB8888 && fmt!=WL_SHM_FORMAT_ARGB8888 &&
         fmt!=WL_SHM_FORMAT_XBGR8888 && fmt!=WL_SHM_FORMAT_ABGR8888)) { frame_fail(frame); return; }
    size_t length=(size_t)s*h;
    if (frame->shm_busy || !frame->shm_buffer || !frame->shm_pool || !frame->shm_pixels || frame->shm_fd<0 ||
        frame->shm_length!=length || frame->shm_width!=w || frame->shm_height!=h ||
        frame->shm_stride!=s || frame->shm_format!=fmt) {
        shm_clear(frame);
        int fd=memfd_create("uuremote-wayland-frame",MFD_CLOEXEC);
        if (fd<0 || ftruncate(fd,(off_t)length)!=0) { if(fd>=0)close(fd); frame_fail(frame); return; }
        void *pixels=mmap(NULL,length,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
        if (pixels==MAP_FAILED) { close(fd); frame_fail(frame); return; }
        struct wl_shm_pool *pool=wl_shm_create_pool(shm,fd,(int32_t)length);
        if (!pool) { munmap(pixels,length); close(fd); frame_fail(frame); return; }
        struct wl_buffer *buffer=wl_shm_pool_create_buffer(pool,0,w,h,s,fmt);
        if (!buffer) { wl_shm_pool_destroy(pool); munmap(pixels,length); close(fd); frame_fail(frame); return; }
        frame->shm_fd=fd; frame->shm_pixels=pixels; frame->shm_length=length;
        frame->shm_pool=pool; frame->shm_buffer=buffer;
        frame->shm_width=w; frame->shm_height=h; frame->shm_stride=s; frame->shm_format=fmt;
    }
    frame->pixels=frame->shm_pixels;
    /* copy is valid immediately for SHM, including frame versions <3. */
    if (zwlr_screencopy_frame_v1_get_version(f)<3) {
        zwlr_screencopy_frame_v1_copy(f,frame->shm_buffer); frame->shm_busy=frame->submitted=1;
    }
}
static void copy_flags(void*d,struct zwlr_screencopy_frame_v1*f,uint32_t value) { ((struct frame*)d)->flags=value; }
static void copy_ready(void*d,struct zwlr_screencopy_frame_v1*f,uint32_t hi,uint32_t lo,uint32_t ns) {
    struct frame *frame=d;
    frame->shm_busy=0;
    if(frame->dmabuf_used) {
        /* GBM READ mapping synchronizes readback, possibly via a staging buffer. */
        uint32_t stride=0;
        frame->pixels=gbm.bo_map(frame->bo,0,0,frame->width,frame->height,1,&stride,&frame->map_data);
        if(frame->pixels==MAP_FAILED) frame->pixels=NULL;
        if(!frame->pixels || stride<frame->width*4 || stride>131072 || (uint64_t)stride*frame->height>256*1024*1024) {
            dma_disable("cpu-map"); frame_fail(frame); return;
        }
        frame->stride=stride;
        if(!dmabuf_reported) {fprintf(stderr,"Wayland dma-buf CPU mapping active (not zero-copy)\n"); dmabuf_reported=1;}
    }
    frame->done=1;
}
static void copy_failed(void*d,struct zwlr_screencopy_frame_v1*f) { ((struct frame*)d)->shm_busy=0; frame_fail(d); }
static void copy_damage(void*d,struct zwlr_screencopy_frame_v1*f,uint32_t x,uint32_t y,uint32_t w,uint32_t h) {}
static void copy_dmabuf(void*d,struct zwlr_screencopy_frame_v1*f,uint32_t fmt,uint32_t w,uint32_t h) {
    struct frame *frame=d;
    if(w && h && w<=16384 && h<=16384 && (uint64_t)w*h*4<=256*1024*1024 && dma_format_bit(fmt)) {
        frame->dma_format=fmt; frame->dma_width=w; frame->dma_height=h;
    }
}
static void submit_shm(struct frame *frame) {
    if(frame->shm_buffer && !frame->shm_busy && !frame->failed) {
        zwlr_screencopy_frame_v1_copy(frame->handle,frame->shm_buffer); frame->shm_busy=frame->submitted=1;
    } else frame_fail(frame);
}
static void dma_created(void*d,struct zwp_linux_buffer_params_v1*params,struct wl_buffer*buffer) {
    struct frame *frame=d;
    zwp_linux_buffer_params_v1_destroy(params); frame->params=NULL;
    frame->pixels=NULL;
    frame->buffer=buffer; frame->dmabuf_used=1;
    frame->width=frame->dma_width; frame->height=frame->dma_height;
    frame->format=frame->dma_format==0x34325258 ? WL_SHM_FORMAT_XRGB8888:
        frame->dma_format==0x34325241 ? WL_SHM_FORMAT_ARGB8888:frame->dma_format;
    wl_buffer_add_listener(buffer,&buffer_listener,frame);
    frame->released=0;
    zwlr_screencopy_frame_v1_copy(frame->handle,buffer); frame->submitted=1;
}
static void dma_failed(void*d,struct zwp_linux_buffer_params_v1*params) {
    struct frame *frame=d;
    zwp_linux_buffer_params_v1_destroy(params); frame->params=NULL;
    dma_disable("import"); dma_clear(frame); submit_shm(frame);
}
static const struct zwp_linux_buffer_params_v1_listener dma_params_listener={dma_created,dma_failed};
static int dma_start(struct frame *frame) {
    if(!dmabuf_requested || dmabuf_disabled || !gbm.device || frame->failed || !frame->shm_buffer ||
       !(dmabuf_linear_formats&dma_format_bit(frame->dma_format))) return 0;
    if(frame->buffer && frame->bo && frame->released && frame->bo_width==frame->dma_width &&
       frame->bo_height==frame->dma_height && frame->bo_format==frame->dma_format) {
        frame->pixels=NULL; frame->dmabuf_used=1; frame->released=0;
        frame->width=frame->dma_width; frame->height=frame->dma_height;
        frame->format=frame->dma_format==0x34325258 ? WL_SHM_FORMAT_XRGB8888:
            frame->dma_format==0x34325241 ? WL_SHM_FORMAT_ARGB8888:frame->dma_format;
        zwlr_screencopy_frame_v1_copy(frame->handle,frame->buffer); frame->submitted=1; return 1;
    }
    dma_clear(frame);
    /* Single-plane linear RGB only; unsupported imports are asynchronous failures,
       never create_immed protocol errors. No privilege or device probing changes. */
    frame->bo=gbm.bo_create(gbm.device,frame->dma_width,frame->dma_height,frame->dma_format,(1u<<2)|(1u<<4));
    frame->bo_fd=-1;
    if(!frame->bo) {dma_disable("allocation"); return 0;}
    uint32_t stride=gbm.bo_get_stride(frame->bo);
    if(gbm.bo_get_plane_count(frame->bo)!=1 || gbm.bo_get_modifier(frame->bo)!=0 ||
       stride<frame->dma_width*4 || stride>131072 || (uint64_t)stride*frame->dma_height>256*1024*1024 ||
       (frame->bo_fd=gbm.bo_get_fd(frame->bo))<0) {dma_disable("layout-or-export"); dma_clear(frame); return 0;}
    frame->bo_format=frame->dma_format; frame->bo_width=frame->dma_width; frame->bo_height=frame->dma_height;
    frame->params=zwp_linux_dmabuf_v1_create_params(dmabuf);
    zwp_linux_buffer_params_v1_add_listener(frame->params,&dma_params_listener,frame);
    zwp_linux_buffer_params_v1_add(frame->params,frame->bo_fd,0,gbm.bo_get_offset(frame->bo,0),stride,0,0);
    zwp_linux_buffer_params_v1_create(frame->params,frame->dma_width,frame->dma_height,frame->dma_format,0);
    return 1;
}
static void copy_buffer_done(void*d,struct zwlr_screencopy_frame_v1*f) {
    struct frame *frame=d;
    if(!dma_start(frame)) submit_shm(frame);
}
static const struct zwlr_screencopy_frame_v1_listener copy_listener={copy_buffer,copy_flags,copy_ready,copy_failed,copy_damage,copy_dmabuf,copy_buffer_done};
static void request_frame(struct frame *frame) {
    /* Initial milestone: video excludes cursor, viewer draws its own pointer. */
    frame->handle=zwlr_screencopy_manager_v1_capture_output(copy_manager,0,outputs[selected].handle);
    zwlr_screencopy_frame_v1_add_listener(frame->handle,&copy_listener,frame);
}
static void reset(void) {
    if (key_state) xkb_state_unref(key_state);
    if (keymap) xkb_keymap_unref(keymap);
    if (xkb) xkb_context_unref(xkb);
    for(unsigned i=0;i<2;i++) { frame_clear(&frames[i]); dma_clear(&frames[i]); shm_clear(&frames[i]); }
    if(gbm.device) gbm.device_destroy(gbm.device);
    if(gbm.fd>=0) close(gbm.fd);
    if(gbm.library) dlclose(gbm.library);
    memset(&gbm,0,sizeof(gbm)); gbm.fd=-1;
    if(keyboard) zwp_virtual_keyboard_v1_destroy(keyboard);
    if(pointer) zwlr_virtual_pointer_v1_destroy(pointer);
    if(power) zwlr_output_power_v1_destroy(power);
    if(keyboard_manager) zwp_virtual_keyboard_manager_v1_destroy(keyboard_manager);
    if(pointer_manager) zwlr_virtual_pointer_manager_v1_destroy(pointer_manager);
    if(power_manager) zwlr_output_power_manager_v1_destroy(power_manager);
    if(copy_manager) zwlr_screencopy_manager_v1_destroy(copy_manager);
    if(dmabuf) zwp_linux_dmabuf_v1_destroy(dmabuf);
    for(unsigned i=0;i<output_count;i++) if(outputs[i].handle) wl_output_destroy(outputs[i].handle);
    if(seat) wl_seat_destroy(seat);
    if(shm) wl_shm_destroy(shm);
    if(registry) wl_registry_destroy(registry);
    if (display) wl_display_disconnect(display);
    display=NULL; registry=NULL; shm=NULL; seat=NULL; copy_manager=NULL; pointer_manager=NULL; keyboard_manager=NULL;
    pointer=NULL; keyboard=NULL; xkb=NULL; keymap=NULL; key_state=NULL;
    power_manager=NULL; power=NULL; power_failed=0;
    dmabuf=NULL; dmabuf_requested=dmabuf_disabled=dmabuf_reported=0; dmabuf_linear_formats=0;
    pending=0; output_count=0; initialized=0; memset(outputs,0,sizeof(outputs));
    memset(held_keys,0,sizeof(held_keys));
    free(keymap_text); keymap_text=NULL; keymap_text_len=keymap_insert=0; spare_count=0;
}
int uu_wl_init(int32_t *w,int32_t *h) {
    pthread_mutex_lock(&lock);
    if(initialized) { *w=width; *h=height; pthread_mutex_unlock(&lock); return 0; }
    int result=-1;
    const char *dma=getenv("UU_WAYLAND_DMABUF"); dmabuf_requested=dma && !strcmp(dma,"1");
    display=wl_display_connect(NULL); if (!display) goto end;
    registry=wl_display_get_registry(display);
    wl_registry_add_listener(registry,&registry_listener,NULL);
    if(sync_display()!=0 || sync_display()!=0) goto end;
    dma_init();
    /* wlr-virtual-pointer is an optional input path. When the compositor does not
       advertise it, pointer injection is skipped and the caller may drive input
       through uinput; screencopy + virtual keyboard are still enough to capture. */
    if(!shm || !seat || !copy_manager || !output_count) goto end;
    const char *name=getenv("UU_WAYLAND_OUTPUT"); selected=0;
    if(name && *name) {
        for(selected=0;selected<output_count;selected++) if(!strcmp(outputs[selected].name,name)) break;
        if(selected==output_count) goto end;
    }
    if(!uu_output_transform_supported(outputs[selected].transform,getenv("NIRI_SOCKET")!=NULL)) goto end;
    width=outputs[selected].width; height=outputs[selected].height;
    if(outputs[selected].transform&1) {uint32_t t=width;width=height;height=t;}
    if(!width || !height || width>16384 || height>16384 || (uint64_t)width*height*4>256*1024*1024) goto end;
    if(power_manager) {
        power=zwlr_output_power_manager_v1_get_output_power(power_manager,outputs[selected].handle);
        zwlr_output_power_v1_add_listener(power,&power_listener,NULL);
    }
    if(pointer_manager) pointer=zwlr_virtual_pointer_manager_v1_create_virtual_pointer_with_output(pointer_manager,seat,outputs[selected].handle);
    /* Some compositors reject a virtual keyboard from anything that is not their active
       input method (Denial does), and a rejected proxy poisons later requests. Allow the
       caller to opt out and rely on uinput for key input instead. */
    const char *skip_kbd=getenv("UU_WAYLAND_NO_VIRTUAL_KEYBOARD");
    if(keyboard_manager && !(skip_kbd && *skip_kbd && strcmp(skip_kbd,"0")))
        keyboard=zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(keyboard_manager,seat);
    if(keyboard) {
        xkb=xkb_context_new(XKB_CONTEXT_NO_FLAGS);
        if(!xkb) goto end;
        struct xkb_rule_names names={.layout="us"};
        keymap=xkb_keymap_new_from_names(xkb,&names,XKB_KEYMAP_COMPILE_NO_FLAGS);
        if(!keymap) goto end;
        key_state=xkb_state_new(keymap); if(!key_state) goto end;
        keymap_text=xkb_keymap_get_as_string(keymap,XKB_KEYMAP_FORMAT_TEXT_V1);
        if(!keymap_text) goto end;
        keymap_text_len=strlen(keymap_text)+1;
        if(upload_keymap(keymap_text,keymap_text_len)!=0) goto end;
        if(sync_display()!=0) goto end;
        /* Text typing needs unmapped keycodes and a place to declare them. A keymap without either
           only costs that path - uu_wl_type_utf8 refuses and the caller keeps its own fallback. */
        keymap_insert=symbols_close(keymap_text);
        spare_count=getenv("NIRI_SOCKET")?find_niri_text_codes(spare_codes,UU_TEXT_SPARE):find_spare_codes(spare_codes,UU_TEXT_SPARE);
    }
    initialized=1; *w=width; *h=height; result=0;
end:
    if(result!=0) reset();
    pthread_mutex_unlock(&lock); return result;
}
/* Switch the pointer binding and main-process capture atomically; encoder subprocesses
   select their own output via UU_WAYLAND_OUTPUT. Keyboard identity stays unchanged. */
int uu_wl_select_output(const char *name,uint32_t expected_w,uint32_t expected_h) {
    pthread_mutex_lock(&lock); int result=-1;
    if(!initialized || !name || sync_display()!=0) goto end;
    unsigned target;
    for(target=0;target<output_count;target++) if(!strcmp(outputs[target].name,name)) break;
    if(target==output_count || !uu_output_transform_supported(outputs[target].transform,getenv("NIRI_SOCKET")!=NULL)) goto end;
    uint32_t w=outputs[target].width,h=outputs[target].height;
    if(outputs[target].transform&1) {uint32_t t=w;w=h;h=t;}
    if(w!=expected_w || h!=expected_h || !w || !h || w>16384 || h>16384 || (uint64_t)w*h*4>256*1024*1024) goto end;
    struct zwlr_virtual_pointer_v1 *next=NULL;
    if(pointer_manager) {
        next=zwlr_virtual_pointer_manager_v1_create_virtual_pointer_with_output(pointer_manager,seat,outputs[target].handle);
        if(!next) goto end;
        if(sync_display()!=0) {zwlr_virtual_pointer_v1_destroy(next);goto end;}
    }
    for(unsigned i=0;i<2;i++) { frame_clear(&frames[i]); dma_clear(&frames[i]); shm_clear(&frames[i]); }
    pending=0;
    if(pointer) zwlr_virtual_pointer_v1_destroy(pointer);
    pointer=next; selected=target; width=w; height=h;
    if(power) { zwlr_output_power_v1_destroy(power); power=NULL; }
    power_failed=0;
    if(power_manager) {
        power=zwlr_output_power_manager_v1_get_output_power(power_manager,outputs[selected].handle);
        zwlr_output_power_v1_add_listener(power,&power_listener,NULL);
    }
    result=0;
end:
    pthread_mutex_unlock(&lock); return result;
}
int uu_wl_wake(void) {
    pthread_mutex_lock(&lock); int result=-1;
    /* Output power management is optional; missing it must not block screencopy. */
    if(initialized && (!power || power_failed)) {
        /* No power manager, or the compositor refused the mode (outputs without DPMS,
           headless outputs): streaming continues, the screen simply stays as it is. */
        result=0;
    }
    if(initialized && power && !power_failed) {
        zwlr_output_power_v1_set_mode(power,ZWLR_OUTPUT_POWER_V1_MODE_ON);
        if(sync_display()==0 && !power_failed) result=0;
    }
    pthread_mutex_unlock(&lock); return result;
}
int uu_wl_capture(uint8_t *out,size_t length) {
    pthread_mutex_lock(&lock);
    int result=-1,retried=0;
retry:;
    struct frame *frame=&frames[pending];
    if(!initialized || length!=(size_t)width*height*4) goto end;
    if(!frame->handle) request_frame(frame);
    if(dispatch_until(&frame->done)!=0) goto end;
    if(frame->failed && frame->dmabuf_used && !retried) {
        /* Mapping/copy failure retries once on SHM; connection errors do not. */
        dma_disable("copy"); retried=1;
        /* dma-buf stays disabled, so its cached buffers are never reused. */
        for(unsigned i=0;i<2;i++) { frame_clear(&frames[i]); dma_clear(&frames[i]); }
        pending=0; goto retry;
    }
    if(frame->failed || !frame->pixels || frame->width!=outputs[selected].width || frame->height!=outputs[selected].height) goto end;
    zwlr_screencopy_frame_v1_destroy(frame->handle); frame->handle=NULL;
    const char *pipeline=getenv("UU_WAYLAND_PIPELINE");
    if(!pipeline || strcmp(pipeline,"0")) {
        struct frame *next=&frames[1-pending];
        request_frame(next);
        /* Submit the next GPU copy before copying/delivering these CPU pixels. */
        if(dispatch_until(&next->submitted)!=0 || wl_display_flush(display)<0) goto end;
    }
    {
        for(uint32_t y=0;y<height;y++) {
            uint8_t *row=out+(size_t)y*width*4;
            if(outputs[selected].transform==WL_OUTPUT_TRANSFORM_NORMAL &&
               frame->format!=WL_SHM_FORMAT_XBGR8888 && frame->format!=WL_SHM_FORMAT_ABGR8888) {
                uint32_t sy=(frame->flags&ZWLR_SCREENCOPY_FRAME_V1_FLAGS_Y_INVERT)?height-1-y:y;
                memcpy(row,(uint8_t*)frame->pixels+(size_t)sy*frame->stride,width*4);
                continue;
            }
            for(uint32_t x=0;x<width;x++) {
                uint32_t sx=x,sy=y,sw=frame->width,sh=frame->height;
                uu_source_pixel(outputs[selected].transform,sw,sh,x,y,&sx,&sy);
                if(frame->flags&ZWLR_SCREENCOPY_FRAME_V1_FLAGS_Y_INVERT) sy=sh-1-sy;
                const uint8_t *pixel=(uint8_t*)frame->pixels+(size_t)sy*frame->stride+sx*4;
                memcpy(row+x*4,pixel,4);
                if(frame->format==WL_SHM_FORMAT_XBGR8888 || frame->format==WL_SHM_FORMAT_ABGR8888) {
                    row[x*4]=pixel[2];row[x*4+2]=pixel[0];
                }
            }
        }
        result=0;
    }
    frame_clear(frame); pending=1-pending;
end:
    if(result!=0) for(unsigned i=0;i<2;i++) { frame_clear(&frames[i]); dma_clear(&frames[i]); }
    pthread_mutex_unlock(&lock); return result;
}
int uu_wl_move(int32_t x,int32_t y,int relative) {
    pthread_mutex_lock(&lock); int result=-1;
    /* -2: no virtual pointer at all, so the caller may use uinput; -1 stays a real failure. */
    if(initialized && !pointer) { result=-2; goto end; }
    if(initialized) {
        if(relative) {
            /* UU reports pixels (source DPR=1); Wayland motion is in logical points. */
            double scale=outputs[selected].scale;
            zwlr_virtual_pointer_v1_motion(pointer,timestamp(),wl_fixed_from_double(x/scale),wl_fixed_from_double(y/scale));
        }
        else if(x>=0 && y>=0 && x<(int32_t)width && y<(int32_t)height) {
            uint32_t sx,sy,sw=outputs[selected].width,sh=outputs[selected].height;
            /* niri/Smithay applies the output transform to absolute device coordinates. */
            if(getenv("NIRI_SOCKET")) uu_source_pixel(outputs[selected].transform,sw,sh,x,y,&sx,&sy);
            else {sx=x;sy=y;sw=width;sh=height;}
            zwlr_virtual_pointer_v1_motion_absolute(pointer,timestamp(),sx,sy,sw,sh);
        }
        else goto end;
        zwlr_virtual_pointer_v1_frame(pointer); result=wl_display_flush(display)<0?-1:0;
    }
end: pthread_mutex_unlock(&lock); return result;
}
int uu_wl_button(int button,int pressed) {
    pthread_mutex_lock(&lock); int result=-1;
    if(initialized && !pointer) { goto end; } /* no virtual pointer: report failure so the caller falls back to uinput */
    if(initialized) {
        if(button>=4 && button<=7) {
            if(pressed) {
                uint32_t axis=(button<=5)?WL_POINTER_AXIS_VERTICAL_SCROLL:WL_POINTER_AXIS_HORIZONTAL_SCROLL;
                int direction=(button==4 || button==6)?-1:1;
                zwlr_virtual_pointer_v1_axis_source(pointer,WL_POINTER_AXIS_SOURCE_WHEEL);
                zwlr_virtual_pointer_v1_axis_discrete(pointer,timestamp(),axis,wl_fixed_from_int(direction*10),direction);
            }
        } else {
            int code=button==1?0x110:button==2?0x112:button==3?0x111:button==8?0x113:button==9?0x114:0;
            if(!code) goto end;
            zwlr_virtual_pointer_v1_button(pointer,timestamp(),code,pressed?WL_POINTER_BUTTON_STATE_PRESSED:WL_POINTER_BUTTON_STATE_RELEASED);
        }
        zwlr_virtual_pointer_v1_frame(pointer); result=wl_display_flush(display)<0?-1:0;
    }
end: pthread_mutex_unlock(&lock); return result;
}
int uu_wl_key(uint64_t symbol,int pressed) {
    pthread_mutex_lock(&lock); int result=-1;
    if(initialized && !keyboard) { pthread_mutex_unlock(&lock); return -1; } /* no virtual keyboard: caller falls back to uinput */
    if(initialized) {
        for(xkb_keycode_t code=xkb_keymap_min_keycode(keymap);code<=xkb_keymap_max_keycode(keymap);code++) {
            int matches=0;
            for(xkb_level_index_t level=0;level<xkb_keymap_num_levels_for_key(keymap,code,0);level++) {
                const xkb_keysym_t *syms;
                int count=xkb_keymap_key_get_syms_by_level(keymap,code,0,level,&syms);
                if(count==1 && syms[0]==symbol) { matches=1; break; }
            }
            if(!matches || code<8 || code>=sizeof(held_keys)) continue;
            if(held_keys[code]==!!pressed) { result=0; break; }
            held_keys[code]=!!pressed;
            zwp_virtual_keyboard_v1_key(keyboard,timestamp(),code-8,pressed?WL_KEYBOARD_KEY_STATE_PRESSED:WL_KEYBOARD_KEY_STATE_RELEASED);
            xkb_state_update_key(key_state,code,pressed?XKB_KEY_DOWN:XKB_KEY_UP);
            zwp_virtual_keyboard_v1_modifiers(keyboard,
                xkb_state_serialize_mods(key_state,XKB_STATE_MODS_DEPRESSED),
                xkb_state_serialize_mods(key_state,XKB_STATE_MODS_LATCHED),
                xkb_state_serialize_mods(key_state,XKB_STATE_MODS_LOCKED),
                xkb_state_serialize_layout(key_state,XKB_STATE_LAYOUT_EFFECTIVE));
            result=wl_display_flush(display)<0?-1:0; break;
        }
    }
    pthread_mutex_unlock(&lock); return result;
}

int uu_wl_release_all(void) {
    pthread_mutex_lock(&lock);
    int result=-1;
    if(initialized) {
        /* Sweep this client's virtual keyboard directly: a keysym lookup can
           fail for unmapped OEM/keypad symbols before reaching held modifiers. */
        if(keyboard) {
            for(xkb_keycode_t code=xkb_keymap_min_keycode(keymap);code<=xkb_keymap_max_keycode(keymap);code++) {
                if(code<8 || code>=sizeof(held_keys) || !xkb_keymap_key_get_name(keymap,code)) continue;
                zwp_virtual_keyboard_v1_key(keyboard,timestamp(),code-8,WL_KEYBOARD_KEY_STATE_RELEASED);
                if(held_keys[code]) xkb_state_update_key(key_state,code,XKB_KEY_UP);
            }
            zwp_virtual_keyboard_v1_modifiers(keyboard,
                xkb_state_serialize_mods(key_state,XKB_STATE_MODS_DEPRESSED),
                xkb_state_serialize_mods(key_state,XKB_STATE_MODS_LATCHED),
                xkb_state_serialize_mods(key_state,XKB_STATE_MODS_LOCKED),
                xkb_state_serialize_layout(key_state,XKB_STATE_LAYOUT_EFFECTIVE));
        }
        memset(held_keys,0,sizeof(held_keys));
        if(pointer) {
            for(unsigned code=0x110;code<=0x114;code++)
                zwlr_virtual_pointer_v1_button(pointer,timestamp(),code,WL_POINTER_BUTTON_STATE_RELEASED);
            zwlr_virtual_pointer_v1_frame(pointer);
        }
        result=wl_display_flush(display)<0?-1:0;
    }
    pthread_mutex_unlock(&lock);return result;
}
