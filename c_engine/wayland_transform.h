#include <stdint.h>
/* Map upright output pixels to untransformed capture/device coordinates (Smithay Transform). */
static inline void uu_source_pixel(int transform,uint32_t w,uint32_t h,uint32_t x,uint32_t y,uint32_t *sx,uint32_t *sy) {
    *sx=x;*sy=y;
    switch(transform) {
        case 1:*sx=y;*sy=h-1-x;break;
        case 2:*sx=w-1-x;*sy=h-1-y;break;
        case 3:*sx=w-1-y;*sy=x;break;
    }
}
/* Rotated capture and pointer mapping have hardware acceptance only on niri. */
static inline int uu_output_transform_supported(int transform,int niri) {
    return transform==0 || (niri && transform>=1 && transform<=3);
}
