use crate::types::FfiWaker;

extern "C" {
    /// Provided by the host runtime (tau-host)
    fn tau_host_sigchld_subscribe(waker: FfiWaker);
}

#[no_mangle]
pub extern "C" fn tau_sys_sigchld_subscribe(waker: FfiWaker) {
    unsafe {
        tau_host_sigchld_subscribe(waker);
    }
}
