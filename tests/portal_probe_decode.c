/* Synthetic decoder validation only. Never opens a Portal session. */
#define main portal_probe_main
#include "../contrib/portal-dmabuf-probe.c"
#undef main
int main(void) {
    GError *error=NULL;
    GSubprocess *source=g_subprocess_new(G_SUBPROCESS_FLAGS_STDOUT_PIPE|G_SUBPROCESS_FLAGS_STDERR_SILENCE,
        &error,"ffmpeg","-v","error","-f","lavfi","-i","color=size=32x16:rate=1",
        "-frames:v","1","-c:v","libx264","-f","h264","pipe:1",NULL);
    assert(source); GBytes *packet=NULL;
    assert(g_subprocess_communicate(source,NULL,NULL,&packet,NULL,&error));
    assert(g_subprocess_get_successful(source));
    gsize length=0; const uint8_t *data=g_bytes_get_data(packet,&length); assert(length<=UINT32_MAX);
    assert(!decode_one_frame(data,(uint32_t)length,32,16));
    assert(decode_one_frame(data,(uint32_t)length,16,32)!=0); /* equal area, wrong dimensions */
    assert(decode_one_frame((const uint8_t *)"invalid",7,32,16)!=0);
    g_bytes_unref(packet); g_object_unref(source);
    puts("PASS: in-memory software decode, exact dimensions, malformed packet rejection");
}
