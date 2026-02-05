//! Plugin binary lifetime guard and safe wrappers for dynamic types.
//!
//! `PluginGuard` is an `Arc`-wrapped handle to a loaded plugin's shared library.
//! As long as any clone exists, the library stays mapped — vtables, function
//! pointers, and `.text` references remain valid.
//!
//! This enables safe cross-plugin trait objects: a `PluginBox<dyn Stream>` can
//! be sent to another plugin or the host, and the creating plugin's binary
//! won't be unmapped until all such references are gone.
//!
//! # Hot reload
//!
//! When a plugin is reloaded:
//! 1. The NEW binary is loaded (`dlopen` → new handle, new `PluginGuard`)
//! 2. The OLD binary stays mapped because existing `PluginBox`/`PluginArc`
//!    values still hold clones of the old guard
//! 3. As those values are dropped, the old guard's ref count decreases
//! 4. When the last old reference drops, the old binary is `dlclose`'d
//!
//! Both binaries coexist temporarily. Each has its own `.text`, `.data`, and
//! static variables. `RTLD_LOCAL` ensures their symbols don't collide.
//!
//! # Concrete vs dynamic types
//!
//! - **Concrete data** (`String`, `Vec<T>`, structs) can cross plugin boundaries
//!   freely — every plugin has its own copy of the drop glue.
//! - **Dynamic types** (`dyn Trait`, `fn()` pointers, closures) contain pointers
//!   into a specific plugin's `.text`. Wrap them in [`PluginBox`], [`PluginArc`],
//!   or [`PluginFn`] to keep the creating plugin alive.
//!
//! # Per-plugin guard storage
//!
//! Because `tau` is an **rlib** (statically compiled into each plugin), each
//! plugin gets its own copy of the [`PLUGIN_GUARD`] static. The host sets it
//! during `plugin_init` via the [`define_plugin!`](crate::define_plugin) macro.
//!
//! [`PluginGuard::current()`] reads from this per-plugin static, so it always
//! returns the correct guard for the plugin whose code is executing.
//!
//! Convenience constructors like [`PluginBox::current`] use this automatically.

use std::sync::{Arc, OnceLock};

// =============================================================================
// Per-plugin guard static
// =============================================================================

/// Per-plugin guard storage.
///
/// Since `tau` is an rlib, each plugin's cdylib gets its own copy of this static.
/// The [`define_plugin!`](crate::define_plugin) macro sets it during `plugin_init`.
pub static PLUGIN_GUARD: OnceLock<PluginGuard> = OnceLock::new();

/// Get this plugin's guard.
///
/// # Panics
///
/// Panics if called before `plugin_init` has run (i.e., before `define_plugin!`
/// has stored the guard).
pub fn plugin_guard() -> PluginGuard {
    PLUGIN_GUARD
        .get()
        .expect("plugin_guard() called before plugin_init — is define_plugin! missing?")
        .clone()
}

// =============================================================================
// PluginBinary — the ref-counted dlopen handle
// =============================================================================

/// Opaque handle to a loaded plugin binary. `dlclose` is called on drop.
///
/// This is the inner type behind `PluginGuard`. Created by the host,
/// shared via `Arc`, never directly accessed by plugin code.
///
/// The `drop_fn` is a host-provided callback that performs the actual
/// `dlclose`. This avoids plugins needing to link against libc directly
/// for this purpose — the host owns the dlopen handle.
pub struct PluginBinary {
    /// Opaque data for the drop callback (typically the dlopen handle).
    handle: *mut (),
    /// Host-provided function to unload the binary.
    /// Called exactly once, when the last `Arc<PluginBinary>` drops.
    drop_fn: Option<unsafe fn(*mut ())>,
    /// Plugin ID for debugging / logging.
    plugin_id: u64,
}

// dlopen handles are process-global, safe to share across threads.
unsafe impl Send for PluginBinary {}
unsafe impl Sync for PluginBinary {}

impl Drop for PluginBinary {
    fn drop(&mut self) {
        if let Some(f) = self.drop_fn {
            unsafe { f(self.handle) };
        }
    }
}

// =============================================================================
// PluginGuard — the clone-able reference
// =============================================================================

/// Ref-counted guard that keeps a plugin's shared library loaded.
///
/// Clone this and attach it to any value that contains pointers into the
/// plugin's `.text` section (trait objects, function pointers, closures).
///
/// Created by the host during plugin loading and passed to the plugin
/// at init time. The plugin stores it and clones it into any
/// `PluginBox`/`PluginArc`/`PluginFn` it creates.
#[derive(Clone)]
pub struct PluginGuard {
    inner: Arc<PluginBinary>,
}

impl PluginGuard {
    /// Create a new guard.
    ///
    /// # Safety
    ///
    /// `handle` and `drop_fn` must be valid. `drop_fn(handle)` will be called
    /// exactly once when the last clone of this guard is dropped.
    pub unsafe fn new(
        handle: *mut (),
        drop_fn: unsafe fn(*mut ()),
        plugin_id: u64,
    ) -> Self {
        Self {
            inner: Arc::new(PluginBinary {
                handle,
                drop_fn: Some(drop_fn),
                plugin_id,
            }),
        }
    }

    /// Get the current plugin's guard.
    ///
    /// Reads from the per-plugin [`PLUGIN_GUARD`] static. Since `tau` is an
    /// rlib, each plugin has its own copy of that static, so this always
    /// returns the correct guard for the calling plugin.
    ///
    /// # Panics
    ///
    /// Panics if called before `define_plugin!` has run (plugin not yet initialized).
    pub fn current() -> Self {
        plugin_guard()
    }

    /// Try to get the current plugin's guard, returning `None` if the plugin
    /// has not been initialized yet.
    pub fn try_current() -> Option<Self> {
        PLUGIN_GUARD.get().cloned()
    }

    /// Create a null guard (for host-owned values that don't belong to a plugin).
    pub fn host() -> Self {
        Self {
            inner: Arc::new(PluginBinary {
                handle: std::ptr::null_mut(),
                drop_fn: None,
                plugin_id: 0,
            }),
        }
    }

    /// Number of live references to this plugin binary.
    pub fn ref_count(&self) -> usize {
        Arc::strong_count(&self.inner)
    }

    /// The plugin ID (for debugging).
    pub fn plugin_id(&self) -> u64 {
        self.inner.plugin_id
    }
}

// =============================================================================
// PluginFn — a function pointer whose plugin binary is kept alive
// =============================================================================

/// A function pointer that keeps its creating plugin's binary loaded.
///
/// `F` is a concrete `fn(A) -> R` type (which is `Copy`). The guard ensures the
/// code the pointer refers to stays mapped.
///
/// # Closures
///
/// For closures, use `PluginBox<dyn FnOnce(A) -> R>` instead and call via
/// [`PluginBox::call_once`]. `PluginFn` is only for bare `fn()` pointers.
pub struct PluginFn<F: Copy> {
    f: F,
    _guard: PluginGuard,
}

impl<F: Copy> PluginFn<F> {
    /// Wrap a function pointer with a plugin guard.
    pub fn new(f: F, guard: PluginGuard) -> Self {
        Self { f, _guard: guard }
    }

    /// Get the raw function pointer.
    ///
    /// # Safety
    ///
    /// The returned pointer is only valid while this `PluginFn` (or a clone of
    /// its guard) is alive. Do not stash the raw pointer and use it after drop.
    pub fn as_ptr(&self) -> F {
        self.f
    }

    /// Get a clone of the guard (to create related guarded values).
    pub fn guard(&self) -> PluginGuard {
        self._guard.clone()
    }
}

impl<F: Copy> Clone for PluginFn<F> {
    fn clone(&self) -> Self {
        Self { f: self.f, _guard: self._guard.clone() }
    }
}

// Call impls for common fn pointer arities.
// fn() pointers are Copy, so calling never consumes — these just delegate.

impl<R> PluginFn<fn() -> R> {
    pub fn call(&self) -> R { (self.f)() }
}
impl<A, R> PluginFn<fn(A) -> R> {
    pub fn call(&self, a: A) -> R { (self.f)(a) }
}
impl<A, B, R> PluginFn<fn(A, B) -> R> {
    pub fn call(&self, a: A, b: B) -> R { (self.f)(a, b) }
}
impl<A, B, C, R> PluginFn<fn(A, B, C) -> R> {
    pub fn call(&self, a: A, b: B, c: C) -> R { (self.f)(a, b, c) }
}
impl<A, B, C, D, R> PluginFn<fn(A, B, C, D) -> R> {
    pub fn call(&self, a: A, b: B, c: C, d: D) -> R { (self.f)(a, b, c, d) }
}

// =============================================================================
// PluginBox<T> — unique ownership of a dynamic type, plugin kept alive
// =============================================================================

/// A `Box<T>` that keeps its creating plugin's binary loaded.
///
/// Use this for unique ownership of trait objects that must outlive the plugin
/// that created them. The plugin's shared library won't be `dlclose`'d until
/// this value is dropped.
///
/// # Example
///
/// ```ignore
/// // Plugin creates a stream, wraps it with its own guard:
/// let stream: Box<dyn Stream<Item = Event>> = Box::new(my_stream);
/// let safe_stream = PluginBox::new(stream, my_guard.clone());
///
/// // safe_stream can be sent to the host or another plugin.
/// // Even if this plugin is "reloaded", the old binary stays mapped.
/// ```
pub struct PluginBox<T: ?Sized> {
    inner: Box<T>,
    _guard: PluginGuard,
}

impl<T> PluginBox<T> {
    /// Wrap a value with the current plugin's guard.
    ///
    /// Convenience for `PluginBox::new(Box::new(value), PluginGuard::current())`.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a plugin context.
    pub fn current(value: T) -> Self {
        Self {
            inner: Box::new(value),
            _guard: PluginGuard::current(),
        }
    }
}

impl<T: ?Sized> PluginBox<T> {
    /// Wrap a boxed value with a plugin guard.
    pub fn new(value: Box<T>, guard: PluginGuard) -> Self {
        Self { inner: value, _guard: guard }
    }

    /// Wrap a boxed value with the current plugin's guard.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a plugin context.
    pub fn boxed_current(value: Box<T>) -> Self {
        Self { inner: value, _guard: PluginGuard::current() }
    }

    /// Access the inner value.
    pub fn as_ref(&self) -> &T {
        &self.inner
    }

    /// Mutably access the inner value.
    pub fn as_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Get a clone of the guard (to create related guarded values).
    pub fn guard(&self) -> PluginGuard {
        self._guard.clone()
    }

    /// Convert into a pinned PluginBox (for futures/streams that need Pin).
    pub fn into_pin(self) -> std::pin::Pin<PluginBox<T>> {
        // Safe: PluginBox<T> owns the T and won't move it
        unsafe { std::pin::Pin::new_unchecked(self) }
    }
}

impl<T: ?Sized> std::ops::Deref for PluginBox<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.inner }
}

impl<T: ?Sized> std::ops::DerefMut for PluginBox<T> {
    fn deref_mut(&mut self) -> &mut T { &mut self.inner }
}

// =============================================================================
// PluginArc<T> — shared ownership of a dynamic type, plugin kept alive
// =============================================================================

/// An `Arc<T>` that keeps its creating plugin's binary loaded.
///
/// Use this for shared ownership of trait objects across multiple consumers.
/// Cloning a `PluginArc` clones both the `Arc<T>` and the `PluginGuard`,
/// so the plugin stays alive as long as any clone exists.
pub struct PluginArc<T: ?Sized> {
    inner: Arc<T>,
    _guard: PluginGuard,
}

impl<T> PluginArc<T> {
    /// Wrap a value with the current plugin's guard.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a plugin context.
    pub fn current(value: T) -> Self {
        Self {
            inner: Arc::new(value),
            _guard: PluginGuard::current(),
        }
    }
}

impl<T: ?Sized> PluginArc<T> {
    /// Wrap an Arc'd value with a plugin guard.
    pub fn new(value: Arc<T>, guard: PluginGuard) -> Self {
        Self { inner: value, _guard: guard }
    }

    /// Wrap an Arc'd value with the current plugin's guard.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a plugin context.
    pub fn arc_current(value: Arc<T>) -> Self {
        Self { inner: value, _guard: PluginGuard::current() }
    }

    /// Create from a concrete value.
    pub fn from_value(value: T, guard: PluginGuard) -> Self
    where
        T: Sized,
    {
        Self { inner: Arc::new(value), _guard: guard }
    }

    /// Get a clone of the guard.
    pub fn guard(&self) -> PluginGuard {
        self._guard.clone()
    }
}

impl<T: ?Sized> Clone for PluginArc<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _guard: self._guard.clone(),
        }
    }
}

impl<T: ?Sized> std::ops::Deref for PluginArc<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.inner }
}

// =============================================================================
// FnOnce/FnMut/Fn call support for PluginBox
// =============================================================================

// The key problem with FnOnce: calling it *consumes* the closure. We need to
// destructure PluginBox to separate the callable from the guard, call the
// closure, and only then let the guard drop. Rust's drop order guarantees
// locals drop in reverse declaration order, so:
//
//   let PluginBox { inner, _guard } = self;
//   let result = inner(args);    // closure code executes (plugin still mapped)
//   // _guard drops here          // plugin may now be unmapped
//   // result is returned
//
// This is safe: the guard outlives the call.

macro_rules! impl_plugin_box_fn {
    ( $($arg:ident : $T:ident),* ) => {
        // ── FnOnce: consumes self ──

        impl<$($T,)* R> PluginBox<dyn FnOnce($($T),*) -> R> {
            /// Call the closure, consuming the `PluginBox`.
            ///
            /// The plugin binary remains loaded for the duration of the call.
            /// After the call returns, the guard drops (potentially unmapping
            /// the plugin if this was the last reference).
            pub fn call_once(self, $($arg: $T),*) -> R {
                let PluginBox { inner, _guard } = self;
                let result = inner($($arg),*);
                drop(_guard);
                result
            }
        }

        impl<$($T,)* R> PluginBox<dyn FnOnce($($T),*) -> R + Send> {
            /// Call the closure, consuming the `PluginBox`. (`Send` variant)
            pub fn call_once(self, $($arg: $T),*) -> R {
                let PluginBox { inner, _guard } = self;
                let result = inner($($arg),*);
                drop(_guard);
                result
            }
        }

        // ── FnMut: callable multiple times via &mut self ──

        impl<$($T,)* R> PluginBox<dyn FnMut($($T),*) -> R> {
            /// Call the closure by mutable reference.
            pub fn call_mut(&mut self, $($arg: $T),*) -> R {
                (self.inner)($($arg),*)
            }
        }

        impl<$($T,)* R> PluginBox<dyn FnMut($($T),*) -> R + Send> {
            /// Call the closure by mutable reference. (`Send` variant)
            pub fn call_mut(&mut self, $($arg: $T),*) -> R {
                (self.inner)($($arg),*)
            }
        }

        // ── Fn: callable multiple times via &self ──

        impl<$($T,)* R> PluginBox<dyn Fn($($T),*) -> R> {
            /// Call the closure by shared reference.
            pub fn call(&self, $($arg: $T),*) -> R {
                (self.inner)($($arg),*)
            }
        }

        impl<$($T,)* R> PluginBox<dyn Fn($($T),*) -> R + Send> {
            /// Call the closure by shared reference. (`Send` variant)
            pub fn call(&self, $($arg: $T),*) -> R {
                (self.inner)($($arg),*)
            }
        }

        impl<$($T,)* R> PluginBox<dyn Fn($($T),*) -> R + Send + Sync> {
            /// Call the closure by shared reference. (`Send + Sync` variant)
            pub fn call(&self, $($arg: $T),*) -> R {
                (self.inner)($($arg),*)
            }
        }

        // ── Fn via PluginArc: shared ownership + shared calling ──

        impl<$($T,)* R> PluginArc<dyn Fn($($T),*) -> R + Send + Sync> {
            /// Call the closure through the Arc.
            pub fn call(&self, $($arg: $T),*) -> R {
                (self.inner)($($arg),*)
            }
        }
    };
}

impl_plugin_box_fn!();
impl_plugin_box_fn!(a: A);
impl_plugin_box_fn!(a: A, b: B);
impl_plugin_box_fn!(a: A, b: B, c: C);
impl_plugin_box_fn!(a: A, b: B, c: C, d: D);

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // Track whether the "dlclose" callback was invoked
    static UNLOADED: AtomicBool = AtomicBool::new(false);

    unsafe fn mock_dlclose(_handle: *mut ()) {
        UNLOADED.store(true, Ordering::SeqCst);
    }

    fn mock_guard() -> PluginGuard {
        UNLOADED.store(false, Ordering::SeqCst);
        unsafe { PluginGuard::new(0x1234 as *mut (), mock_dlclose, 1) }
    }

    fn host_guard() -> PluginGuard {
        PluginGuard::host()
    }

    // ── PluginGuard ──

    #[test]
    fn guard_ref_count() {
        let g = host_guard();
        assert_eq!(g.ref_count(), 1);
        let g2 = g.clone();
        assert_eq!(g.ref_count(), 2);
        drop(g2);
        assert_eq!(g.ref_count(), 1);
    }

    #[test]
    fn guard_calls_drop_fn_on_last_drop() {
        UNLOADED.store(false, Ordering::SeqCst);
        {
            let g = mock_guard();
            let _g2 = g.clone();
            drop(g);
            // Still one clone alive
            assert!(!UNLOADED.load(Ordering::SeqCst));
        }
        // Both dropped → dlclose called
        assert!(UNLOADED.load(Ordering::SeqCst));
    }

    #[test]
    fn host_guard_no_drop_fn() {
        // Dropping a host guard should not panic (drop_fn is None)
        let g = host_guard();
        drop(g);
    }

    // ── PluginBox ──

    #[test]
    fn plugin_box_deref() {
        let b = PluginBox::new(Box::new(42u32), host_guard());
        assert_eq!(*b, 42);
    }

    #[test]
    fn plugin_box_deref_mut() {
        let mut b = PluginBox::new(Box::new(42u32), host_guard());
        *b = 99;
        assert_eq!(*b, 99);
    }

    #[test]
    fn plugin_box_trait_object() {
        trait Greet { fn greet(&self) -> &str; }
        struct Hello;
        impl Greet for Hello { fn greet(&self) -> &str { "hello" } }

        let b: PluginBox<dyn Greet> = PluginBox::new(Box::new(Hello), host_guard());
        assert_eq!(b.greet(), "hello");
    }

    #[test]
    fn plugin_box_keeps_plugin_alive() {
        UNLOADED.store(false, Ordering::SeqCst);
        let guard = mock_guard();
        let b = PluginBox::new(Box::new(42), guard.clone());
        drop(guard);
        // PluginBox still holds a clone
        assert!(!UNLOADED.load(Ordering::SeqCst));
        drop(b);
        assert!(UNLOADED.load(Ordering::SeqCst));
    }

    // ── PluginArc ──

    #[test]
    fn plugin_arc_clone_shares_data() {
        let a = PluginArc::from_value(42u32, host_guard());
        let b = a.clone();
        assert_eq!(*a, 42);
        assert_eq!(*b, 42);
    }

    #[test]
    fn plugin_arc_trait_object() {
        trait Greet: Send + Sync { fn greet(&self) -> &str; }
        struct Hello;
        impl Greet for Hello { fn greet(&self) -> &str { "hello" } }

        let a: PluginArc<dyn Greet> = PluginArc::new(Arc::new(Hello), host_guard());
        let b = a.clone();
        assert_eq!(a.greet(), "hello");
        assert_eq!(b.greet(), "hello");
    }

    // ── PluginFn ──

    #[test]
    fn plugin_fn_call() {
        fn double(x: i32) -> i32 { x * 2 }
        let f = PluginFn::new(double as fn(i32) -> i32, host_guard());
        assert_eq!(f.call(21), 42);
    }

    #[test]
    fn plugin_fn_clone_shares_guard() {
        fn noop() {}
        let f = PluginFn::new(noop as fn(), host_guard());
        let before = f.guard().ref_count();
        let g = f.clone();
        assert_eq!(f.guard().ref_count(), before + 1);
        drop(g);
        assert_eq!(f.guard().ref_count(), before);
    }

    #[test]
    fn plugin_fn_keeps_plugin_alive() {
        UNLOADED.store(false, Ordering::SeqCst);
        fn noop() {}
        let guard = mock_guard();
        let f = PluginFn::new(noop as fn(), guard.clone());
        drop(guard);
        assert!(!UNLOADED.load(Ordering::SeqCst));
        drop(f);
        assert!(UNLOADED.load(Ordering::SeqCst));
    }

    // ── FnOnce ──

    #[test]
    fn plugin_box_fn_once_no_args() {
        let called = Arc::new(AtomicUsize::new(0));
        let c = called.clone();
        let f: PluginBox<dyn FnOnce() -> u32> = PluginBox::new(
            Box::new(move || { c.fetch_add(1, Ordering::SeqCst); 42 }),
            host_guard(),
        );
        assert_eq!(f.call_once(), 42);
        assert_eq!(called.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn plugin_box_fn_once_with_arg() {
        let f: PluginBox<dyn FnOnce(String) -> usize> = PluginBox::new(
            Box::new(|s: String| s.len()),
            host_guard(),
        );
        assert_eq!(f.call_once("hello".to_string()), 5);
    }

    // ── FnMut ──

    #[test]
    fn plugin_box_fn_mut_no_args() {
        let mut counter = 0u32;
        let mut f: PluginBox<dyn FnMut() -> u32> = PluginBox::new(
            Box::new(move || { counter += 1; counter }),
            host_guard(),
        );
        assert_eq!(f.call_mut(), 1);
        assert_eq!(f.call_mut(), 2);
        assert_eq!(f.call_mut(), 3);
    }

    #[test]
    fn plugin_box_fn_mut_with_arg() {
        let mut sum = 0i64;
        let mut f: PluginBox<dyn FnMut(i64) -> i64> = PluginBox::new(
            Box::new(move |x| { sum += x; sum }),
            host_guard(),
        );
        assert_eq!(f.call_mut(10), 10);
        assert_eq!(f.call_mut(20), 30);
    }

    // ── Fn ──

    #[test]
    fn plugin_box_fn_no_args() {
        let f: PluginBox<dyn Fn() -> &'static str> = PluginBox::new(
            Box::new(|| "hello"),
            host_guard(),
        );
        assert_eq!(f.call(), "hello");
        assert_eq!(f.call(), "hello"); // callable multiple times
    }

    #[test]
    fn plugin_box_fn_with_arg() {
        let f: PluginBox<dyn Fn(i32, i32) -> i32> = PluginBox::new(
            Box::new(|a, b| a + b),
            host_guard(),
        );
        assert_eq!(f.call(3, 4), 7);
        assert_eq!(f.call(10, 20), 30);
    }

    #[test]
    fn plugin_arc_fn_shared_call() {
        let f: PluginArc<dyn Fn(i32) -> i32 + Send + Sync> = PluginArc::new(
            Arc::new(|x: i32| x * 3),
            host_guard(),
        );
        let g = f.clone();
        assert_eq!(f.call(7), 21);
        assert_eq!(g.call(7), 21);
    }

    // ── FnOnce ──

    #[test]
    fn fn_once_guard_alive_during_call() {
        UNLOADED.store(false, Ordering::SeqCst);
        let guard = mock_guard();

        let f: PluginBox<dyn FnOnce() -> bool> = PluginBox::new(
            Box::new(|| {
                // During the call, the guard from PluginBox is still alive
                !UNLOADED.load(Ordering::SeqCst)
            }),
            guard.clone(),
        );
        drop(guard); // only PluginBox's clone remains

        let was_alive = f.call_once();
        assert!(was_alive, "plugin should be alive during the call");
        // Now the PluginBox is consumed → guard drops → dlclose
        assert!(UNLOADED.load(Ordering::SeqCst));
    }
}
