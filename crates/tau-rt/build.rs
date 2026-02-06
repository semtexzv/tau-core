fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-undefined,dynamic_lookup");
    
    #[cfg(target_os = "linux")]
    println!("cargo:rustc-link-arg=-Wl,--allow-shlib-undefined");
}
