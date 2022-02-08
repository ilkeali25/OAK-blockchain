// This file is part of OAK Blockchain.

// Copyright (C) 2021 OAK Network
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

//! # Automation time pallet
//!
//! DISCLAIMER: This pallet is still in it's early stages. At this point
//! we only support scheduling two tasks per minute, and sending an on-chain
//! with a custom message.
//!
//! This pallet allows a user to schedule tasks. Tasks can scheduled for any whole minute in the future.
//! In order to run tasks this pallet consumes up to a certain amount of weight during `on_initialize`.
//!
//! The pallet supports the following tasks:
//! * On-chain events with custom text
//!

#![cfg_attr(not(feature = "std"), no_std)]
pub use pallet::*;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

mod benchmarking;
pub mod weights;

use core::convert::TryInto;
use frame_support::{inherent::Vec, pallet_prelude::*, sp_runtime::traits::Hash, BoundedVec};
use frame_system::pallet_prelude::*;
use pallet_timestamp::{self as timestamp};
use scale_info::TypeInfo;
use sp_runtime::{traits::SaturatedConversion, Perbill};
use sp_std::vec;

pub use weights::WeightInfo;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	type AccountOf<T> = <T as frame_system::Config>::AccountId;
	type UnixTime = u64;

	/// The enum that stores all action specific data.
	#[derive(Debug, Eq, PartialEq, Encode, Decode, TypeInfo)]
	#[scale_info(skip_type_params(T))]
	pub enum Action {
		Notify {
			message: Vec<u8>,
		},
	}

	/// The struct that stores all information needed for a task.
	#[derive(Debug, Eq, PartialEq, Encode, Decode, TypeInfo)]
	#[scale_info(skip_type_params(T))]
	pub struct Task<T: Config> {
		owner_id: AccountOf<T>,
		provided_id: Vec<u8>,
		time: UnixTime,
		action: Action,
	}

	impl<T: Config> Task<T> {
		pub fn create_event_task(
			owner_id: AccountOf<T>,
			provided_id: Vec<u8>,
			time: UnixTime,
			message: Vec<u8>,
		) -> Task<T> {
			let action = Action::Notify { message };
			Task::<T> { owner_id, provided_id, time, action }
		}
	}

	#[derive(Debug, Encode, Decode, TypeInfo)]
	#[scale_info(skip_type_params(T))]
	pub struct TaskHashInput<T: Config> {
		owner_id: AccountOf<T>,
		provided_id: Vec<u8>,
	}

	impl<T: Config> TaskHashInput<T> {
		pub fn create_hash_input(owner_id: AccountOf<T>, provided_id: Vec<u8>) -> TaskHashInput<T> {
			TaskHashInput::<T> { owner_id, provided_id }
		}
	}

	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_timestamp::Config {
		type Event: From<Event<Self>> + IsType<<Self as frame_system::Config>::Event>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;

		/// The maximum number of tasks that can be scheduled for a time slot.
		#[pallet::constant]
		type MaxTasksPerSlot: Get<u32>;

		/// The maximum weight per block.
		#[pallet::constant]
		type MaxBlockWeight: Get<Weight>;

		/// The maximum percentage of weight per block used for scheduled tasks.
		#[pallet::constant]
		type MaxWeightPercentage: Get<Perbill>;

		/// The time each block takes.
		#[pallet::constant]
		type SecondsPerBlock: Get<u64>;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::storage]
	#[pallet::getter(fn get_scheduled_tasks)]
	pub type ScheduledTasks<T: Config> =
		StorageMap<_, Twox64Concat, u64, BoundedVec<T::Hash, T::MaxTasksPerSlot>>;

	#[pallet::storage]
	#[pallet::getter(fn get_task)]
	pub type Tasks<T: Config> = StorageMap<_, Twox64Concat, T::Hash, Task<T>>;

	#[pallet::storage]
	#[pallet::getter(fn get_task_queue)]
	pub type TaskQueue<T: Config> = StorageValue<_, Vec<T::Hash>, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn get_last_slot)]
	pub type LastTimeSlot<T: Config> = StorageValue<_, UnixTime, ValueQuery>;

	#[pallet::error]
	pub enum Error<T> {
		/// Time must end in a whole minute.
		InvalidTime,
		/// Time must be in the future.
		PastTime,
		/// The message cannot be empty.
		EmptyMessage,
		/// The provided_id cannot be empty
		EmptyProvidedId,
		/// There can be no duplicate tasks.
		DuplicateTask,
		/// Time slot is full. No more tasks can be scheduled for this time.
		TimeSlotFull,
		/// You are not the owner of the task.
		NotTaskOwner,
		/// The task does not exist.
		TaskDoesNotExist,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Schedule task success.
		TaskScheduled {
			who: T::AccountId,
			task_id: T::Hash,
		},
		// Cancelled a task.
		TaskCancelled {
			who: T::AccountId,
			task_id: T::Hash,
		},
		/// Notify event for the task.
		Notify {
			message: Vec<u8>,
		},
		/// A Task was not found.
		TaskNotFound {
			task_id: T::Hash,
		},
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_: T::BlockNumber) -> Weight {
			let max_weight: Weight = T::MaxWeightPercentage::get() * T::MaxBlockWeight::get();
			Self::trigger_tasks(max_weight);
			// Until we calculate the weights (ENG-157) we will just assumed we used the max weight.
			max_weight
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Schedule a task to fire an event with a custom message.
		///
		/// Before the task can be scheduled the task must past validation checks.
		/// * The transaction is signed
		/// * The provided_id's length > 0
		/// * The message's length > 0
		/// * The time is valid
		///
		/// # Parameters
		/// * `provided_id`: An id provided by the user. This id must be unique for the user.
		/// * `time`: The unix standard time in seconds for when the task should run.
		/// * `message`: The message you want the event to have.
		///
		/// # Errors
		/// * `InvalidTime`: Time must end in a whole minute.
		/// * `PastTime`: Time must be in the future.
		/// * `EmptyMessage`: The message cannot be empty.
		/// * `DuplicateTask`: There can be no duplicate tasks.
		/// * `TimeSlotFull`: Time slot is full. No more tasks can be scheduled for this time.
		#[pallet::weight(<T as Config>::WeightInfo::schedule_notify_task_existing_slot())]
		pub fn schedule_notify_task(
			origin: OriginFor<T>,
			provided_id: Vec<u8>,
			time: UnixTime,
			message: Vec<u8>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			if provided_id.len() == 0 {
				Err(Error::<T>::EmptyProvidedId)?
			}
			if message.len() == 0 {
				Err(Error::<T>::EmptyMessage)?
			}
			Self::is_valid_time(time)?;

			let task_id = Self::schedule_task(who.clone(), provided_id.clone(), time)?;

			let task = Task::<T>::create_event_task(who.clone(), provided_id, time, message);
			<Tasks<T>>::insert(task_id, task);

			Self::deposit_event(Event::TaskScheduled { who, task_id });
			Ok(().into())
		}

		/// Cancel a task.
		///
		/// Tasks can only can be cancelled by their owners.
		///
		/// # Parameters
		/// * `task_id`: The id of the task.
		///
		/// # Errors
		/// * `NotTaskOwner`: You are not the owner of the task.
		/// * `TaskDoesNotExist`: The task does not exist.
		#[pallet::weight(<T as Config>::WeightInfo::cancel_overflow_task())]
		pub fn cancel_task(origin: OriginFor<T>, task_id: T::Hash) -> DispatchResult {
			let who = ensure_signed(origin)?;

			match Self::get_task(task_id) {
				None => Err(Error::<T>::TaskDoesNotExist)?,
				Some(task) => {
					if who != task.owner_id {
						Err(Error::<T>::NotTaskOwner)?
					}
					Self::remove_task(task_id, task);
				},
			}
			Ok(().into())
		}

		/// Sudo can force cancel a task.
		///
		/// # Parameters
		/// * `task_id`: The id of the task.
		///
		/// # Errors
		/// * `TaskDoesNotExist`: The task does not exist.
		#[pallet::weight(<T as Config>::WeightInfo::force_cancel_overflow_task())]
		pub fn force_cancel_task(origin: OriginFor<T>, task_id: T::Hash) -> DispatchResult {
			ensure_root(origin)?;

			match Self::get_task(task_id) {
				None => Err(Error::<T>::TaskDoesNotExist)?,
				Some(task) => Self::remove_task(task_id, task),
			}

			Ok(().into())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Based on the block time, return the time slot and if it's the last block in the slot.
		///
		/// In order to do this we:
		/// * Get the most recent timestamp from the block.
		/// * Convert the ms unix timestamp to seconds.
		/// * Bring the timestamp down to the last whole minute.
		fn get_current_time_slot() -> UnixTime {
			let now = <timestamp::Pallet<T>>::get().saturated_into::<UnixTime>();
			let now = now / 1000;
			let diff_to_min = now % 60;
			now - diff_to_min
		}

		/// Checks to see if the scheduled time is a valid timestamp.
		///
		/// In order for a time to be valid it must end in a whole minute and be in the future.
		fn is_valid_time(scheduled_time: UnixTime) -> Result<(), Error<T>> {
			let remainder = scheduled_time % 60;
			if remainder != 0 {
				Err(<Error<T>>::InvalidTime)?;
			}

			let current_time_slot = Self::get_current_time_slot();
			if scheduled_time <= current_time_slot {
				Err(<Error<T>>::PastTime)?;
			}

			Ok(())
		}

		/// Trigger tasks for the block time.
		///
		/// Complete as many tasks as possible given the maximum weight.
		/// TODO (ENG-157): calculate weights.
		pub fn trigger_tasks(max_weight: Weight) -> Weight {
			// need to calculate cost of all but the inner IF.
			let mut weight_left: Weight = max_weight - 20_000;

			// There is a chance we use more than our max_weight to update the task queue.
			// This would occur if the system is not producting blocks for a very long time.
			// Regardless of how long it takes we still need to update the task queue.
			let update_weight = Self::update_task_queue();

			// need to calculate the weight of running just 1 task below.
			if weight_left < update_weight + 10_000 {
				return update_weight
			} else {
				weight_left -= update_weight;
			}

			let task_queue = Self::get_task_queue();

			if task_queue.len() > 0 {
				// calculate cost of all but the run_tasks fcn.
				weight_left -= 10_000;
				let (tasks_left, new_weight_left) = Self::run_tasks(task_queue, weight_left);

				TaskQueue::<T>::put(tasks_left);
				weight_left = new_weight_left;
			}

			max_weight - weight_left
		}

		/// Update the task queue.
		///
		/// This function will check for any time slots we have passed or are in.
		/// For all relevant time slots it will append those task_ids to the task queue, starting with the oldest.
		/// It will then remove those time slots from the Scheduled tasks map.
		///
		/// TODO (ENG-157): calculate weights.
		fn update_task_queue() -> Weight {
			// need to calculate the base fn weight.
			let base_weight = 10_000;
			let mut total_weight = base_weight;

			let time_slot = Self::get_current_time_slot();
			let last_time_slot = Self::get_last_slot();

			if time_slot != last_time_slot {
				let task_queue = Self::get_task_queue();
				let diff = (time_slot - last_time_slot) / 60;
				let (append_weight, updated_task_queue) =
					Self::append_to_task_queue(task_queue, last_time_slot, diff);
				TaskQueue::<T>::put(updated_task_queue);
				// need to figure out how much it costs for all but the fcn call in this if statement.
				total_weight += append_weight + 10_000;
			}
			LastTimeSlot::<T>::put(time_slot);
			total_weight
		}

		/// TODO (ENG-157): calculate weights.
		fn append_to_task_queue(
			mut task_queue: Vec<T::Hash>,
			last_time_slot: UnixTime,
			diff: u64,
		) -> (Weight, Vec<T::Hash>) {
			for i in 0..diff {
				let new_time_slot = last_time_slot + (i + 1) * 60;
				if let Some(task_ids) = Self::get_scheduled_tasks(new_time_slot) {
					task_queue.append(&mut task_ids.into_inner());
					ScheduledTasks::<T>::remove(new_time_slot);
				}
			}
			// need to figure out how much each iteration costs.
			let cost = diff * 20_000;
			(cost, task_queue)
		}

		/// Runs as many tasks as the weight allows from the provided vec of task_ids.
		///
		/// Returns a vec with the tasks that were not run and the remaining weight.
		/// TODO (ENG-157): calculate weights.
		fn run_tasks(
			mut task_ids: Vec<T::Hash>,
			mut weight_left: Weight,
		) -> (Vec<T::Hash>, Weight) {
			// need to calculate the weight of the fn minus the loop.
			weight_left -= 10_000;

			let mut consumed_task_index: usize = 0;
			for task_id in task_ids.iter() {
				consumed_task_index += 1;

				let action_weight = match Self::get_task(task_id) {
					None => {
						Self::deposit_event(Event::TaskNotFound { task_id: task_id.clone() });
						10_000
					},
					Some(task) => match task.action {
						Action::Notify { message } => Self::run_notify_task(message),
					},
				};

				// need to calculate the look cost minus the action
				weight_left = weight_left - action_weight - 10_000;

				// need to calculate the max cost of the loop
				if weight_left < 20_000 {
					break
				}
			}

			if consumed_task_index == task_ids.len() {
				return (vec![], weight_left)
			} else {
				return (task_ids.split_off(consumed_task_index), weight_left)
			}
		}

		/// Fire the notify event with the custom message.
		/// TODO: Calculate weight (ENG-157).
		fn run_notify_task(message: Vec<u8>) -> Weight {
			Self::deposit_event(Event::Notify { message });
			10_000
		}

		fn remove_task(task_id: T::Hash, task: Task<T>) {
			let mut found_task: bool = false;
			match Self::get_scheduled_tasks(task.time) {
				None => {
					let mut task_queue = Self::get_task_queue();
					for i in 0..task_queue.len() {
						if task_queue[i] == task_id {
							task_queue.remove(i);
							TaskQueue::<T>::put(task_queue);
							found_task = true;
							break
						}
					}
				},
				Some(mut task_ids) =>
					for i in 0..task_ids.len() {
						if task_ids[i] == task_id {
							if task_ids.len() == 1 {
								<ScheduledTasks<T>>::remove(task.time);
							} else {
								task_ids.remove(i);
								<ScheduledTasks<T>>::insert(task.time, task_ids);
							}
							found_task = true;
							break
						}
					},
			}

			if !found_task {
				Self::deposit_event(Event::TaskNotFound { task_id });
			}

			<Tasks<T>>::remove(task_id);
			Self::deposit_event(Event::TaskCancelled { who: task.owner_id, task_id });
		}

		/// Schedule task and return it's task_id.
		pub fn schedule_task(
			owner_id: AccountOf<T>,
			provided_id: Vec<u8>,
			time: u64,
		) -> Result<T::Hash, Error<T>> {
			let task_hash_input =
				TaskHashInput::<T> { owner_id: owner_id.clone(), provided_id: provided_id.clone() };
			let task_id = T::Hashing::hash_of(&task_hash_input);

			if let Some(_) = Self::get_task(task_id) {
				Err(Error::<T>::DuplicateTask)?
			}

			match Self::get_scheduled_tasks(time) {
				None => {
					let task_ids: BoundedVec<T::Hash, T::MaxTasksPerSlot> =
						vec![task_id].try_into().unwrap();
					<ScheduledTasks<T>>::insert(time, task_ids);
				},
				Some(mut task_ids) => {
					if let Err(_) = task_ids.try_push(task_id) {
						Err(Error::<T>::TimeSlotFull)?
					}
					<ScheduledTasks<T>>::insert(time, task_ids);
				},
			}
			Ok(task_id)
		}
	}
}