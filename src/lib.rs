//! Zero overhead tracing garbage collection for rust, by abusing the borrow checker.
//!
//! ## Planned Features
//! 1. Easy to use, since `Gc<T>` is `Copy` and coerces to a reference.
//! 2. Absolutely zero overhead when modifying pointers, since `Gc<T>` is `Copy`.
//! 3. Support for important libraries builtin to the collector
//! 4. Unsafe code has complete freedom to manipulate garbage collected pointers, and it doesn't need to understand the distinction
//! 5. Uses rust's lifetime system to ensure all roots are known at explicit safepoints, without any runtime overhead.
//! 6. Collection can only happen with an explicit `safepoint` call and has no overhead between these calls,
//! 7. Optional graceful handling of allocation failures.

/*
 * I want this library to use 'mostly' stable features,
 * unless there's good justification to use an unstable feature.
 */
use std::mem;
use std::ops::{Deref, DerefMut};
use std::fmt::{Debug};

#[macro_use]
mod manually_traced;
pub mod cell;


pub use self::cell::{GcCell};

/// Invoke the closure with a temporary [GcContext],
/// then perform a safepoint afterwards.
///
/// Normally returns a tuple `($updated_root, $closure_result)`.
///
/// If a value is provided it is considered as a root of garbage collection
/// both for the safepoint and the duration of the entire context.
///
/// # Safety
/// This macro is completely safe, although it expands to unsafe code internally.
// TODO: Document all forms of this macro
#[macro_export(local_inner_macros)]
macro_rules! safepoint_recurse {
    ($context:ident, |$sub_context:ident, $new_root:ident| $closure:expr) => {{
        let ((), result) = safepoint_recurse!($context, (), |$sub_context, $new_root| $closure);
        result
    }};
    ($context:ident, $root:expr, |$sub_context:ident, $new_root:ident| $closure:expr) => {{
        let mut root = $root;
        let result = unsafe { __recurse_context!($context, &mut root, |$sub_context, $new_root| {
            $closure
        }) };
        /*
         * NOTE: We're assuming result is unmanaged here
         * The borrow checker will verify this is true (by marking this as a mutation).
         * If you need a manged result, use the @managed_result variant
         */
        let updated_root = safepoint!($context, $root);
        (updated_root, result)
    }};
    ($context:ident, $root:expr, @managed_result, |$sub_context:ident, $new_root:ident| $closure:expr) => {{
        use $crate::{GcContext};
        let mut root = $root;
        let erased_result = unsafe { __recurse_context!(
            $context, &mut root,
            |$sub_context, $new_root| {
                let result = $closure;
                $sub_context.rebrand_static(result)
            }
        ) };
        /*
         * Rebrand back to the current collector lifetime
         * It could have possibly been allocated from the sub-context inside the closure,
         * but as long as it was valid at the end of the closure it's valid now.
         * We trust that GcContext::recurse_context
         * did not perform a collection after calling the closure.
         */
        let result = unsafe { $context.rebrand_self(erased_result) };
        safepoint!($context, (root, result))
    }};
}

/// Create a new sub-context for the duration of the closure
///
/// The specified `root` object will be appended to the shadowstack
/// and is guarenteed to live for the entire lifetime of the closure (and the created sub-context).
///
/// Unlike `safepoint_recurse!` this doesn't imply a safepoint anywhere.
///
/// # Safety
/// This doesn't actually mutate the original collector.
/// It is possible user code could trigger a collection in the closure
/// without the borrow checker noticing invalid pointers elsewhere.
/// (See docs for [GcContext::recurse_context])
///
/// It is not publicly exposed for this reason
#[macro_export]
macro_rules! __recurse_context {
    ($context:ident, $root:expr, |$sub_context:ident, $new_root:ident| $closure:expr) => {{
        use $crate::{GcContext};
        // TODO: Panic safety
        $context.recurse_context(&mut $root, |mut $sub_context, erased_root| {
            /*
             * NOTE: Guarenteed to live for the lifetime of the entire closure.
             * However, it could be relocated if 'sub_collector' is collected
             */
            let $new_root = $sub_context.rebrand_self(erased_root);
            $closure
        })
    }};
}

/// Indicate it's safe to begin a garbage collection,
/// while keeping the specified root alive.
///
/// All other garbage collected pointers that aren't reachable from the root are invalidated.
/// They have a lifetime that references the [GarbageCollectorRef]
/// and the borrow checker considers the safepoint a 'mutation'.
///
/// The root is exempted from the "mutation" and rebound to the new lifetime.
///
/// ## Example
/// ```
/// let root = safepoint!(collector, root);
/// ```
///
/// ## Safety
/// This macro is completely safe, although it expands to unsafe code internally.
#[macro_export]
macro_rules! safepoint {
    ($collector:ident, $value:expr) => {unsafe {
        use $crate::{GcContext};
        // TODO: What happens if we panic during a collection
        let mut erased = $collector.rebrand_static($value);
        $collector.basic_safepoint(&mut &mut erased);
        $collector.rebrand_self(erased)
    }};
}

/// A garbage collector implementation.
///
/// These implementations should be completely safe and zero-overhead.
pub unsafe trait GcSystem {}

/// The context of garbage collection,
/// which can be frozen at a safepoint.
///
/// This is essentially used to maintain a shadow-stack to a set of roots,
/// which are guarenteed not to be collected until a safepoint.
///
/// This context doesn't necessarily support allocation (see [GcAllocContext] for that).
pub unsafe trait GcContext {
    type System: GcSystem;
    /// Inform the garbage collection system we are at a safepoint
    /// and are ready for a potential garbage collection.
    ///
    /// ## Safety
    /// This method is unsafe and should never be invoked by user code.
    ///
    /// See the [safepoint!] macro for a safe wrapper.
    unsafe fn basic_safepoint<T: Trace>(&mut self, value: &mut &mut T);

    #[inline(always)]
    #[doc(hidden)]
    unsafe fn rebrand_static<T: GcBrand<'static, Self::System>>(&self, value: T) -> T::Branded {
        let  branded = mem::transmute_copy(&value);
        mem::forget(value);
        branded
    }
    #[inline(always)]
    #[doc(hidden)]
    unsafe fn rebrand_self<'a, T: GcBrand<'a, Self::System>>(&'a self, value: T) -> T::Branded {
        let branded = mem::transmute_copy(&value);
        mem::forget(value);
        branded
    }

    /// Invoke the closure with a temporary [GcContext].
    ///
    /// The specified value is
    /// guarenteed to live throughout the created context for the closure.
    /// However, because it could possibly be relocated by a collection,
    /// it's bound to the lifetime of the sub-collector.
    ///
    /// ## Safety
    /// This macro doesn't imply garbage collection,
    /// so it doesn't mutate the collector directly.
    /// However the specified closure could trigger a collection in the sub-context.
    /// This would in undefined behavior if the collection
    /// invalidates a pointer tied to this context.
    ///
    /// For this reason, this function should never be invoked by user code.
    ///
    /// See the [safepoint_recurse!] macro for a safe wrapper
    unsafe fn recurse_context<T, F, R>(&self, value: &mut &mut T, func: F) -> R
        where T: Trace, F: for <'gc> FnOnce(&'gc mut Self, &'gc mut T) -> R;
}
/// A simple interface to allocating
///
/// Some garbage collectors implement more complex interfaces,
/// so implementing this is optional
pub unsafe trait GcSimpleAlloc<'gc, T: GcSafe + 'gc>: 'gc {
    type Ref: GcRef<'gc, T>;
    /// Allocate the specified object in this garbage collector,
    /// binding it to the lifetime of this collector.
    ///
    /// The object will never be collected until the next safepoint,
    /// which is considered a mutation by the borrow checker and will be statically checked.
    /// Therefore, we can statically guarantee the pointers will survive until the next safepoint.
    ///
    /// See `safepoint!` docs on how to properly invoke a safepoint
    /// and transfer values across it.
    ///
    /// This gives a immutable reference to the resulting object.
    /// Once allocated, the object can only be correctly modified with a `GcCell`
    fn alloc(&self, value: T) -> Self::Ref;
}

/// A garbage collected pointer to a value.
///
/// This is the equivalent of a garbage collected smart-pointer.
/// It's so smart, you can even coerce it to a reference bound to the lifetime of the `GarbageCollectorRef`.
/// However, all those references are invalidated by the borrow checker as soon as
/// your reference to the collector reaches a safepoint.
/// The objects can only survive garbage collection if they live in this smart-pointer.
///
/// The smart pointer is simply a guarantee to the garbage collector
/// that this points to a garbage collected object with the correct header,
/// and not some arbitrary bits that you've decided to heap allocate.
pub trait GcRef<'gc, T: GcSafe + ?Sized + 'gc>: GcSafe + Copy
    + Deref<Target=&'gc T> {
    /// The type of the garbage collection
    /// system that created this pointer
    type System: GcSystem;
    /// The value of the underlying pointer
    fn value(&self) -> &'gc T;
    #[inline]
    unsafe fn as_raw_ptr(&self) -> *mut T {
        self.value() as *const T as *mut T
    }
}

/// Indicates that a type can be safely allocated by a garbage collector.
///
/// ## Safety
/// Custom destructors must never reference garbage collected pointers.
/// The garbage collector may have already freed the other objects
/// before calling this type's drop function.
///
/// Unlike java finalizers, this allows us to deallocate objects normally
/// and avoids a second pass over the objects
/// to check for resurrected objects.
pub unsafe trait GcSafe: Trace {
    /// If this type needs a destructor run
    ///
    /// This is usually equivalent to `std::mem::needs_drop`.
    /// However procedurally derived code can sometimes provide
    /// a no-op drop implementation (for safety),
    /// which would lead to a false positive with `std::mem::needs_drop()`
    const NEEDS_DROP: bool;

    /// Assert this type is GC safe
    ///
    /// Only used by procedural derive
    #[doc(hidden)]
    fn assert_gc_safe() {}
}
/// Assert that a type implements Copy
///
/// Used by the derive code
#[doc(hidden)]
pub fn assert_copy<T: Copy>() {}

/// A wrapper type that assumes its contents don't need to be traced
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct AssumeNotTraced<T>(T);
impl<T> AssumeNotTraced<T> {
    /// Assume the specified value doesn't need to be traced
    ///
    /// ## Safety
    /// Undefined behavior if the value contains anything that need to be traced
    /// by a garbage collector.
    #[inline]
    pub unsafe fn new(value: T) -> Self {
        AssumeNotTraced(value)
    }
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}
impl<T> Deref for AssumeNotTraced<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> DerefMut for AssumeNotTraced<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

unsafe impl<T> Trace for AssumeNotTraced<T> {
    const NEEDS_TRACE: bool = false;
    #[inline(always)] // This method does nothing and is always a win to inline
    fn visit<V: GcVisitor>(&mut self, _visitor: &mut V) -> Result<(), V::Err> {
        Ok(())
    }
}
unsafe impl<T> TraceImmutable for AssumeNotTraced<T> {
    #[inline(always)]
    fn visit_immutable<V: GcVisitor>(&self, _visitor: &mut V) -> Result<(), V::Err> {
        Ok(())
    }
}
unsafe impl<T> NullTrace for AssumeNotTraced<T> {}
/// No tracing implies GcSafe
unsafe impl<T> GcSafe for AssumeNotTraced<T> {
    const NEEDS_DROP: bool = std::mem::needs_drop::<T>();
}
unsafe_gc_brand!(AssumeNotTraced, T);

/// Changes all references to garbage
/// collected objects to match `'new_gc`.
///
/// This indicates that its safe to transmute to the new `Branded` type
/// and all that will change is the lifetimes.
pub unsafe trait GcBrand<'new_gc, S: GcSystem>: Trace {
    type Branded: Trace + 'new_gc;
}

/// Indicates that a type can be traced by a garbage collector.
///
/// This doesn't necessarily mean that the type is safe to allocate in a garbage collector ([GcSafe]).
///
/// ## Safety
/// See the documentation of the `trace` method for more info.
/// Essentially, this object must faithfully trace anything that
/// could contain garbage collected pointers or other `Trace` items.
pub unsafe trait Trace {
    /// Whether this type needs to be traced by the garbage collector.
    ///
    /// Some primitive types don't need to be traced at all,
    /// and can be simply ignored by the garbage collector.
    ///
    /// Collections should usually delegate this decision to their element type,
    /// claiming the need for tracing only if their elements do.
    /// For example, to decide `Vec<u32>::NEEDS_TRACE` you'd check whether `u32::NEEDS_TRACE` (false),
    /// and so then `Vec<u32>` doesn't need to be traced.
    /// By the same logic, `Vec<Gc<u32>>` does need to be traced,
    /// since it contains a garbage collected pointer.
    ///
    /// If there are multiple types involved, you should check if any of them need tracing.
    /// One perfect example of this is structure/tuple types which check
    /// `field1::NEEDS_TRACE || field2::NEEDS_TRACE || field3::needs_trace`.
    /// The fields which don't need tracing will always ignored by `GarbageCollector::trace`,
    /// while the fields that do will be properly traced.
    ///
    /// False negatives will always result in completely undefined behavior.
    /// False positives could result in un-necessary tracing, but are perfectly safe otherwise.
    /// Therefore, when in doubt you always assume this is true.
    ///
    /// If this is true `NullTrace` should (but doesn't have to) be implemented.
    const NEEDS_TRACE: bool;
    /// Visit each field in this type
    ///
    /// Users should never invoke this method, and always call the `V::visit` instead.
    /// Only the collector itself is premitted to call this method,
    /// and **it is undefined behavior for the user to invoke this**.
    ///
    /// Structures should trace each of their fields,
    /// and collections should trace each of their elements.
    ///
    /// ### Safety
    /// Some types (like `Gc`) need special actions taken when they're traced,
    /// but those are somewhat rare and are usually already provided by the garbage collector.
    ///
    /// Unless I explicitly document actions as legal I may decide to change i.
    /// I am only bound by the constraints of [semantic versioning](http://semver.org/) in the trace function
    /// if I explicitly document it as safe behavior in this method's documentation.
    /// If you try something that isn't explicitly documented here as permitted behavior,
    /// the collector may choose to override your memory with `0xDEADBEEF`.
    /// ## Always Permitted
    /// - Reading your own memory (includes iteration)
    ///   - Interior mutation is undefined behavior, even if you use `GcCell`
    /// - Calling `GcVisitor::visit` with the specified collector
    ///   - `GarbageCollector::trace` already verifies that it owns the data, so you don't need to do that
    /// - Panicking
    ///   - This should be reserved for cases where you are seriously screwed up,
    ///       and can't fulfill your contract to trace your interior properly.
    ///     - One example is `Gc<T>` which panics if the garbage collectors are mismatched
    ///   - This rule may change in future versions, depending on how we deal with multi-threading.
    /// ## Never Permitted Behavior
    /// - Forgetting a element of a collection, or field of a structure
    ///   - If you forget an element undefined behavior will result
    ///   - This is why we always prefer automatically derived implementations where possible.
    ///     - You will never trigger undefined behavior with an automatic implementation,
    ///       and it'll always be completely sufficient for safe code (aside from destructors).
    ///     - With an automatically derived implementation you will never miss a field
    /// - It is undefined behavior to mutate any of your own data.
    ///   - The mutable `&mut self` is just so copying collectors can relocate GC pointers
    /// - Invoking this function directly, without delegating to `GcVisitor`
    fn visit<V: GcVisitor>(&mut self, visitor: &mut V) -> Result<(), V::Err>;
}
/// A type that can be safely traced/relocated
/// without having to use a mutable reference
///
/// Types with interior mutability (like `RefCell` or `Cell<Gc<T>>`)
/// can safely implement this, since they allow safely relocating the pointer
/// without a mutable reference.
/// Likewise primitives (with new garbage collected data) can also
/// implement this (since they have nothing to trace).
pub unsafe trait TraceImmutable: Trace {
    fn visit_immutable<V: GcVisitor>(&self, visitor: &mut V) -> Result<(), V::Err>;
}

/// Marker types for types that don't need to be traced
///
/// If this trait is implemented `Trace::NEEDS_TRACE` must be false
pub unsafe trait NullTrace: Trace + TraceImmutable {}

/// Visits garbage collected objects
///
/// This should only be used by a [GarbageCollectionSystem]
pub unsafe trait GcVisitor: Sized {
    type Err: Debug;

    #[inline(always)]
    fn visit<T: Trace + ?Sized>(&mut self, value: &mut T) -> Result<(), Self::Err> {
        value.visit(self)
    }
    #[inline(always)]
    fn visit_immutable<T: TraceImmutable + ?Sized>(&mut self, value: &T) -> Result<(), Self::Err> {
        value.visit_immutable(self)
    }
}
