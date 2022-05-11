#![no_std]

static mut x: i32 = 0;

#[no_mangle]
pub unsafe extern "C" fn handle() {
    gstd::debug!(gstd::format!("loop begin: {}", x));
    loop {
        x = x / 123;
        if x == 1 {
            break;
        }
    }
    gstd::debug!(gstd::format!("loop end: {}", x));
}

#[no_mangle]
pub unsafe extern "C" fn init() {}
