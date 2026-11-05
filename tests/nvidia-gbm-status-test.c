/* Unit tests for aggregation and CUDA cleanup propagation; use fake libcuda. */
#define main probe_main
#include "../contrib/nvidia-gbm-egl-probe.c"
#undef main
#include <assert.h>

int main(void) {
    assert(summary_exit(PASS, PASS, PASS, NOT_APPLICABLE, PASS, PASS, PASS) == 0);
    assert(summary_exit(PASS, PASS, INCONCLUSIVE, PASS, PASS, NOT_APPLICABLE, PASS) == 2);
    assert(summary_exit(PASS, PASS, PASS, PASS, INCONCLUSIVE, PASS, PASS) == 2);
    assert(summary_exit(PASS, PASS, PASS, PASS, PASS, PASS, FAIL) == 1);
    assert(summary_exit(PASS, PASS, INCONCLUSIVE, PASS, FAIL, NOT_APPLICABLE, PASS) == 1);
    assert(summary_exit(PASS, PASS, PASS, FAIL, PASS, PASS, PASS) == 1);
    unsetenv("FAKE_CUDA_FAIL");
    assert(try_cuda(1) == PASS);
    const char *failures[] = {"map", "unmap", "unregister", "destroy"};
    for (size_t i = 0; i < sizeof(failures) / sizeof(failures[0]); i++) {
        setenv("FAKE_CUDA_FAIL", failures[i], 1);
        assert(try_cuda(1) == FAIL);
    }
    unsetenv("FAKE_CUDA_FAIL");
#ifndef HAVE_CUDA_EGL_HEADERS
    assert(try_cuda_egl((EGLImageKHR)1) == INCONCLUSIVE);
#endif
    setenv("FAKE_CUDA_FAIL", "unregister", 1);
    assert(try_cuda_egl((EGLImageKHR)1) == FAIL);
    setenv("FAKE_CUDA_FAIL", "destroy", 1);
    assert(try_cuda_egl((EGLImageKHR)1) == FAIL);
    puts("status tests passed");
    return 0;
}
