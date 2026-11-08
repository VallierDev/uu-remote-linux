/* Standalone, no server/Portal/display connection:
 * cc -std=c11 -Wall -Wextra -Werror -g -fsanitize=address,undefined \
 *   tests/portal_pipewire_loop_lifecycle.c -o /tmp/portal-loop-test \
 *   $(pkg-config --cflags --libs libpipewire-0.3) -pthread
 * /tmp/portal-loop-test
 *
 * Tests thread-loop locking and stop ordering only, not a real stream's
 * remove_buffer or GPU/producer revocation and lifetime guarantees.
 */
#define _POSIX_C_SOURCE 200809L
#include <pipewire/pipewire.h>
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#define CHECK(x) do { if (!(x)) { \
    fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #x); \
    _Exit(1); \
} } while (0)

struct test {
    struct pw_thread_loop *loop;
    pthread_mutex_t mutex;
    pthread_cond_t cond;
    int callback_count, hold_callback, release_callback;
    int worker_attempting, worker_acquired;
};

static struct timespec deadline_ms(long ms)
{
    struct timespec t;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &t) == 0);
    t.tv_sec += ms / 1000;
    t.tv_nsec += (ms % 1000) * 1000000L;
    if (t.tv_nsec >= 1000000000L) { t.tv_sec++; t.tv_nsec -= 1000000000L; }
    return t;
}

/* Called with mutex held; one absolute deadline also bounds spurious wakeups. */
static int wait_for(struct test *t, int *value, int target, long ms)
{
    struct timespec end = deadline_ms(ms);
    while (*value < target) {
        int r = pthread_cond_timedwait(&t->cond, &t->mutex, &end);
        if (r == ETIMEDOUT) return *value >= target;
        CHECK(r == 0);
    }
    return 1;
}

static int callback(struct spa_loop *loop, bool async, uint32_t seq,
                    const void *data, size_t size, void *user_data)
{
    struct test *t = user_data;
    (void)loop; (void)async; (void)seq; (void)data; (void)size;
    CHECK(pw_thread_loop_in_thread(t->loop));
    CHECK(pthread_mutex_lock(&t->mutex) == 0);
    t->callback_count++;
    CHECK(pthread_cond_broadcast(&t->cond) == 0);
    if (t->hold_callback)
        CHECK(wait_for(t, &t->release_callback, 1, 3000));
    CHECK(pthread_mutex_unlock(&t->mutex) == 0);
    return 0;
}

static void *lock_worker(void *data)
{
    struct test *t = data;
    CHECK(pthread_mutex_lock(&t->mutex) == 0);
    t->worker_attempting = 1;
    CHECK(pthread_cond_broadcast(&t->cond) == 0);
    CHECK(pthread_mutex_unlock(&t->mutex) == 0);
    pw_thread_loop_lock(t->loop);
    CHECK(pthread_mutex_lock(&t->mutex) == 0);
    t->worker_acquired = 1;
    CHECK(pthread_cond_broadcast(&t->cond) == 0);
    CHECK(pthread_mutex_unlock(&t->mutex) == 0);
    pw_thread_loop_unlock(t->loop);
    return NULL;
}

static void watchdog(int sig)
{
    (void)sig;
    const char msg[] = "FAIL: watchdog timeout (possible deadlock)\n";
    (void)write(STDERR_FILENO, msg, sizeof(msg) - 1);
    _Exit(2);
}

int main(int argc, char **argv)
{
    struct test t = {0};
    pthread_condattr_t attr;
    pthread_t worker;
    CHECK(signal(SIGALRM, watchdog) != SIG_ERR);
    alarm(15); /* Covers even a stuck PipeWire lock, stop, or pthread_join. */
    CHECK(pthread_mutex_init(&t.mutex, NULL) == 0);
    CHECK(pthread_condattr_init(&attr) == 0);
    CHECK(pthread_condattr_setclock(&attr, CLOCK_MONOTONIC) == 0);
    CHECK(pthread_cond_init(&t.cond, &attr) == 0);
    CHECK(pthread_condattr_destroy(&attr) == 0);
    pw_init(&argc, &argv);
    t.loop = pw_thread_loop_new("lifecycle-test", NULL);
    CHECK(t.loop != NULL);
    CHECK(pw_thread_loop_start(t.loop) == 0);

    /* Establish that the loop is dispatching before testing the locked case. */
    CHECK(pw_loop_invoke(pw_thread_loop_get_loop(t.loop), callback, 0,
                         NULL, 0, false, &t) >= 0);
    CHECK(pthread_mutex_lock(&t.mutex) == 0);
    CHECK(wait_for(&t, &t.callback_count, 1, 3000));
    CHECK(pthread_mutex_unlock(&t.mutex) == 0);

    /* Lock first: enqueue without blocking; callback must wait for unlock. */
    pw_thread_loop_lock(t.loop);
    CHECK(pw_loop_invoke(pw_thread_loop_get_loop(t.loop), callback, 1,
                         NULL, 0, false, &t) >= 0);
    CHECK(pthread_mutex_lock(&t.mutex) == 0);
    CHECK(!wait_for(&t, &t.callback_count, 2, 200));
    CHECK(pthread_mutex_unlock(&t.mutex) == 0);
    pw_thread_loop_unlock(t.loop);
    CHECK(pthread_mutex_lock(&t.mutex) == 0);
    CHECK(wait_for(&t, &t.callback_count, 2, 3000));
    t.hold_callback = 1;
    CHECK(pthread_mutex_unlock(&t.mutex) == 0);

    /* Callback first: hold it in the loop thread while another thread tries
     * the loop lock. Never wait for that lock while holding our mutex. */
    CHECK(pw_loop_invoke(pw_thread_loop_get_loop(t.loop), callback, 2,
                         NULL, 0, false, &t) >= 0);
    CHECK(pthread_mutex_lock(&t.mutex) == 0);
    CHECK(wait_for(&t, &t.callback_count, 3, 3000));
    CHECK(pthread_mutex_unlock(&t.mutex) == 0);
    CHECK(pthread_create(&worker, NULL, lock_worker, &t) == 0);
    CHECK(pthread_mutex_lock(&t.mutex) == 0);
    CHECK(wait_for(&t, &t.worker_attempting, 1, 3000));
    CHECK(!wait_for(&t, &t.worker_acquired, 1, 200));
    t.release_callback = 1;
    CHECK(pthread_cond_broadcast(&t.cond) == 0);
    CHECK(wait_for(&t, &t.worker_acquired, 1, 3000));
    CHECK(pthread_mutex_unlock(&t.mutex) == 0);
    CHECK(pthread_join(worker, NULL) == 0);

    /* Stop only after callbacks and lock user finish, and without loop lock. */
    pw_thread_loop_stop(t.loop);
    CHECK(t.callback_count == 3);
    pw_thread_loop_destroy(t.loop);
    pw_deinit();
    CHECK(pthread_cond_destroy(&t.cond) == 0);
    CHECK(pthread_mutex_destroy(&t.mutex) == 0);
    alarm(0);
    puts("PASS: PipeWire thread-loop lock, callbacks, then stop");
    return 0;
}
