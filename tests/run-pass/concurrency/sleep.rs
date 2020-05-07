// ignore-windows: Concurrency on Windows is not supported yet.
// compile-flags: -Zmiri-disable-isolation

use std::{thread, time};

/// This test was copied from the documentation.
fn sleep() {
    let hundred_millis = time::Duration::from_millis(100);
    let now = time::Instant::now();
    thread::sleep(hundred_millis);
    assert!(now.elapsed() >= hundred_millis);
    assert!(now.elapsed() <= time::Duration::from_millis(200));
}

fn main() {
    sleep();
}