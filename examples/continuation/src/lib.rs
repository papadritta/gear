#![no_std]

use codec::{Decode, Encode};
use gstd::{msg, prelude::*, ActorId, exec, debug};
use scale_info::TypeInfo;
use sp_core::{
    crypto::UncheckedFrom,
    sr25519::{Pair as Sr25519Pair, Public, Signature},
    Pair,
};

const INITIAL_GAS_HINT: u64 = 5_000_000_000;

static mut PUB_KEY: Public = Public([0u8; 32]);
static mut STATE: State = State::Start;
static mut GAS_HINT: u64 = 0;
static mut INPUT_DATA: Vec<SignedMessage> = vec![];
static mut RESULT: Vec<VerificationResult> = vec![];
static mut MAX_GAS: u64 = 0;

#[derive(Clone, Copy, Debug)]
enum State {
    Start,
    Processing(usize),
    End,
}

#[derive(Debug, Decode, TypeInfo)]
pub struct SignedMessage {
    pub message: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Debug)]
struct Verified;

#[derive(Debug)]
struct CompletedResult {
    signature: Option<Verified>,
    remained_gas: u64,
}

#[derive(Debug, Default)]
struct VerificationResult {
    gas: u64,
    result: Option<CompletedResult>,
}

#[derive(Debug, Decode, TypeInfo)]
pub struct InputArgs {
    pub signatory: ActorId,
    pub signed_messages: Vec<SignedMessage>,
    pub gas_hint: Option<u64>,
}

gstd::metadata! {
    title: "demo continuation",
    init:
        input: InputArgs,
}

#[no_mangle]
pub unsafe extern "C" fn init() {
    let args: InputArgs = msg::load().expect("Failed to decode `InputArgs`");

    debug!("args = {:?}", args);

    GAS_HINT = args.gas_hint.unwrap_or(INITIAL_GAS_HINT);
    INPUT_DATA = args.signed_messages;
    if INPUT_DATA.is_empty() {
        panic!("No input data");
    }

    RESULT.resize_with(INPUT_DATA.len(), Default::default);

    PUB_KEY = Public::unchecked_from(<[u8; 32]>::from(args.signatory));
}

const fn middle_point(a: u64, b: u64) -> u64 {
    let remainder_a = a % 2;
    let remainder_b = b % 2;

    a / 2 + b / 2 + (remainder_a + remainder_b) / 2
}

fn do_work(i: usize, gas: u64) {
    unsafe { &mut RESULT[i] }.gas = gas;

    // the same way as in verify.rs from subkey
    let mut signature: Signature = Default::default();
    let data = unsafe { &INPUT_DATA[i] };
    if data.signature.len() != signature.0.len() {
        unsafe { &mut RESULT[i] }.result = Some(CompletedResult {
            signature: None,
            remained_gas: exec::gas_available(),
        });

        return;
    }

    signature.as_mut().copy_from_slice(&data.signature);

    let verified = if Sr25519Pair::verify(&signature, &data.message, &unsafe { PUB_KEY }) {
        Some(Verified)
    } else {
        None
    };

    unsafe { &mut RESULT[i] }.result = Some(CompletedResult {
        signature: verified,
        remained_gas: exec::gas_available(),
    });
}

#[no_mangle]
pub unsafe extern "C" fn handle() {
    match STATE {
        State::Start => {
            let gas = GAS_HINT;

            STATE = State::Processing(0);
            MAX_GAS = gas;

            msg::send_bytes(
                exec::program_id(),
                vec![],
                exec::gas_available() - gas,
                0,
            );

            do_work(0, gas);
        }

        State::Processing(i) => {
            if msg::source() != exec::program_id() {
                panic!("New messages are not accepted while processing");
            }

            let result = &RESULT[i];
            let (next_i, next_gas) = if let Some(ref completed_result) = result.result {
                // previous computation finished successfully
                let used_gas = result.gas - completed_result.remained_gas;
                let new_max = MAX_GAS.max(used_gas);

                let next_gas = middle_point(result.gas, MAX_GAS);
                debug!("i = {:?}, next_gas = {:?}, used_gas = {:?}, MAX_GAS = {:?}, result = {:?}", i, next_gas, used_gas, MAX_GAS, result);

                MAX_GAS = new_max;

                (i + 1, next_gas)
            } else {
                // gas limit exceeded
                (i, 2 * result.gas)
            };

            if next_i < INPUT_DATA.len() {
                STATE = State::Processing(next_i);
    
                msg::send_bytes(
                    exec::program_id(),
                    vec![],
                    exec::gas_available() - next_gas,
                    0,
                );

                do_work(next_i, next_gas);
            } else {
                STATE = State::End;
    
                msg::send_bytes(
                    exec::program_id(),
                    vec![],
                    exec::gas_available() - 10_000_000,
                    0,
                );
            }
        }

        State::End => {
            debug!("INPUT_DATA = {:?}", INPUT_DATA);
            debug!("RESULT = {:?}", RESULT);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn handle_reply() {}
