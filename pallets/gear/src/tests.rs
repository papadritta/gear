// This file is part of Gear.

// Copyright (C) 2021 Gear Technologies Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use codec::Encode;
use common::{self, GasToFeeConverter, Origin as _};
use frame_support::{assert_noop, assert_ok};
use frame_system::Pallet as SystemPallet;
use gear_runtime_interface as gear_ri;
use pallet_balances::{self, Pallet as BalancesPallet};
use tests_distributor::{Request, WASM_BINARY_BLOATY as DISTRIBUTOR_WASM_BINARY};
use tests_program_factory::{CreateProgram, WASM_BINARY_BLOATY as PROGRAM_FACTORY_WASM_BINARY};

use super::{
    manager::HandleKind,
    mock::{
        new_test_ext, run_to_block, Event as MockEvent, Gear, Origin, System, Test, BLOCK_AUTHOR,
        LOW_BALANCE_USER, USER_1, USER_2, USER_3,
    },
    pallet, DispatchOutcome, Error, Event, ExecutionResult, GasAllowance, Mailbox, MessageInfo,
    Pallet as GearPallet, Reason,
};

use utils::*;

#[test]
fn submit_program_expected_failure() {
    init_logger();
    new_test_ext().execute_with(|| {
        let balance = BalancesPallet::<Test>::free_balance(USER_1);
        assert_noop!(
            GearPallet::<Test>::submit_program(
                Origin::signed(USER_1).into(),
                ProgramCodeKind::Default.to_bytes(),
                DEFAULT_SALT.to_vec(),
                EMPTY_PAYLOAD.to_vec(),
                DEFAULT_GAS_LIMIT,
                balance + 1
            ),
            Error::<Test>::NotEnoughBalanceForReserve
        );

        assert_noop!(
            submit_program_default(LOW_BALANCE_USER, ProgramCodeKind::Default),
            Error::<Test>::NotEnoughBalanceForReserve
        );

        // Gas limit is too high
        let block_gas_limit = <Test as pallet::Config>::BlockGasLimit::get();
        assert_noop!(
            GearPallet::<Test>::submit_program(
                Origin::signed(USER_1).into(),
                ProgramCodeKind::Default.to_bytes(),
                DEFAULT_SALT.to_vec(),
                EMPTY_PAYLOAD.to_vec(),
                block_gas_limit + 1,
                0
            ),
            Error::<Test>::GasLimitTooHigh
        );
    })
}

#[test]
fn submit_program_fails_on_duplicate_id() {
    init_logger();
    new_test_ext().execute_with(|| {
        assert_ok!(submit_program_default(USER_1, ProgramCodeKind::Default));
        // Finalize block to let queue processing run
        run_to_block(2, None);
        // By now this program id is already in the storage
        assert_noop!(
            submit_program_default(USER_1, ProgramCodeKind::Default),
            Error::<Test>::ProgramAlreadyExists
        );
    })
}

#[test]
fn send_message_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let user1_initial_balance = BalancesPallet::<Test>::free_balance(USER_1);
        let user2_initial_balance = BalancesPallet::<Test>::free_balance(USER_2);

        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::Default);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        assert_ok!(send_default_message(USER_1, program_id));

        // Balances check
        // Gas spends on sending 2 default messages (submit program and send message to program)
        let user1_potential_msgs_spends = GasConverter::gas_to_fee(2 * DEFAULT_GAS_LIMIT);
        // User 1 has sent two messages
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_1),
            user1_initial_balance - user1_potential_msgs_spends
        );

        // Clear messages from the queue to refund unused gas
        run_to_block(2, None);

        // Checking that sending a message to a non-program address works as a value transfer
        let mail_value = 20_000;

        // Take note of up-to-date users balance
        let user1_initial_balance = BalancesPallet::<Test>::free_balance(USER_1);

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            USER_2.into_origin(),
            EMPTY_PAYLOAD.to_vec(),
            DEFAULT_GAS_LIMIT,
            mail_value,
        ));

        // Transfer of `mail_value` completed.
        // Gas limit is ignored for messages headed to a mailbox - no funds have been reserved.
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_1),
            user1_initial_balance - mail_value
        );
        // The recipient has already received the funds
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_2),
            user2_initial_balance + mail_value
        );

        // Ensure the message didn't burn any gas (i.e. never went through processing pipeline)
        let remaining_weight = 100_000;
        run_to_block(3, Some(remaining_weight));

        // Messages were sent by user 1 only
        let actual_gas_burned = remaining_weight - GasAllowance::<Test>::get();
        assert_eq!(actual_gas_burned, 0);
    });
}

#[test]
fn send_message_expected_failure() {
    init_logger();
    new_test_ext().execute_with(|| {
        // Submitting failing in init program and check message is failed to be sent to it
        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::GreedyInit);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };
        run_to_block(2, None);

        assert_noop!(
            send_default_message(LOW_BALANCE_USER, program_id),
            Error::<Test>::ProgramIsNotInitialized
        );

        // Submit valid program and test failing actions on it
        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::Default);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        assert_noop!(
            send_default_message(LOW_BALANCE_USER, program_id),
            Error::<Test>::NotEnoughBalanceForReserve
        );

        // Value transfer is attempted if `value` field is greater than 0
        assert_noop!(
            GearPallet::<Test>::send_message(
                Origin::signed(LOW_BALANCE_USER).into(),
                USER_1.into_origin(),
                EMPTY_PAYLOAD.to_vec(),
                1, // gas limit must be greater than 0 to have changed the state during reserve()
                100
            ),
            pallet_balances::Error::<Test>::InsufficientBalance
        );

        // Gas limit too high
        let block_gas_limit = <Test as pallet::Config>::BlockGasLimit::get();
        assert_noop!(
            GearPallet::<Test>::send_message(
                Origin::signed(USER_1).into(),
                program_id,
                EMPTY_PAYLOAD.to_vec(),
                block_gas_limit + 1,
                0
            ),
            Error::<Test>::GasLimitTooHigh
        );
    })
}

#[test]
fn messages_processing_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::Default);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };
        assert_ok!(send_default_message(USER_1, program_id));

        run_to_block(2, None);

        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(2).into());

        assert_ok!(send_default_message(USER_1, USER_2.into_origin()));
        assert_ok!(send_default_message(USER_1, program_id));

        run_to_block(3, None);

        // "Mail" from user to user should not be processed as messages
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(1).into());
    });
}

#[test]
fn spent_gas_to_reward_block_author_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let block_author_initial_balance = BalancesPallet::<Test>::free_balance(BLOCK_AUTHOR);
        assert_ok!(submit_program_default(USER_1, ProgramCodeKind::Default));
        run_to_block(2, None);

        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(1).into());

        // The block author should be paid the amount of Currency equal to
        // the `gas_charge` incurred while processing the `InitProgram` message
        let gas_spent = GasConverter::gas_to_fee(
            <Test as pallet::Config>::BlockGasLimit::get() - GasAllowance::<Test>::get(),
        );
        assert_eq!(
            BalancesPallet::<Test>::free_balance(BLOCK_AUTHOR),
            block_author_initial_balance + gas_spent
        );
    })
}

#[test]
fn unused_gas_released_back_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let user1_initial_balance = BalancesPallet::<Test>::free_balance(USER_1);
        let huge_send_message_gas_limit = 50_000;

        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::Default);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            program_id,
            EMPTY_PAYLOAD.to_vec(),
            huge_send_message_gas_limit,
            0
        ));
        // Spends for submit program with default gas limit and sending default message with a huge gas limit
        let user1_potential_msgs_spends =
            GasConverter::gas_to_fee(DEFAULT_GAS_LIMIT + huge_send_message_gas_limit);
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_1),
            user1_initial_balance - user1_potential_msgs_spends
        );
        assert_eq!(
            BalancesPallet::<Test>::reserved_balance(USER_1),
            (DEFAULT_GAS_LIMIT + huge_send_message_gas_limit) as _,
        );

        run_to_block(2, None);
        let user1_actual_msgs_spends = GasConverter::gas_to_fee(
            <Test as pallet::Config>::BlockGasLimit::get() - GasAllowance::<Test>::get(),
        );
        assert!(user1_potential_msgs_spends > user1_actual_msgs_spends);
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_1),
            user1_initial_balance - user1_actual_msgs_spends
        );
    })
}

#[test]
fn lazy_pages() {
    // This test access different pages in linear wasm memory
    // and check that lazy-pages (see gear-lazy-pages) works correct:
    // For each page, which has been loaded from storage <=> page has been accessed.
    let wat = r#"
	(module
		(import "env" "memory" (memory 1))
        (import "env" "alloc" (func $alloc (param i32) (result i32)))
		(export "handle" (func $handle))
		(export "init" (func $init))
		(func $init
            ;; allocate 9 pages in init, so mem will contain 10 pages
            i32.const 0x0
            i32.const 0x9
            call $alloc
            i32.store
        )
        (func $handle
            ;; write access page 0
            i32.const 0x0
            i32.const 0x42
            i32.store

            ;; write access page 2
            i32.const 0x20000
            i32.const 0x42
            i32.store

            ;; read access page 5
            i32.const 0x0
            i32.const 0x50000
            i32.load
            i32.store

            ;; write access page 8 and 9 by one store
            i32.const 0x8fffc
            i64.const 0xffffffffffffffff
            i64.store
		)
	)"#;

    init_logger();
    new_test_ext().execute_with(|| {
        let pid = {
            let code = ProgramCodeKind::Custom(wat).to_bytes();
            let salt = DEFAULT_SALT.to_vec();
            let prog_id = generate_program_id(&code, &salt);
            let res = GearPallet::<Test>::submit_program(
                Origin::signed(USER_1).into(),
                code,
                salt,
                EMPTY_PAYLOAD.to_vec(),
                5_000_000,
                0,
            )
            .map(|_| prog_id);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        run_to_block(2, Some(10_000_000));
        log::debug!("submit done {:?}", pid);
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(1).into());

        let res = GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            pid,
            EMPTY_PAYLOAD.to_vec(),
            1_000_000,
            100,
        );
        log::debug!("res = {:?}", res);
        assert_ok!(res);

        run_to_block(3, Some(10_000_000));

        // Dirty hack: lazy pages info is stored in thread local static variables,
        // so after contract execution lazy-pages information
        // remains correct and we can use it here.
        let released_pages = gear_ri::gear_ri::get_released_pages();
        let lazy_pages = gear_ri::gear_ri::get_wasm_lazy_pages_numbers();

        // checks not accessed pages
        assert_eq!(lazy_pages, [1, 3, 4, 6, 7]);
        // checks accessed pages
        assert_eq!(released_pages, [0, 2, 5, 8, 9]);
    });
}

#[test]
fn block_gas_limit_works() {
    // Same as `ProgramCodeKind::GreedyInit`, but greedy handle
    let wat = r#"
	(module
		(import "env" "memory" (memory 1))
		(export "handle" (func $handle))
		(export "init" (func $init))
		(func $init)
        (func $doWork (param $size i32)
            (local $counter i32)
            i32.const 0
            set_local $counter
            loop $while
                get_local $counter
                i32.const 1
                i32.add
                set_local $counter
                get_local $counter
                get_local $size
                i32.lt_s
                if
                    br $while
                end
            end $while
        )
        (func $handle
            i32.const 10
            call $doWork
		)
	)"#;

    init_logger();
    new_test_ext().execute_with(|| {
        let remaining_weight = 100_000;

        // Submit programs and get their ids
        let pid1 = {
            let res = submit_program_default(USER_1, ProgramCodeKind::OutgoingWithValueInHandle);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };
        let pid2 = {
            let res = submit_program_default(USER_1, ProgramCodeKind::Custom(wat));
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        run_to_block(2, Some(remaining_weight));
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(2).into());

        // We send 10M of gas from inside the program (see `ProgramCodeKind::OutgoingWithValueInHandle` WAT code).
        let gas_to_send = 10_000_000;

        // Count gas needed to process programs with default payload
        let expected_gas_msg_to_pid1 = GearPallet::<Test>::get_gas_spent(
            USER_1.into_origin(),
            HandleKind::Handle(pid1),
            EMPTY_PAYLOAD.to_vec(),
        )
        .expect("internal error: get gas spent (pid1) failed")
            - gas_to_send;
        let expected_gas_msg_to_pid2 = GearPallet::<Test>::get_gas_spent(
            USER_1.into_origin(),
            HandleKind::Handle(pid2),
            EMPTY_PAYLOAD.to_vec(),
        )
        .expect("internal error: get gas spent (pid2) failed");

        // TrapInHandle code kind is used because processing default payload in its
        // context requires such an amount of gas, that the following assertion can be passed.
        assert!(expected_gas_msg_to_pid1 + expected_gas_msg_to_pid2 > remaining_weight);

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            pid1,
            EMPTY_PAYLOAD.to_vec(),
            expected_gas_msg_to_pid1,
            100
        ));
        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            pid1,
            EMPTY_PAYLOAD.to_vec(),
            expected_gas_msg_to_pid1,
            100
        ));

        run_to_block(3, Some(remaining_weight));
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(2).into());

        // Run to the next block to reset the gas limit
        run_to_block(4, Some(remaining_weight));

        // Add more messages to queue
        // Total `gas_limit` of three messages (2 to pid1 and 1 to pid2) exceeds the block gas limit
        assert!(remaining_weight < 2 * expected_gas_msg_to_pid1 + expected_gas_msg_to_pid2);
        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            pid1,
            EMPTY_PAYLOAD.to_vec(),
            expected_gas_msg_to_pid1,
            200
        ));
        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            pid2,
            EMPTY_PAYLOAD.to_vec(),
            expected_gas_msg_to_pid2,
            100
        ));
        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            pid1,
            EMPTY_PAYLOAD.to_vec(),
            expected_gas_msg_to_pid1,
            200
        ));

        // Try to process 3 messages
        run_to_block(5, Some(remaining_weight));

        // Message #2 steps beyond the block gas allowance and is re-queued
        // Message #1 is dequeued and processed, message #3 stays in the queue:
        //
        // | 1 |        | 3 |
        // | 2 |  ===>  | 2 |
        // | 3 |        |   |
        //
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(1).into());
        assert_eq!(
            GasAllowance::<Test>::get(),
            remaining_weight - expected_gas_msg_to_pid1
        );

        // Try to process 2 messages
        run_to_block(6, Some(remaining_weight));

        // Message #3 get dequeued and processed
        // Message #2 gas limit still exceeds the remaining allowance:
        //
        // | 3 |        | 2 |
        // | 2 |  ===>  |   |
        //
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(1).into());
        assert_eq!(
            GasAllowance::<Test>::get(),
            remaining_weight - expected_gas_msg_to_pid1
        );

        run_to_block(7, Some(remaining_weight));

        // This time message #2 makes it into the block:
        //
        // | 2 |        |   |
        // |   |  ===>  |   |
        //
        SystemPallet::<Test>::assert_last_event(Event::MessagesDequeued(1).into());
        assert_eq!(
            GasAllowance::<Test>::get(),
            remaining_weight - expected_gas_msg_to_pid2
        );
    });
}

#[test]
fn mailbox_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        // caution: runs to block 2
        let reply_to_id = setup_mailbox_test_state(USER_1);

        // Ensure that all the gas has been returned to the sender upon messages processing
        assert_eq!(BalancesPallet::<Test>::reserved_balance(USER_1), 0);

        let mailbox_message = {
            let res = GearPallet::<Test>::remove_from_mailbox(USER_1.into_origin(), reply_to_id);
            assert!(res.is_some());
            res.expect("was asserted previously")
        };

        assert_eq!(mailbox_message.id, reply_to_id,);

        // Gas limit should have been ignored by the code that puts a message into a mailbox
        assert_eq!(mailbox_message.gas_limit, 0);
        assert_eq!(mailbox_message.value, 1000);
    })
}

#[test]
fn init_message_logging_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let mut next_block = 2;
        let codes = [
            (ProgramCodeKind::Default, false, ""),
            // Will fail, because tests use default gas limit, which is very low for successful greedy init
            (ProgramCodeKind::GreedyInit, true, "Gas limit exceeded"),
        ];

        for (code_kind, is_failing, trap_explanation) in codes {
            SystemPallet::<Test>::reset_events();

            assert_ok!(submit_program_default(USER_1, code_kind));

            let event = match SystemPallet::<Test>::events()
                .last()
                .map(|r| r.event.clone())
            {
                Some(MockEvent::Gear(e)) => e,
                _ => unreachable!("Should be one Gear event"),
            };

            run_to_block(next_block, None);

            let msg_info = match event {
                Event::InitMessageEnqueued(info) => info,
                _ => unreachable!("expect Event::InitMessageEnqueued"),
            };

            SystemPallet::<Test>::assert_has_event(if is_failing {
                Event::InitFailure(
                    msg_info,
                    Reason::Dispatch(trap_explanation.as_bytes().to_vec()),
                )
                .into()
            } else {
                Event::InitSuccess(msg_info).into()
            });

            next_block += 1;
        }
    })
}

#[test]
fn program_lifecycle_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        // Submitting first program and getting its id
        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::Default);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        assert!(!Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        run_to_block(2, None);

        assert!(Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        // Submitting second program, which fails on initialization, therefore goes to limbo.
        let program_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::GreedyInit);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        assert!(!Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        run_to_block(3, None);

        assert!(!Gear::is_initialized(program_id));
        // while at the same time being stuck in "limbo"
        assert!(Gear::is_failed(program_id));

        // Program author is allowed to remove the program and reclaim funds
        // An attempt to remove a program on behalf of another account will make no changes
        assert_ok!(GearPallet::<Test>::remove_stale_program(
            // Not the author
            Origin::signed(LOW_BALANCE_USER).into(),
            program_id,
        ));
        // Program in the storage
        assert!(common::get_program(program_id).is_some());
        // and is still in the limbo
        assert!(GearPallet::<Test>::is_failed(program_id));

        assert_ok!(GearPallet::<Test>::remove_stale_program(
            Origin::signed(USER_1).into(),
            program_id,
        ));
        // Program not in the storage
        assert!(common::get_program(program_id).is_none());
        // This time the program has been removed from limbo
        assert!(crate::ProgramsLimbo::<Test>::get(program_id).is_none());
    })
}

#[test]
fn events_logging_works() {
    let wat_trap_in_handle = r#"
	(module
		(import "env" "memory" (memory 1))
		(export "handle" (func $handle))
		(export "init" (func $init))
		(func $handle
			unreachable
		)
		(func $init)
	)"#;

    let wat_trap_in_init = r#"
	(module
		(import "env" "memory" (memory 1))
		(export "handle" (func $handle))
		(export "init" (func $init))
		(func $handle)
		(func $init
            unreachable
        )
	)"#;

    init_logger();
    new_test_ext().execute_with(|| {
        let mut nonce = 0;
        let mut next_block = 2;
        let tests = [
            // Code, init failure reason, handle succeed flag
            (ProgramCodeKind::Default, None, true),
            (
                ProgramCodeKind::GreedyInit,
                Some("Gas limit exceeded".as_bytes().to_vec()),
                false,
            ),
            (
                ProgramCodeKind::Custom(wat_trap_in_init),
                Some(Vec::new()),
                false,
            ),
            (ProgramCodeKind::Custom(wat_trap_in_handle), None, false),
        ];
        for (code_kind, init_failure_reason, handle_succeed) in tests {
            SystemPallet::<Test>::reset_events();

            let program_id = {
                let res = submit_program_default(USER_1, code_kind);
                assert_ok!(res);
                res.expect("submit result was asserted")
            };

            let init_msg_info = MessageInfo {
                program_id,
                message_id: compute_user_message_id(EMPTY_PAYLOAD, nonce),
                origin: USER_1.into_origin(),
            };
            nonce += 1;

            SystemPallet::<Test>::assert_last_event(
                Event::InitMessageEnqueued(init_msg_info.clone()).into(),
            );

            run_to_block(next_block, None);
            next_block += 1;

            // Init failed program checks
            if let Some(init_failure_reason) = init_failure_reason {
                SystemPallet::<Test>::assert_has_event(
                    Event::InitFailure(init_msg_info, Reason::Dispatch(init_failure_reason)).into(),
                );
                // Sending messages to failed-to-init programs shouldn't be allowed
                assert_noop!(
                    send_default_message(USER_1, program_id),
                    Error::<Test>::ProgramIsNotInitialized
                );
                continue;
            }

            SystemPallet::<Test>::assert_has_event(Event::InitSuccess(init_msg_info).into());

            let dispatch_msg_info = MessageInfo {
                program_id,
                message_id: compute_user_message_id(EMPTY_PAYLOAD, nonce),
                origin: USER_1.into_origin(),
            };
            // Messages to fully-initialized programs are accepted
            assert_ok!(send_default_message(USER_1, program_id));
            SystemPallet::<Test>::assert_last_event(
                Event::DispatchMessageEnqueued(dispatch_msg_info.clone()).into(),
            );

            run_to_block(next_block, None);

            SystemPallet::<Test>::assert_has_event(
                Event::MessageDispatched(DispatchOutcome {
                    message_id: dispatch_msg_info.message_id,
                    outcome: if handle_succeed {
                        ExecutionResult::Success
                    } else {
                        ExecutionResult::Failure(Vec::new())
                    },
                })
                .into(),
            );

            nonce += 1;
            next_block += 1;
        }
    })
}

#[test]
fn send_reply_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        // caution: runs to block 2
        let reply_to_id = setup_mailbox_test_state(USER_1);

        let prog_id = generate_program_id(
            &ProgramCodeKind::OutgoingWithValueInHandle.to_bytes(),
            &DEFAULT_SALT.to_vec(),
        );

        // Top up program's account balance by 2000 to allow user claim 1000 from mailbox
        assert_ok!(
            <BalancesPallet::<Test> as frame_support::traits::Currency<_>>::transfer(
                &USER_1,
                &AccountId::from_origin(prog_id),
                2000,
                frame_support::traits::ExistenceRequirement::AllowDeath
            )
        );

        assert_ok!(GearPallet::<Test>::send_reply(
            Origin::signed(USER_1).into(),
            reply_to_id,
            EMPTY_PAYLOAD.to_vec(),
            10_000_000,
            1000, // `prog_id` sent message with value of 1000 (see program code)
        ));

        // global nonce is 2 before sending reply message
        // `submit_program` and `send_message` messages were sent before in `setup_mailbox_test_state`
        let expected_reply_message_id = compute_user_message_id(EMPTY_PAYLOAD, 2);

        let event = match SystemPallet::<Test>::events()
            .last()
            .map(|r| r.event.clone())
        {
            Some(MockEvent::Gear(e)) => e,
            _ => unreachable!("Should be one Gear event"),
        };

        let MessageInfo {
            message_id: actual_reply_message_id,
            ..
        } = match event {
            Event::DispatchMessageEnqueued(info) => info,
            _ => unreachable!("expect Event::DispatchMessageEnqueued"),
        };

        assert_eq!(expected_reply_message_id, actual_reply_message_id);
    })
}

#[test]
fn send_reply_failure_to_claim_from_mailbox() {
    init_logger();
    new_test_ext().execute_with(|| {
        // Expecting error as long as the user doesn't have messages in mailbox
        assert_noop!(
            GearPallet::<Test>::send_reply(
                Origin::signed(USER_1).into(),
                5.into_origin(), // non existent `reply_to_id`
                EMPTY_PAYLOAD.to_vec(),
                DEFAULT_GAS_LIMIT,
                0
            ),
            Error::<Test>::NoMessageInMailbox
        );

        // caution: runs to block 2
        let reply_to_id = setup_mailbox_test_state(USER_1);

        // Program doesn't have enough balance: 1000 units of currency is claimed by `USER_1` first
        assert_noop!(
            GearPallet::<Test>::send_reply(
                Origin::signed(USER_1).into(),
                reply_to_id,
                EMPTY_PAYLOAD.to_vec(),
                5_000_000,
                0
            ),
            pallet_balances::Error::<Test>::InsufficientBalance
        );
    })
}

#[test]
fn send_reply_value_claiming_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let prog_id = {
            let res = submit_program_default(USER_1, ProgramCodeKind::OutgoingWithValueInHandle);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        // This value is actually a constants in WAT. Alternatively can be read from Mailbox.
        let locked_value = 1000;

        let mut next_block = 2;
        let mut program_nonce = 0u64;

        let user_messages_data = [
            // gas limit, value
            (1_000_000, 100),
            (20_000_000, 2000),
        ];
        for (gas_limit_to_reply, value_to_reply) in user_messages_data {
            let reply_to_id = populate_mailbox_from_program(
                prog_id,
                USER_1,
                USER_1,
                next_block,
                program_nonce,
                20_000_000,
                0,
            );
            program_nonce += 1;
            next_block += 1;

            // Top up program's account so it could send value in message
            let send_to_program_amount = locked_value * 2;
            assert_ok!(
                <BalancesPallet::<Test> as frame_support::traits::Currency<_>>::transfer(
                    &USER_1,
                    &AccountId::from_origin(prog_id),
                    send_to_program_amount,
                    frame_support::traits::ExistenceRequirement::AllowDeath
                )
            );

            let user_balance = BalancesPallet::<Test>::free_balance(USER_1);
            assert_eq!(BalancesPallet::<Test>::reserved_balance(USER_1), 0);

            assert_ok!(GearPallet::<Test>::send_reply(
                Origin::signed(USER_1).into(),
                reply_to_id,
                EMPTY_PAYLOAD.to_vec(),
                gas_limit_to_reply,
                value_to_reply,
            ));

            let user_expected_balance =
                user_balance - value_to_reply - GasConverter::gas_to_fee(gas_limit_to_reply)
                    + locked_value;
            assert_eq!(
                BalancesPallet::<Test>::free_balance(USER_1),
                user_expected_balance
            );
            assert_eq!(
                BalancesPallet::<Test>::reserved_balance(USER_1),
                GasConverter::gas_to_fee(gas_limit_to_reply)
            );
        }
    })
}

// user 1 sends to prog msg
// prog send to user 1 msg to mailbox
// user 1 claims it from mailbox

#[test]
fn claim_value_from_mailbox_works() {
    init_logger();
    new_test_ext().execute_with(|| {
        let sender_balance = BalancesPallet::<Test>::free_balance(USER_2);
        let claimer_balance = BalancesPallet::<Test>::free_balance(USER_1);

        let gas_sent = 20_000_000;
        let value_sent = 1000;

        let prog_id = {
            let res = submit_program_default(USER_3, ProgramCodeKind::OutgoingWithValueInHandle);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };

        let reply_to_id =
            populate_mailbox_from_program(prog_id, USER_2, USER_1, 2, 0, gas_sent, value_sent);

        let gas_burned = GasConverter::gas_to_fee(
            GearPallet::<Test>::get_gas_spent(
                USER_1.into_origin(),
                HandleKind::Handle(prog_id),
                EMPTY_PAYLOAD.to_vec(),
            )
            .expect("program exists and not faulty"),
        );

        run_to_block(3, None);

        assert_ok!(GearPallet::<Test>::claim_value_from_mailbox(
            Origin::signed(USER_1).into(),
            reply_to_id,
        ));

        assert_eq!(BalancesPallet::<Test>::reserved_balance(USER_1), 0);
        assert_eq!(BalancesPallet::<Test>::reserved_balance(USER_2), 0);

        let expected_claimer_balance = claimer_balance + value_sent;
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_1),
            expected_claimer_balance
        );

        // We send 10M of gas from inside the program (see `ProgramCodeKind::OutgoingWithValueInHandle` WAT code).
        let gas_to_send = 10_000_000;
        // Gas left returns to sender from consuming of value tree while claiming.
        let expected_sender_balance = sender_balance - value_sent - gas_burned + gas_to_send;
        assert_eq!(
            BalancesPallet::<Test>::free_balance(USER_2),
            expected_sender_balance
        );

        SystemPallet::<Test>::assert_last_event(Event::ClaimedValueFromMailbox(reply_to_id).into());
    })
}

#[test]
fn distributor_initialize() {
    init_logger();
    new_test_ext().execute_with(|| {
        let initial_balance = BalancesPallet::<Test>::free_balance(USER_1)
            + BalancesPallet::<Test>::free_balance(BLOCK_AUTHOR);

        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            DISTRIBUTOR_WASM_BINARY
                .expect("Wasm binary missing!")
                .to_vec(),
            DEFAULT_SALT.to_vec(),
            EMPTY_PAYLOAD.to_vec(),
            10_000_000,
            0,
        ));

        run_to_block(2, None);

        // At this point there is a message in USER_1's mailbox, however, since messages in
        // mailbox are stripped of the `gas_limit`, the respective gas tree has been consumed
        // and the value unreserved back to the original sender (USER_1)
        let final_balance = BalancesPallet::<Test>::free_balance(USER_1)
            + BalancesPallet::<Test>::free_balance(BLOCK_AUTHOR);

        assert_eq!(initial_balance, final_balance);
    });
}

#[test]
fn distributor_distribute() {
    init_logger();
    new_test_ext().execute_with(|| {
        let initial_balance = BalancesPallet::<Test>::free_balance(USER_1)
            + BalancesPallet::<Test>::free_balance(BLOCK_AUTHOR);
        let code = DISTRIBUTOR_WASM_BINARY
            .expect("Wasm binary missing!")
            .to_vec();

        let program_id = generate_program_id(&code, DEFAULT_SALT);

        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            code,
            DEFAULT_SALT.to_vec(),
            EMPTY_PAYLOAD.to_vec(),
            10_000_000,
            0,
        ));

        run_to_block(2, None);

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            program_id,
            Request::Receive(10).encode(),
            20_000_000,
            0,
        ));

        run_to_block(3, None);

        // Despite some messages are still in the mailbox all gas locked in value trees
        // has been refunded to the sender so the free balances should add up
        let final_balance = BalancesPallet::<Test>::free_balance(USER_1)
            + BalancesPallet::<Test>::free_balance(BLOCK_AUTHOR);

        assert_eq!(initial_balance, final_balance);
    });
}

#[test]
fn test_code_submission_pass() {
    init_logger();
    new_test_ext().execute_with(|| {
        let code = ProgramCodeKind::Default.to_bytes();
        let code_hash = sp_io::hashing::blake2_256(&code).into();

        assert_ok!(GearPallet::<Test>::submit_code(
            Origin::signed(USER_1),
            code.clone()
        ));

        let saved_code = common::get_code(code_hash);
        assert_eq!(saved_code, Some(code));

        let expected_meta = Some(common::CodeMetadata::new(USER_1.into_origin(), 1));
        let actual_meta = common::get_code_metadata(code_hash);
        assert_eq!(expected_meta, actual_meta);

        SystemPallet::<Test>::assert_last_event(Event::CodeSaved(code_hash).into());
    })
}

#[test]
fn test_same_code_submission_fails() {
    init_logger();
    new_test_ext().execute_with(|| {
        let code = ProgramCodeKind::Default.to_bytes();

        assert_ok!(GearPallet::<Test>::submit_code(
            Origin::signed(USER_1),
            code.clone()
        ),);
        // Trying to set the same code twice.
        assert_noop!(
            GearPallet::<Test>::submit_code(Origin::signed(USER_1), code.clone()),
            Error::<Test>::CodeAlreadyExists,
        );
        // Trying the same from another origin
        assert_noop!(
            GearPallet::<Test>::submit_code(Origin::signed(USER_2), code.clone()),
            Error::<Test>::CodeAlreadyExists,
        );
    })
}

#[test]
fn test_code_is_not_submitted_twice_after_program_submission() {
    init_logger();
    new_test_ext().execute_with(|| {
        let code = ProgramCodeKind::Default.to_bytes();
        let code_hash = sp_io::hashing::blake2_256(&code).into();

        // First submit program, which will set code and metadata
        assert_ok!(submit_program_default(USER_1, ProgramCodeKind::Default));
        SystemPallet::<Test>::assert_has_event(Event::CodeSaved(code_hash).into());
        assert!(common::code_exists(code_hash));

        // Trying to set the same code twice.
        assert_noop!(
            GearPallet::<Test>::submit_code(Origin::signed(USER_2), code),
            Error::<Test>::CodeAlreadyExists,
        );
    })
}

#[test]
fn test_code_is_not_resetted_within_program_submission() {
    init_logger();
    new_test_ext().execute_with(|| {
        let code = ProgramCodeKind::Default.to_bytes();
        let code_hash = sp_io::hashing::blake2_256(&code).into();

        // First submit code
        assert_ok!(GearPallet::<Test>::submit_code(
            Origin::signed(USER_1),
            code.clone()
        ));
        let expected_code_saved_events = 1;
        let expected_meta = common::get_code_metadata(code_hash);
        assert!(expected_meta.is_some());

        // Submit program from another origin. Should not change meta or code.
        assert_ok!(submit_program_default(USER_2, ProgramCodeKind::Default));
        let actual_meta = common::get_code_metadata(code_hash);
        let actual_code_saved_events = SystemPallet::<Test>::events()
            .iter()
            .filter(|e| matches!(e.event, MockEvent::Gear(Event::CodeSaved(_))))
            .count();

        assert_eq!(expected_meta, actual_meta);
        assert_eq!(expected_code_saved_events, actual_code_saved_events);
    })
}

#[test]
fn test_create_program() {
    use WasmKind::{Constructable, NonConstructable};

    // Expected values are counted only in the context of the messages with child (created from factory program) address destination
    // regardless of other messages sent before/after them.
    type ExpectedChildrenMessagesDequeued = u32;
    type ExpectedChildrenMessagesDispatched = u32;
    type ExpectedChildrenInitSuccessNum = u32;
    type ExpectedChildrenData = (
        ExpectedChildrenMessagesDequeued,
        ExpectedChildrenMessagesDispatched,
        ExpectedChildrenInitSuccessNum,
    );

    type TestData<'a> = (
        Vec<CreateProgram>,
        Option<WasmKind<'a>>,
        ExpectedChildrenData,
    );

    enum WasmKind<'a> {
        Constructable(Vec<ProgramCodeKind<'a>>),
        NonConstructable(ProgramCodeKind<'a>),
    }

    // Such a code, that `gear_core::program::Program` struct can't be initialized with it
    let non_constructable_wat = r#"
    (module)
    "#;
    // Same as ProgramCodeKind::Default, but has a different hash (init and handle method are swapped)
    let child2_wat = r#"
    (module
        (import "env" "memory" (memory 1))
        (export "handle" (func $handle))
        (export "init" (func $init))
        (func $init)
        (func $handle)
    )
    "#;

    let factory_code = PROGRAM_FACTORY_WASM_BINARY.expect("wasm binary missing!");
    let factory_id = generate_program_id(factory_code, DEFAULT_SALT);

    let child1_code_kind = ProgramCodeKind::Default;
    let child2_code_kind = ProgramCodeKind::Custom(child2_wat);
    let invalid_prog_code_kind = ProgramCodeKind::Custom(non_constructable_wat);

    let child1_code_hash = sp_io::hashing::blake2_256(child1_code_kind.to_bytes().as_slice());
    let child2_code_hash = sp_io::hashing::blake2_256(child2_code_kind.to_bytes().as_slice());
    let invalid_prog_code_hash =
        sp_io::hashing::blake2_256(invalid_prog_code_kind.to_bytes().as_slice());

    // TODO #617 add balances (gas) checks
    let tests = vec![
        (
            "Create single child (simple)",
            // 1 child init message succeed, 1 child message dispatched = 2 dequeued 
            (vec![CreateProgram::Default(true)], Some(Constructable(vec![child1_code_kind])), (2, 1, 1)),
        ),
        (
            "Try to create a child with non existing code for the code hash",
            // Messages are skipped
            (vec![CreateProgram::Default(true)], None, (0, 0, 0))
        ),
        (
            "Try to create a child providing few gas (child init will fail)",
            // 1 child init message fails but processed (dequeued), 
            // 1 child dispatch message dequeued, but skipped in process queue
            (vec![CreateProgram::Custom(vec![(child1_code_hash, b"default".to_vec(), 1000)])],
            Some(Constructable(vec![child1_code_kind])), (2, 0, 0))
        ),
        (
            "Try to create a program with non constructable code",
            // Messages skipped (non constructable code isn't stored, so the same as non existing code test)
            (vec![CreateProgram::Custom(vec![(invalid_prog_code_hash, b"default".to_vec(), 10_000)])],
            Some(NonConstructable(invalid_prog_code_kind)), (0, 0, 0))
        ),
        (
            "Try to create a program with existing address",
            // 1 dispatch message from factory is sent to the existing destination (so 1 dequeued)
            // child init message is skipped, because duplicates existing contract
            (vec![CreateProgram::Custom(vec![(child1_code_hash, DEFAULT_SALT.to_vec(), 10_000)])],
            Some(Constructable(vec![child1_code_kind])), (1, 1, 0))
        ),
        (
            "Try to create a program with existing address, but the original \
            program and its duplicate are being created from the factory program.",
            // 2 children init messages are successfully processed (+2 dequeued)
            // 6 children handle messages are successfully dispatched (+6 dequeued)
            (vec![
                CreateProgram::Custom(
                    vec![
                        (child1_code_hash, b"default".to_vec(), 10_000),
                        (child1_code_hash, b"default".to_vec(), 10_000), // duplicate
                    ]
                ),
                CreateProgram::Custom(
                    vec![
                        (child2_code_hash, b"default".to_vec(), 10_000),
                        (child2_code_hash, b"default".to_vec(), 10_000), // duplicate
                    ]
                ),
                CreateProgram::Custom(
                    vec![
                        (child2_code_hash, b"default".to_vec(), 10_000), // duplicate
                        (child1_code_hash, b"default".to_vec(), 10_000), // duplicate
                    ]
                ),
            ],
            Some(Constructable(vec![child1_code_kind, child2_code_kind])),
            (8, 6, 2)
            )
        ),
        (
            "Simple passing case for creating multiple children",
            // 3 children init messages + 3 children dispatch messages = 6 messages dequeued
            (vec![CreateProgram::Custom(vec![
                    (child1_code_hash, b"salt1".to_vec(), 10_000),
                    (child1_code_hash, b"salt2".to_vec(), 10_000),
                    (child2_code_hash, b"salt3".to_vec(), 10_000),
                ]),
            ],
            Some(Constructable(vec![child1_code_kind, child2_code_kind])),
            (6, 3, 3)
            )
        ),
        (
            "Trying to create a child and its duplicate. The first child creation message will fail \
            in init due to lack of gas, the duplicate will be skipped, despite having \
            enough gas limit",
            (vec![
                CreateProgram::Custom(
                    // 1 failing child init message is processed (+1 dequeued, but 0 successfully init)
                    // 2 dispatch messages are sent, dequeued, but skipped in `process_queue` (+2 dequeued) 
                    vec![
                        (child1_code_hash, b"salt1".to_vec(), 1000), // fail init (not enough gas)
                        (child1_code_hash, b"salt1".to_vec(), 10_000), // duplicate
                    ]
                ),
                // this message is in the next block
                CreateProgram::Custom(
                    // messages aren't queued
                    vec![
                        // Not a duplicate (no program with such id), nor the candidate
                        // Still messages aren't queued, because such messages are intended 
                        // to be sent to limbo program (see payload upper).
                        (child1_code_hash, b"salt1".to_vec(), 10_000), 
                    ]
                ),
            ],
            Some(Constructable(vec![child1_code_kind])),
            (3, 0, 0)
            )
        ),
        (
            "Creating multiple children with some duplicates and some failing in init",
            (vec![
                CreateProgram::Custom(
                    vec![
                        // one successful init with one handle message (2 dequeued, 1 dispatched)
                        (child1_code_hash, b"salt1".to_vec(), 10_000),
                        // init fail (not enough gas), handle message is consumed, but not executed  (2 dequeued, 0 dispatched)
                        (child1_code_hash, b"salt2".to_vec(), 1000),
                    ]
                ),
                CreateProgram::Custom(
                    vec![
                        // init fail (not enough gas), handle message is consumed, but not executed (2 dequeued, 0 dispatched)
                        (child2_code_hash, b"salt1".to_vec(), 3000),
                        // init message is skipped (duplicate), handle message is consumed, but not executed (1 dequeued, 0  dispatched) 
                        (child2_code_hash, b"salt1".to_vec(), 10_000),
                         // one successful init with one handle message (2 dequeued, 1 dispatched)
                        (child2_code_hash, b"salt2".to_vec(), 10_000),
                    ]
                ),
                CreateProgram::Custom(
                    vec![
                        // init is skipped (program with such address exists, but in limbo), dispatch message is skipped, because of limbo destination (0 dispatched, 0 dequeued)
                        (child2_code_hash, b"salt1".to_vec(), 10_000),
                         // one successful init with one handle message (2 dequeued, 1 dispatched)
                        (child2_code_hash, b"salt3".to_vec(), 10_000),
                    ]
                ),
            ],
            Some(Constructable(vec![child1_code_kind, child2_code_kind])),
            (11, 3, 3)
            )
        ),
        (
            "Factory sending different message kinds", 
            (vec![
                // init and handle dispatch are created (2 dequeued, 1 dispatched)
                CreateProgram::Default(true),
                // init and handle_reply are created (1 dequeued, 0 dispatched)
                CreateProgram::Default(false),
                // init and handle_reply are created (1 dequeued, 0 dispatched)
                CreateProgram::Default(false),
                // init and handle dispatch are created (2 dequeued, 1 dispatched)
                CreateProgram::Default(true),
                CreateProgram::Custom(
                    vec![
                        // init and handle dispatch are created (2 dequeued, 1 dispatched)
                        (child1_code_hash, b"salt1".to_vec(), 10_000),
                        // init and handle dispatch are created (2 dequeued, 1 dispatched)
                        (child1_code_hash, b"salt2".to_vec(), 10_000),
                        // init is skipped, handle is processed and dispatched (1 dequeued, 1 dispatched)
                        (child1_code_hash, b"salt2".to_vec(), 10_000), // duplicate
                    ]
                )
            ], Some(Constructable(vec![child1_code_kind])), (11, 5, 6)
            ),
        ),
        (
            "Creating multiple children with non existent code hash", 
            (vec![
                CreateProgram::Custom(
                    vec![
                        (child1_code_hash, b"salt1".to_vec(), 10_000),
                        (child1_code_hash, b"salt2".to_vec(), 10_000),
                        (child1_code_hash, b"salt2".to_vec(), 10_000), // duplicate, but will be skipped for no code hash
                    ]
                )
            ], None, (0, 0, 0)
            ),
        ),
        (
            "Trying to create a child and its duplicates. The first child creation message will succeed \
            in init, the duplicates will be skipped, but handle messages, which were intended for the duplicates \
            will be executed in the context of original child",
            (vec![
                CreateProgram::Custom(
                    vec![
                        // 1 successful child init and handle (+2 dequeued, +1 dispatched)
                        (child1_code_hash, b"salt1".to_vec(), 10_000),
                        // init is skipped (duplicate), but handle message is sent and executed (+1 dequeued, +1 dispatched)
                        (child1_code_hash, b"salt1".to_vec(), 10_000),
                    ]
                ),
                CreateProgram::Custom(
                    vec![
                        // init is skipped (duplicate), but handle message is sent and executed (+1 dequeued, +1 dispatched)
                        (child1_code_hash, b"salt1".to_vec(), 10_000), 
                    ]
                ),
            ],
            Some(Constructable(vec![child1_code_kind])),
            (4, 3, 1)
            )
        ),
    ];

    let create_program_test = |test: TestData| {
        let (payloads, populate_code_data, expected_data) = test;
        let (children_messages_dequeued, children_messages_dispatched, children_programs_inits) =
            expected_data;

        let mut next_block = 2;
        let mut other_messages_dequeued = 0;
        let mut other_messages_dispatched = 0;
        let mut other_program_inits = 0;

        if let Some(wasm_kind) = populate_code_data {
            match wasm_kind {
                Constructable(code_kinds) => {
                    // By that we save code/code hash to the storage
                    for code_kind in code_kinds {
                        assert_ok!(submit_program_default(USER_2, code_kind));

                        run_to_block(next_block, None);
                        next_block += 1;

                        other_program_inits += 1;
                        other_messages_dequeued += 1;
                    }
                }
                // non constructable code
                NonConstructable(code_kind) => {
                    let code = code_kind.to_bytes();
                    let code_hash = sp_io::hashing::blake2_256(&code).into();
                    assert_noop!(
                        GearPallet::<Test>::submit_code(Origin::signed(USER_2), code),
                        Error::<Test>::FailedToConstructProgram,
                    );
                    let saved_code = common::get_code(code_hash);
                    assert!(saved_code.is_none());
                }
            };
        }

        // Creating factory
        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_2).into(),
            factory_code.to_vec(),
            DEFAULT_SALT.to_vec(),
            EMPTY_PAYLOAD.to_vec(),
            10_000_000,
            0,
        ));

        run_to_block(next_block, None);
        next_block += 1;

        other_program_inits += 1;
        other_messages_dequeued += 1;

        let mut senders = [USER_1, USER_2, USER_3].iter().cycle().copied();
        for payload in payloads {
            // Trying to create children
            let current_sender = senders
                .next()
                .expect("iterator is not cycled and not empty");

            assert_ok!(GearPallet::<Test>::send_message(
                Origin::signed(current_sender).into(),
                factory_id,
                payload.encode(),
                99_000_000,
                0,
            ));

            run_to_block(next_block, None);
            next_block += 1;

            if let CreateProgram::Default(false) = payload {
                // Such payloads generate `handle_reply` messages
                let message_id = {
                    let nonce_before_reply_send = common::get_program(factory_id)
                        .map(|prog| prog.nonce - 1)
                        .expect("program was initialized");
                    compute_program_message_id(factory_id.as_bytes(), nonce_before_reply_send)
                };
                assert_ok!(GearPallet::<Test>::claim_value_from_mailbox(
                    Origin::signed(current_sender).into(),
                    message_id
                ));
            }

            other_messages_dispatched += 1;
            other_messages_dequeued += 1;
        }

        // Checks
        let expected_dequeued = children_messages_dequeued + other_messages_dequeued;
        let expected_dispatched = children_messages_dispatched + other_messages_dispatched;
        let expected_children_amount = children_programs_inits + other_program_inits;

        let mut actual_dequeued = 0;
        let mut actual_dispatched = 0;
        let mut actual_children_amount = 0;
        SystemPallet::<Test>::events()
            .iter()
            .for_each(|e| match e.event {
                MockEvent::Gear(Event::InitSuccess(_)) => actual_children_amount += 1,
                MockEvent::Gear(Event::MessagesDequeued(num)) => actual_dequeued += num,
                MockEvent::Gear(Event::MessageDispatched(_)) => actual_dispatched += 1,
                _ => {}
            });

        assert_eq!(expected_dequeued, actual_dequeued);
        assert_eq!(expected_dispatched, actual_dispatched);
        assert_eq!(expected_children_amount, actual_children_amount);
    };

    for (description, test) in tests {
        init_logger();
        log::debug!("New test: {:?}\n", description);
        new_test_ext().execute_with(|| {
            create_program_test(test);
        });
    }
}

// todo [sab] test create child with wait in init
// todo [sab] tests for a new logic with balance transfers
// todo [sab] test when dispatch (handle/handle_reply) in queue before init

#[test]
fn messages_to_uninitialized_program_wait() {
    use tests_init_wait::WASM_BINARY_BLOATY;

    init_logger();
    new_test_ext().execute_with(|| {
        System::reset_events();

        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(1).into(),
            WASM_BINARY_BLOATY.expect("Wasm binary missing!").to_vec(),
            vec![],
            Vec::new(),
            50_000_000u64,
            0u128
        ));

        let event = match SystemPallet::<Test>::events()
            .last()
            .map(|r| r.event.clone())
        {
            Some(MockEvent::Gear(e)) => e,
            _ => unreachable!("Should be one Gear event"),
        };

        let MessageInfo { program_id, .. } = match event {
            Event::InitMessageEnqueued(info) => info,
            _ => unreachable!("expect Event::InitMessageEnqueued"),
        };

        assert!(!Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        run_to_block(2, None);

        assert!(!Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(1).into(),
            program_id,
            vec![],
            10_000u64,
            0u128
        ));

        run_to_block(3, None);

        assert_eq!(common::waiting_init_take_messages(program_id).len(), 1);
    })
}

#[test]
fn uninitialized_program_should_accept_replies() {
    use tests_init_wait::WASM_BINARY_BLOATY;

    init_logger();
    new_test_ext().execute_with(|| {
        System::reset_events();

        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            WASM_BINARY_BLOATY.expect("Wasm binary missing!").to_vec(),
            vec![],
            Vec::new(),
            99_000_000u64,
            0u128
        ));

        let event = match SystemPallet::<Test>::events()
            .last()
            .map(|r| r.event.clone())
        {
            Some(MockEvent::Gear(e)) => e,
            _ => unreachable!("Should be one Gear event"),
        };

        let MessageInfo { program_id, .. } = match event {
            Event::InitMessageEnqueued(info) => info,
            _ => unreachable!("expect Event::InitMessageEnqueued"),
        };

        assert!(!Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        run_to_block(2, None);

        // there should be one message for the program author
        let mailbox = Gear::mailbox(USER_1);
        assert!(mailbox.is_some());

        let mailbox = mailbox.unwrap();
        let mut keys = mailbox.keys();

        let message_id = keys.next();
        assert!(message_id.is_some());
        let message_id = message_id.unwrap();

        assert!(keys.next().is_none());

        assert_ok!(GearPallet::<Test>::send_reply(
            Origin::signed(USER_1).into(),
            *message_id,
            b"PONG".to_vec(),
            50_000_000u64,
            0,
        ));

        run_to_block(3, None);

        assert!(Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));
    })
}

#[test]
fn defer_program_initialization() {
    use tests_init_wait::WASM_BINARY_BLOATY;

    init_logger();
    new_test_ext().execute_with(|| {
        System::reset_events();

        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            WASM_BINARY_BLOATY.expect("Wasm binary missing!").to_vec(),
            vec![],
            Vec::new(),
            99_000_000u64,
            0u128
        ));

        let event = match SystemPallet::<Test>::events()
            .last()
            .map(|r| r.event.clone())
        {
            Some(MockEvent::Gear(e)) => e,
            _ => unreachable!("Should be one Gear event"),
        };

        let MessageInfo { program_id, .. } = match event {
            Event::InitMessageEnqueued(info) => info,
            _ => unreachable!("expect Event::InitMessageEnqueued"),
        };

        run_to_block(2, None);

        let mailbox = Gear::mailbox(USER_1).expect("should be one message for the program author");
        let mut keys = mailbox.keys();

        let message_id = keys.next().expect("message keys cannot be empty");

        assert_ok!(GearPallet::<Test>::send_reply(
            Origin::signed(USER_1).into(),
            *message_id,
            b"PONG".to_vec(),
            50_000_000u64,
            0,
        ));

        run_to_block(3, None);

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            program_id,
            vec![],
            30_000_000u64,
            0u128
        ));

        run_to_block(4, None);

        assert_eq!(
            Gear::mailbox(USER_1)
                .expect("should be one reply for the program author")
                .into_values()
                .count(),
            1
        );

        let message = Gear::mailbox(USER_1)
            .expect("should be one reply for the program author")
            .into_values()
            .next();
        assert!(message.is_some());

        assert_eq!(message.unwrap().payload, b"Hello, world!".encode());
    })
}

#[test]
fn wake_messages_after_program_inited() {
    use tests_init_wait::WASM_BINARY_BLOATY;

    init_logger();
    new_test_ext().execute_with(|| {
        System::reset_events();

        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            WASM_BINARY_BLOATY.expect("Wasm binary missing!").to_vec(),
            vec![],
            Vec::new(),
            99_000_000u64,
            0u128
        ));

        let event = match SystemPallet::<Test>::events()
            .last()
            .map(|r| r.event.clone())
        {
            Some(MockEvent::Gear(e)) => e,
            _ => unreachable!("Should be one Gear event"),
        };

        let MessageInfo { program_id, .. } = match event {
            Event::InitMessageEnqueued(info) => info,
            _ => unreachable!("expect Event::InitMessageEnqueued"),
        };

        run_to_block(2, None);

        // While program is not inited all messages addressed to it are waiting.
        // There could be dozens of them.
        let n = 10;
        for _ in 0..n {
            assert_ok!(GearPallet::<Test>::send_message(
                Origin::signed(USER_3).into(),
                program_id,
                vec![],
                25_000_000u64,
                0u128
            ));
        }

        run_to_block(3, None);

        let message_id = Gear::mailbox(USER_1).and_then(|t| {
            let mut keys = t.keys();
            keys.next().cloned()
        });
        assert!(message_id.is_some());

        assert_ok!(GearPallet::<Test>::send_reply(
            Origin::signed(USER_1).into(),
            message_id.unwrap(),
            b"PONG".to_vec(),
            50_000_000u64,
            0,
        ));

        run_to_block(20, None);

        let actual_n = Gear::mailbox(USER_3)
            .map(|t| {
                t.into_values().fold(0usize, |i, m| {
                    assert_eq!(m.payload, b"Hello, world!".encode());
                    i + 1
                })
            })
            .unwrap_or(0);

        assert_eq!(actual_n, n);
    })
}

#[test]
fn exit_init() {
    use tests_exit_init::WASM_BINARY_BLOATY;

    init_logger();
    new_test_ext().execute_with(|| {
        System::reset_events();

        let code = WASM_BINARY_BLOATY.expect("Wasm binary missing!").to_vec();
        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            code.clone(),
            vec![],
            Vec::new(),
            10_000_000u64,
            0u128
        ));

        let program_id = utils::get_last_program_id();

        run_to_block(2, None);

        assert!(!Gear::is_failed(program_id));
        assert!(!Gear::is_initialized(program_id));

        let actual_n = Gear::mailbox(USER_1)
            .map(|t| t.into_values().fold(0usize, |i, _| i + 1))
            .unwrap_or(0);

        assert_eq!(actual_n, 0);

        // Program is removed and can be submitted again
        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            code,
            vec![],
            Vec::new(),
            10_000_000u64,
            0u128
        ));
    })
}

#[test]
fn exit_handle() {
    use tests_exit_handle::WASM_BINARY_BLOATY;

    init_logger();
    new_test_ext().execute_with(|| {
        System::reset_events();

        let code = WASM_BINARY_BLOATY.expect("Wasm binary missing!").to_vec();
        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            code.clone(),
            vec![],
            Vec::new(),
            10_000_000u64,
            0u128
        ));

        let program_id = utils::get_last_program_id();

        run_to_block(2, None);

        assert!(Gear::is_initialized(program_id));

        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(USER_1).into(),
            program_id,
            vec![],
            10_000_000u64,
            0u128
        ));

        run_to_block(3, None);

        assert!(!Gear::is_failed(program_id));

        let actual_n = Gear::mailbox(USER_1)
            .map(|t| t.into_values().fold(0usize, |i, _| i + 1))
            .unwrap_or(0);

        assert_eq!(actual_n, 0);

        assert!(!Gear::is_initialized(program_id));
        assert!(!Gear::is_failed(program_id));

        // Program is removed and can be submitted again
        assert_ok!(GearPallet::<Test>::submit_program(
            Origin::signed(USER_1).into(),
            code,
            vec![],
            Vec::new(),
            10_000_000u64,
            0u128
        ));
    })
}

mod utils {
    use codec::Encode;
    use frame_support::dispatch::{DispatchErrorWithPostInfo, DispatchResultWithPostInfo};
    use sp_core::H256;

    use super::{
        assert_ok, pallet, run_to_block, Event, GearPallet, Mailbox, MessageInfo, MockEvent,
        Origin, SystemPallet, Test,
    };

    pub(super) const DEFAULT_GAS_LIMIT: u64 = 10_000;
    pub(super) const DEFAULT_SALT: &'static [u8; 4] = b"salt";
    pub(super) const EMPTY_PAYLOAD: &'static [u8; 0] = b"";

    pub(super) type DispatchCustomResult<T> = Result<T, DispatchErrorWithPostInfo>;
    pub(super) type AccountId = <Test as frame_system::Config>::AccountId;
    pub(super) type GasConverter = <Test as pallet::Config>::GasConverter;
    type BlockNumber = <Test as frame_system::Config>::BlockNumber;

    pub(super) fn init_logger() {
        let _ = env_logger::Builder::from_default_env()
            .format_module_path(false)
            .format_level(true)
            .try_init();
    }

    // Creates a new program and puts message from program to `user` in mailbox
    // using extrinsic calls. Imitates real-world sequence of calls.
    //
    // *NOTE*:
    // 1) usually called inside first block
    // 2) runs to block 2 all the messages place to message queue/storage
    //
    // Returns id of the message in the mailbox
    pub(super) fn setup_mailbox_test_state(user: AccountId) -> H256 {
        let prog_id = {
            let res = submit_program_default(user, ProgramCodeKind::OutgoingWithValueInHandle);
            assert_ok!(res);
            res.expect("submit result was asserted")
        };
        populate_mailbox_from_program(prog_id, user, user, 2, 0, 20_000_000, 0)
    }

    // Puts message from `prog_id` for the `user` in mailbox and returns its id
    pub(super) fn populate_mailbox_from_program(
        prog_id: H256,
        sender: AccountId,
        claimer: AccountId,
        block_num: BlockNumber,
        program_nonce: u64,
        gas_limit: u64,
        value: u128,
    ) -> H256 {
        assert_ok!(GearPallet::<Test>::send_message(
            Origin::signed(sender).into(),
            prog_id,
            Vec::new(),
            gas_limit, // `prog_id` program sends message in handle which sets gas limit to 10_000_000.
            value,
        ));
        run_to_block(block_num, None);

        {
            let expected_code = ProgramCodeKind::OutgoingWithValueInHandle.to_bytes();
            assert_eq!(
                common::get_program(prog_id)
                    .expect("program must exist")
                    .code_hash,
                sp_io::hashing::blake2_256(&expected_code).into(),
                "can invoke send to mailbox only from `ProgramCodeKind::OutgoingWithValueInHandle` program"
            );
        }

        assert!(Mailbox::<Test>::contains_key(claimer));

        compute_program_message_id(prog_id.as_bytes(), program_nonce)
    }

    // Submits program with default options (salt, gas limit, value, payload)
    pub(super) fn submit_program_default(
        user: AccountId,
        code_kind: ProgramCodeKind,
    ) -> DispatchCustomResult<H256> {
        let code = code_kind.to_bytes();
        let salt = DEFAULT_SALT.to_vec();
        // alternatively, get from last event
        let prog_id = generate_program_id(&code, &salt);
        GearPallet::<Test>::submit_program(
            Origin::signed(user).into(),
            code,
            salt,
            EMPTY_PAYLOAD.to_vec(),
            DEFAULT_GAS_LIMIT,
            0,
        )
        .map(|_| prog_id)
    }

    pub(super) fn generate_program_id(code: &[u8], salt: &[u8]) -> H256 {
        let code_hash = sp_io::hashing::blake2_256(code);
        let mut data = Vec::with_capacity(code_hash.len() + salt.len());

        code_hash.encode_to(&mut data);
        salt.encode_to(&mut data);

        sp_io::hashing::blake2_256(&data).into()
    }

    pub(super) fn send_default_message(from: AccountId, to: H256) -> DispatchResultWithPostInfo {
        GearPallet::<Test>::send_message(
            Origin::signed(from).into(),
            to,
            EMPTY_PAYLOAD.to_vec(),
            DEFAULT_GAS_LIMIT,
            0,
        )
    }

    pub(super) fn compute_user_message_id(payload: &[u8], global_nonce: u128) -> H256 {
        let mut id = payload.encode();
        id.extend_from_slice(&global_nonce.to_le_bytes());
        sp_io::hashing::blake2_256(&id).into()
    }

    pub(super) fn compute_program_message_id(program_id: &[u8], program_nonce: u64) -> H256 {
        let mut id = program_id.to_vec();
        id.extend_from_slice(&program_nonce.to_le_bytes());
        sp_io::hashing::blake2_256(&id).into()
    }

    pub(super) fn get_last_program_id() -> H256 {
        let event = match SystemPallet::<Test>::events()
            .last()
            .map(|r| r.event.clone())
        {
            Some(MockEvent::Gear(e)) => e,
            _ => unreachable!("Should be one Gear event"),
        };

        let MessageInfo { program_id, .. } = match event {
            Event::InitMessageEnqueued(info) => info,
            _ => unreachable!("expect Event::InitMessageEnqueued"),
        };

        program_id
    }

    #[derive(Debug, Copy, Clone)]
    pub(super) enum ProgramCodeKind<'a> {
        Default,
        Custom(&'a str),
        GreedyInit,
        OutgoingWithValueInHandle,
    }

    impl<'a> ProgramCodeKind<'a> {
        pub(super) fn to_bytes(self) -> Vec<u8> {
            let source = match self {
                ProgramCodeKind::Default => {
                    r#"
                    (module
                        (import "env" "memory" (memory 1))
                        (export "handle" (func $handle))
                        (export "init" (func $init))
                        (func $handle)
                        (func $init)
                    )"#
                }
                ProgramCodeKind::GreedyInit => {
                    // Initialization function for that program requires a lot of gas.
                    // So, providing `DEFAULT_GAS_LIMIT` will end up processing with
                    // "Gas limit exceeded" execution outcome error message.
                    r#"
                    (module
                        (import "env" "memory" (memory 1))
                        (export "init" (func $init))
                        (func $doWork (param $size i32)
                            (local $counter i32)
                            i32.const 0
                            set_local $counter
                            loop $while
                                get_local $counter
                                i32.const 1
                                i32.add
                                set_local $counter
                                get_local $counter
                                get_local $size
                                i32.lt_s
                                if
                                    br $while
                                end
                            end $while
                        )
                        (func $init
                            i32.const 4
                            call $doWork
                        )
                    )"#
                }
                ProgramCodeKind::OutgoingWithValueInHandle => {
                    // Sending message to USER_1 is hardcoded!
                    // Program sends message in handle which sets gas limit to 10_000_000 and value to 1000.
                    // [warning] - program payload data is inaccurate, don't make assumptions about it!
                    r#"
                    (module
                        (import "env" "gr_send" (func $send (param i32 i32 i32 i64 i32 i32)))
                        (import "env" "gr_source" (func $gr_source (param i32)))
                        (import "env" "memory" (memory 1))
                        (export "handle" (func $handle))
                        (export "init" (func $init))
                        (export "handle_reply" (func $handle_reply))
                        (func $handle
                            (local $msg_source i32)
                            (local $msg_val i32)
                            (i32.store offset=2
                                (get_local $msg_source)
                                (i32.const 1)
                            )
                            (i32.store offset=10
                                (get_local $msg_val)
                                (i32.const 1000)
                            )
                            (call $send (i32.const 2) (i32.const 0) (i32.const 32) (i64.const 10000000) (i32.const 10) (i32.const 40000))
                        )
                        (func $handle_reply)
                        (func $init)
                    )"#
                }
                ProgramCodeKind::Custom(code) => code,
            };

            wabt::Wat2Wasm::new()
                .validate(false)
                .convert(source)
                .expect("failed to parse module")
                .as_ref()
                .to_vec()
        }
    }
}
