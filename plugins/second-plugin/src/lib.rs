tau::define_plugin! {
    fn init() {
        let guard = tau::PluginGuard::current();
        println!("[second-plugin] init! plugin_id={}", guard.plugin_id());
    }
    fn destroy() {}
    fn request(data: &[u8]) -> u64 {
        let guard = tau::PluginGuard::current();
        println!("[second-plugin] request, plugin_id={}", guard.plugin_id());
        0
    }
}
