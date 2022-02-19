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

use codec::{Decode, Encode};
use common::{Origin as _, ValueTree};
use frame_support::{
    assert_ok,
    traits::{GenesisBuild, OffchainWorker, OnFinalize, OnIdle, OnInitialize},
    BasicExternalities,
};
use frame_system as system;
use gear_core::{
    ids::{CodeId, ProgramId},
    message::StoredDispatch,
};
use gear_runtime::{
    AuraConfig, Call, Gear, GrandpaConfig, Runtime, Signature, SudoConfig, System,
    TransactionPaymentConfig, UncheckedExtrinsic, Usage,
};
use parking_lot::RwLock;
use primitive_types::H256;
use rand::{rngs::StdRng, RngCore};
use sp_consensus_aura::sr25519::AuthorityId as AuraId;
use sp_consensus_aura::{Slot, AURA_ENGINE_ID};
use sp_core::{
    offchain::{
        testing::{PoolState, TestOffchainExt, TestTransactionPoolExt},
        Duration, OffchainDbExt, OffchainWorkerExt, TransactionPoolExt,
    },
    sr25519, Pair, Public,
};
use sp_finality_grandpa::AuthorityId as GrandpaId;
use sp_io::offchain;
use sp_runtime::{
    traits::{IdentifyAccount, Verify},
    AccountId32, Digest, DigestItem,
};

use sp_std::collections::btree_map::BTreeMap;
use std::sync::Arc;

type GasNodeKeyOf<T> = <<T as pallet_gear::Config>::GasHandler as ValueTree>::Key;
type GasBalanceOf<T> = <<T as pallet_gear::Config>::GasHandler as ValueTree>::Balance;

// Generate a crypto pair from seed.
pub(crate) fn get_from_seed<TPublic: Public>(seed: &str) -> <TPublic::Pair as Pair>::Public {
    TPublic::Pair::from_string(&format!("//{}", seed), None)
        .expect("static values are valid; qed")
        .public()
}

type AccountPublic = <Signature as Verify>::Signer;

// Generate an account ID from seed.
pub(crate) fn get_account_id_from_seed<TPublic: Public>(seed: &str) -> AccountId32
where
    AccountPublic: From<<TPublic::Pair as Pair>::Public>,
{
    AccountPublic::from(get_from_seed::<TPublic>(seed)).into_account()
}

// Generate an Aura authority key.
pub(crate) fn authority_keys_from_seed(s: &str) -> (AccountId32, AuraId, GrandpaId) {
    (
        get_account_id_from_seed::<sr25519::Public>(s),
        get_from_seed::<AuraId>(s),
        get_from_seed::<GrandpaId>(s),
    )
}

pub(crate) fn create_random_accounts(
    rng: &mut StdRng,
    root_acc: &AccountId32,
) -> Vec<(AccountId32, u128)> {
    let initial_accounts_num = 1 + (rng.next_u32() % 1000); // [1..1000]
    let mut accounts = vec![(root_acc.clone(), 1_000_000_000_000_000_u128)];
    for _ in 1..initial_accounts_num {
        let mut acc_id = [0_u8; 32];
        rng.fill_bytes(&mut acc_id);
        let balance = (rng.next_u64() >> 14) as u128; // approx. up to 10^15
        accounts.push((acc_id.into(), balance));
    }
    accounts
}

pub(crate) fn new_test_ext(
    balances: Vec<(impl Into<AccountId32>, u128)>,
    initial_authorities: Vec<(AccountId32, AuraId, GrandpaId)>,
    root_key: AccountId32,
) -> sp_io::TestExternalities {
    let mut t = system::GenesisConfig::default()
        .build_storage::<Runtime>()
        .unwrap();

    pallet_balances::GenesisConfig::<Runtime> {
        balances: balances
            .into_iter()
            .map(|(acc, balance)| (acc.into(), balance))
            .chain(
                initial_authorities
                    .iter()
                    .cloned()
                    .map(|(acc, _, _)| (acc, 1000)),
            )
            .collect(),
    }
    .assimilate_storage(&mut t)
    .unwrap();

    AuraConfig {
        authorities: initial_authorities
            .iter()
            .cloned()
            .map(|(_, x, _)| x)
            .collect(),
    }
    .assimilate_storage(&mut t)
    .unwrap();

    BasicExternalities::execute_with_storage(&mut t, || {
        <GrandpaConfig as GenesisBuild<Runtime>>::build(&GrandpaConfig {
            authorities: initial_authorities
                .iter()
                .map(|x| (x.2.clone(), 1))
                .collect(),
        });
        // pallet_grandpa::Pallet::<Runtime>::initialize(
        //     &initial_authorities
        //         .iter()
        //         .map(|x| (x.2.clone(), 1))
        //         .collect(),
        // );

        <TransactionPaymentConfig as GenesisBuild<Runtime>>::build(
            &TransactionPaymentConfig::default(),
        );
    }); // .unwrap();

    SudoConfig {
        key: Some(root_key),
    }
    .assimilate_storage(&mut t)
    .unwrap();

    let mut ext: sp_io::TestExternalities = t.into(); //= sp_io::TestExternalities::new(t);
    ext.execute_with(|| System::set_block_number(1));
    ext
}

pub(crate) fn with_offchain_ext(
    balances: Vec<(impl Into<AccountId32>, u128)>,
    initial_authorities: Vec<(AccountId32, AuraId, GrandpaId)>,
    root_key: AccountId32,
) -> (sp_io::TestExternalities, Arc<RwLock<PoolState>>) {
    let mut ext = new_test_ext(balances, initial_authorities, root_key);
    let (offchain, _) = TestOffchainExt::new();
    let (pool, pool_state) = TestTransactionPoolExt::new();

    ext.register_extension(OffchainDbExt::new(offchain.clone()));
    ext.register_extension(OffchainWorkerExt::new(offchain));
    ext.register_extension(TransactionPoolExt::new(pool));

    (ext, pool_state)
}

#[allow(dead_code)]
pub(crate) fn run_to_block(n: u32, remaining_weight: Option<u64>) {
    // All blocks are to be authored by validator at index 0
    let slot = Slot::from(0);
    let pre_digest = Digest {
        logs: vec![DigestItem::PreRuntime(AURA_ENGINE_ID, slot.encode())],
    };

    while System::block_number() < n {
        System::on_finalize(System::block_number());
        let new_block_number = System::block_number() + 1;
        System::set_block_number(new_block_number);
        System::initialize(&new_block_number, &System::parent_hash(), &pre_digest);
        System::on_initialize(System::block_number());
        Gear::on_initialize(System::block_number());
        let remaining_weight =
            remaining_weight.unwrap_or_else(<Runtime as pallet_gas::Config>::BlockGasLimit::get);
        Gear::on_idle(System::block_number(), remaining_weight);
    }
}

pub(crate) fn run_to_block_with_ocw(
    n: u32,
    pool: Arc<RwLock<PoolState>>,
    remaining_weight: Option<u64>,
) {
    // All blocks are to be authored by validator at index 0
    let slot = Slot::from(0);
    let pre_digest = Digest {
        logs: vec![DigestItem::PreRuntime(AURA_ENGINE_ID, slot.encode())],
    };

    let now = System::block_number();
    for i in now + 1..=n {
        System::on_finalize(i - 1);
        System::set_block_number(i);
        log::debug!("📦 Processing block {}", i);
        System::initialize(&i, &System::parent_hash(), &pre_digest);
        System::on_initialize(i);
        Gear::on_initialize(i);
        process_tx_pool(pool.clone());
        log::debug!("✅ Done processing transaction pool at block {}", i);
        let remaining_weight =
            remaining_weight.unwrap_or_else(<Runtime as pallet_gas::Config>::BlockGasLimit::get);
        Gear::on_idle(i, remaining_weight);
        increase_offchain_time(1_000);
        Usage::offchain_worker(i);
    }
}

fn increase_offchain_time(ms: u64) {
    offchain::sleep_until(offchain::timestamp().add(Duration::from_millis(ms)));
}

pub(crate) fn init_logger() {
    let _ = env_logger::Builder::from_default_env()
        .format_module_path(false)
        .format_level(true)
        .try_init();
}

pub(crate) fn generate_program_id(code: &[u8], salt: &[u8]) -> H256 {
    ProgramId::generate(CodeId::generate(code), salt).into_origin()
}

pub(crate) fn process_tx_pool(pool: Arc<RwLock<PoolState>>) {
    let mut guard = pool.write();
    guard.transactions.iter().cloned().for_each(|bytes| {
        let tx = UncheckedExtrinsic::decode(&mut &bytes[..]).unwrap();
        if let Call::Usage(pallet_usage::Call::collect_waitlist_rent { payees_list }) = tx.function
        {
            log::debug!(
                "Sending collect_wait_list extrinsic with payees_list {:?}",
                payees_list
            );
            assert_ok!(Usage::collect_waitlist_rent(
                system::RawOrigin::None.into(),
                payees_list
            ));
        }
    });
    guard.transactions = vec![];
}

pub(crate) fn total_gas_in_wait_list() -> u64 {
    // Iterate through the wait list and record the respective gas nodes value limits
    // attributing the latter to the nearest `node_with_value` ID to avoid duplication
    let specified_value_by_node_id: BTreeMap<GasNodeKeyOf<Runtime>, GasBalanceOf<Runtime>> =
        frame_support::storage::PrefixIterator::<(u64, H256)>::new(
            common::STORAGE_WAITLIST_PREFIX.to_vec(),
            common::STORAGE_WAITLIST_PREFIX.to_vec(),
            |_, mut value| {
                let (dispatch, _) = <(StoredDispatch, u32)>::decode(&mut value)?;
                let node = pallet_gas::Pallet::<Runtime>::get_node(dispatch.id().into_origin())
                    .expect("There is always a value node for a valid dispatch ID");
                let node_with_value = node
                    .node_with_value::<Runtime>()
                    .expect("There is always a node with concrete value for a node");
                let value = node_with_value
                    .inner_value()
                    .expect("Node with value must have value");
                Ok((value, node_with_value.id))
            },
        )
        .map(|(gas, node_id)| (node_id, gas))
        .collect();

    specified_value_by_node_id
        .into_iter()
        .fold(0_u64, |acc, (_, val)| acc + val)
}

pub(crate) fn total_reserved_balance() -> u128 {
    // Iterate through the wait list and record the respective gas nodes value limits
    // attributing the latter to the nearest `node_with_value` ID to avoid duplication
    <system::Account<Runtime>>::iter()
        .map(|(_, v)| v.data.reserved)
        .fold(0, |acc, v| acc.saturating_add(v))
}
