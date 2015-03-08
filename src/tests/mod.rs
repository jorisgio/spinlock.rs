use super::*;

#[test]
fn test_read() {
    let spin = SpinLock::new(0u32);
    let x = spin.read().unwrap();
    assert_eq!(*x, 0u32);
}

#[test]
fn test_write() {
    let spin = SpinLock::new(0u32);
    {
        let mut x = spin.write().unwrap();
        assert_eq!(*x, 0u32);
        *x = 42u32;
    }
    let x = spin.read().unwrap();
    assert_eq!(*x, 42u32);
}
