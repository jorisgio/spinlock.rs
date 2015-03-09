spinlock-rs
===========

A spinlock implementation in rust

[documentation](http://jorisgio.github.io/spinlock.rs/spinlock/)

Build
-----

Run `cargo build`

Usage
-----

The library implements a Reader/Writer lock. When locking a spin lock for shared
Read access, you will get a reference to the protected data, and while locking
for an exclusive Write access, you will get a mutable reference.

```rust
extern crate spinlock;
use spinlock::SpinLock;

fn main() {
	let spin = SpinLock::new(0);

	// Write access
	{
		let mut data = spin.write().unwrap();
		*data += 1;
	}
	// Read access
	{
		let data = spin.read().unwrap();
		println!("{}", *data);
	}
}
```

Please note that the spinlock doesn't deal itself with reference counting. You
might want to use `Arc<SpinLock<T>>` to share the lock between threads.

