// Fast thread-local executor index using C's __thread
// Avoids Rust's thread_local! closure overhead on the hot path.
//
// The executor index identifies which Executor instance the current thread owns.
// EXECUTOR_UNINIT means no executor has been initialized on this thread.

#include <stdint.h>
#include <stdbool.h>

#define EXECUTOR_UNINIT UINT16_MAX

static __thread uint16_t current_executor_idx = EXECUTOR_UNINIT;

uint16_t tls_get_executor(void) {
    return current_executor_idx;
}

void tls_set_executor(uint16_t idx) {
    current_executor_idx = idx;
}

bool tls_is_current_executor(uint16_t executor_idx) {
    return current_executor_idx == executor_idx;
}
