use crate::types::FfiWaker;
use crate::ffi::tau_host_sigchld_subscribe;

#[no_mangle]
pub extern "C" fn tau_sys_sigchld_subscribe(waker: FfiWaker) {
    unsafe {
        tau_host_sigchld_subscribe(waker);
    }
}
