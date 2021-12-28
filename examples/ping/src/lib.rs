#![no_std]

use gstd::{debug, msg, prelude::*};

static mut MESSAGE_LOG: Vec<String> = vec![];

#[no_mangle]
pub unsafe extern "C" fn handle() {
    let new_msg = String::from_utf8(msg::load_bytes()).expect("Invalid message");

    if new_msg == "PING" {
        msg::reply_bytes("PONG", 12_000_000, 0);
    }

    MESSAGE_LOG.push(new_msg);

    debug!("{:?} total message(s) stored: ", MESSAGE_LOG.len());

    for log in MESSAGE_LOG.iter() {
        debug!(log);
    }
}

#[no_mangle]
pub unsafe extern "C" fn init() {}

#[gstd::test]
fn receives_ping() {
    let p1 = gstd::test::init_self(());
    let reply = p1.send_and_wait_for_reply("PING").expect("Failed to wait for reply");
    gstd::test::assert_eq!(reply, "PONG");

    let p2 = gstd::test::mock_from_network("0xD4BFD16DA3D6AA3256ADEEC76315C");
    let reply = p1.send_and_wait_for_reply("PING").expect("Failed to wait for reply");
    gstd::test::assert_eq!(reply, "PONG");
}
