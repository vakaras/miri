#![feature(thread_local)]

use std::thread;

#[thread_local]
static mut A: u8 = 0;
#[thread_local]
static mut B: u8 = 0;
static mut C: u8 = 0;

unsafe fn get_a_ref() -> *mut u8 {
    &mut A
}

struct Sender(*mut u8);

unsafe impl Send for Sender {}

fn main() {
    let ptr = unsafe {
        let x = get_a_ref();
        *x = 5;
        assert_eq!(A, 5);
        B = 15;
        C = 25;
        Sender(&mut A)
    };

    thread::spawn(move || unsafe {
        assert_eq!(*ptr.0, 5);
        assert_eq!(A, 0);
        assert_eq!(B, 0);
        assert_eq!(C, 25);
        B = 14;
        C = 24;
        let y = get_a_ref();
        assert_eq!(*y, 0);
        *y = 4;
        assert_eq!(*ptr.0, 5);
        assert_eq!(A, 4);
        assert_eq!(*get_a_ref(), 4);
    })
    .join()
    .unwrap();

    unsafe {
        assert_eq!(*get_a_ref(), 5);
        assert_eq!(A, 5);
        assert_eq!(B, 15);
        assert_eq!(C, 24);
    }
}