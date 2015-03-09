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
//
const SHARED : usize = 0x80000000;
const EXCLWAIT : usize = 0x00100000;
const MAXWAIT : u64 = 2_000_000_000;
const SPIN_COUNT : u32 = 0xFF;
    
pub struct SpinLock<T> {
    count : AtomicUsize,
    data : UnsafeCell<T>,
    poison : Flag,
}

unsafe impl<T: Send + Sync> Send for SpinLock<T> {}
unsafe impl<T: Send + Sync> Sync for SpinLock<T> {}


impl<T : Send + Sync> SpinLock<T> {
    pub fn new(data : T) -> SpinLock<T> {
        SpinLock {
            count : AtomicUsize::new(0),
            data : UnsafeCell::new(data),
            poison : Flag::new(),
        }
    }

    #[inline]
    pub fn write(&self) -> LockResult<SpinLockWriteGuard<T>> {
        if self.count.fetch_add(1, Ordering::Acquire) != 0 {
            return self.write_contested()
        }
        SpinLockWriteGuard::new(self, &self.data)
    }

    pub fn write_contested(&self) -> LockResult<SpinLockWriteGuard<T>> {
        let mut base_time : u64 = 0;
        let mut time : u64;
        let mut i = 0u32;

        self.count.fetch_add(EXCLWAIT - 1, Ordering::Relaxed);
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
                } else if time - base_time > MAXWAIT {
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

    #[inline]
    pub fn read(&self) -> LockResult<SpinLockReadGuard<T>> {
        if self.count.compare_and_swap(0, SHARED | 1, Ordering::Acquire) != 0 {
            return self.read_contested()
        }
        SpinLockReadGuard::new(self, &self.data)
    }

    pub fn read_contested(&self) -> LockResult<SpinLockReadGuard<T>> {
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
                } else if time - base_time > MAXWAIT {
                    panic!("Spinning on a spin lock for too long")
                }
                thread::yield_now();
            } else {
                i+=1;
            }
        }
        SpinLockReadGuard::new(self, &self.data)
    }

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

    pub fn try_write(&self) -> TryLockResult<SpinLockWriteGuard<T>> {
        if self.count.compare_and_swap(0, 1, Ordering::Acquire) == 0 {
            Ok(try!(SpinLockWriteGuard::new(self, &self.data)))
        } else {
            Err(TryLockError::WouldBlock)
        }
    }

    #[inline]
    pub fn is_poisonned(&self) -> bool {
        self.poison.get()
    }

}

impl<T> SpinLock<T> {

    #[inline]
    fn read_unlock(&self) {
        self.count.fetch_sub(1, Ordering::Release);

        while self.count.load(Ordering::Relaxed) == SHARED {
            if self.count.compare_and_swap(SHARED, 0, Ordering::Relaxed) == SHARED {
                break
            }
        }
    }

    fn write_unlock(&self) {
        self.count.fetch_sub(1, Ordering::Release);
    }
}

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
