use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Request {
    id: u64,
    payload: String,
}

#[no_mangle]
pub extern "C" fn plugin_run() {
    // Use serde directly (not via tau)
    let req = Request { id: 1, payload: "test".into() };
    let json = serde_json::to_string(&req).unwrap();
    let _: Request = serde_json::from_str(&json).unwrap();
    
    // Also use tau's json
    let val = tau::json::json!({"test": 123});
    drop(val);
}
