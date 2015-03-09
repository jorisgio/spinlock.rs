//! A reader/writer spin lock implementation.
//! A spin lock is a particular kind of lock which will not put the thread to sleep when it tries
//! to acquire a lock already hold by another thread or codepath. Instead, the thread will *spin*
//! on the lock, which means it will loop trying to acquire it again and again.
//!
//! # Timeouts 
//!
//! This implementation has a timeout. Spinlock are fast but should only be used to protect short
//! critical sections and a lock holder should only hold the lock for a very short period of time
//! to avoid wasting CPU cycles. If a thread tries to acquire a lock for more than `MAX_WAIT`
//! nanoseconds, it will panic.
//!
//! # Optimistic
//!
//! The implementation is Optimistic. The acquire codepath assumes that the lock operation will
//! succeed and doesn't try to minimize the cost of the initial acquire operation. If the access
//! is contested, the implementation then tries to minimize the pressure on the CPU cache until it
//! it can actually acquire the lock.
//! After `SPIN_COUNT` retries, the acquire path will call `thread::yield_now()` between each retry
//! to allow for other threads to run and drop the lock.
//!
//! # Favor exclusive access
//!
//! If a thread is spinning in an contested exclusive access attempt, no new shared access will be
//! granted. This is designed to avoid Reader DOSing the Writers.
//!
//! # Poisonning
//!
//! If an exclusive holder panics while holding the lock, it might void the coherency rules.
//! Indeed, the protected data might be in a state in which it is not supposed to be. To prevent
//! this, if an exclusive holder panics, the lock is poisonned and no other access to the lock will
//! be granted.

#![feature(optin_builtin_traits)]
#![feature(unsafe_destructor)]
#![feature(core)]
#![feature(std_misc)]
#![feature(test)]
extern crate time;

use std::cell::UnsafeCell;
pub use std::sync::{
    TryLockError,
    LockResult,
    TryLockResult,
    PoisonError,
};
use std::thread;
use std::ops::{Deref, DerefMut};

use std::sync::atomic::{Ordering, AtomicUsize};


// Panic guards, taken from std::sync 

// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

struct Flag { failed: UnsafeCell<bool> }
unsafe impl Send for Flag {}
unsafe impl Sync for Flag {}

impl Flag {
    #[inline]
    fn new() -> Flag {
        Flag { failed : UnsafeCell { value : false }}
    }

    #[inline]
    fn borrow(&self) -> LockResult<Guard> {
        let ret = Guard { panicking: thread::panicking() };
        if unsafe { *self.failed.get() } {
            Err(PoisonError::new(ret))
        } else {
            Ok(ret)
        }
    }

    #[inline]
    fn done(&self, guard: &Guard) {
        if !guard.panicking && thread::panicking() {
            unsafe { *self.failed.get() = true; }
        }
    }

    #[inline]
    fn get(&self) -> bool {
        unsafe { *self.failed.get() }
    }
}

struct Guard {
    panicking: bool,
}

fn map_result<T, U, F>(result: LockResult<T>, f: F)
                           -> LockResult<U>
                           where F: FnOnce(T) -> U {
    match result {
        Ok(t) => Ok(f(t)),
        Err(err) => Err(PoisonError::new(f(err.into_inner())))
    }
}

// SpinLock implementation

// The counter is a number of SHARED readers instead of EXCLUSIVE writers
const SHARED : usize = std::isize::MIN as usize;
// A thread is trying to acquire an exclusive lock
const EXCLWAIT : usize = SHARED >> 4;
/// Maximum time spent in spinning in constested path in nanoseconds
pub const MAX_WAIT : u64 = 2_000_000_000;
/// Number of spin retry in contested path 
pub const SPIN_COUNT : u32 = 0xFF;
    
/// A Reader/Writer spinlock
///
/// A lock protects the underlying data of type `T` for shared memory concurent access.
/// The lock semantic allows any number of concurent readers or alternatively at most one writer.
/// A reader is given a reference to the protected data and is able to read it with the guarantee 
/// that the data will not change while it holds this reference.
/// A writer is given a mutable reference to the protected data and is able to read and write it
/// with the guarantee that the data will not change while it holds this reference.
///
/// The protected data must implement the markers `Send` and the marker `Sync`.
pub struct SpinLock<T> {
    count : AtomicUsize,
    data : UnsafeCell<T>,
    poison : Flag,
}

unsafe impl<T: Send + Sync> Send for SpinLock<T> {}
unsafe impl<T: Send + Sync> Sync for SpinLock<T> {}


impl<T : Send + Sync> SpinLock<T> {
    /// Create a new `SpinLock` wrapping the supplied data
    #[inline]
    pub fn new(data : T) -> SpinLock<T> {
        SpinLock {
            count : AtomicUsize::new(0),
            data : UnsafeCell::new(data),
            poison : Flag::new(),
        }
    }

    /// Lock the `SpinLock` for exclusive access and returns a RAII guard which will drop the lock
    /// when it is dropped.
    ///
    /// If another holder as paniced while holding a write lock on the same spinlock, the lock will
    /// be poisonned. In this case, `write` will fail with a `PoisonError`.
    ///
    /// # Panics
    ///
    /// Panics if the thread is waiting more than `MAX_WAIT` nanoseconds
    ///
    /// # Examples
    ///
    /// ```
    /// let spin = SpinLock::new(42);
    /// {
    ///     let data = spin.write().unwrap();
    ///     *data += 1;
    /// }
    /// let data = spin.read().unwrap();
    /// assert_eq!(*data, 43);
    /// ```
    #[inline]
    pub fn write(&self) -> LockResult<SpinLockWriteGuard<T>> {
        if self.count.fetch_add(1, Ordering::Acquire) != 0 {
            return self.write_contested()
        }
        SpinLockWriteGuard::new(self, &self.data)
    }

    // Contested acquire path. Spin on the lock until acquiring or timeout
    fn write_contested(&self) -> LockResult<SpinLockWriteGuard<T>> {
        let mut base_time : u64 = 0;
        let mut time : u64;
        let mut i = 0u32;

        // Notify that we want exclusive access
        self.count.fetch_add(EXCLWAIT - 1, Ordering::Relaxed);
        // Clear SHARED flag
        self.count.fetch_and(! SHARED, Ordering::Relaxed);
        loop {
            // Use relaxed ordering to avoid trashing the cache coherency handling for free 
            let count = self.count.load(Ordering::Relaxed);
            if count & (EXCLWAIT - 1) == 0 && 
                self.count.compare_and_swap(count, count - EXCLWAIT + 1, Ordering::Acquire) == count { 
                    break;
                }

            if i >= SPIN_COUNT  {
                time = time::precise_time_ns();
                if base_time == 0 {
                    base_time = time;
                } else if time - base_time > MAX_WAIT {
                    // XXX we converted the SHARED locks to exclusive ones, i'm not sure how bad
                    // actually is
                    self.count.fetch_sub(EXCLWAIT, Ordering::Relaxed);
                    panic!("Spinning on a spin lock for too long")
                }
                thread::yield_now();
            } else {
                i += 1;
            }
        }
        SpinLockWriteGuard::new(self, &self.data)
    }

    /// Lock the `SpinLock` for shared access and returns a RAII guard which will drop the lock
    /// when it is dropped.
    ///
    /// If another holder as paniced while holding a write lock on the same spinlock, the lock will
    /// be poisonned. In this case, `write` will fail with a `PoisonError`.
    ///
    /// # Panics
    ///
    /// Panics if the thread is waiting more than `MAX_WAIT` nanoseconds
    ///
    /// # Examples
    ///
    /// ```
    /// let spin = SpinLock::new(42);
    /// let data = spin.read().unwrap();
    /// assert_eq!(*data, 42);
    /// ```
    #[inline]
    pub fn read(&self) -> LockResult<SpinLockReadGuard<T>> {
        if self.count.compare_and_swap(0, SHARED | 1, Ordering::Acquire) != 0 {
            return self.read_contested()
        }
        SpinLockReadGuard::new(self, &self.data)
    }

    // Contested acquire path. Spin on the lock until acquiring or timeout
    fn read_contested(&self) -> LockResult<SpinLockReadGuard<T>> {
        let mut i = 0u32;
        let mut base_time : u64 = 0;
        let mut time : u64;
        loop {
            let count = self.count.load(Ordering::Relaxed);
            if count == 0 {
                if self.count.compare_and_swap(0, SHARED | 1, Ordering::Acquire) == 0 {
                    break
                }
            } else if count & SHARED == SHARED {
                if self.count.compare_and_swap(count, count + 1, Ordering::Acquire) == count {
                    break
                }
            }
            if i >= SPIN_COUNT  {
                time = time::precise_time_ns();
                if base_time == 0 {
                    base_time = time;
                } else if time - base_time > MAX_WAIT {
                    panic!("Spinning on a spin lock for too long")
                }
                thread::yield_now();
            } else {
                i+=1;
            }
        }
        SpinLockReadGuard::new(self, &self.data)
    }

    /// Attempt to acquire the lock with shared access.
    ///
    /// This function will never spin, and will return immediatly if the access is contested. 
    /// Returns a RAII guard if the access is successful, or `TryLockError::WouldBlock` if the
    /// access could not be granted.
    ///
    /// If an exclusive holder has panic while holding the lock, the lock will be poisonned and
    /// `Poisonned(PoisonError)` will be returned.
    ///
    /// # Example
    ///
    /// ```
    /// let spin = SpinLock::new(42);
    ///
    /// match spin.try_read() {
    ///     Ok(data) => assert_eq!(*data, 42),
    ///     Err(TryLockError::WouldBlock) => (), // Sorry luke it's not your turn
    ///     Err(Poisonned(_)) => panic!("Lock is poisonned"),
    /// }
    /// ```
    #[inline]
    pub fn try_read(&self) -> TryLockResult<SpinLockReadGuard<T>> {
        if self.count.compare_and_swap(0, 1 | SHARED, Ordering::Acquire) == 0 {
            return Ok(try!(SpinLockReadGuard::new(self, &self.data)))
        } else {
            let count = self.count.load(Ordering::Relaxed);
            if count & SHARED == SHARED && 
                self.count.compare_and_swap(count, count + 1, Ordering::Acquire) == count { 
                return Ok(try!(SpinLockReadGuard::new(self, &self.data)))
            }
            return Err(TryLockError::WouldBlock)
        }
    }

    /// Attempt to acquire the lock with exclusive access.
    ///
    /// This function will never spin, and will return immediatly if the access is contested. 
    /// Returns a RAII guard if the access is successful, or `TryLockError::WouldBlock` if the
    /// access could not be granted.
    ///
    /// If an exclusive holder has panic while holding the lock, the lock will be poisonned and
    /// `Poisonned(PoisonError)` will be returned.
    ///
    /// # Example
    ///
    /// ```
    /// let spin = SpinLock::new(42);
    ///
    /// match spin.try_wirte() {
    ///     Ok(data) => {
    ///         assert_eq!(*data, 42);
    ///         *data += 1;
    ///         assert_eq(*data, 43);
    ///     },
    ///     Err(TryLockError::WouldBlock) => (), // Sorry luke it's not your turn
    ///     Err(Poisonned(_)) => panic!("Lock is poisonned"),
    /// }
    /// ```
    pub fn try_write(&self) -> TryLockResult<SpinLockWriteGuard<T>> {
        if self.count.compare_and_swap(0, 1, Ordering::Acquire) == 0 {
            Ok(try!(SpinLockWriteGuard::new(self, &self.data)))
        } else {
            Err(TryLockError::WouldBlock)
        }
    }

}

impl<T> SpinLock<T> {

    #[inline]
    fn read_unlock(&self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
        // Clear SHARED flag if SHARED count is 0
        self.count.compare_and_swap(SHARED, 0, Ordering::Relaxed);
    }

    fn write_unlock(&self) {
        self.count.fetch_sub(1, Ordering::Release);
    }

    /// Check if the lock is poisonned
    #[inline]
    pub fn is_poisonned(&self) -> bool {
        self.poison.get()
    }
}


/// RAII structure used to release the shared read access of a lock when dropped.
#[must_use]
pub struct SpinLockReadGuard<'a, T : 'a> {
    lock : &'a SpinLock<T>,
    data : &'a UnsafeCell<T>,
}

impl<'a, T> !Send for SpinLockReadGuard<'a, T> {}

impl<'a, T> SpinLockReadGuard<'a, T> {
    fn new(lock : &'a SpinLock<T>, data : &'a UnsafeCell<T>) -> LockResult<SpinLockReadGuard<'a, T>> {
        map_result(lock.poison.borrow(), |_| {
            SpinLockReadGuard {
                lock : lock,
                data : data,
            }
        })
    }
}

impl<'a, T> Deref for SpinLockReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T { unsafe { &*self.data.get() } }
}

#[unsafe_destructor]
impl<'a, T> Drop for SpinLockReadGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.read_unlock();
    }
}

/// RAII structure used to release the exclusive write access of a lock when dropped.
#[must_use]
pub struct SpinLockWriteGuard<'a, T : 'a> {
    lock : &'a SpinLock<T>,
    data : &'a UnsafeCell<T>,
    poison :  Guard,
}

impl<'a, T> !Send for SpinLockWriteGuard<'a, T> {}

impl<'a, T> SpinLockWriteGuard<'a, T> {
    fn new(lock : &'a SpinLock<T>, data : &'a UnsafeCell<T>) -> LockResult<SpinLockWriteGuard<'a, T>> {
        map_result(lock.poison.borrow(), |guard| {
            SpinLockWriteGuard {
                lock : lock,
                data : data,
                poison : guard,
            }
        })
    }
}

impl<'a, T> Deref for SpinLockWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &T { unsafe { &*self.data.get() } }
}
    
impl<'a, T> DerefMut for SpinLockWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.data.get() } }
}

#[unsafe_destructor]
impl<'a, T> Drop for SpinLockWriteGuard<'a, T> {
    fn drop(&mut self) {
        self.lock.poison.done(&self.poison);
        self.lock.write_unlock();
    }
}

#[cfg(test)]
mod tests;
