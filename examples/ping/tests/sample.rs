extern crate gstd; // Defines global allocator from galloc

use gtest::{System, Program, Log};

fn send_ping(prog: &Program, log: &Log) {
    let res = prog.send_bytes(100001, "PING");

    assert!(res.contains(log));
}

#[test]
fn some_test() {
    let sys = System::new();
    sys.init_logger();

    let ping_pong = Program::from_file(&sys, "../target/wasm32-unknown-unknown/release/demo_ping.wasm");
    ping_pong.send_bytes(100001, "INIT");

    let expected = Log::builder().dest(100001).source(1).payload_bytes("PONG");

    send_ping(&ping_pong, &expected);
    send_ping(&ping_pong, &expected);
    send_ping(&ping_pong, &expected);
    send_ping(&ping_pong, &expected);
}
