#include <assert.h>
#include "../c_engine/wayland_transform.h"
int main(void) {
    const unsigned expected[4][6]={{0,1,2,3,4,5},{3,0,4,1,5,2},{5,4,3,2,1,0},{2,5,1,4,0,3}};
    for(int t=0;t<4;t++) {
        unsigned dw=t&1?2:3,dh=t&1?3:2;
        for(unsigned y=0;y<dh;y++) for(unsigned x=0;x<dw;x++) {
            uint32_t sx,sy;uu_source_pixel(t,3,2,x,y,&sx,&sy);
            assert(sx<3 && sy<2 && sy*3+sx==expected[t][y*dw+x]);
        }
    }
}
