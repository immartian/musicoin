// Copyright 2015, 2016 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Transaction Queue
//!
//! `TransactionQueue` keeps track of all transactions seen by the node (received from other peers) and own transactions
//! and orders them by priority. Top priority transactions are those with low nonce height (difference between
//! transaction's nonce and next nonce expected from this sender). If nonces are equal transaction's gas price is used
//! for comparison (higher gas price = higher priority).
//!
//! # Usage Example
//!
//! ```rust
//! extern crate ethcore_util as util;
//! extern crate ethcore;
//! extern crate ethkey;
//! extern crate rustc_serialize;
//!
//! use util::{Uint, U256, Address};
//! use ethkey::{Random, Generator};
//!	use ethcore::miner::{TransactionQueue, AccountDetails, TransactionOrigin};
//!	use ethcore::transaction::*;
//!	use rustc_serialize::hex::FromHex;
//!
//! fn main() {
//!		let key = Random.generate().unwrap();
//!		let t1 = Transaction { action: Action::Create, value: U256::from(100), data: "3331600055".from_hex().unwrap(),
//!			gas: U256::from(100_000), gas_price: U256::one(), nonce: U256::from(10) };
//!		let t2 = Transaction { action: Action::Create, value: U256::from(100), data: "3331600055".from_hex().unwrap(),
//!			gas: U256::from(100_000), gas_price: U256::one(), nonce: U256::from(11) };
//!
//!		let st1 = t1.sign(&key.secret(), None);
//!		let st2 = t2.sign(&key.secret(), None);
//!		let default_account_details = |_a: &Address| AccountDetails {
//!			nonce: U256::from(10),
//!			balance: U256::from(1_000_000),
//!		};
//!		let gas_estimator = |_tx: &SignedTransaction| 2.into();
//!
//!		let mut txq = TransactionQueue::default();
//!		txq.add(st2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
//!		txq.add(st1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
//!
//!		// Check status
//!		assert_eq!(txq.status().pending, 2);
//!		// Check top transactions
//!		let top = txq.top_transactions();
//!		assert_eq!(top.len(), 2);
//!		assert_eq!(top[0], st1);
//!		assert_eq!(top[1], st2);
//!
//!		// And when transaction is removed (but nonce haven't changed)
//!		// it will move subsequent transactions to future
//!		txq.remove_invalid(&st1.hash(), &default_account_details);
//!		assert_eq!(txq.status().pending, 0);
//!		assert_eq!(txq.status().future, 1);
//!		assert_eq!(txq.top_transactions().len(), 0);
//!	}
//! ```
//!
//!	# Maintaing valid state
//!
//!	1. Whenever transaction is imported to queue (to queue) all other transactions from this sender are revalidated in current. It means that they are moved to future and back again (height recalculation & gap filling).
//!	2. Whenever invalid transaction is removed:
//!		- When it's removed from `future` - all `future` transactions heights are recalculated and then
//!		  we check if the transactions should go to `current` (comparing state nonce)
//!		- When it's removed from `current` - all transactions from this sender (`current` & `future`) are recalculated.
//!	3. `remove_all` is used to inform the queue about client (state) nonce changes.
//!     - It removes all transactions (either from `current` or `future`) with nonce < client nonce
//!     - It moves matching `future` transactions to `current`
//! 4. `remove_old` is used as convenient method to update the state nonce for all senders in the queue.
//!		- Invokes `remove_all` with latest state nonce for all senders.

use std::ops::Deref;
use std::cmp::Ordering;
use std::cmp;
use std::collections::{HashSet, HashMap, BTreeSet, BTreeMap};
use linked_hash_map::LinkedHashMap;
use util::{Address, H256, Uint, U256};
use util::table::Table;
use transaction::*;
use error::{Error, TransactionError};
use client::TransactionImportResult;
use miner::local_transactions::{LocalTransactionsList, Status as LocalTransactionStatus};

/// Transaction origin
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionOrigin {
	/// Transaction coming from local RPC
	Local,
	/// External transaction received from network
	External,
	/// Transactions from retracted blocks
	RetractedBlock,
}

impl PartialOrd for TransactionOrigin {
	fn partial_cmp(&self, other: &TransactionOrigin) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for TransactionOrigin {
	#[cfg_attr(feature="dev", allow(match_same_arms))]
	fn cmp(&self, other: &TransactionOrigin) -> Ordering {
		if *other == *self {
			return Ordering::Equal;
		}

		match (*self, *other) {
			(TransactionOrigin::RetractedBlock, _) => Ordering::Less,
			(_, TransactionOrigin::RetractedBlock) => Ordering::Greater,
			(TransactionOrigin::Local, _) => Ordering::Less,
			_ => Ordering::Greater,
		}
	}
}

impl TransactionOrigin {
	fn is_local(&self) -> bool {
		*self == TransactionOrigin::Local
	}
}

#[derive(Clone, Debug)]
/// Light structure used to identify transaction and its order
struct TransactionOrder {
	/// Primary ordering factory. Difference between transaction nonce and expected nonce in state
	/// (e.g. Tx(nonce:5), State(nonce:0) -> height: 5)
	/// High nonce_height = Low priority (processed later)
	nonce_height: U256,
	/// Gas Price of the transaction.
	/// Low gas price = Low priority (processed later)
	gas_price: U256,
	/// Gas usage priority factor. Usage depends on strategy.
	/// Represents the linear increment in required gas price for heavy transactions.
	///
	/// High gas limit + Low gas price = Low priority
	/// High gas limit + High gas price = High priority
	gas_factor: U256,
	/// Gas (limit) of the transaction. Usage depends on strategy.
	/// Low gas limit = High priority (processed earlier)
	gas: U256,
	/// Transaction ordering strategy
	strategy: PrioritizationStrategy,
	/// Hash to identify associated transaction
	hash: H256,
	/// Origin of the transaction
	origin: TransactionOrigin,
	/// Penalties
	penalties: usize,
}


impl TransactionOrder {

	fn for_transaction(tx: &VerifiedTransaction, base_nonce: U256, min_gas_price: U256, strategy: PrioritizationStrategy) -> Self {
		let factor = (tx.transaction.gas >> 15) * min_gas_price;
		TransactionOrder {
			nonce_height: tx.nonce() - base_nonce,
			gas_price: tx.transaction.gas_price,
			gas: tx.transaction.gas,
			gas_factor: factor,
			strategy: strategy,
			hash: tx.hash(),
			origin: tx.origin,
			penalties: 0,
		}
	}

	fn update_height(mut self, nonce: U256, base_nonce: U256) -> Self {
		self.nonce_height = nonce - base_nonce;
		self
	}

	fn penalize(mut self) -> Self {
		self.penalties = self.penalties.saturating_add(1);
		self
	}
}

impl Eq for TransactionOrder {}
impl PartialEq for TransactionOrder {
	fn eq(&self, other: &TransactionOrder) -> bool {
		self.cmp(other) == Ordering::Equal
	}
}
impl PartialOrd for TransactionOrder {
	fn partial_cmp(&self, other: &TransactionOrder) -> Option<Ordering> {
		Some(self.cmp(other))
	}
}

impl Ord for TransactionOrder {
	fn cmp(&self, b: &TransactionOrder) -> Ordering {
		// First check number of penalties
		if self.penalties != b.penalties {
			return self.penalties.cmp(&b.penalties);
		}

		// Local transactions should always have priority
		if self.origin != b.origin {
			return self.origin.cmp(&b.origin);
		}

		// Check nonce_height
		if self.nonce_height != b.nonce_height {
			return self.nonce_height.cmp(&b.nonce_height);
		}

		match self.strategy {
			PrioritizationStrategy::GasAndGasPrice => {
				if self.gas != b.gas {
					return self.gas.cmp(&b.gas);
				}
			},
			PrioritizationStrategy::GasFactorAndGasPrice => {
				// avoiding overflows
				// (gp1 - g1) > (gp2 - g2) <=>
				// (gp1 + g2) > (gp2 + g1)
				let f_a = self.gas_price + b.gas_factor;
				let f_b = b.gas_price + self.gas_factor;
				if f_a != f_b {
					return f_b.cmp(&f_a);
				}
			},
			PrioritizationStrategy::GasPriceOnly => {},
		}

		// Then compare gas_prices
		if self.gas_price != b.gas_price {
			return b.gas_price.cmp(&self.gas_price);
		}

		// Compare hashes
		self.hash.cmp(&b.hash)
	}
}

/// Verified transaction (with sender)
#[derive(Debug)]
struct VerifiedTransaction {
	/// Transaction
	transaction: SignedTransaction,
	/// transaction origin
	origin: TransactionOrigin,
}

impl VerifiedTransaction {
	fn new(transaction: SignedTransaction, origin: TransactionOrigin) -> Result<Self, Error> {
		try!(transaction.sender());
		Ok(VerifiedTransaction {
			transaction: transaction,
			origin: origin,
		})
	}

	fn hash(&self) -> H256 {
		self.transaction.hash()
	}

	fn nonce(&self) -> U256 {
		self.transaction.nonce
	}

	fn sender(&self) -> Address {
		self.transaction.sender().expect("Sender is verified in new; qed")
	}
}

#[derive(Debug, Default)]
struct GasPriceQueue {
	backing: BTreeMap<U256, HashSet<H256>>,
}

impl GasPriceQueue {
	/// Insert an item into a BTreeMap/HashSet "multimap".
	pub fn insert(&mut self, gas_price: U256, hash: H256) -> bool {
		self.backing.entry(gas_price).or_insert_with(Default::default).insert(hash)
	}

	/// Remove an item from a BTreeMap/HashSet "multimap".
	/// Returns true if the item was removed successfully.
	pub fn remove(&mut self, gas_price: &U256, hash: &H256) -> bool {
		if let Some(mut hashes) = self.backing.get_mut(gas_price) {
			let only_one_left = hashes.len() == 1;
			if !only_one_left {
				// Operation may be ok: only if hash is in gas-price's Set.
				return hashes.remove(hash);
			}
			if hash != hashes.iter().next().expect("We know there is only one element in collection, tested above; qed") {
				// Operation failed: hash not the single item in gas-price's Set.
				return false;
			}
		} else {
			// Operation failed: gas-price not found in Map.
			return false;
		}
		// Operation maybe ok: only if hash not found in gas-price Set.
		self.backing.remove(gas_price).is_some()
	}
}

impl Deref for GasPriceQueue {
	type Target=BTreeMap<U256, HashSet<H256>>;

	fn deref(&self) -> &Self::Target {
		&self.backing
	}
}

/// Holds transactions accessible by (address, nonce) and by priority
///
/// `TransactionSet` keeps number of entries below limit, but it doesn't
/// automatically happen during `insert/remove` operations.
/// You have to call `enforce_limit` to remove lowest priority transactions from set.
struct TransactionSet {
	by_priority: BTreeSet<TransactionOrder>,
	by_address: Table<Address, U256, TransactionOrder>,
	by_gas_price: GasPriceQueue,
	limit: usize,
	gas_limit: U256,
}

impl TransactionSet {
	/// Inserts `TransactionOrder` to this set. Transaction does not need to be unique -
	/// the same transaction may be validly inserted twice. Any previous transaction that
	/// it replaces (i.e. with the same `sender` and `nonce`) should be returned.
	fn insert(&mut self, sender: Address, nonce: U256, order: TransactionOrder) -> Option<TransactionOrder> {
		if !self.by_priority.insert(order.clone()) {
			return Some(order.clone());
		}
		let order_hash = order.hash.clone();
		let order_gas_price = order.gas_price.clone();
		let by_address_replaced = self.by_address.insert(sender, nonce, order);
		// If transaction was replaced remove it from priority queue
		if let Some(ref old_order) = by_address_replaced {
			assert!(self.by_priority.remove(old_order), "hash is in `by_address`; all transactions in `by_address` must be in `by_priority`; qed");
			assert!(self.by_gas_price.remove(&old_order.gas_price, &old_order.hash),
				"hash is in `by_address`; all transactions' gas_prices in `by_address` must be in `by_gas_limit`; qed");
		}
		self.by_gas_price.insert(order_gas_price, order_hash);
		assert_eq!(self.by_priority.len(), self.by_address.len());
		assert_eq!(self.by_gas_price.values().map(|v| v.len()).fold(0, |a, b| a + b), self.by_address.len());
		by_address_replaced
	}

	/// Remove low priority transactions if there is more than specified by given `limit`.
	///
	/// It drops transactions from this set but also removes associated `VerifiedTransaction`.
	/// Returns addresses and lowest nonces of transactions removed because of limit.
	fn enforce_limit(&mut self, by_hash: &mut HashMap<H256, VerifiedTransaction>, local: &mut LocalTransactionsList) -> Option<HashMap<Address, U256>> {
		let mut count = 0;
		let mut gas: U256 = 0.into();
		let to_drop : Vec<(Address, U256)> = {
			self.by_priority
				.iter()
				.filter(|order| {
					count = count + 1;
					let r = gas.overflowing_add(order.gas);
					if r.1 { return false }
					gas = r.0;
					// Own and retracted transactions are allowed to go above all limits.
					order.origin != TransactionOrigin::Local && order.origin != TransactionOrigin::RetractedBlock &&
					(gas > self.gas_limit || count > self.limit)
				})
				.map(|order| by_hash.get(&order.hash)
					.expect("All transactions in `self.by_priority` and `self.by_address` are kept in sync with `by_hash`."))
				.map(|tx| (tx.sender(), tx.nonce()))
				.collect()
		};

		Some(to_drop.into_iter()
			.fold(HashMap::new(), |mut removed, (sender, nonce)| {
				let order = self.drop(&sender, &nonce)
					.expect("Transaction has just been found in `by_priority`; so it is in `by_address` also.");
				trace!(target: "txqueue", "Dropped out of limit transaction: {:?}", order.hash);

				let order = by_hash.remove(&order.hash)
					.expect("hash is in `by_priorty`; all hashes in `by_priority` must be in `by_hash`; qed");

				if order.origin.is_local() {
					local.mark_dropped(order.transaction);
				}

				let min = removed.get(&sender).map_or(nonce, |val| cmp::min(*val, nonce));
				removed.insert(sender, min);
				removed
			}))
	}

	/// Drop transaction from this set (remove from `by_priority` and `by_address`)
	fn drop(&mut self, sender: &Address, nonce: &U256) -> Option<TransactionOrder> {
		if let Some(tx_order) = self.by_address.remove(sender, nonce) {
			assert!(self.by_gas_price.remove(&tx_order.gas_price, &tx_order.hash),
				"hash is in `by_address`; all transactions' gas_prices in `by_address` must be in `by_gas_limit`; qed");
			assert!(self.by_priority.remove(&tx_order),
				"hash is in `by_address`; all transactions' gas_prices in `by_address` must be in `by_priority`; qed");
			assert_eq!(self.by_priority.len(), self.by_address.len());
			assert_eq!(self.by_gas_price.values().map(|v| v.len()).fold(0, |a, b| a + b), self.by_address.len());
			return Some(tx_order);
		}
		assert_eq!(self.by_priority.len(), self.by_address.len());
		assert_eq!(self.by_gas_price.values().map(|v| v.len()).fold(0, |a, b| a + b), self.by_address.len());
		None
	}

	/// Drop all transactions.
	fn clear(&mut self) {
		self.by_priority.clear();
		self.by_address.clear();
		self.by_gas_price.backing.clear();
	}

	/// Sets new limit for number of transactions in this `TransactionSet`.
	/// Note the limit is not applied (no transactions are removed) by calling this method.
	fn set_limit(&mut self, limit: usize) {
		self.limit = limit;
	}

	/// Get the minimum gas price that we can accept into this queue that wouldn't cause the transaction to
	/// immediately be dropped. 0 if the queue isn't at capacity; 1 plus the lowest if it is.
	fn gas_price_entry_limit(&self) -> U256 {
		match self.by_gas_price.keys().next() {
			Some(k) if self.by_priority.len() >= self.limit => *k + 1.into(),
			_ => U256::default(),
		}
	}
}

#[derive(Debug)]
/// Current status of the queue
pub struct TransactionQueueStatus {
	/// Number of pending transactions (ready to go to block)
	pub pending: usize,
	/// Number of future transactions (waiting for transactions with lower nonces first)
	pub future: usize,
}

/// Details of account
pub struct AccountDetails {
	/// Most recent account nonce
	pub nonce: U256,
	/// Current account balance
	pub balance: U256,
}

/// Transactions with `gas > (gas_limit + gas_limit * Factor(in percents))` are not imported to the queue.
const GAS_LIMIT_HYSTERESIS: usize = 10; // (100/GAS_LIMIT_HYSTERESIS) %

/// Describes the strategy used to prioritize transactions in the queue.
#[cfg_attr(feature="dev", allow(enum_variant_names))]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PrioritizationStrategy {
	/// Use only gas price. Disregards the actual computation cost of the transaction.
	/// i.e. Higher gas price = Higher priority
	GasPriceOnly,
	/// Use gas limit and then gas price.
	/// i.e. Higher gas limit = Lower priority
	GasAndGasPrice,
	/// Calculate and use priority based on gas and gas price.
	/// PRIORITY = GAS_PRICE - GAS/2^15 * MIN_GAS_PRICE
	///
	/// Rationale:
	/// Heavy transactions are paying linear cost (GAS * GAS_PRICE)
	/// while the computation might be more expensive.
	///
	/// i.e.
	/// 1M gas tx with `gas_price=30*min` has the same priority
	/// as 32k gas tx with `gas_price=min`
	GasFactorAndGasPrice,
}

/// `TransactionQueue` implementation
pub struct TransactionQueue {
	/// Prioritization strategy for this queue
	strategy: PrioritizationStrategy,
	/// Gas Price threshold for transactions that can be imported to this queue (defaults to 0)
	minimal_gas_price: U256,
	/// The maximum amount of gas any individual transaction may use.
	tx_gas_limit: U256,
	/// Current gas limit (block gas limit * factor). Transactions above the limit will not be accepted (default to !0)
	gas_limit: U256,
	/// Priority queue for transactions that can go to block
	current: TransactionSet,
	/// Priority queue for transactions that has been received but are not yet valid to go to block
	future: TransactionSet,
	/// All transactions managed by queue indexed by hash
	by_hash: HashMap<H256, VerifiedTransaction>,
	/// Last nonce of transaction in current (to quickly check next expected transaction)
	last_nonces: HashMap<Address, U256>,
	/// List of local transactions and their statuses.
	local_transactions: LocalTransactionsList,
}

impl Default for TransactionQueue {
	fn default() -> Self {
		TransactionQueue::new(PrioritizationStrategy::GasPriceOnly)
	}
}

impl TransactionQueue {
	/// Creates new instance of this Queue
	pub fn new(strategy: PrioritizationStrategy) -> Self {
		Self::with_limits(strategy, 1024, !U256::zero(), !U256::zero())
	}

	/// Create new instance of this Queue with specified limits
	pub fn with_limits(strategy: PrioritizationStrategy, limit: usize, gas_limit: U256, tx_gas_limit: U256) -> Self {
		let current = TransactionSet {
			by_priority: BTreeSet::new(),
			by_address: Table::new(),
			by_gas_price: Default::default(),
			limit: limit,
			gas_limit: gas_limit,
		};

		let future = TransactionSet {
			by_priority: BTreeSet::new(),
			by_address: Table::new(),
			by_gas_price: Default::default(),
			limit: limit,
			gas_limit: gas_limit,
		};

		TransactionQueue {
			strategy: strategy,
			minimal_gas_price: U256::zero(),
			tx_gas_limit: tx_gas_limit,
			gas_limit: !U256::zero(),
			current: current,
			future: future,
			by_hash: HashMap::new(),
			last_nonces: HashMap::new(),
			local_transactions: LocalTransactionsList::default(),
		}
	}

	/// Set the new limit for `current` and `future` queue.
	pub fn set_limit(&mut self, limit: usize) {
		self.current.set_limit(limit);
		self.future.set_limit(limit);
		// And ensure the limits
		self.current.enforce_limit(&mut self.by_hash, &mut self.local_transactions);
		self.future.enforce_limit(&mut self.by_hash, &mut self.local_transactions);
	}

	/// Returns current limit of transactions in the queue.
	pub fn limit(&self) -> usize {
		self.current.limit
	}

	/// Get the minimal gas price.
	pub fn minimal_gas_price(&self) -> &U256 {
		&self.minimal_gas_price
	}

	/// Sets new gas price threshold for incoming transactions.
	/// Any transaction already imported to the queue is not affected.
	pub fn set_minimal_gas_price(&mut self, min_gas_price: U256) {
		self.minimal_gas_price = min_gas_price;
	}

	/// Get one more than the lowest gas price in the queue iff the pool is
	/// full, otherwise 0.
	pub fn effective_minimum_gas_price(&self) -> U256 {
		self.current.gas_price_entry_limit()
	}

	/// Sets new gas limit. Transactions with gas slightly (`GAS_LIMIT_HYSTERESIS`) above the limit won't be imported.
	/// Any transaction already imported to the queue is not affected.
	pub fn set_gas_limit(&mut self, gas_limit: U256) {
		let extra = gas_limit / U256::from(GAS_LIMIT_HYSTERESIS);

		self.gas_limit = match gas_limit.overflowing_add(extra) {
			(_, true) => !U256::zero(),
			(val, false) => val,
		};
	}

	/// Sets new total gas limit.
	pub fn set_total_gas_limit(&mut self, gas_limit: U256) {
		self.future.gas_limit = gas_limit;
		self.current.gas_limit = gas_limit;
		self.future.enforce_limit(&mut self.by_hash, &mut self.local_transactions);
	}

	/// Set the new limit for the amount of gas any individual transaction may have.
	/// Any transaction already imported to the queue is not affected.
	pub fn set_tx_gas_limit(&mut self, limit: U256) {
		self.tx_gas_limit = limit;
	}

	/// Returns current status for this queue
	pub fn status(&self) -> TransactionQueueStatus {
		TransactionQueueStatus {
			pending: self.current.by_priority.len(),
			future: self.future.by_priority.len(),
		}
	}

	/// Add signed transaction to queue to be verified and imported.
	///
	/// NOTE fetch_account and gas_estimator should be cheap to compute
	/// otherwise it might open up an attack vector.
	pub fn add<F, G>(
		&mut self,
		tx: SignedTransaction,
		origin: TransactionOrigin,
		fetch_account: &F,
		gas_estimator: &G,
	) -> Result<TransactionImportResult, Error> where
		F: Fn(&Address) -> AccountDetails,
		G: Fn(&SignedTransaction) -> U256,
	{
		if origin == TransactionOrigin::Local {
			let hash = tx.hash();
			let cloned_tx = tx.clone();

			let result = self.add_internal(tx, origin, fetch_account, gas_estimator);
			match result {
				Ok(TransactionImportResult::Current) => {
					self.local_transactions.mark_pending(hash);
				},
				Ok(TransactionImportResult::Future) => {
					self.local_transactions.mark_future(hash);
				},
				Err(Error::Transaction(ref err)) => {
					// Sometimes transactions are re-imported, so
					// don't overwrite transactions if they are already on the list
					if !self.local_transactions.contains(&cloned_tx.hash()) {
						self.local_transactions.mark_rejected(cloned_tx, err.clone());
					}
				},
				Err(_) => {
					self.local_transactions.mark_invalid(cloned_tx);
				},
			}
			result
		} else {
			self.add_internal(tx, origin, fetch_account, gas_estimator)
		}
	}

	/// Adds signed transaction to the queue.
	fn add_internal<F, G>(
		&mut self,
		tx: SignedTransaction,
		origin: TransactionOrigin,
		fetch_account: &F,
		gas_estimator: &G,
	) -> Result<TransactionImportResult, Error> where
		F: Fn(&Address) -> AccountDetails,
		G: Fn(&SignedTransaction) -> U256,
	{

		if tx.gas_price < self.minimal_gas_price && origin != TransactionOrigin::Local {
			trace!(target: "txqueue",
				"Dropping transaction below minimal gas price threshold: {:?} (gp: {} < {})",
				tx.hash(),
				tx.gas_price,
				self.minimal_gas_price
			);

			return Err(Error::Transaction(TransactionError::InsufficientGasPrice {
				minimal: self.minimal_gas_price,
				got: tx.gas_price,
			}));
		}

		let full_queues_lowest = self.effective_minimum_gas_price();
		if tx.gas_price < full_queues_lowest && origin != TransactionOrigin::Local {
			trace!(target: "txqueue",
				"Dropping transaction below lowest gas price in a full queue: {:?} (gp: {} < {})",
				tx.hash(),
				tx.gas_price,
				full_queues_lowest
			);

			return Err(Error::Transaction(TransactionError::InsufficientGasPrice {
				minimal: full_queues_lowest,
				got: tx.gas_price,
			}));
		}

		if tx.gas > self.gas_limit || tx.gas > self.tx_gas_limit {
			trace!(target: "txqueue",
				"Dropping transaction above gas limit: {:?} ({} > min({}, {}))",
				tx.hash(),
				tx.gas,
				self.gas_limit,
				self.tx_gas_limit
			);
			return Err(Error::Transaction(TransactionError::GasLimitExceeded {
				limit: self.gas_limit,
				got: tx.gas,
			}));
		}

		let minimal_gas = gas_estimator(&tx);
		if tx.gas < minimal_gas {
			trace!(target: "txqueue",
				"Dropping transaction with insufficient gas: {:?} ({} > {})",
				tx.hash(),
				tx.gas,
				minimal_gas,
			);

			return Err(Error::Transaction(TransactionError::InsufficientGas {
				minimal: minimal_gas,
				got: tx.gas,
			}));
		}

		// Verify signature
		try!(tx.check_low_s());

		let vtx = try!(VerifiedTransaction::new(tx, origin));
		let client_account = fetch_account(&vtx.sender());

		let cost = vtx.transaction.value + vtx.transaction.gas_price * vtx.transaction.gas;
		if client_account.balance < cost {
			trace!(target: "txqueue",
				"Dropping transaction without sufficient balance: {:?} ({} < {})",
				vtx.hash(),
				client_account.balance,
				cost
			);

			return Err(Error::Transaction(TransactionError::InsufficientBalance {
				cost: cost,
				balance: client_account.balance
			}));
		}

		let r = self.import_tx(vtx, client_account.nonce).map_err(Error::Transaction);
		assert_eq!(self.future.by_priority.len() + self.current.by_priority.len(), self.by_hash.len());
		r
	}

	/// Removes all transactions from particular sender up to (excluding) given client (state) nonce.
	/// Client (State) Nonce = next valid nonce for this sender.
	pub fn remove_all(&mut self, sender: Address, client_nonce: U256) {
		// Check if there is anything in current...
		let should_check_in_current = self.current.by_address.row(&sender)
			// If nonce == client_nonce nothing is changed
			.and_then(|by_nonce| by_nonce.keys().find(|nonce| *nonce < &client_nonce))
			.map(|_| ());
		// ... or future
		let should_check_in_future = self.future.by_address.row(&sender)
			// if nonce == client_nonce we need to promote to current
			.and_then(|by_nonce| by_nonce.keys().find(|nonce| *nonce <= &client_nonce))
			.map(|_| ());

		if should_check_in_current.or(should_check_in_future).is_none() {
			return;
		}

		self.remove_all_internal(sender, client_nonce);
	}

	/// Always updates future and moves transactions from current to future.
	fn remove_all_internal(&mut self, sender: Address, client_nonce: U256) {
		// We will either move transaction to future or remove it completely
		// so there will be no transactions from this sender in current
		self.last_nonces.remove(&sender);
		// First update height of transactions in future to avoid collisions
		self.update_future(&sender, client_nonce);
		// This should move all current transactions to future and remove old transactions
		self.move_all_to_future(&sender, client_nonce);
		// And now lets check if there is some batch of transactions in future
		// that should be placed in current. It should also update last_nonces.
		self.move_matching_future_to_current(sender, client_nonce, client_nonce);
		assert_eq!(self.future.by_priority.len() + self.current.by_priority.len(), self.by_hash.len());
	}

	/// Checks the current nonce for all transactions' senders in the queue and removes the old transactions.
	pub fn remove_old<F>(&mut self, fetch_nonce: F) where
		F: Fn(&Address) -> U256,
	{
		let senders = self.current.by_address.keys()
			.chain(self.future.by_address.keys())
			.cloned()
			.collect::<HashSet<_>>();

		for sender in senders {
			self.remove_all(sender, fetch_nonce(&sender));
		}
	}

	/// Penalize transactions from sender of transaction with given hash.
	/// I.e. it should change the priority of the transaction in the queue.
	///
	/// NOTE: We need to penalize all transactions from particular sender
	/// to avoid breaking invariants in queue (ordered by nonces).
	/// Consecutive transactions from this sender would fail otherwise (because of invalid nonce).
	pub fn penalize(&mut self, transaction_hash: &H256) {
		let transaction = match self.by_hash.get(transaction_hash) {
			None => return,
			Some(t) => t,
		};

		// Never penalize local transactions
		if transaction.origin.is_local() {
			return;
		}

		let sender = transaction.sender();

		// Penalize all transactions from this sender
		let nonces_from_sender = match self.current.by_address.row(&sender) {
			Some(row_map) => row_map.keys().cloned().collect::<Vec<U256>>(),
			None => vec![],
		};
		for k in nonces_from_sender {
			let order = self.current.drop(&sender, &k).expect("transaction known to be in self.current; qed");
			self.current.insert(sender, k, order.penalize());
		}
		// Same thing for future
		let nonces_from_sender = match self.future.by_address.row(&sender) {
			Some(row_map) => row_map.keys().cloned().collect::<Vec<U256>>(),
			None => vec![],
		};
		for k in nonces_from_sender {
			let order = self.future.drop(&sender, &k).expect("transaction known to be in self.future; qed");
			self.future.insert(sender, k, order.penalize());
		}
	}

	/// Removes invalid transaction identified by hash from queue.
	/// Assumption is that this transaction nonce is not related to client nonce,
	/// so transactions left in queue are processed according to client nonce.
	///
	/// If gap is introduced marks subsequent transactions as future
	pub fn remove_invalid<T>(&mut self, transaction_hash: &H256, fetch_account: &T)
		where T: Fn(&Address) -> AccountDetails {

		assert_eq!(self.future.by_priority.len() + self.current.by_priority.len(), self.by_hash.len());
		let transaction = self.by_hash.remove(transaction_hash);
		if transaction.is_none() {
			// We don't know this transaction
			return;
		}

		let transaction = transaction.expect("None is tested in early-exit condition above; qed");
		let sender = transaction.sender();
		let nonce = transaction.nonce();
		let current_nonce = fetch_account(&sender).nonce;

		trace!(target: "txqueue", "Removing invalid transaction: {:?}", transaction.hash());

		// Mark in locals
		if self.local_transactions.contains(transaction_hash) {
			self.local_transactions.mark_invalid(transaction.transaction.clone());
		}

		// Remove from future
		let order = self.future.drop(&sender, &nonce);
		if order.is_some() {
			self.update_future(&sender, current_nonce);
			// And now lets check if there is some chain of transactions in future
			// that should be placed in current
			self.move_matching_future_to_current(sender, current_nonce, current_nonce);
			assert_eq!(self.future.by_priority.len() + self.current.by_priority.len(), self.by_hash.len());
			return;
		}

		// Remove from current
		let order = self.current.drop(&sender, &nonce);
		if order.is_some() {
			// This will keep consistency in queue
			// Moves all to future and then promotes a batch from current:
			self.remove_all_internal(sender, current_nonce);
			assert_eq!(self.future.by_priority.len() + self.current.by_priority.len(), self.by_hash.len());
			return;
		}
	}

	/// Marks all transactions from particular sender as local transactions
	fn mark_transactions_local(&mut self, sender: &Address) {
		fn mark_local<F: FnMut(H256)>(sender: &Address, set: &mut TransactionSet, mut mark: F) {
			// Mark all transactions from this sender as local
			let nonces_from_sender = set.by_address.row(sender)
				.map(|row_map| {
					row_map.iter().filter_map(|(nonce, order)| if order.origin.is_local() {
						None
					} else {
						Some(*nonce)
					}).collect::<Vec<U256>>()
				})
				.unwrap_or_else(Vec::new);

			for k in nonces_from_sender {
				let mut order = set.drop(sender, &k).expect("transaction known to be in self.current/self.future; qed");
				order.origin = TransactionOrigin::Local;
				mark(order.hash);
				set.insert(*sender, k, order);
			}
		}

		let local = &mut self.local_transactions;
		mark_local(sender, &mut self.current, |hash| local.mark_pending(hash));
		mark_local(sender, &mut self.future, |hash| local.mark_future(hash));
	}

	/// Update height of all transactions in future transactions set.
	fn update_future(&mut self, sender: &Address, current_nonce: U256) {
		// We need to drain all transactions for current sender from future and reinsert them with updated height
		let all_nonces_from_sender = match self.future.by_address.row(sender) {
			Some(row_map) => row_map.keys().cloned().collect::<Vec<U256>>(),
			None => vec![],
		};
		for k in all_nonces_from_sender {
			let order = self.future.drop(sender, &k).expect("iterating over a collection that has been retrieved above; qed");
			if k >= current_nonce {
				self.future.insert(*sender, k, order.update_height(k, current_nonce));
			} else {
				trace!(target: "txqueue", "Removing old transaction: {:?} (nonce: {} < {})", order.hash, k, current_nonce);
				// Remove the transaction completely
				self.by_hash.remove(&order.hash).expect("All transactions in `future` are also in `by_hash`");
			}
		}
	}

	/// Drop all transactions from given sender from `current`.
	/// Either moves them to `future` or removes them from queue completely.
	fn move_all_to_future(&mut self, sender: &Address, current_nonce: U256) {
		let all_nonces_from_sender = match self.current.by_address.row(sender) {
			Some(row_map) => row_map.keys().cloned().collect::<Vec<U256>>(),
			None => vec![],
		};

		for k in all_nonces_from_sender {
			// Goes to future or is removed
			let order = self.current.drop(sender, &k).expect("iterating over a collection that has been retrieved above;
															 qed");
			if k >= current_nonce {
				let order = order.update_height(k, current_nonce);
				if order.origin.is_local() {
					self.local_transactions.mark_future(order.hash);
				}
				if let Some(old) = self.future.insert(*sender, k, order.clone()) {
					Self::replace_orders(*sender, k, old, order, &mut self.future, &mut self.by_hash, &mut self.local_transactions);
				}
			} else {
				trace!(target: "txqueue", "Removing old transaction: {:?} (nonce: {} < {})", order.hash, k, current_nonce);
				let tx = self.by_hash.remove(&order.hash).expect("All transactions in `future` are also in `by_hash`");
				if tx.origin.is_local() {
					self.local_transactions.mark_mined(tx.transaction);
				}
			}
		}
		self.future.enforce_limit(&mut self.by_hash, &mut self.local_transactions);
	}

	/// Returns top transactions from the queue ordered by priority.
	pub fn top_transactions(&self) -> Vec<SignedTransaction> {
		self.current.by_priority
			.iter()
			.map(|t| self.by_hash.get(&t.hash).expect("All transactions in `current` and `future` are always included in `by_hash`"))
			.map(|t| t.transaction.clone())
			.collect()
	}

	/// Returns local transactions (some of them might not be part of the queue anymore).
	pub fn local_transactions(&self) -> &LinkedHashMap<H256, LocalTransactionStatus> {
		self.local_transactions.all_transactions()
	}

	#[cfg(test)]
	fn future_transactions(&self) -> Vec<SignedTransaction> {
		self.future.by_priority
			.iter()
			.map(|t| self.by_hash.get(&t.hash).expect("All transactions in `current` and `future` are always included in `by_hash`"))
			.map(|t| t.transaction.clone())
			.collect()
	}

	/// Returns hashes of all transactions from current, ordered by priority.
	pub fn pending_hashes(&self) -> Vec<H256> {
		self.current.by_priority
			.iter()
			.map(|t| t.hash)
			.collect()
	}

	/// Returns true if there is at least one local transaction pending
	pub fn has_local_pending_transactions(&self) -> bool {
		self.current.by_priority.iter().any(|tx| tx.origin == TransactionOrigin::Local)
	}

	/// Finds transaction in the queue by hash (if any)
	pub fn find(&self, hash: &H256) -> Option<SignedTransaction> {
		match self.by_hash.get(hash) { Some(transaction_ref) => Some(transaction_ref.transaction.clone()), None => None }
	}

	/// Removes all elements (in any state) from the queue
	pub fn clear(&mut self) {
		self.current.clear();
		self.future.clear();
		self.by_hash.clear();
		self.last_nonces.clear();
	}

	/// Returns highest transaction nonce for given address.
	pub fn last_nonce(&self, address: &Address) -> Option<U256> {
		self.last_nonces.get(address).cloned()
	}

	/// Checks if there are any transactions in `future` that should actually be promoted to `current`
	/// (because nonce matches).
	fn move_matching_future_to_current(&mut self, address: Address, mut current_nonce: U256, first_nonce: U256) {
		let mut update_last_nonce_to = None;
		{
			let by_nonce = self.future.by_address.row_mut(&address);
			if by_nonce.is_none() {
				return;
			}
			let mut by_nonce = by_nonce.expect("None is tested in early-exit condition above; qed");
			while let Some(order) = by_nonce.remove(&current_nonce) {
				// remove also from priority and gas_price
				self.future.by_priority.remove(&order);
				self.future.by_gas_price.remove(&order.gas_price, &order.hash);
				// Put to current
				let order = order.update_height(current_nonce, first_nonce);
				if order.origin.is_local() {
					self.local_transactions.mark_pending(order.hash);
				}
				if let Some(old) = self.current.insert(address, current_nonce, order.clone()) {
					Self::replace_orders(address, current_nonce, old, order, &mut self.current, &mut self.by_hash, &mut self.local_transactions);
				}
				update_last_nonce_to = Some(current_nonce);
				current_nonce = current_nonce + U256::one();
			}
		}
		self.future.by_address.clear_if_empty(&address);
		if let Some(x) = update_last_nonce_to {
			// Update last inserted nonce
			self.last_nonces.insert(address, x);
		}
	}

	/// Adds VerifiedTransaction to this queue.
	///
	/// Determines if it should be placed in current or future. When transaction is
	/// imported to `current` also checks if there are any `future` transactions that should be promoted because of
	/// this.
	///
	/// It ignores transactions that has already been imported (same `hash`) and replaces the transaction
	/// iff `(address, nonce)` is the same but `gas_price` is higher.
	///
	/// Returns `true` when transaction was imported successfuly
	fn import_tx(&mut self, tx: VerifiedTransaction, state_nonce: U256) -> Result<TransactionImportResult, TransactionError> {

		if self.by_hash.get(&tx.hash()).is_some() {
			// Transaction is already imported.
			trace!(target: "txqueue", "Dropping already imported transaction: {:?}", tx.hash());
			return Err(TransactionError::AlreadyImported);
		}

		let min_gas_price = (self.minimal_gas_price, self.strategy);
		let address = tx.sender();
		let nonce = tx.nonce();
		let hash = tx.hash();

		// The transaction might be old, let's check that.
		// This has to be the first test, otherwise calculating
		// nonce height would result in overflow.
		if nonce < state_nonce {
			// Droping transaction
			trace!(target: "txqueue", "Dropping old transaction: {:?} (nonce: {} < {})", tx.hash(), nonce, state_nonce);
			return Err(TransactionError::Old);
		}

		// Update nonces of transactions in future (remove old transactions)
		self.update_future(&address, state_nonce);
		// State nonce could be updated. Maybe there are some more items waiting in future?
		self.move_matching_future_to_current(address, state_nonce, state_nonce);
		// Check the next expected nonce (might be updated by move above)
		let next_nonce = self.last_nonces
			.get(&address)
			.cloned()
			.map_or(state_nonce, |n| n + U256::one());

		if tx.origin.is_local() {
			self.mark_transactions_local(&address);
		}

		// Future transaction
		if nonce > next_nonce {
			// We have a gap - put to future.
			// Insert transaction (or replace old one with lower gas price)
			try!(check_too_cheap(
				Self::replace_transaction(tx, state_nonce, min_gas_price, &mut self.future, &mut self.by_hash, &mut self.local_transactions)
			));
			// Enforce limit in Future
			let removed = self.future.enforce_limit(&mut self.by_hash, &mut self.local_transactions);
			// Return an error if this transaction was not imported because of limit.
			try!(check_if_removed(&address, &nonce, removed));

			debug!(target: "txqueue", "Importing transaction to future: {:?}", hash);
			debug!(target: "txqueue", "status: {:?}", self.status());
			return Ok(TransactionImportResult::Future);
		}

		// We might have filled a gap - move some more transactions from future
		self.move_matching_future_to_current(address, nonce, state_nonce);
		self.move_matching_future_to_current(address, nonce + U256::one(), state_nonce);

		// Replace transaction if any
		try!(check_too_cheap(
			Self::replace_transaction(tx, state_nonce, min_gas_price, &mut self.current, &mut self.by_hash, &mut self.local_transactions)
		));
		// Keep track of highest nonce stored in current
		let new_max = self.last_nonces.get(&address).map_or(nonce, |n| cmp::max(nonce, *n));
		self.last_nonces.insert(address, new_max);

		// Also enforce the limit
		let removed = self.current.enforce_limit(&mut self.by_hash, &mut self.local_transactions);
		// If some transaction were removed because of limit we need to update last_nonces also.
		self.update_last_nonces(&removed);
		// Trigger error if the transaction we are importing was removed.
		try!(check_if_removed(&address, &nonce, removed));

		debug!(target: "txqueue", "Imported transaction to current: {:?}", hash);
		debug!(target: "txqueue", "status: {:?}", self.status());
		Ok(TransactionImportResult::Current)
	}

	/// Updates
	fn update_last_nonces(&mut self, removed_min_nonces: &Option<HashMap<Address, U256>>) {
		if let Some(ref min_nonces) = *removed_min_nonces {
			for (sender, nonce) in min_nonces.iter() {
				if *nonce == U256::zero() {
					self.last_nonces.remove(sender);
				} else {
					self.last_nonces.insert(*sender, *nonce - U256::one());
				}
			}
		}
	}

	/// Replaces transaction in given set (could be `future` or `current`).
	///
	/// If there is already transaction with same `(sender, nonce)` it will be replaced iff `gas_price` is higher.
	/// One of the transactions is dropped from set and also removed from queue entirely (from `by_hash`).
	///
	/// Returns `true` if transaction actually got to the queue (`false` if there was already a transaction with higher
	/// gas_price)
	fn replace_transaction(
		tx: VerifiedTransaction,
		base_nonce: U256,
		min_gas_price: (U256, PrioritizationStrategy),
		set: &mut TransactionSet,
		by_hash: &mut HashMap<H256, VerifiedTransaction>,
		local: &mut LocalTransactionsList,
	) -> bool {
		let order = TransactionOrder::for_transaction(&tx, base_nonce, min_gas_price.0, min_gas_price.1);
		let hash = tx.hash();
		let address = tx.sender();
		let nonce = tx.nonce();

		let old_hash = by_hash.insert(hash, tx);
		assert!(old_hash.is_none(), "Each hash has to be inserted exactly once.");

		trace!(target: "txqueue", "Inserting: {:?}", order);

		if let Some(old) = set.insert(address, nonce, order.clone()) {
			Self::replace_orders(address, nonce, old, order, set, by_hash, local)
		} else {
			true
		}
	}

	fn replace_orders(
		address: Address,
		nonce: U256,
		old: TransactionOrder,
		order: TransactionOrder,
		set: &mut TransactionSet,
		by_hash: &mut HashMap<H256, VerifiedTransaction>,
		local: &mut LocalTransactionsList,
	) -> bool {
		// There was already transaction in queue. Let's check which one should stay
		let old_hash = old.hash;
		let new_hash = order.hash;
		let old_fee = old.gas_price;
		let new_fee = order.gas_price;
		if old_fee.cmp(&new_fee) == Ordering::Greater {
			trace!(target: "txqueue", "Didn't insert transaction because gas price was too low: {:?} ({:?} stays in the queue)", order.hash, old.hash);
			// Put back old transaction since it has greater priority (higher gas_price)
			set.insert(address, nonce, old);
			// and remove new one
			let order = by_hash.remove(&order.hash).expect("The hash has been just inserted and no other line is altering `by_hash`.");
			if order.origin.is_local() {
				local.mark_replaced(order.transaction, old_fee, old_hash);
			}
			false
		} else {
			trace!(target: "txqueue", "Replaced transaction: {:?} with transaction with higher gas price: {:?}", old.hash, order.hash);
			// Make sure we remove old transaction entirely
			let old = by_hash.remove(&old.hash).expect("The hash is coming from `future` so it has to be in `by_hash`.");
			if old.origin.is_local() {
				local.mark_replaced(old.transaction, new_fee, new_hash);
			}
			true
		}
	}
}

fn check_too_cheap(is_in: bool) -> Result<(), TransactionError> {
	if is_in {
		Ok(())
	} else {
		Err(TransactionError::TooCheapToReplace)
	}
}

fn check_if_removed(sender: &Address, nonce: &U256, dropped: Option<HashMap<Address, U256>>) -> Result<(), TransactionError> {
	match dropped {
		Some(ref dropped) => match dropped.get(sender) {
			Some(min) if nonce >= min => {
				Err(TransactionError::LimitReached)
			},
			_ => Ok(()),
		},
		_ => Ok(()),
	}
}


#[cfg(test)]
mod test {
	extern crate rustc_serialize;
	use util::table::*;
	use util::*;
	use ethkey::{Random, Generator};
	use error::{Error, TransactionError};
	use super::*;
	use super::{TransactionSet, TransactionOrder, VerifiedTransaction};
	use miner::local_transactions::LocalTransactionsList;
	use client::TransactionImportResult;
	use transaction::{SignedTransaction, Transaction, Action};

	fn unwrap_tx_err(err: Result<TransactionImportResult, Error>) -> TransactionError {
		match err.unwrap_err() {
			Error::Transaction(e) => e,
			_ => panic!("Expected transaction error!"),
		}
	}

	fn default_nonce() -> U256 { 123.into() }
	fn default_gas_val() -> U256 { 100_000.into() }
	fn default_gas_price() -> U256 { 1.into() }

	fn new_unsigned_tx(nonce: U256, gas: U256, gas_price: U256) -> Transaction {
		Transaction {
			action: Action::Create,
			value: U256::from(100),
			data: "3331600055".from_hex().unwrap(),
			gas: gas,
			gas_price: gas_price,
			nonce: nonce
		}
	}

	fn new_tx(nonce: U256, gas_price: U256) -> SignedTransaction {
		let keypair = Random.generate().unwrap();
		new_unsigned_tx(nonce, default_gas_val(), gas_price).sign(keypair.secret(), None)
	}

	fn new_tx_with_gas(gas: U256, gas_price: U256) -> SignedTransaction {
		let keypair = Random.generate().unwrap();
		new_unsigned_tx(default_nonce(), gas, gas_price).sign(keypair.secret(), None)
	}

	fn new_tx_default() -> SignedTransaction {
		new_tx(default_nonce(), default_gas_price())
	}

	fn default_account_details(_address: &Address) -> AccountDetails {
		AccountDetails {
			nonce: default_nonce(),
			balance: !U256::zero()
		}
	}

	fn gas_estimator(_tx: &SignedTransaction) -> U256 {
		U256::zero()
	}

	fn new_tx_pair(nonce: U256, gas_price: U256, nonce_increment: U256, gas_price_increment: U256) -> (SignedTransaction, SignedTransaction) {
		let tx1 = new_unsigned_tx(nonce, default_gas_val(), gas_price);
		let tx2 = new_unsigned_tx(nonce + nonce_increment, default_gas_val(), gas_price + gas_price_increment);

		let keypair = Random.generate().unwrap();
		let secret = &keypair.secret();
		(tx1.sign(secret, None), tx2.sign(secret, None))
	}

	/// Returns two consecutive transactions, both with increased gas price
	fn new_tx_pair_with_gas_price_increment(gas_price_increment: U256) -> (SignedTransaction, SignedTransaction) {
		let gas = default_gas_price() + gas_price_increment;
		let tx1 = new_unsigned_tx(default_nonce(), default_gas_val(), gas);
		let tx2 = new_unsigned_tx(default_nonce() + 1.into(), default_gas_val(), gas);

		let keypair = Random.generate().unwrap();
		let secret = &keypair.secret();
		(tx1.sign(secret, None), tx2.sign(secret, None))
	}

	fn new_tx_pair_default(nonce_increment: U256, gas_price_increment: U256) -> (SignedTransaction, SignedTransaction) {
		new_tx_pair(default_nonce(), default_gas_price(), nonce_increment, gas_price_increment)
	}

	/// Returns two transactions with identical (sender, nonce) but different gas price/hash.
	fn new_similar_tx_pair() -> (SignedTransaction, SignedTransaction) {
		new_tx_pair_default(0.into(), 1.into())
	}

	#[test]
	fn test_ordering() {
		assert_eq!(TransactionOrigin::Local.cmp(&TransactionOrigin::External), Ordering::Less);
		assert_eq!(TransactionOrigin::RetractedBlock.cmp(&TransactionOrigin::Local), Ordering::Less);
		assert_eq!(TransactionOrigin::RetractedBlock.cmp(&TransactionOrigin::External), Ordering::Less);

		assert_eq!(TransactionOrigin::External.cmp(&TransactionOrigin::Local), Ordering::Greater);
		assert_eq!(TransactionOrigin::Local.cmp(&TransactionOrigin::RetractedBlock), Ordering::Greater);
		assert_eq!(TransactionOrigin::External.cmp(&TransactionOrigin::RetractedBlock), Ordering::Greater);
	}

	fn transaction_order(tx: &VerifiedTransaction, nonce: U256) -> TransactionOrder {
		TransactionOrder::for_transaction(tx, nonce, 0.into(), PrioritizationStrategy::GasPriceOnly)
	}

	#[test]
	fn should_return_correct_nonces_when_dropped_because_of_limit() {
		// given
		let mut txq = TransactionQueue::with_limits(PrioritizationStrategy::GasPriceOnly, 2, !U256::zero(), !U256::zero());
		let (tx1, tx2) = new_tx_pair(123.into(), 1.into(), 1.into(), 0.into());
		let sender = tx1.sender().unwrap();
		let nonce = tx1.nonce;
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 2);
		assert_eq!(txq.last_nonce(&sender), Some(nonce + 1.into()));

		// when
		let tx = new_tx(123.into(), 1.into());
		let res = txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		// No longer the case as we don't even consider a transaction that isn't above a full
		// queue's minimum gas price.
		// We may want to reconsider this in the near future so leaving this code in as a
		// possible alternative.
		/*
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(txq.status().pending, 2);
		assert_eq!(txq.last_nonce(&sender), Some(nonce));
		*/
		assert_eq!(unwrap_tx_err(res), TransactionError::InsufficientGasPrice {
			minimal: 2.into(),
			got: 1.into(),
		});
		assert_eq!(txq.status().pending, 2);
		assert_eq!(txq.last_nonce(&sender), Some(tx2.nonce));
	}

	#[test]
	fn should_create_transaction_set() {
		// given
		let mut local = LocalTransactionsList::default();
		let mut set = TransactionSet {
			by_priority: BTreeSet::new(),
			by_address: Table::new(),
			by_gas_price: Default::default(),
			limit: 1,
			gas_limit: !U256::zero(),
		};
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		let tx1 = VerifiedTransaction::new(tx1, TransactionOrigin::External).unwrap();
		let tx2 = VerifiedTransaction::new(tx2, TransactionOrigin::External).unwrap();
		let mut by_hash = {
			let mut x = HashMap::new();
			let tx1 = VerifiedTransaction::new(tx1.transaction.clone(), TransactionOrigin::External).unwrap();
			let tx2 = VerifiedTransaction::new(tx2.transaction.clone(), TransactionOrigin::External).unwrap();
			x.insert(tx1.hash(), tx1);
			x.insert(tx2.hash(), tx2);
			x
		};
		// Insert both transactions
		let order1 = transaction_order(&tx1, U256::zero());
		set.insert(tx1.sender(), tx1.nonce(), order1.clone());
		let order2 = transaction_order(&tx2, U256::zero());
		set.insert(tx2.sender(), tx2.nonce(), order2.clone());
		assert_eq!(set.by_priority.len(), 2);
		assert_eq!(set.by_address.len(), 2);

		// when
		set.enforce_limit(&mut by_hash, &mut local);

		// then
		assert_eq!(by_hash.len(), 1);
		assert_eq!(set.by_priority.len(), 1);
		assert_eq!(set.by_address.len(), 1);
		assert_eq!(set.by_priority.iter().next().unwrap().clone(), order1);
		set.clear();
		assert_eq!(set.by_priority.len(), 0);
		assert_eq!(set.by_address.len(), 0);
	}

	#[test]
	fn should_replace_transaction_in_set() {
		let mut set = TransactionSet {
			by_priority: BTreeSet::new(),
			by_address: Table::new(),
			by_gas_price: Default::default(),
			limit: 1,
			gas_limit: !U256::zero(),
		};
		// Create two transactions with same nonce
		// (same hash)
		let (tx1, tx2) = new_tx_pair_default(0.into(), 0.into());
		let tx1 = VerifiedTransaction::new(tx1, TransactionOrigin::External).unwrap();
		let tx2 = VerifiedTransaction::new(tx2, TransactionOrigin::External).unwrap();
		let by_hash = {
			let mut x = HashMap::new();
			let tx1 = VerifiedTransaction::new(tx1.transaction.clone(), TransactionOrigin::External).unwrap();
			let tx2 = VerifiedTransaction::new(tx2.transaction.clone(), TransactionOrigin::External).unwrap();
			x.insert(tx1.hash(), tx1);
			x.insert(tx2.hash(), tx2);
			x
		};
		// Insert both transactions
		let order1 = transaction_order(&tx1, U256::zero());
		set.insert(tx1.sender(), tx1.nonce(), order1.clone());
		assert_eq!(set.by_priority.len(), 1);
		assert_eq!(set.by_address.len(), 1);
		assert_eq!(set.by_gas_price.len(), 1);
		assert_eq!(*set.by_gas_price.iter().next().unwrap().0, 1.into());
		assert_eq!(set.by_gas_price.iter().next().unwrap().1.len(), 1);
		// Two different orders (imagine nonce changed in the meantime)
		let order2 = transaction_order(&tx2, U256::one());
		set.insert(tx2.sender(), tx2.nonce(), order2.clone());
		assert_eq!(set.by_priority.len(), 1);
		assert_eq!(set.by_address.len(), 1);
		assert_eq!(set.by_gas_price.len(), 1);
		assert_eq!(*set.by_gas_price.iter().next().unwrap().0, 1.into());
		assert_eq!(set.by_gas_price.iter().next().unwrap().1.len(), 1);

		// then
		assert_eq!(by_hash.len(), 1);
		assert_eq!(set.by_priority.len(), 1);
		assert_eq!(set.by_address.len(), 1);
		assert_eq!(set.by_gas_price.len(), 1);
		assert_eq!(*set.by_gas_price.iter().next().unwrap().0, 1.into());
		assert_eq!(set.by_gas_price.iter().next().unwrap().1.len(), 1);
		assert_eq!(set.by_priority.iter().next().unwrap().clone(), order2);
	}

	#[test]
	fn should_not_insert_same_transaction_twice_into_set() {
		let mut set = TransactionSet {
			by_priority: BTreeSet::new(),
			by_address: Table::new(),
			by_gas_price: Default::default(),
			limit: 2,
			gas_limit: !U256::zero(),
		};
		let tx = new_tx_default();
		let tx1 = VerifiedTransaction::new(tx.clone(), TransactionOrigin::External).unwrap();
		let order1 = TransactionOrder::for_transaction(&tx1, 0.into(), 1.into(), PrioritizationStrategy::GasPriceOnly);
		assert!(set.insert(tx1.sender(), tx1.nonce(), order1).is_none());
		let tx2 = VerifiedTransaction::new(tx, TransactionOrigin::External).unwrap();
		let order2 = TransactionOrder::for_transaction(&tx2, 0.into(), 1.into(), PrioritizationStrategy::GasPriceOnly);
		assert!(set.insert(tx2.sender(), tx2.nonce(), order2).is_some());
	}

	#[test]
	fn should_give_correct_gas_price_entry_limit() {
		let mut set = TransactionSet {
			by_priority: BTreeSet::new(),
			by_address: Table::new(),
			by_gas_price: Default::default(),
			limit: 1,
			gas_limit: !U256::zero(),
		};

		assert_eq!(set.gas_price_entry_limit(), 0.into());
		let tx = new_tx_default();
		let tx1 = VerifiedTransaction::new(tx.clone(), TransactionOrigin::External).unwrap();
		let order1 = TransactionOrder::for_transaction(&tx1, 0.into(), 1.into(), PrioritizationStrategy::GasPriceOnly);
		assert!(set.insert(tx1.sender(), tx1.nonce(), order1.clone()).is_none());
		assert_eq!(set.gas_price_entry_limit(), 2.into());
	}

	#[test]
	fn should_handle_same_transaction_imported_twice_with_different_state_nonces() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx, tx2) = new_similar_tx_pair();
		let prev_nonce = |a: &Address| AccountDetails{ nonce: default_account_details(a).nonce - U256::one(), balance:
			!U256::zero() };

		// First insert one transaction to future
		let res = txq.add(tx, TransactionOrigin::External, &prev_nonce, &gas_estimator);
		assert_eq!(res.unwrap(), TransactionImportResult::Future);
		assert_eq!(txq.status().future, 1);

		// now import second transaction to current
		let res = txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator);

		// and then there should be only one transaction in current (the one with higher gas_price)
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(txq.status().pending, 1);
		assert_eq!(txq.status().future, 0);
		assert_eq!(txq.current.by_priority.len(), 1);
		assert_eq!(txq.current.by_address.len(), 1);
		assert_eq!(txq.top_transactions()[0], tx2);
	}

	#[test]
	fn should_move_all_transactions_from_future() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx, tx2) = new_tx_pair_default(1.into(), 1.into());
		let prev_nonce = |a: &Address| AccountDetails{ nonce: default_account_details(a).nonce - U256::one(), balance:
			!U256::zero() };

		// First insert one transaction to future
		let res = txq.add(tx.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator);
		assert_eq!(res.unwrap(), TransactionImportResult::Future);
		assert_eq!(txq.status().future, 1);

		// now import second transaction to current
		let res = txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(txq.status().pending, 2);
		assert_eq!(txq.status().future, 0);
		assert_eq!(txq.current.by_priority.len(), 2);
		assert_eq!(txq.current.by_address.len(), 2);
		assert_eq!(txq.top_transactions()[0], tx);
		assert_eq!(txq.top_transactions()[1], tx2);
	}

	#[test]
	fn should_import_tx() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();

		// when
		let res = txq.add(tx, TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		let stats = txq.status();
		assert_eq!(stats.pending, 1);
	}

	#[test]
	fn should_order_by_gas() {
		// given
		let mut txq = TransactionQueue::new(PrioritizationStrategy::GasAndGasPrice);
		let tx1 = new_tx_with_gas(50000.into(), 40.into());
		let tx2 = new_tx_with_gas(40000.into(), 30.into());
		let tx3 = new_tx_with_gas(30000.into(), 10.into());
		let tx4 = new_tx_with_gas(50000.into(), 20.into());
		txq.set_minimal_gas_price(15.into());

		// when
		let res1 = txq.add(tx1, TransactionOrigin::External, &default_account_details, &gas_estimator);
		let res2 = txq.add(tx2, TransactionOrigin::External, &default_account_details, &gas_estimator);
		let res3 = txq.add(tx3, TransactionOrigin::External, &default_account_details, &gas_estimator);
		let res4 = txq.add(tx4, TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert_eq!(res1.unwrap(), TransactionImportResult::Current);
		assert_eq!(res2.unwrap(), TransactionImportResult::Current);
		assert_eq!(unwrap_tx_err(res3), TransactionError::InsufficientGasPrice {
			minimal: U256::from(15),
			got: U256::from(10),
		});
		assert_eq!(res4.unwrap(), TransactionImportResult::Current);
		let stats = txq.status();
		assert_eq!(stats.pending, 3);
		assert_eq!(txq.top_transactions()[0].gas, 40000.into());
		assert_eq!(txq.top_transactions()[1].gas, 50000.into());
		assert_eq!(txq.top_transactions()[2].gas, 50000.into());
		assert_eq!(txq.top_transactions()[1].gas_price, 40.into());
		assert_eq!(txq.top_transactions()[2].gas_price, 20.into());
	}

	#[test]
	fn should_order_by_gas_factor() {
		// given
		let mut txq = TransactionQueue::new(PrioritizationStrategy::GasFactorAndGasPrice);

		let tx1 = new_tx_with_gas(150_000.into(), 40.into());
		let tx2 = new_tx_with_gas(40_000.into(), 16.into());
		let tx3 = new_tx_with_gas(30_000.into(), 15.into());
		let tx4 = new_tx_with_gas(150_000.into(), 62.into());
		txq.set_minimal_gas_price(15.into());

		// when
		let res1 = txq.add(tx1, TransactionOrigin::External, &default_account_details, &gas_estimator);
		let res2 = txq.add(tx2, TransactionOrigin::External, &default_account_details, &gas_estimator);
		let res3 = txq.add(tx3, TransactionOrigin::External, &default_account_details, &gas_estimator);
		let res4 = txq.add(tx4, TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert_eq!(res1.unwrap(), TransactionImportResult::Current);
		assert_eq!(res2.unwrap(), TransactionImportResult::Current);
		assert_eq!(res3.unwrap(), TransactionImportResult::Current);
		assert_eq!(res4.unwrap(), TransactionImportResult::Current);
		let stats = txq.status();
		assert_eq!(stats.pending, 4);
		assert_eq!(txq.top_transactions()[0].gas, 30_000.into());
		assert_eq!(txq.top_transactions()[1].gas, 150_000.into());
		assert_eq!(txq.top_transactions()[2].gas, 40_000.into());
		assert_eq!(txq.top_transactions()[3].gas, 150_000.into());
		assert_eq!(txq.top_transactions()[0].gas_price, 15.into());
		assert_eq!(txq.top_transactions()[1].gas_price, 62.into());
		assert_eq!(txq.top_transactions()[2].gas_price, 16.into());
		assert_eq!(txq.top_transactions()[3].gas_price, 40.into());
	}

	#[test]
	fn gas_limit_should_never_overflow() {
		// given
		let mut txq = TransactionQueue::default();
		txq.set_gas_limit(U256::zero());
		assert_eq!(txq.gas_limit, U256::zero());

		// when
		txq.set_gas_limit(!U256::zero());

		// then
		assert_eq!(txq.gas_limit, !U256::zero());
	}

	#[test]
	fn should_not_import_transaction_above_gas_limit() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		let gas = tx.gas;
		let limit = gas / U256::from(2);
		txq.set_gas_limit(limit);

		// when
		let res = txq.add(tx, TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert_eq!(unwrap_tx_err(res), TransactionError::GasLimitExceeded {
			limit: U256::from(55_000), // Should be 110% of set_gas_limit
			got: gas,
		});
		let stats = txq.status();
		assert_eq!(stats.pending, 0);
		assert_eq!(stats.future, 0);
	}


	#[test]
	fn should_drop_transactions_from_senders_without_balance() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		let account = |a: &Address| AccountDetails {
			nonce: default_account_details(a).nonce,
			balance: U256::one()
		};

		// when
		let res = txq.add(tx, TransactionOrigin::External, &account, &gas_estimator);

		// then
		assert_eq!(unwrap_tx_err(res), TransactionError::InsufficientBalance {
			balance: U256::from(1),
			cost: U256::from(100_100),
		});
		let stats = txq.status();
		assert_eq!(stats.pending, 0);
		assert_eq!(stats.future, 0);
	}

	#[test]
	fn should_not_import_transaction_below_min_gas_price_threshold_if_external() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		txq.set_minimal_gas_price(tx.gas_price + U256::one());

		// when
		let res = txq.add(tx, TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert_eq!(unwrap_tx_err(res), TransactionError::InsufficientGasPrice {
			minimal: U256::from(2),
			got: U256::from(1),
		});
		let stats = txq.status();
		assert_eq!(stats.pending, 0);
		assert_eq!(stats.future, 0);
	}

	#[test]
	fn should_import_transaction_below_min_gas_price_threshold_if_local() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		txq.set_minimal_gas_price(tx.gas_price + U256::one());

		// when
		let res = txq.add(tx, TransactionOrigin::Local, &default_account_details, &gas_estimator);

		// then
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		let stats = txq.status();
		assert_eq!(stats.pending, 1);
		assert_eq!(stats.future, 0);
	}

	#[test]
	fn should_reject_incorectly_signed_transaction() {
		use rlp::{self, RlpStream, Stream};

		// given
		let mut txq = TransactionQueue::default();
		let tx = new_unsigned_tx(123.into(), 100.into(), 1.into());
		let stx = {
			let mut s = RlpStream::new_list(9);
			s.append(&tx.nonce);
			s.append(&tx.gas_price);
			s.append(&tx.gas);
			s.append_empty_data(); // action=create
			s.append(&tx.value);
			s.append(&tx.data);
			s.append(&0u64); // v
			s.append(&U256::zero()); // r
			s.append(&U256::zero()); // s
			rlp::decode(s.as_raw())
		};
		// when
		let res = txq.add(stx, TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		assert!(res.is_err());
	}

	#[test]
	fn should_import_txs_from_same_sender() {
		// given
		let mut txq = TransactionQueue::default();

		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());

		// when
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		let top = txq.top_transactions();
		assert_eq!(top[0], tx);
		assert_eq!(top[1], tx2);
		assert_eq!(top.len(), 2);
	}

	#[test]
	fn should_prioritize_local_transactions_within_same_nonce_height() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		// the second one has same nonce but higher `gas_price`
		let (_, tx2) = new_similar_tx_pair();

		// when
		// first insert the one with higher gas price
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		// then the one with lower gas price, but local
		txq.add(tx.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();

		// then
		let top = txq.top_transactions();
		assert_eq!(top[0], tx); // local should be first
		assert_eq!(top[1], tx2);
		assert_eq!(top.len(), 2);
	}

	#[test]
	fn when_importing_local_should_mark_others_from_the_same_sender_as_local() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		// the second one has same nonce but higher `gas_price`
		let (_, tx0) = new_similar_tx_pair();

		txq.add(tx0.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		// the one with higher gas price is first
		assert_eq!(txq.top_transactions()[0], tx0);
		assert_eq!(txq.top_transactions()[1], tx1);

		// when
		// insert second as local
		txq.add(tx2.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();

		// then
		// the order should be updated
		assert_eq!(txq.top_transactions()[0], tx1);
		assert_eq!(txq.top_transactions()[1], tx2);
		assert_eq!(txq.top_transactions()[2], tx0);
	}

	#[test]
	fn should_prioritize_reimported_transactions_within_same_nonce_height() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		// the second one has same nonce but higher `gas_price`
		let (_, tx2) = new_similar_tx_pair();

		// when
		// first insert local one with higher gas price
		txq.add(tx2.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		// then the one with lower gas price, but from retracted block
		txq.add(tx.clone(), TransactionOrigin::RetractedBlock, &default_account_details, &gas_estimator).unwrap();

		// then
		let top = txq.top_transactions();
		assert_eq!(top[0], tx); // retracted should be first
		assert_eq!(top[1], tx2);
		assert_eq!(top.len(), 2);
	}

	#[test]
	fn should_not_prioritize_local_transactions_with_different_nonce_height() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());

		// when
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();

		// then
		let top = txq.top_transactions();
		assert_eq!(top[0], tx);
		assert_eq!(top[1], tx2);
		assert_eq!(top.len(), 2);
	}

	#[test]
	fn should_penalize_transactions_from_sender_in_future() {
		// given
		let prev_nonce = |a: &Address| AccountDetails{ nonce: default_account_details(a).nonce - U256::one(), balance: !U256::zero() };
		let mut txq = TransactionQueue::default();
		// txa, txb - slightly bigger gas price to have consistent ordering
		let (txa, txb) = new_tx_pair_default(1.into(), 0.into());
		let (tx1, tx2) = new_tx_pair_with_gas_price_increment(3.into());

		// insert everything
		txq.add(txa.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap();
		txq.add(txb.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap();
		txq.add(tx1.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap();

		assert_eq!(txq.status().future, 4);

		// when
		txq.penalize(&tx1.hash());

		// then
		let top = txq.future_transactions();
		assert_eq!(top[0], txa);
		assert_eq!(top[1], txb);
		assert_eq!(top[2], tx1);
		assert_eq!(top[3], tx2);
		assert_eq!(top.len(), 4);
	}

	#[test]
	fn should_not_penalize_local_transactions() {
		// given
		let mut txq = TransactionQueue::default();
		// txa, txb - slightly bigger gas price to have consistent ordering
		let (txa, txb) = new_tx_pair_default(1.into(), 0.into());
		let (tx1, tx2) = new_tx_pair_with_gas_price_increment(3.into());

		// insert everything
		txq.add(txa.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(txb.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx1.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();

		let top = txq.top_transactions();
		assert_eq!(top[0], tx1);
		assert_eq!(top[1], txa);
		assert_eq!(top[2], tx2);
		assert_eq!(top[3], txb);
		assert_eq!(top.len(), 4);

		// when
		txq.penalize(&tx1.hash());

		// then (order is the same)
		let top = txq.top_transactions();
		assert_eq!(top[0], tx1);
		assert_eq!(top[1], txa);
		assert_eq!(top[2], tx2);
		assert_eq!(top[3], txb);
		assert_eq!(top.len(), 4);
	}

	#[test]
	fn should_penalize_transactions_from_sender() {
		// given
		let mut txq = TransactionQueue::default();
		// txa, txb - slightly bigger gas price to have consistent ordering
		let (txa, txb) = new_tx_pair_default(1.into(), 0.into());
		let (tx1, tx2) = new_tx_pair_with_gas_price_increment(3.into());

		// insert everything
		txq.add(txa.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(txb.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		let top = txq.top_transactions();
		assert_eq!(top[0], tx1);
		assert_eq!(top[1], txa);
		assert_eq!(top[2], tx2);
		assert_eq!(top[3], txb);
		assert_eq!(top.len(), 4);

		// when
		txq.penalize(&tx1.hash());

		// then
		let top = txq.top_transactions();
		assert_eq!(top[0], txa);
		assert_eq!(top[1], txb);
		assert_eq!(top[2], tx1);
		assert_eq!(top[3], tx2);
		assert_eq!(top.len(), 4);
	}

	#[test]
	fn should_return_pending_hashes() {
			// given
		let mut txq = TransactionQueue::default();

		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());

		// when
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		let top = txq.pending_hashes();
		assert_eq!(top[0], tx.hash());
		assert_eq!(top[1], tx2.hash());
		assert_eq!(top.len(), 2);
	}

	#[test]
	fn should_put_transaction_to_futures_if_gap_detected() {
		// given
		let mut txq = TransactionQueue::default();

		let (tx, tx2) = new_tx_pair_default(2.into(), 0.into());

		// when
		let res1 = txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		let res2 = txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		assert_eq!(res1, TransactionImportResult::Current);
		assert_eq!(res2, TransactionImportResult::Future);
		let stats = txq.status();
		assert_eq!(stats.pending, 1);
		assert_eq!(stats.future, 1);
		let top = txq.top_transactions();
		assert_eq!(top.len(), 1);
		assert_eq!(top[0], tx);
	}

	#[test]
	fn should_correctly_update_futures_when_removing() {
		// given
		let prev_nonce = |a: &Address| AccountDetails{ nonce: default_account_details(a).nonce - U256::one(), balance:
			!U256::zero() };
		let next2_nonce = default_nonce() + U256::from(3);

		let mut txq = TransactionQueue::default();

		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());
		txq.add(tx.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 2);

		// when
		txq.remove_all(tx.sender().unwrap(), next2_nonce);
		// should remove both transactions since they are not valid

		// then
		assert_eq!(txq.status().pending, 0);
		assert_eq!(txq.status().future, 0);
	}

	#[test]
	fn should_move_transactions_if_gap_filled() {
		// given
		let mut txq = TransactionQueue::default();
		let kp = Random.generate().unwrap();
		let secret = kp.secret();
		let tx = new_unsigned_tx(123.into(), default_gas_val(), 1.into()).sign(secret, None);
		let tx1 = new_unsigned_tx(124.into(), default_gas_val(), 1.into()).sign(secret, None);
		let tx2 = new_unsigned_tx(125.into(), default_gas_val(), 1.into()).sign(secret, None);

		txq.add(tx, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 1);
		txq.add(tx2, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 1);

		// when
		txq.add(tx1, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		let stats = txq.status();
		assert_eq!(stats.pending, 3);
		assert_eq!(stats.future, 0);
		assert_eq!(txq.future.by_priority.len(), 0);
		assert_eq!(txq.future.by_address.len(), 0);
		assert_eq!(txq.future.by_gas_price.len(), 0);
	}

	#[test]
	fn should_remove_transaction() {
		// given
		let mut txq2 = TransactionQueue::default();
		let (tx, tx2) = new_tx_pair_default(3.into(), 0.into());
		txq2.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq2.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq2.status().pending, 1);
		assert_eq!(txq2.status().future, 1);

		// when
		txq2.remove_all(tx.sender().unwrap(), tx.nonce + U256::one());
		txq2.remove_all(tx2.sender().unwrap(), tx2.nonce + U256::one());


		// then
		let stats = txq2.status();
		assert_eq!(stats.pending, 0);
		assert_eq!(stats.future, 0);
	}

	#[test]
	fn should_move_transactions_to_future_if_gap_introduced() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());
		let tx3 = new_tx_default();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 1);
		txq.add(tx3.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 3);

		// when
		txq.remove_invalid(&tx.hash(), &default_account_details);

		// then
		let stats = txq.status();
		assert_eq!(stats.future, 1);
		assert_eq!(stats.pending, 1);
	}

	#[test]
	fn should_clear_queue() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());

		// add
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		let stats = txq.status();
		assert_eq!(stats.pending, 2);

		// when
		txq.clear();

		// then
		let stats = txq.status();
		assert_eq!(stats.pending, 0);
	}

	#[test]
	fn should_drop_old_transactions_when_hitting_the_limit() {
		// given
		let mut txq = TransactionQueue::with_limits(PrioritizationStrategy::GasPriceOnly, 1, !U256::zero(), !U256::zero());
		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());
		let sender = tx.sender().unwrap();
		let nonce = tx.nonce;
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 1);

		// when
		let res = txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator);

		// then
		let t = txq.top_transactions();
		assert_eq!(unwrap_tx_err(res), TransactionError::InsufficientGasPrice { minimal: 2.into(), got: 1.into() });
		assert_eq!(txq.status().pending, 1);
		assert_eq!(t.len(), 1);
		assert_eq!(t[0], tx);
		assert_eq!(txq.last_nonce(&sender), Some(nonce));
	}

	#[test]
	fn should_limit_future_transactions() {
		let mut txq = TransactionQueue::with_limits(PrioritizationStrategy::GasPriceOnly, 1, !U256::zero(), !U256::zero());
		txq.current.set_limit(10);
		let (tx1, tx2) = new_tx_pair_default(4.into(), 1.into());
		let (tx3, tx4) = new_tx_pair_default(4.into(), 2.into());
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx3.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 2);

		// when
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 1);
		txq.add(tx4.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		assert_eq!(txq.status().future, 1);
	}

	#[test]
	fn should_limit_by_gas() {
		let mut txq = TransactionQueue::with_limits(PrioritizationStrategy::GasPriceOnly, 100, default_gas_val() * U256::from(2), !U256::zero());
		let (tx1, tx2) = new_tx_pair_default(U256::from(1), U256::from(1));
		let (tx3, tx4) = new_tx_pair_default(U256::from(1), U256::from(2));
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx3.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		// limited by gas
		txq.add(tx4.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap_err();
		assert_eq!(txq.status().pending, 2);
	}

	#[test]
	fn should_keep_own_transactions_above_gas_limit() {
		let mut txq = TransactionQueue::with_limits(PrioritizationStrategy::GasPriceOnly, 100, default_gas_val() * U256::from(2), !U256::zero());
		let (tx1, tx2) = new_tx_pair_default(U256::from(1), U256::from(1));
		let (tx3, tx4) = new_tx_pair_default(U256::from(1), U256::from(2));
		let (tx5, _) = new_tx_pair_default(U256::from(1), U256::from(2));
		txq.add(tx1.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		// Not accepted because of limit
		txq.add(tx5.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap_err();
		txq.add(tx3.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx4.clone(), TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 4);
	}

	#[test]
	fn should_drop_transactions_with_old_nonces() {
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		let last_nonce = tx.nonce + U256::one();
		let fetch_last_nonce = |_a: &Address| AccountDetails { nonce: last_nonce, balance: !U256::zero() };

		// when
		let res = txq.add(tx, TransactionOrigin::External, &fetch_last_nonce, &gas_estimator);

		// then
		assert_eq!(unwrap_tx_err(res), TransactionError::Old);
		let stats = txq.status();
		assert_eq!(stats.pending, 0);
		assert_eq!(stats.future, 0);
	}

	#[test]
	fn should_not_insert_same_transaction_twice() {
		// given
		let nonce = |a: &Address| AccountDetails { nonce: default_account_details(a).nonce + U256::one(),
			balance: !U256::zero() };
		let mut txq = TransactionQueue::default();
		let (_tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 1);
		assert_eq!(txq.status().pending, 0);

		// when
		let res = txq.add(tx2.clone(), TransactionOrigin::External, &nonce, &gas_estimator);

		// then
		assert_eq!(unwrap_tx_err(res), TransactionError::AlreadyImported);
		let stats = txq.status();
		assert_eq!(stats.future, 1);
		assert_eq!(stats.pending, 0);
	}

	#[test]
	fn should_accept_same_transaction_twice_if_removed() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 2);

		// when
		txq.remove_invalid(&tx1.hash(), &default_account_details);
		assert_eq!(txq.status().pending, 0);
		assert_eq!(txq.status().future, 1);
		txq.add(tx1.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		let stats = txq.status();
		assert_eq!(stats.future, 0);
		assert_eq!(stats.pending, 2);
	}

	#[test]
	fn should_not_move_to_future_if_state_nonce_is_higher() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx, tx2) = new_tx_pair_default(1.into(), 0.into());
		let tx3 = new_tx_default();
		txq.add(tx2.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 1);
		txq.add(tx3.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx.clone(), TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().pending, 3);

		// when
		let sender = tx.sender().unwrap();
		txq.remove_all(sender, default_nonce() + U256::one());

		// then
		let stats = txq.status();
		assert_eq!(stats.future, 0);
		assert_eq!(stats.pending, 2);
	}

	#[test]
	fn should_replace_same_transaction_when_has_higher_fee() {
		init_log();
		// given
		let mut txq = TransactionQueue::default();
		let keypair = Random.generate().unwrap();
		let tx = new_unsigned_tx(123.into(), default_gas_val(), 1.into()).sign(keypair.secret(), None);
		let tx2 = {
			let mut tx2 = (*tx).clone();
			tx2.gas_price = U256::from(200);
			tx2.sign(keypair.secret(), None)
		};

		// when
		txq.add(tx, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		let stats = txq.status();
		assert_eq!(stats.pending, 1);
		assert_eq!(stats.future, 0);
		assert_eq!(txq.top_transactions()[0].gas_price, U256::from(200));
	}

	#[test]
	fn should_replace_same_transaction_when_importing_to_futures() {
		// given
		let mut txq = TransactionQueue::default();
		let keypair = Random.generate().unwrap();
		let tx0 = new_unsigned_tx(123.into(), default_gas_val(), 1.into()).sign(keypair.secret(), None);
		let tx1 = {
			let mut tx1 = (*tx0).clone();
			tx1.nonce = U256::from(124);
			tx1.sign(keypair.secret(), None)
		};
		let tx2 = {
			let mut tx2 = (*tx1).clone();
			tx2.gas_price = U256::from(200);
			tx2.sign(keypair.secret(), None)
		};

		// when
		txq.add(tx1, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 1);
		txq.add(tx0, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();

		// then
		let stats = txq.status();
		assert_eq!(stats.future, 0);
		assert_eq!(stats.pending, 2);
		assert_eq!(txq.top_transactions()[1].gas_price, U256::from(200));
	}

	#[test]
	fn should_recalculate_height_when_removing_from_future() {
		// given
		let previous_nonce = |a: &Address| AccountDetails{ nonce: default_account_details(a).nonce - U256::one(), balance:
			!U256::zero() };
		let next_nonce = |a: &Address| AccountDetails{ nonce: default_account_details(a).nonce + U256::one(), balance:
			!U256::zero() };
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		txq.add(tx1.clone(), TransactionOrigin::External, &previous_nonce, &gas_estimator).unwrap();
		txq.add(tx2, TransactionOrigin::External, &previous_nonce, &gas_estimator).unwrap();
		assert_eq!(txq.status().future, 2);

		// when
		txq.remove_invalid(&tx1.hash(), &next_nonce);

		// then
		let stats = txq.status();
		assert_eq!(stats.future, 0);
		assert_eq!(stats.pending, 1);
	}

	#[test]
	fn should_return_none_when_transaction_from_given_address_does_not_exist() {
		// given
		let txq = TransactionQueue::default();

		// then
		assert_eq!(txq.last_nonce(&Address::default()), None);
	}

	#[test]
	fn should_return_correct_nonce_when_transactions_from_given_address_exist() {
		// given
		let mut txq = TransactionQueue::default();
		let tx = new_tx_default();
		let from = tx.sender().unwrap();
		let nonce = tx.nonce;
		let details = |_a: &Address| AccountDetails { nonce: nonce, balance: !U256::zero() };

		// when
		txq.add(tx, TransactionOrigin::External, &details, &gas_estimator).unwrap();

		// then
		assert_eq!(txq.last_nonce(&from), Some(nonce));
	}

	#[test]
	fn should_remove_old_transaction_even_if_newer_transaction_was_not_known() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		let (nonce1, nonce2) = (tx1.nonce, tx2.nonce);
		let details1 = |_a: &Address| AccountDetails { nonce: nonce1, balance: !U256::zero() };

		// Insert first transaction
		txq.add(tx1, TransactionOrigin::External, &details1, &gas_estimator).unwrap();

		// when
		txq.remove_all(tx2.sender().unwrap(), nonce2 + U256::one());

		// then
		assert!(txq.top_transactions().is_empty());
	}

	#[test]
	fn should_return_valid_last_nonce_after_remove_all() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(4.into(), 0.into());
		let sender = tx1.sender().unwrap();
		let (nonce1, nonce2) = (tx1.nonce, tx2.nonce);
		let details1 = |_a: &Address| AccountDetails { nonce: nonce1, balance: !U256::zero() };

		// when
		// Insert first transaction
		assert_eq!(txq.add(tx1, TransactionOrigin::External, &details1, &gas_estimator).unwrap(), TransactionImportResult::Current);
		// Second should go to future
		assert_eq!(txq.add(tx2, TransactionOrigin::External, &details1, &gas_estimator).unwrap(), TransactionImportResult::Future);
		// Now block is imported
		txq.remove_all(sender, nonce2 - U256::from(1));
		// tx2 should be not be promoted to current
		assert_eq!(txq.status().pending, 0);
		assert_eq!(txq.status().future, 1);

		// then
		assert_eq!(txq.last_nonce(&sender), None);
	}

	#[test]
	fn should_return_true_if_there_is_local_transaction_pending() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		assert_eq!(txq.has_local_pending_transactions(), false);

		// when
		assert_eq!(txq.add(tx1, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap(), TransactionImportResult::Current);
		assert_eq!(txq.has_local_pending_transactions(), false);
		assert_eq!(txq.add(tx2, TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap(), TransactionImportResult::Current);

		// then
		assert_eq!(txq.has_local_pending_transactions(), true);
	}

	#[test]
	fn should_keep_right_order_in_future() {
		// given
		let mut txq = TransactionQueue::with_limits(PrioritizationStrategy::GasPriceOnly, 1, !U256::zero(), !U256::zero());
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		let prev_nonce = |a: &Address| AccountDetails { nonce: default_account_details(a).nonce - U256::one(), balance:
			default_account_details(a).balance };

		// when
		assert_eq!(txq.add(tx2, TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap(), TransactionImportResult::Future);
		assert_eq!(txq.add(tx1.clone(), TransactionOrigin::External, &prev_nonce, &gas_estimator).unwrap(), TransactionImportResult::Future);

		// then
		assert_eq!(txq.future.by_priority.len(), 1);
		assert_eq!(txq.future.by_priority.iter().next().unwrap().hash, tx1.hash());
	}

	#[test]
	fn should_return_correct_last_nonce() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2, tx2_2, tx3) = {
			let keypair = Random.generate().unwrap();
			let secret = &keypair.secret();
			let nonce = 123.into();
			let gas = default_gas_val();
			let tx = new_unsigned_tx(nonce, gas, 1.into());
			let tx2 = new_unsigned_tx(nonce + 1.into(), gas, 1.into());
			let tx2_2 = new_unsigned_tx(nonce + 1.into(), gas, 5.into());
			let tx3 = new_unsigned_tx(nonce + 2.into(), gas, 1.into());


			(tx.sign(secret, None), tx2.sign(secret, None), tx2_2.sign(secret, None), tx3.sign(secret, None))
		};
		let sender = tx1.sender().unwrap();
		txq.add(tx1, TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2, TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx3, TransactionOrigin::Local, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.future.by_priority.len(), 0);
		assert_eq!(txq.current.by_priority.len(), 3);

		// when
		let res = txq.add(tx2_2, TransactionOrigin::Local, &default_account_details, &gas_estimator);

		// then
		assert_eq!(txq.last_nonce(&sender).unwrap(), 125.into());
		assert_eq!(res.unwrap(), TransactionImportResult::Current);
		assert_eq!(txq.current.by_priority.len(), 3);
	}

	#[test]
	fn should_reject_transactions_below_base_gas() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		let high_gas = |_: &SignedTransaction| 100_001.into();

		// when
		let res1 = txq.add(tx1, TransactionOrigin::Local, &default_account_details, &gas_estimator);
		let res2 = txq.add(tx2, TransactionOrigin::Local, &default_account_details, &high_gas);

		// then
		assert_eq!(res1.unwrap(), TransactionImportResult::Current);
		assert_eq!(unwrap_tx_err(res2), TransactionError::InsufficientGas {
			minimal: 100_001.into(),
			got: 100_000.into(),
		});

	}

	#[test]
	fn should_clear_all_old_transactions() {
		// given
		let mut txq = TransactionQueue::default();
		let (tx1, tx2) = new_tx_pair_default(1.into(), 0.into());
		let (tx3, tx4) = new_tx_pair_default(1.into(), 0.into());
		let nonce1 = tx1.nonce;

		// Insert all transactions
		txq.add(tx1, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx2, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx3, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		txq.add(tx4, TransactionOrigin::External, &default_account_details, &gas_estimator).unwrap();
		assert_eq!(txq.top_transactions().len(), 4);

		// when
		txq.remove_old(|_| nonce1 + U256::one());

		// then
		assert_eq!(txq.top_transactions().len(), 2);
	}

}