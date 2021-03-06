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

//! A provider for the LES protocol. This is typically a full node, who can
//! give as much data as necessary to its peers.

use ethcore::blockchain_info::BlockChainInfo;
use ethcore::client::{BlockChainClient, ProvingBlockChainClient};
use ethcore::transaction::SignedTransaction;
use ethcore::ids::BlockId;

use util::{Bytes, H256};

use request;

/// Defines the operations that a provider for `LES` must fulfill.
///
/// These are defined at [1], but may be subject to change.
/// Requests which can't be fulfilled should return either an empty RLP list
/// or empty vector where appropriate.
///
/// [1]: https://github.com/ethcore/parity/wiki/Light-Ethereum-Subprotocol-(LES)
#[cfg_attr(feature = "ipc", ipc(client_ident="LightProviderClient"))]
pub trait Provider: Send + Sync {
	/// Provide current blockchain info.
	fn chain_info(&self) -> BlockChainInfo;

	/// Find the depth of a common ancestor between two blocks.
	/// If either block is unknown or an ancestor can't be found
	/// then return `None`.
	fn reorg_depth(&self, a: &H256, b: &H256) -> Option<u64>;

	/// Earliest block where state queries are available.
	/// If `None`, no state queries are servable.
	fn earliest_state(&self) -> Option<u64>;

	/// Provide a list of headers starting at the requested block,
	/// possibly in reverse and skipping `skip` at a time.
	///
	/// The returned vector may have any length in the range [0, `max`], but the
	/// results within must adhere to the `skip` and `reverse` parameters.
	fn block_headers(&self, req: request::Headers) -> Vec<Bytes>;

	/// Provide as many as possible of the requested blocks (minus the headers) encoded
	/// in RLP format.
	fn block_bodies(&self, req: request::Bodies) -> Vec<Bytes>;

	/// Provide the receipts as many as possible of the requested blocks.
	/// Returns a vector of RLP-encoded lists of receipts.
	fn receipts(&self, req: request::Receipts) -> Vec<Bytes>;

	/// Provide a set of merkle proofs, as requested. Each request is a
	/// block hash and request parameters.
	///
	/// Returns a vector of RLP-encoded lists satisfying the requests.
	fn proofs(&self, req: request::StateProofs) -> Vec<Bytes>;

	/// Provide contract code for the specified (block_hash, account_hash) pairs.
	/// Each item in the resulting vector is either the raw bytecode or empty.
	fn contract_code(&self, req: request::ContractCodes) -> Vec<Bytes>;

	/// Provide header proofs from the Canonical Hash Tries as well as the headers 
	/// they correspond to -- each element in the returned vector is a 2-tuple.
	/// The first element is a block header and the second a merkle proof of 
	/// the header in a requested CHT.
	fn header_proofs(&self, req: request::HeaderProofs) -> Vec<Bytes>;

	/// Provide pending transactions.
	fn pending_transactions(&self) -> Vec<SignedTransaction>;
}

// Implementation of a light client data provider for a client.
impl<T: ProvingBlockChainClient + ?Sized> Provider for T {
	fn chain_info(&self) -> BlockChainInfo {
		BlockChainClient::chain_info(self)
	}

	fn reorg_depth(&self, a: &H256, b: &H256) -> Option<u64> {
		self.tree_route(a, b).map(|route| route.index as u64)
	}

	fn earliest_state(&self) -> Option<u64> {
		Some(self.pruning_info().earliest_state)
	}

	fn block_headers(&self, req: request::Headers) -> Vec<Bytes> {
		use request::HashOrNumber;
		use ethcore::views::HeaderView;

		let best_num = self.chain_info().best_block_number;
		let start_num = match req.start {
			HashOrNumber::Number(start_num) => start_num,
			HashOrNumber::Hash(hash) => match self.block_header(BlockId::Hash(hash)) {
				None => {
					trace!(target: "les_provider", "Unknown block hash {} requested", hash);
					return Vec::new();
				}
				Some(header) => {
					let num = HeaderView::new(&header).number();
					if req.max == 1 || self.block_hash(BlockId::Number(num)) != Some(hash) {
						// Non-canonical header or single header requested.
						return vec![header];
					}

					num
				}
			}
		};
		
		(0u64..req.max as u64)
			.map(|x: u64| x.saturating_mul(req.skip + 1))
			.take_while(|x| if req.reverse { x < &start_num } else { best_num - start_num >= *x })
			.map(|x| if req.reverse { start_num - x } else { start_num + x })
			.map(|x| self.block_header(BlockId::Number(x)))
			.take_while(|x| x.is_some())
			.flat_map(|x| x)
			.collect()
	}

	fn block_bodies(&self, req: request::Bodies) -> Vec<Bytes> {
		req.block_hashes.into_iter()
			.map(|hash| self.block_body(BlockId::Hash(hash)))
			.map(|body| body.unwrap_or_else(|| ::rlp::EMPTY_LIST_RLP.to_vec()))
			.collect()
	}

	fn receipts(&self, req: request::Receipts) -> Vec<Bytes> {
		req.block_hashes.into_iter()
			.map(|hash| self.block_receipts(&hash))
			.map(|receipts| receipts.unwrap_or_else(|| ::rlp::EMPTY_LIST_RLP.to_vec()))
			.collect()
	}

	fn proofs(&self, req: request::StateProofs) -> Vec<Bytes> {
		use rlp::{RlpStream, Stream};

		let mut results = Vec::with_capacity(req.requests.len());

		for request in req.requests {
			let proof = match request.key2 {
				Some(key2) => self.prove_storage(request.key1, key2, request.from_level, BlockId::Hash(request.block)),
				None => self.prove_account(request.key1, request.from_level, BlockId::Hash(request.block)),
			};

			let mut stream = RlpStream::new_list(proof.len());
			for node in proof {
				stream.append_raw(&node, 1);
			}

			results.push(stream.out());
		}

		results
	}

	fn contract_code(&self, req: request::ContractCodes) -> Vec<Bytes> {
		req.code_requests.into_iter()
			.map(|req| {
				self.code_by_hash(req.account_key, BlockId::Hash(req.block_hash))
			})
			.collect()
	}

	fn header_proofs(&self, req: request::HeaderProofs) -> Vec<Bytes> {
		req.requests.into_iter().map(|_| ::rlp::EMPTY_LIST_RLP.to_vec()).collect()
	}

	fn pending_transactions(&self) -> Vec<SignedTransaction> {
		BlockChainClient::pending_transactions(self)
	}
}