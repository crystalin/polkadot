// Copyright 2021 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::*;
use crate::inclusion::CandidatePendingAvailability;
use bitvec::{order::Lsb0 as BitOrderLsb0, vec::BitVec};
use frame_benchmarking::{account, benchmarks, impl_benchmark_test_suite};
use frame_system::RawOrigin;
use primitives::v1::{
	collator_signature_payload, AvailabilityBitfield, CandidateCommitments, CandidateDescriptor,
	CandidateHash, CollatorId, CommittedCandidateReceipt, CompactStatement, CoreIndex,
	CoreOccupied, DisputeStatement, DisputeStatementSet, GroupIndex, HeadData, Id as ParaId,
	InvalidDisputeStatementKind, PersistedValidationData, Signed, SignedStatement, SigningContext,
	UncheckedSigned, ValidatorId, ValidatorIndex, ValidityAttestation,
};
use sp_core::{crypto::CryptoType, Pair, H256};
use sp_runtime::traits::{One, Zero};
use std::{collections::HashMap, convert::TryInto};

// Brainstorming worst case aspects:
//
// - there are many fresh disputes, where the disputes have just been initiated.
// - create a new `DisputeState` with blank bitfields.
// - make sure spam slotes is incremented by have DisputeStatementSet U DisputeState < byzantize_thresh
// - force one side to have a super majority, so we enable slashing

fn run_to_block<T: Config>(to: u32) {
	let to = to.into();
	while frame_system::Pallet::<T>::block_number() < to {
		let b = frame_system::Pallet::<T>::block_number();
		crate::initializer::Pallet::<T>::on_finalize(b);

		let b = b + One::one();
		frame_system::Pallet::<T>::set_block_number(b);
		crate::initializer::Pallet::<T>::on_initialize(b);
	}
}

fn candidate_availability_mock<T: Config>(
	seed: u32,
	candidate_hash: CandidateHash,
	availability_votes: BitVec<BitOrderLsb0, u8>,
) -> CandidatePendingAvailability<T::Hash, T::BlockNumber> {
	// TODO can make benchmark gated function for making this struct inclusion so fields don't
	// need to be pub(crate)
	CandidatePendingAvailability::<T::Hash, T::BlockNumber> {
		core: seed.into(), // CoreIndex - we need these to correspond to freed cores
		hash: candidate_hash,
		descriptor: Default::default(),
		availability_votes,
		backers: Default::default(),
		relay_parent_number: Zero::zero(),
		backed_in_number: One::one(),
		backing_group: seed.into(),
	}
}

fn availability_bitvec<T: Config>(cores: u32, concluding: Vec<u32>) -> AvailabilityBitfield {
	let mut bitfields = bitvec::bitvec![bitvec::order::Lsb0, u8; 0; 0];
	for i in 0..cores {
		// the first `availability` cores are marked as available
		if concluding.contains(&(i as u32)) {
			bitfields.push(true);

		// make sure the core is occupied so the lookup works
		// crate::scheduler::AvailabilityCores::<T>::mutate(|cores| {
		// 	cores[i as usize] = Some(CoreOccupied::Parachain);
		// });

		// fill corresponding storage items for inclusion
		} else {
			bitfields.push(false)
		}

		crate::scheduler::AvailabilityCores::<T>::mutate(|cores| {
			// TODO this could be be one op where we set all the entries
			cores[i as usize] = Some(CoreOccupied::Parachain)
		});
	}

	bitfields.into()
}

fn add_availability<T: Config>(
	seed: u32,
	availability_votes: BitVec<BitOrderLsb0, u8>,
	candidate_hash: CandidateHash,
) -> CandidateHash {
	let candidate_availability =
		candidate_availability_mock::<T>(seed, candidate_hash, availability_votes);
	// TODO notes: commitments does not include any data that would lead to heavy code
	// paths in `enact_candidate`. But enact_candidates does return a weight so maybe
	// that should be used. (Relevant for when bitfields indicate a candidate is available)
	let commitments = CandidateCommitments::<u32>::default();
	let para_id = ParaId::from(seed);
	crate::inclusion::PendingAvailability::<T>::insert(para_id, candidate_availability);
	crate::inclusion::PendingAvailabilityCommitments::<T>::insert(&para_id, commitments);

	candidate_hash
}

benchmarks! {
	enter_max_disputed {
		let config = crate::configuration::Pallet::<T>::config();
		let max_validators = config.max_validators.unwrap_or(200);
		// let max_validators = 20;
		let validators_per_core = config.max_validators_per_core.unwrap_or(5);
		let max_cores = max_validators / validators_per_core;
		let max_candidates = max_cores; // assuming we can only have 1 candidate per core. TODO check if this is ok
		let max_statements = max_validators;
		let byzantine_statement_thresh = max_statements / 3;
		// let disputed = max_candidates / 3; // half of candidates are disputed.
		let concluding = 5;
		let backed = 1;
		let disputed = max_candidates - concluding - backed;

		// we need to make sure every backed candidate has a free core
		assert!(concluding >= backed);

		let header = T::Header::new(
			One::one(),			// number
			Default::default(), // extrinsics_root,
			Default::default(), // storage_root,
			Default::default(), // parent_hash,
			Default::default(), // digest,

		);

		// make sure parachains exist prior to session change.
		for i in 0..max_cores {
			let para_id = ParaId::from(i as u32);
			crate::paras::Parachains::<T>::append(para_id);
		}
		assert_eq!(crate::paras::Parachains::<T>::get().iter().count(), max_cores as usize);


		let validator_pairs = (0..max_validators).map(|i| {
			let (pair, seed) = <ValidatorId as CryptoType>::Pair::generate();
			let account: T::AccountId = account("validator", i, i);
			(account, pair)
		}).collect::<Vec<_>>();

		// create map of validator public id => signing pair.
		let validator_map = validator_pairs
			.iter()
			.map(|(_, pair)| (pair.public(), pair.clone()))
			.collect::<HashMap<_,_>>();

		let validators_public = validator_pairs.iter().map(|(a, v)| (a, v.public()));

		// initialize session 1.
		crate::initializer::Pallet::<T>::test_trigger_on_new_session(
			true, // indicate the validator set has changed
			1, // session index
			validators_public.clone(), // validators
			None, // queued - when this is None validators are considered queued
		);


		run_to_block::<T>(2);
		frame_system::Pallet::<T>::set_parent_hash(header.hash());

		// setup at session change.
		assert_eq!(crate::scheduler::AvailabilityCores::<T>::get().len(), max_cores as usize);
		assert_eq!(crate::scheduler::ValidatorGroups::<T>::get().len(), max_cores as usize);

		// let mut validate_group_map = HashMap::<_, _>::new();
		// let validator_groups = crate::scheduler::ValidatorGroups::<T>::get();
		// for (i, group) in validator_groups.iter().enumerate() {
		// 	for validator_index in group {
		// 		validate_group_map.insert(validate_group_map as u32, group);
		// 	}
		// }

		// assert the current session is 0.
		let current_session = 1;
		assert_eq!(<crate::shared::Pallet<T>>::session_index(), current_session);

		// get validators from session info. We need to refetch them since they have been shuffled.
		let validators_shuffled = crate::session_info::Pallet::<T>::session_info(current_session)
			.unwrap()
			.validators
			.clone()
			.into_iter()
			.map(|public| {
				let pair = validator_map.get(&public).unwrap().clone();
				(public, pair)
			})
			.collect::<Vec<_>>();

		// This is not technically correct because we are saying every validator has voted a core available, but just doing for simplicity.
		let validator_availability_votes_yes = bitvec::bitvec![bitvec::order::Lsb0, u8; 1; max_validators as usize];
		let validator_availability_votes_no = bitvec::bitvec![bitvec::order::Lsb0, u8; 0; max_validators as usize];

		let signing_context = SigningContext { parent_hash: header.hash(), session_index: 1 };

		let backed_candidates = (0..backed).map(|seed| {
			let para_id = ParaId::from(seed + concluding);
			let core_idx = CoreIndex(seed + concluding);
			let group_idx = crate::scheduler::Pallet::<T>::group_assigned_to_core(
				core_idx,
				2u32.into()
			)
			.unwrap();

			let collator_pair = <CollatorId as CryptoType>::Pair::generate().0;
			let relay_parent = header.hash();
			let head_data: HeadData = Default::default();
			let persisted_validation_data_hash = PersistedValidationData::<H256> {
				parent_head: head_data.clone(), // dummy parent_head
				relay_parent_number: (*header.number()).try_into().map_err(|_| ()).expect("header.number defined above"),
				relay_parent_storage_root: Default::default(), // equivalent to header.storage_root,
				max_pov_size: config.max_pov_size,
			}
			.hash();

			let pov_hash = Default::default();
			let validation_code_hash = Default::default();
			let signature = collator_pair.sign(&collator_signature_payload(
				&relay_parent,
				&para_id,
				&persisted_validation_data_hash,
				&pov_hash,
				&validation_code_hash,
			));

			// set the headdata
			crate::paras::Heads::<T>::insert(&para_id, head_data.clone());

			// setup scheduled cores to go with backed candidate
			crate::scheduler::Scheduled::<T>::append(crate::scheduler::CoreAssignment {
				core: core_idx,
				para_id,
				kind: crate::scheduler::AssignmentKind::Parachain,
				group_idx: group_idx,
			});

			// crate::scheduler::AvailabilityCores::<T>::mutate(|cores| {
			// 	// TODO this could be be one op where we set all the entries
			// 	cores[core_idx.0 as usize] = Some(CoreOccupied::Parachain)
			// });

			let mut past_code_meta = crate::paras::ParaPastCodeMeta::<T::BlockNumber>::default();
			past_code_meta.note_replacement(0u32.into(), 0u32.into());
			// Insert ParaPastCodeMeta into `PastCodeMeta` for this para_id
			crate::paras::PastCodeMeta::<T>::insert(&para_id, past_code_meta);
			crate::paras::CurrentCodeHash::<T>::insert(&para_id, validation_code_hash.clone());

			let group_validators = crate::scheduler::Pallet::<T>::group_validators(
				group_idx
			).unwrap();


			let candidate = CommittedCandidateReceipt::<T::Hash> {
				descriptor: CandidateDescriptor::<T::Hash> {
					para_id: para_id,
					relay_parent: relay_parent,
					collator: collator_pair.public(),
					persisted_validation_data_hash: persisted_validation_data_hash,
					pov_hash: pov_hash,
					erasure_root: Default::default(),
					signature: signature,
					para_head: head_data.hash(),
					validation_code_hash: validation_code_hash,
				},
				commitments: CandidateCommitments::<u32> {
					upward_messages: Vec::new(),
					horizontal_messages: Vec::new(),
					new_validation_code: None,
					head_data: head_data, // HeadData
					processed_downward_messages: 0,
					hrmp_watermark: 1u32,
				},
			};

			// let candidate_hash = add_availability::<T>(
			// 	seed,
			// 	// We don't want this backed candidate to immediatley become available
			// 	validator_availability_votes_no.clone(),
			// 	candidate.hash()
			// );
			let candidate_hash = candidate.hash();

			let validity_votes: Vec<ValidityAttestation> =  group_validators.iter().map(|val_idx| {
				let (public, _) = validators_shuffled.get(val_idx.0 as usize).unwrap();
				println!("validator index {:?}", val_idx.0);

				{
					// sanity check we are correctly getting keys
					let temp = crate::shared::ActiveValidatorKeys::<T>::get();
					let public_check = temp.get(val_idx.0 as usize).unwrap();
					assert_eq!(public_check, public);
				}

				let pair = validator_map.get(public).unwrap();
				let sig = UncheckedSigned::<CompactStatement>::benchmark_sign(
					pair,
					CompactStatement::Valid(candidate_hash.clone()),
					&signing_context,
					*val_idx
				)
				.benchmark_signature();

				println!("candidate hash {}", candidate_hash);
				println!("validator id signing {:?}", public);
				// println!("signing context A: {:#?}, {:#?}", signing_context.parent_hash, signing_context.session_index);

				let attestation = ValidityAttestation::Explicit(sig);
				println!("validity attestation {:?}", attestation);

				attestation
			}).collect();

			let backed = BackedCandidate::<T::Hash> {
				candidate: candidate,
				validity_votes: validity_votes,
				validator_indices: bitvec::bitvec![bitvec::order::Lsb0, u8; 1; group_validators.len()],
			};


			backed
		}).collect::<Vec<_>>();


		let concluding_rng = concluding..(concluding + backed);
		let concluding_cores: Vec<_> = concluding_rng.clone().map(|i| i as u32).collect();
		let availability_bitvec = availability_bitvec::<T>(max_cores, concluding_cores);
		let bitfields: Vec<_> = validators_shuffled.iter().enumerate().map(|(i, (public, pair))| {
			let unchecked_signed = UncheckedSigned::<AvailabilityBitfield>::benchmark_sign(
				pair,
				availability_bitvec.clone(),
				&signing_context,
				ValidatorIndex(i as u32),
			);

			unchecked_signed
		})
		.collect();

		concluding_rng.for_each(|seed| {
			// make sure the candidates that are concluding by becoming available are marked as
			// pending availability.
			add_availability::<T>(
				seed,
				validator_availability_votes_yes.clone(),
				CandidateHash(H256::from_low_u64_le(seed as u64))
			);

			crate::scheduler::AvailabilityCores::<T>::mutate(|cores| {
				// TODO this could be be one op where we set all the entries
				cores[seed as usize] = Some(CoreOccupied::Parachain)
			});
		});

		// add logic to not dispute backed candidates
		let mut spam_count = 0;
		let disputes = (concluding + backed..max_candidates).map(|seed| {
			// fill corresponding storage items for inclusion that will be `taken` when `collect_disputed`
			// is called.
			let candidate_hash = add_availability::<T>(
				seed,
				validator_availability_votes_yes.clone(),
				CandidateHash(H256::from_low_u64_le(seed as u64))
			);
			// create the set of statements to dispute the above candidate hash.
			let statement_range = if spam_count < config.dispute_max_spam_slots {
				// if we have not hit the spam dispute statement limit, only make up to the byzantine
				// threshold number of statements.

				// TODO: we could max the amount of spam even more by  taking 3 1/3 chunks of
				// validator set and having them each attest to different statements. Right now we
				// just use 1 1/3 chunk.
				0..byzantine_statement_thresh
			} else {
				// otherwise, make the maximum number of statements, which is over the byzantine
				// threshold and thus these statements will not be counted as potential spam.
				0..max_statements
			};
			let statements = statement_range.map(|validator_index| {
				let validator_pair = &validators_shuffled.get(validator_index as usize).unwrap().1;

				let dispute_statement = DisputeStatement::Invalid(InvalidDisputeStatementKind::Explicit);
				let data = dispute_statement.payload_data(candidate_hash.clone(), current_session);
				let statement_sig = validator_pair.sign(&data);

				(
					dispute_statement,
					ValidatorIndex(validator_index),
					statement_sig,
				)
			}).collect::<Vec<_>>();

			if spam_count < config.dispute_max_spam_slots {
				spam_count += 1;
			}

			// return dispute statements with metadata.
			DisputeStatementSet {
				candidate_hash: candidate_hash.clone(),
				session: current_session,
				statements
			}

		}).collect::<Vec<_>>();



		// ensure availability cores are scheduled for backed candidates.
		// assert_eq!(
		// 	crate::scheduler::Scheduled::<T>::get().iter().count(),
		// 	disputed as usize
		// );

		// assert_eq!(
		// 	crate::disputes::SpamSlots::<T>::get(&current_session),
		// 	None
		// );

		// println!("fA");
		// assert_eq!(
		// 	crate::inclusion::PendingAvailabilityCommitments::<T>::iter().count(),
		// 	// (disputed + available) as usize
		// 	(max_candidates) as usize
		// );
		// println!("fB");
		// assert_eq!(
		// 	crate::inclusion::PendingAvailability::<T>::iter().count(),
		// 	// (disputed + available) as usize
		// 	(max_candidates) as usize
		// );

		let data = ParachainsInherentData {
			bitfields: bitfields, // TODO
			// bitfields: Default::default(), // TODO
			backed_candidates: backed_candidates,
			disputes, // Vec<DisputeStatementSet>
			parent_header: header,
		};
		crate::scheduler::Scheduled::<T>::get().iter().for_each(|s| println!("scheduled loop {:?}", s));
	}: enter(RawOrigin::None, data)
	verify {
		// check that the disputes storage has updated as expected.

		let spam_slots = crate::disputes::SpamSlots::<T>::get(&current_session).unwrap();
		// assert!(
		// 	// we expect the first 1/3rd of validators to have maxed out spam slots. Sub 1 for when
		// 	// there is an odd number of validators.
		// 	&spam_slots[..(byzantine_statement_thresh - 1) as usize]
		// 	.iter()
		// 	.all(|n| *n == config.dispute_max_spam_slots)
		// );
		// assert!(
		// 	&spam_slots[byzantine_statement_thresh as usize ..]
		// 	.iter()
		// 	.all(|n| *n == 0)
		// );

		println!("fC");
		// pending availability data is removed when disputes are collected.
		assert_eq!(
			crate::inclusion::PendingAvailabilityCommitments::<T>::iter().count(),
			backed as usize
		);
		assert_eq!(
			crate::inclusion::PendingAvailability::<T>::iter().count(),
			backed as usize
		);

		println!("fD");
		// max possible number of cores have been scheduled.
		assert_eq!(crate::scheduler::Scheduled::<T>::get().iter().count(), (disputed) as usize);

		println!("fG");
		// all cores are occupied by a parachain.
		assert_eq!(
			crate::scheduler::AvailabilityCores::<T>::get().iter().count(), max_cores as usize
		);
	}
}

// - no spam scenario
// - max backed candidates scenario

impl_benchmark_test_suite!(
	Pallet,
	crate::mock::new_test_ext(Default::default()),
	crate::mock::Test
);
