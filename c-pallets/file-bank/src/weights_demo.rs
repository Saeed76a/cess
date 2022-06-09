// This file is part of Substrate.

// Copyright (C) 2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Autogenerated weights for pallet_file_bank
//!
//! THIS FILE WAS AUTO-GENERATED USING THE SUBSTRATE BENCHMARK CLI VERSION 4.0.0-dev
//! DATE: 2022-06-09, STEPS: `50`, REPEAT: 20, LOW RANGE: `[]`, HIGH RANGE: `[]`
//! EXECUTION: Some(Wasm), WASM-EXECUTION: Compiled, CHAIN: Some("cess-staking-testnet"), DB CACHE: 1024

// Executed Command:
// ./target/release/cess-node
// benchmark
// --chain
// cess-staking-testnet
// --execution=wasm
// --wasm-execution=compiled
// --pallet
// pallet_file_bank
// --extrinsic
// *
// --steps
// 50
// --repeat
// 20
// --template=./.maintain/frame-weight-template.hbs
// --output=./c-pallets/file-bank/src/weights_demo.rs

#![cfg_attr(rustfmt, rustfmt_skip)]
#![allow(unused_parens)]
#![allow(unused_imports)]

use frame_support::{traits::Get, weights::{Weight, constants::RocksDbWeight}};
use sp_std::marker::PhantomData;

/// Weight functions needed for pallet_file_bank.
pub trait WeightInfo {
	fn upload() -> Weight;
	fn upload_filler(v: u32, ) -> Weight;
}

/// Weights for pallet_file_bank using the Substrate node and recommended hardware.
pub struct SubstrateWeight<T>(PhantomData<T>);
impl<T: frame_system::Config> WeightInfo for SubstrateWeight<T> {
	// Storage: FileBank UserHoldSpaceDetails (r:1 w:1)
	// Storage: FileBank File (r:1 w:1)
	// Storage: FileBank UserHoldFileList (r:1 w:1)
	// Storage: FileBank Invoice (r:0 w:1)
	fn upload() -> Weight {
		(51_999_000 as Weight)
			.saturating_add(T::DbWeight::get().reads(3 as Weight))
			.saturating_add(T::DbWeight::get().writes(4 as Weight))
	}
	// Storage: FileMap SchedulerMap (r:1 w:0)
	// Storage: Sminer MinerDetails (r:1 w:0)
	// Storage: FileBank FillerMap (r:0 w:2)
	fn upload_filler(v: u32, ) -> Weight {
		(43_198_000 as Weight)
			// Standard Error: 9_000
			.saturating_add((2_568_000 as Weight).saturating_mul(v as Weight))
			.saturating_add(T::DbWeight::get().reads(2 as Weight))
			.saturating_add(T::DbWeight::get().writes((1 as Weight).saturating_mul(v as Weight)))
	}
}

// For backwards compatibility and tests
impl WeightInfo for () {
	// Storage: FileBank UserHoldSpaceDetails (r:1 w:1)
	// Storage: FileBank File (r:1 w:1)
	// Storage: FileBank UserHoldFileList (r:1 w:1)
	// Storage: FileBank Invoice (r:0 w:1)
	fn upload() -> Weight {
		(51_999_000 as Weight)
			.saturating_add(RocksDbWeight::get().reads(3 as Weight))
			.saturating_add(RocksDbWeight::get().writes(4 as Weight))
	}
	// Storage: FileMap SchedulerMap (r:1 w:0)
	// Storage: Sminer MinerDetails (r:1 w:0)
	// Storage: FileBank FillerMap (r:0 w:2)
	fn upload_filler(v: u32, ) -> Weight {
		(43_198_000 as Weight)
			// Standard Error: 9_000
			.saturating_add((2_568_000 as Weight).saturating_mul(v as Weight))
			.saturating_add(RocksDbWeight::get().reads(2 as Weight))
			.saturating_add(RocksDbWeight::get().writes((1 as Weight).saturating_mul(v as Weight)))
	}
}