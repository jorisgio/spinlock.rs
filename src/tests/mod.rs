extern crate test;

use super::*;

use std::thread;

#[test]
fn read() {
    let spin = SpinLock::new(0u32);
    let x = spin.read().unwrap();
    assert_eq!(*x, 0u32);
}

#[test]
fn write() {
    let spin = SpinLock::new(0u32);
    {
        let mut x = spin.write().unwrap();
        assert_eq!(*x, 0u32);
        *x = 42u32;
    }
    let x = spin.read().unwrap();
    assert_eq!(*x, 42u32);
}

#[test]
fn try_read_success() {
    let spin = SpinLock::new(255);
    let result = spin.try_read();
    assert!(result.is_ok());
    let x = result.unwrap();
    assert_eq!(*x, 255);
}

#[test]
fn try_write_success() {
    let spin = SpinLock::new(255);
    {
        let result = spin.try_write();
        assert!(result.is_ok());
        let mut x = result.unwrap();
        *x = 42;
    }
    let x = spin.read().unwrap();
    assert_eq!(*x, 42);
}

#[test]
fn try_write_block() {
    let spin = SpinLock::new(255);
    let x = spin.write().unwrap();
    {
        let result = spin.try_write();
        assert!(match result { Err(TryLockError::WouldBlock) => true, _ => false });
    } 
    let _ = test::black_box(x);
}

#[test]
fn try_read_block() {
    let spin = SpinLock::new(255);
    let x = spin.write().unwrap();
    {
        let result = spin.try_read();
        assert!(match result { Err(TryLockError::WouldBlock) => true, _ => false });
    }
    let _ = test::black_box(x);
}

#[test]
#[should_fail]
fn indefinite_read() {
    let spin = SpinLock::new(255);
    let x = spin.write().unwrap();
    {
        let _ = spin.read().unwrap();
    }
    let _ = test::black_box(x);
}


struct PoisonnedGuard<'a, T : 'a>(&'a SpinLock<T>);

#[unsafe_destructor]
impl<'a, T> Drop for PoisonnedGuard<'a, T> {
    fn drop(&mut self) {
        assert!(thread::panicking()); 
        assert!(self.0.is_poisonned());
    }
}

#[test]
#[should_fail(expected = "child thread None panicked")]
fn poison() {
    let spin = SpinLock::new(255);
    let guard = PoisonnedGuard(&spin);
    let x = spin.write().unwrap();
    thread::scoped(|| { let _  = spin.write().unwrap(); }).join();
    let _ = &guard;
}

#[test]
fn concurrent_readers() {
    let spin = SpinLock::new(255);
    let mut workers : Vec<thread::JoinGuard<()>> = Vec::with_capacity(10); 
    for _ in (0..9) {
        workers.push(thread::scoped(|| { let data = spin.read().unwrap(); assert_eq!(*data, 255);}) );
    }
    for w in workers {
        w.join();
    }
}

#[test]
fn concurrent_writers() {
    let spin = SpinLock::new(255);
    let mut workers : Vec<thread::JoinGuard<()>> = Vec::with_capacity(10); 
    for i in (0u32..9) {
        let spin = &spin;
        workers.push(thread::scoped(move || { let mut data = spin.write().unwrap(); *data = i ;assert_eq!(*data, i );}) );
    }
    for w in workers {
        w.join();
    }
}
