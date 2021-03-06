use crate::access::{Access, AccessError, BorrowMut, BorrowRef, RawBorrowedMut, RawBorrowedRef};
use crate::{Any, Hash};
use std::any;
use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::future::Future;
use std::marker;
use std::mem::ManuallyDrop;
use std::ops;
use std::pin::Pin;
use std::process;
use std::ptr;
use std::task::{Context, Poll};

/// A shared value.
pub struct Shared<T: ?Sized> {
    inner: ptr::NonNull<SharedBox<T>>,
}

impl<T> Shared<T> {
    /// Construct a new shared value.
    pub fn new(data: T) -> Self {
        let inner = Box::leak(Box::new(SharedBox {
            access: Access::new(),
            count: Cell::new(1),
            data: data.into(),
        }));

        Self {
            inner: inner.into(),
        }
    }

    /// Return a debug formatter, that when printed will display detailed
    /// diagnostics of this shared type.
    pub fn debug(&self) -> SharedDebug<'_, T> {
        SharedDebug { shared: self }
    }

    /// Test if the value is sharable.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    /// let shared = Shared::new(1u32);
    /// assert!(shared.is_readable());
    ///
    /// {
    ///     let guard = shared.borrow_ref().unwrap();
    ///     assert!(shared.is_readable()); // Note: still readable.
    /// }
    ///
    /// {
    ///     let guard = shared.borrow_mut().unwrap();
    ///     assert!(!shared.is_readable());
    /// }
    ///
    /// assert!(shared.is_readable());
    /// ```
    ///
    /// # Taking inner value
    ///
    /// ```rust
    /// use runestick::Shared;
    /// let shared = Shared::new(1u32);
    /// let shared2 = shared.clone();
    /// assert!(shared.is_readable());
    /// shared.take().unwrap();
    /// assert!(!shared2.is_readable());
    /// assert!(shared2.take().is_err());
    /// ```
    pub fn is_readable(&self) -> bool {
        // Safety: Since we have a reference to this shared, we know that the
        // inner is available.
        unsafe { self.inner.as_ref().access.is_shared() }
    }

    /// Test if the value is exclusively accessible.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    /// let shared = Shared::new(1u32);
    /// assert!(shared.is_writable());
    ///
    /// {
    ///     let guard = shared.borrow_ref().unwrap();
    ///     assert!(!shared.is_writable());
    /// }
    ///
    /// assert!(shared.is_writable());
    /// ```
    pub fn is_writable(&self) -> bool {
        // Safety: Since we have a reference to this shared, we know that the
        // inner is available.
        unsafe { self.inner.as_ref().access.is_exclusive() }
    }

    /// Take the interior value, if we have exlusive access to it and there
    /// are no other live exlusive or shared references.
    ///
    /// A value that has been taken can no longer be accessed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    ///
    /// #[derive(Debug)]
    /// struct Foo {
    ///     counter: isize,
    /// }
    ///
    /// let a = Shared::new(Foo { counter: 0 });
    /// let b = a.clone();
    ///
    /// {
    ///     let mut a = a.borrow_mut().unwrap();
    ///     // NB: this is prevented since we have a live reference.
    ///     assert!(b.take().is_err());
    ///     a.counter += 1;
    /// }
    ///
    /// let a = a.take().unwrap();
    /// assert_eq!(a.counter, 1);
    /// ```
    pub fn take(self) -> Result<T, AccessError> {
        // Safety: We know that interior value is alive since this container is
        // alive.
        //
        // Appropriate access is checked when constructing the guards.
        unsafe {
            let inner = self.inner.as_ref();

            // NB: don't drop guard to avoid yielding access back.
            // This will prevent the value from being dropped in the shared
            // destructor and future illegal access of any kind.
            let _ = ManuallyDrop::new(inner.access.take()?);

            // Read the pointer out without dropping the inner structure.
            // The data field will be invalid at this point, which should be
            // flagged through a `taken` access flag.
            //
            // Future access is forever prevented since we never release
            // the access (see above).
            Ok(ptr::read(inner.data.get()))
        }
    }

    /// Get a reference to the interior value while checking for shared access
    /// that holds onto a reference count of the inner value.
    ///
    /// This prevents other exclusive accesses from being performed while the
    /// guard returned from this function is live.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    ///
    /// #[derive(Debug)]
    /// struct Foo {
    ///     counter: isize,
    /// }
    ///
    /// let a = Shared::new(Foo { counter: 0 });
    /// let b = a.clone();
    ///
    /// b.borrow_mut().unwrap().counter += 1;
    ///
    /// {
    ///     // Consumes `a`.
    ///     let mut a = a.owned_ref().unwrap();
    ///     assert_eq!(a.counter, 1);
    ///     assert!(b.borrow_mut().is_err());
    /// }
    ///
    /// let mut b = b.borrow_mut().unwrap();
    /// b.counter += 1;
    /// assert_eq!(b.counter, 2);
    /// ```
    pub fn owned_ref(self) -> Result<OwnedRef<T>, AccessError> {
        // Safety: We know that interior value is alive since this container is
        // alive.
        //
        // Appropriate access is checked when constructing the guards.
        unsafe {
            let guard = self.inner.as_ref().access.shared()?;

            // NB: we need to prevent the Drop impl for Shared from being called,
            // since we are deconstructing its internals.
            let this = ManuallyDrop::new(self);

            Ok(OwnedRef {
                data: this.inner.as_ref().data.get(),
                guard,
                inner: RawSharedBox::from_inner(this.inner),
                _marker: marker::PhantomData,
            })
        }
    }

    /// Get a reference to the interior value while checking for exclusive
    /// access that holds onto a reference count of the inner value.
    ///
    /// This prevents other exclusive and shared accesses from being performed
    /// while the guard returned from this function is live.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    ///
    /// #[derive(Debug)]
    /// struct Foo {
    ///     counter: isize,
    /// }
    ///
    /// let a = Shared::new(Foo { counter: 0 });
    /// let b = a.clone();
    ///
    /// {
    ///     // Consumes `a`.
    ///     let mut a = a.owned_mut().unwrap();
    ///     a.counter += 1;
    ///
    ///     assert!(b.borrow_ref().is_err());
    /// }
    ///
    /// assert_eq!(b.borrow_ref().unwrap().counter, 1);
    /// ```
    pub fn owned_mut(self) -> Result<OwnedMut<T>, AccessError> {
        // Safety: We know that interior value is alive since this container is
        // alive.
        //
        // Appropriate access is checked when constructing the guards.
        unsafe {
            let guard = self.inner.as_ref().access.exclusive()?;

            // NB: we need to prevent the Drop impl for Shared from being called,
            // since we are deconstructing its internals.
            let this = ManuallyDrop::new(self);

            Ok(OwnedMut {
                data: this.inner.as_ref().data.get(),
                guard,
                inner: RawSharedBox::from_inner(this.inner),
                _marker: marker::PhantomData,
            })
        }
    }
}

impl<T: ?Sized> Shared<T> {
    /// Get a reference to the interior value while checking for shared access.
    ///
    /// This prevents other exclusive accesses from being performed while the
    /// guard returned from this function is live.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    ///
    /// #[derive(Debug)]
    /// struct Foo {
    ///     counter: isize,
    /// }
    ///
    /// let a = Shared::new(Foo { counter: 0 });
    ///
    /// a.borrow_mut().unwrap().counter += 1;
    ///
    /// {
    ///     let mut a_ref = a.borrow_ref().unwrap();
    ///     assert_eq!(a_ref.counter, 1);
    ///     assert!(a.borrow_mut().is_err());
    ///     assert!(a.borrow_ref().is_ok());
    /// }
    ///
    /// let mut a = a.borrow_mut().unwrap();
    /// a.counter += 1;
    /// assert_eq!(a.counter, 2);
    /// ```
    pub fn borrow_ref(&self) -> Result<BorrowRef<'_, T>, AccessError> {
        // Safety: We know that interior value is alive since this container is
        // alive.
        //
        // Appropriate access is checked when constructing the guards.
        unsafe {
            let inner = self.inner.as_ref();
            let guard = inner.access.shared()?;
            Ok(BorrowRef::from_raw(inner.data.get(), guard))
        }
    }

    /// Get a reference to the interior value while checking for exclusive access.
    ///
    /// This prevents other shared or exclusive accesses from being performed
    /// while the guard returned from this function is live.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use runestick::Shared;
    ///
    /// #[derive(Debug)]
    /// struct Foo {
    ///     counter: isize,
    /// }
    ///
    /// let a = Shared::new(Foo { counter: 0 });
    ///
    /// {
    ///     let mut a_mut = a.borrow_mut().unwrap();
    ///     a_mut.counter += 1;
    ///     assert_eq!(a_mut.counter, 1);
    ///     assert!(a.borrow_ref().is_err());
    /// }
    ///
    /// let a = a.borrow_ref().unwrap();
    /// assert_eq!(a.counter, 1);
    /// ```
    pub fn borrow_mut(&self) -> Result<BorrowMut<'_, T>, AccessError> {
        // Safety: We know that interior value is alive since this container is
        // alive.
        //
        // Appropriate access is checked when constructing the guards.
        unsafe {
            let inner = self.inner.as_ref();
            let guard = inner.access.exclusive()?;
            Ok(BorrowMut::from_raw(inner.data.get(), guard))
        }
    }
}

impl Shared<Any> {
    /// Take the interior value, if we have exlusive access to it and there
    /// exist no other references.
    pub fn take_downcast<T>(self) -> Result<T, AccessError>
    where
        T: any::Any,
    {
        // Safety: We know that interior value is alive since this container is
        // alive.
        //
        // Appropriate access is checked when constructing the guards.
        unsafe {
            let inner = self.inner.as_ref();

            // NB: don't drop guard to avoid yielding access back.
            // This will prevent the value from being dropped in the shared
            // destructor and future illegal access of any kind.
            let guard = ManuallyDrop::new(inner.access.take()?);

            // Read the pointer out without dropping the inner structure.
            // Note that the data field will after this point be invalid.
            //
            // Future access is forever prevented since we never release
            // exclusive access (see above).
            let any = ptr::read(inner.data.get());

            let expected = Hash::from_type_id(any::TypeId::of::<T>());

            match any.take_mut_ptr(expected) {
                Ok(value) => Ok(*Box::from_raw(value as *mut T)),
                Err(any) => {
                    let actual = any.type_name();

                    // Type coercion failed, so reconstruct the state of the
                    // Shared container.

                    // Drop the guard to release exclusive access.
                    drop(ManuallyDrop::into_inner(guard));

                    // NB: write the potentially modified value back.
                    // It hasn't been modified, but there has been a period of
                    // time now that the value hasn't been valid for.
                    ptr::write(inner.data.get(), any);

                    Err(AccessError::UnexpectedType {
                        actual,
                        expected: any::type_name::<T>(),
                    })
                }
            }
        }
    }

    /// Get a shared value and downcast.
    pub fn downcast_borrow_ref<T>(&self) -> Result<BorrowRef<'_, T>, AccessError>
    where
        T: any::Any,
    {
        unsafe {
            let inner = self.inner.as_ref();
            let guard = inner.access.shared()?;
            let expected = Hash::from_type_id(any::TypeId::of::<T>());

            let data = match (*inner.data.get()).as_ptr(expected) {
                Some(data) => data,
                None => {
                    return Err(AccessError::UnexpectedType {
                        expected: any::type_name::<T>(),
                        actual: (*inner.data.get()).type_name(),
                    });
                }
            };

            Ok(BorrowRef::from_raw(data as *const T, guard))
        }
    }

    /// Get a shared value and downcast.
    pub fn downcast_owned_ref<T>(self) -> Result<OwnedRef<T>, AccessError>
    where
        T: any::Any,
    {
        unsafe {
            let (data, guard) = {
                let inner = self.inner.as_ref();
                let guard = inner.access.shared()?;
                let expected = Hash::from_type_id(any::TypeId::of::<T>());

                match (*inner.data.get()).as_ptr(expected) {
                    Some(data) => (data, guard),
                    None => {
                        return Err(AccessError::UnexpectedType {
                            expected: any::type_name::<T>(),
                            actual: (*inner.data.get()).type_name(),
                        });
                    }
                }
            };

            // NB: we need to prevent the Drop impl for Shared from being called,
            // since we are deconstructing its internals.
            let this = ManuallyDrop::new(self);

            Ok(OwnedRef {
                data: data as *const T,
                guard,
                inner: RawSharedBox::from_inner(this.inner),
                _marker: marker::PhantomData,
            })
        }
    }

    /// Get a exclusive value and downcast.
    pub fn downcast_borrow_mut<T>(&self) -> Result<BorrowMut<'_, T>, AccessError>
    where
        T: any::Any,
    {
        unsafe {
            let inner = self.inner.as_ref();
            let guard = inner.access.exclusive()?;
            let expected = Hash::from_type_id(any::TypeId::of::<T>());

            let data = match (*inner.data.get()).as_mut_ptr(expected) {
                Some(data) => data,
                None => {
                    return Err(AccessError::UnexpectedType {
                        expected: any::type_name::<T>(),
                        actual: (*inner.data.get()).type_name(),
                    });
                }
            };

            Ok(BorrowMut::from_raw(data as *mut T, guard))
        }
    }

    /// Get a shared value and downcast.
    pub fn downcast_owned_mut<T>(self) -> Result<OwnedMut<T>, AccessError>
    where
        T: any::Any,
    {
        unsafe {
            let (data, guard) = {
                let inner = self.inner.as_ref();
                let guard = inner.access.exclusive()?;
                let expected = Hash::from_type_id(any::TypeId::of::<T>());

                match (*inner.data.get()).as_mut_ptr(expected) {
                    Some(data) => (data, guard),
                    None => {
                        return Err(AccessError::UnexpectedType {
                            expected: any::type_name::<T>(),
                            actual: (*inner.data.get()).type_name(),
                        });
                    }
                }
            };

            // NB: we need to prevent the Drop impl for Shared from being called,
            // since we are deconstructing its internals.
            let this = ManuallyDrop::new(self);

            Ok(OwnedMut {
                data: data as *mut T,
                guard,
                inner: RawSharedBox::from_inner(this.inner),
                _marker: marker::PhantomData,
            })
        }
    }
}

impl<T: ?Sized> Clone for Shared<T> {
    fn clone(&self) -> Self {
        unsafe {
            SharedBox::inc(self.inner.as_ptr());
        }

        Self { inner: self.inner }
    }
}

impl<T: ?Sized> Drop for Shared<T> {
    fn drop(&mut self) {
        unsafe {
            SharedBox::dec(self.inner.as_ptr());
        }
    }
}

impl<T: ?Sized> fmt::Debug for Shared<T>
where
    T: any::Any + fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Safety: by virtue of holding onto a shared we can safely access
        // `inner` because it must outlive any `Shared` instances.
        unsafe {
            let inner = self.inner.as_ref();

            if !inner.access.is_shared() {
                write!(fmt, "*not accessible*")
            } else {
                write!(fmt, "{:?}", &&*inner.data.get())
            }
        }
    }
}

/// A debug helper that prints detailed diagnostics on the type being debugged.
///
/// Constructed using [debug][Shared::debug].
pub struct SharedDebug<'a, T: ?Sized> {
    shared: &'a Shared<T>,
}

impl<T: ?Sized> fmt::Debug for SharedDebug<'_, T>
where
    T: any::Any + fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Safety: by virtue of holding onto a shared we can safely access
        // `inner` because it must outlive any `Shared` instances.
        unsafe {
            let inner = self.shared.inner.as_ref();
            let mut debug = fmt.debug_struct("Shared");

            debug.field("access", &inner.access);
            debug.field("count", &inner.count.get());

            if !inner.access.is_shared() {
                debug.field("data", &any::type_name::<T>());
            } else {
                debug.field("data", &&*inner.data.get());
            }

            debug.finish()
        }
    }
}

/// The boxed internals of [Shared].
#[repr(C)]
struct SharedBox<T: ?Sized> {
    /// The access of the shared data.
    access: Access,
    /// The number of strong references to the shared data.
    count: Cell<usize>,
    /// The value being held. Guarded by the `access` field to determine if it
    /// can be access shared or exclusively.
    data: UnsafeCell<T>,
}

impl<T: ?Sized> SharedBox<T> {
    /// Increment the reference count of the inner value.
    unsafe fn inc(this: *const Self) {
        let count = (*this).count.get();

        if count == 0 || count == usize::max_value() {
            process::abort();
        }

        let count = count + 1;
        (*this).count.set(count);
    }

    /// Decrement the reference count in inner, and free the underlying data if
    /// it has reached zero.
    ///
    /// # Safety
    ///
    /// Caller needs to ensure that `this` is a valid pointer.
    unsafe fn dec(this: *mut Self) {
        let count = (*this).count.get();

        if count == 0 {
            process::abort();
        }

        let count = count - 1;
        (*this).count.set(count);

        if count != 0 {
            return;
        }

        if (*this).access.is_taken() {
            // NB: This prevents the inner `T` from being dropped in case it
            // has already been taken (as indicated by `is_taken`).
            //
            // If it has been taken, the shared box contains invalid memory.
            let _ = std::mem::transmute::<_, Box<SharedBox<ManuallyDrop<T>>>>(Box::from_raw(this));
        } else {
            // NB: At the point of the final drop, no on else should be using
            // this.
            debug_assert!((*this).access.is_exclusive());
            let _ = Box::from_raw(this);
        }
    }
}

type DropFn = unsafe fn(*const ());

struct RawSharedBox {
    data: *const (),
    drop_fn: DropFn,
}

impl RawSharedBox {
    /// Construct a raw inner from an existing inner value.
    ///
    /// # Safety
    ///
    /// Should only be constructed over a pointer that is lively owned.
    fn from_inner<T>(inner: ptr::NonNull<SharedBox<T>>) -> Self {
        return Self {
            data: inner.as_ptr() as *const (),
            drop_fn: drop_fn_impl::<T>,
        };

        unsafe fn drop_fn_impl<T>(data: *const ()) {
            SharedBox::dec(data as *mut () as *mut SharedBox<T>);
        }
    }
}

impl Drop for RawSharedBox {
    fn drop(&mut self) {
        // Safety: type and referential safety is guaranteed at construction
        // time, since all constructors are unsafe.
        unsafe {
            (self.drop_fn)(self.data);
        }
    }
}

/// A strong reference to the given type.
pub struct OwnedRef<T: ?Sized> {
    data: *const T,
    guard: RawBorrowedRef,
    inner: RawSharedBox,
    _marker: marker::PhantomData<T>,
}

impl<T: ?Sized> OwnedRef<T> {
    /// Convert into a raw pointer and associated raw access guard.
    ///
    /// # Safety
    ///
    /// The returned pointer must not outlive the associated guard, since this
    /// prevents other uses of the underlying data which is incompatible with
    /// the current.
    pub fn into_raw(this: Self) -> (*const T, RawOwnedRef) {
        let guard = RawOwnedRef {
            _guard: this.guard,
            _inner: this.inner,
        };

        (this.data, guard)
    }
}

impl<T: ?Sized> ops::Deref for OwnedRef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: An owned ref holds onto a hard pointer to the data,
        // preventing it from being dropped for the duration of the owned ref.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> fmt::Debug for OwnedRef<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, fmt)
    }
}

/// A raw guard to a [OwnedRef].
pub struct RawOwnedRef {
    _guard: RawBorrowedRef,
    _inner: RawSharedBox,
}

/// A strong mutable reference to the given type.
pub struct OwnedMut<T: ?Sized> {
    data: *mut T,
    guard: RawBorrowedMut,
    inner: RawSharedBox,
    _marker: marker::PhantomData<T>,
}

impl<T: ?Sized> OwnedMut<T> {
    /// Convert into a raw pointer and associated raw access guard.
    ///
    /// # Safety
    ///
    /// The returned pointer must not outlive the associated guard, since this
    /// prevents other uses of the underlying data which is incompatible with
    /// the current.
    pub fn into_raw(this: Self) -> (*mut T, RawOwnedMut) {
        let guard = RawOwnedMut {
            _guard: this.guard,
            _inner: this.inner,
        };

        (this.data, guard)
    }
}

impl<T: ?Sized> ops::Deref for OwnedMut<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: An owned mut holds onto a hard pointer to the data,
        // preventing it from being dropped for the duration of the owned mut.
        unsafe { &*self.data }
    }
}

impl<T: ?Sized> ops::DerefMut for OwnedMut<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Safety: An owned mut holds onto a hard pointer to the data,
        // preventing it from being dropped for the duration of the owned mut.
        unsafe { &mut *self.data }
    }
}

impl<T: ?Sized> fmt::Debug for OwnedMut<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, fmt)
    }
}

impl<F> Future for OwnedMut<F>
where
    F: Unpin + Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // NB: inner Future is Unpin.
        let this = self.get_mut();
        Pin::new(&mut **this).poll(cx)
    }
}

/// A raw guard to a [OwnedRef].
pub struct RawOwnedMut {
    _guard: RawBorrowedMut,
    _inner: RawSharedBox,
}
