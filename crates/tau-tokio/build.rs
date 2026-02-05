fn main() {
    // Rerun if path changes - helps with caching
    println!("cargo:rerun-if-changed=build.rs");
}
