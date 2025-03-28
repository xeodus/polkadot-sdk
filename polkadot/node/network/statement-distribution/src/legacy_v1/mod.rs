// Copyright (C) Parity Technologies (UK) Ltd.
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
#![allow(dead_code)]
use codec::Encode;
use net_protocol::{filter_by_peer_version, peer_set::ProtocolVersion};

use polkadot_node_network_protocol::{
	self as net_protocol,
	grid_topology::{GridNeighbors, RequiredRouting, SessionBoundGridTopologyStorage},
	peer_set::{IsAuthority, PeerSet, ValidationVersion},
	v1::{self as protocol_v1, StatementMetadata},
	v2 as protocol_v2, v3 as protocol_v3, IfDisconnected, PeerId, UnifiedReputationChange as Rep,
	Versioned, View,
};
use polkadot_node_primitives::{
	SignedFullStatement, Statement, StatementWithPVD, UncheckedSignedFullStatement,
};
use polkadot_node_subsystem_util::{
	self as util, rand, reputation::ReputationAggregator, MIN_GOSSIP_PEERS,
};

use polkadot_node_subsystem::{
	messages::{CandidateBackingMessage, NetworkBridgeEvent, NetworkBridgeTxMessage},
	overseer, ActivatedLeaf, StatementDistributionSenderTrait,
};
use polkadot_primitives::{
	vstaging::CommittedCandidateReceiptV2 as CommittedCandidateReceipt, AuthorityDiscoveryId,
	CandidateHash, CompactStatement, Hash, Id as ParaId, IndexedVec, OccupiedCoreAssumption,
	PersistedValidationData, SignedStatement, SigningContext, UncheckedSignedStatement,
	ValidatorId, ValidatorIndex, ValidatorSignature,
};

use futures::{
	channel::{mpsc, oneshot},
	future::RemoteHandle,
	prelude::*,
};
use indexmap::{map::Entry as IEntry, IndexMap};
use rand::Rng;
use sp_keystore::KeystorePtr;
use util::runtime::RuntimeInfo;

use std::collections::{hash_map::Entry, HashMap, HashSet, VecDeque};

use crate::error::{Error, JfyiError, JfyiErrorResult, Result};

/// Background task logic for requesting of large statements.
mod requester;
use requester::fetch;

/// Background task logic for responding for large statements.
mod responder;

use crate::{metrics::Metrics, LOG_TARGET};

pub use requester::RequesterMessage;
pub use responder::{respond, ResponderMessage};

#[cfg(test)]
mod tests;

const COST_UNEXPECTED_STATEMENT: Rep = Rep::CostMinor("Unexpected Statement");
const COST_UNEXPECTED_STATEMENT_MISSING_KNOWLEDGE: Rep =
	Rep::CostMinor("Unexpected Statement, missing knowlege for relay parent");
const COST_UNEXPECTED_STATEMENT_UNKNOWN_CANDIDATE: Rep =
	Rep::CostMinor("Unexpected Statement, unknown candidate");
const COST_UNEXPECTED_STATEMENT_REMOTE: Rep =
	Rep::CostMinor("Unexpected Statement, remote not allowed");

const COST_FETCH_FAIL: Rep =
	Rep::CostMinor("Requesting `CommittedCandidateReceipt` from peer failed");
const COST_INVALID_SIGNATURE: Rep = Rep::CostMajor("Invalid Statement Signature");
const COST_WRONG_HASH: Rep = Rep::CostMajor("Received candidate had wrong hash");
const COST_DUPLICATE_STATEMENT: Rep =
	Rep::CostMajorRepeated("Statement sent more than once by peer");
const COST_APPARENT_FLOOD: Rep = Rep::Malicious("Peer appears to be flooding us with statements");

const BENEFIT_VALID_STATEMENT: Rep = Rep::BenefitMajor("Peer provided a valid statement");
const BENEFIT_VALID_STATEMENT_FIRST: Rep =
	Rep::BenefitMajorFirst("Peer was the first to provide a valid statement");
const BENEFIT_VALID_RESPONSE: Rep =
	Rep::BenefitMajor("Peer provided a valid large statement response");

/// The maximum amount of candidates each validator is allowed to second at any relay-parent.
/// Short for "Validator Candidate Threshold".
///
/// This is the amount of candidates we keep per validator at any relay-parent.
/// Typically we will only keep 1, but when a validator equivocates we will need to track 2.
const VC_THRESHOLD: usize = 2;

/// Large statements should be rare.
const MAX_LARGE_STATEMENTS_PER_SENDER: usize = 20;

/// Overall state of the legacy-v1 portion of the subsystem.
pub(crate) struct State {
	peers: HashMap<PeerId, PeerData>,
	topology_storage: SessionBoundGridTopologyStorage,
	authorities: HashMap<AuthorityDiscoveryId, PeerId>,
	active_heads: HashMap<Hash, ActiveHeadData>,
	recent_outdated_heads: RecentOutdatedHeads,
	runtime: RuntimeInfo,
}

impl State {
	/// Create a new state.
	pub(crate) fn new(keystore: KeystorePtr) -> Self {
		State {
			peers: HashMap::new(),
			topology_storage: Default::default(),
			authorities: HashMap::new(),
			active_heads: HashMap::new(),
			recent_outdated_heads: RecentOutdatedHeads::default(),
			runtime: RuntimeInfo::new(Some(keystore)),
		}
	}

	/// Query whether the state contains some relay-parent.
	pub(crate) fn contains_relay_parent(&self, relay_parent: &Hash) -> bool {
		self.active_heads.contains_key(relay_parent)
	}
}

#[derive(Default)]
struct RecentOutdatedHeads {
	buf: VecDeque<Hash>,
}

impl RecentOutdatedHeads {
	fn note_outdated(&mut self, hash: Hash) {
		const MAX_BUF_LEN: usize = 10;

		self.buf.push_back(hash);

		while self.buf.len() > MAX_BUF_LEN {
			let _ = self.buf.pop_front();
		}
	}

	fn is_recent_outdated(&self, hash: &Hash) -> bool {
		self.buf.contains(hash)
	}
}

/// Tracks our impression of a single peer's view of the candidates a validator has seconded
/// for a given relay-parent.
///
/// It is expected to receive at most `VC_THRESHOLD` from us and be aware of at most `VC_THRESHOLD`
/// via other means.
#[derive(Default)]
struct VcPerPeerTracker {
	local_observed: arrayvec::ArrayVec<CandidateHash, VC_THRESHOLD>,
	remote_observed: arrayvec::ArrayVec<CandidateHash, VC_THRESHOLD>,
}

impl VcPerPeerTracker {
	/// Note that the remote should now be aware that a validator has seconded a given candidate (by
	/// hash) based on a message that we have sent it from our local pool.
	fn note_local(&mut self, h: CandidateHash) {
		if !note_hash(&mut self.local_observed, h) {
			gum::warn!(
				target: LOG_TARGET,
				"Statement distribution is erroneously attempting to distribute more \
				than {} candidate(s) per validator index. Ignoring",
				VC_THRESHOLD,
			);
		}
	}

	/// Note that the remote should now be aware that a validator has seconded a given candidate (by
	/// hash) based on a message that it has sent us.
	///
	/// Returns `true` if the peer was allowed to send us such a message, `false` otherwise.
	fn note_remote(&mut self, h: CandidateHash) -> bool {
		note_hash(&mut self.remote_observed, h)
	}

	/// Returns `true` if the peer is allowed to send us such a message, `false` otherwise.
	fn is_wanted_candidate(&self, h: &CandidateHash) -> bool {
		!self.remote_observed.contains(h) && !self.remote_observed.is_full()
	}
}

fn note_hash(
	observed: &mut arrayvec::ArrayVec<CandidateHash, VC_THRESHOLD>,
	h: CandidateHash,
) -> bool {
	if observed.contains(&h) {
		return true
	}

	observed.try_push(h).is_ok()
}

/// knowledge that a peer has about goings-on in a relay parent.
#[derive(Default)]
struct PeerRelayParentKnowledge {
	/// candidates that the peer is aware of because we sent statements to it. This indicates that
	/// we can send other statements pertaining to that candidate.
	sent_candidates: HashSet<CandidateHash>,
	/// candidates that peer is aware of, because we received statements from it.
	received_candidates: HashSet<CandidateHash>,
	/// fingerprints of all statements a peer should be aware of: those that
	/// were sent to the peer by us.
	sent_statements: HashSet<(CompactStatement, ValidatorIndex)>,
	/// fingerprints of all statements a peer should be aware of: those that
	/// were sent to us by the peer.
	received_statements: HashSet<(CompactStatement, ValidatorIndex)>,
	/// How many candidates this peer is aware of for each given validator index.
	seconded_counts: HashMap<ValidatorIndex, VcPerPeerTracker>,
	/// How many statements we've received for each candidate that we're aware of.
	received_message_count: HashMap<CandidateHash, usize>,

	/// How many large statements this peer already sent us.
	///
	/// Flood protection for large statements is rather hard and as soon as we get
	/// `https://github.com/paritytech/polkadot/issues/2979` implemented also no longer necessary.
	/// Reason: We keep messages around until we fetched the payload, but if a node makes up
	/// statements and never provides the data, we will keep it around for the slot duration. Not
	/// even signature checking would help, as the sender, if a validator, can just sign arbitrary
	/// invalid statements and will not face any consequences as long as it won't provide the
	/// payload.
	///
	/// Quick and temporary fix, only accept `MAX_LARGE_STATEMENTS_PER_SENDER` per connected node.
	///
	/// Large statements should be rare, if they were not, we would run into problems anyways, as
	/// we would not be able to distribute them in a timely manner. Therefore
	/// `MAX_LARGE_STATEMENTS_PER_SENDER` can be set to a relatively small number. It is also not
	/// per candidate hash, but in total as candidate hashes can be made up, as illustrated above.
	///
	/// An attacker could still try to fill up our memory, by repeatedly disconnecting and
	/// connecting again with new peer ids, but we assume that the resulting effective bandwidth
	/// for such an attack would be too low.
	large_statement_count: usize,

	/// We have seen a message that that is unexpected from this peer, so note this fact
	/// and stop subsequent logging and peer reputation flood.
	unexpected_count: usize,
}

impl PeerRelayParentKnowledge {
	/// Updates our view of the peer's knowledge with this statement's fingerprint based
	/// on something that we would like to send to the peer.
	///
	/// NOTE: assumes `self.can_send` returned true before this call.
	///
	/// Once the knowledge has incorporated a statement, it cannot be incorporated again.
	///
	/// This returns `true` if this is the first time the peer has become aware of a
	/// candidate with the given hash.
	fn send(&mut self, fingerprint: &(CompactStatement, ValidatorIndex)) -> bool {
		debug_assert!(
			self.can_send(fingerprint),
			"send is only called after `can_send` returns true; qed",
		);

		let new_known = match fingerprint.0 {
			CompactStatement::Seconded(ref h) => {
				self.seconded_counts.entry(fingerprint.1).or_default().note_local(*h);

				let was_known = self.is_known_candidate(h);
				self.sent_candidates.insert(*h);
				!was_known
			},
			CompactStatement::Valid(_) => false,
		};

		self.sent_statements.insert(fingerprint.clone());

		new_known
	}

	/// This returns `true` if the peer cannot accept this statement, without altering internal
	/// state, `false` otherwise.
	fn can_send(&self, fingerprint: &(CompactStatement, ValidatorIndex)) -> bool {
		let already_known = self.sent_statements.contains(fingerprint) ||
			self.received_statements.contains(fingerprint);

		if already_known {
			return false
		}

		match fingerprint.0 {
			CompactStatement::Valid(ref h) => {
				// The peer can only accept Valid statements for which it is aware
				// of the corresponding candidate.
				self.is_known_candidate(h)
			},
			CompactStatement::Seconded(_) => true,
		}
	}

	/// Attempt to update our view of the peer's knowledge with this statement's fingerprint based
	/// on a message we are receiving from the peer.
	///
	/// Provide the maximum message count that we can receive per candidate. In practice we should
	/// not receive more statements for any one candidate than there are members in the group
	/// assigned to that para, but this maximum needs to be lenient to account for equivocations
	/// that may be cross-group. As such, a maximum of 2 * `n_validators` is recommended.
	///
	/// This returns an error if the peer should not have sent us this message according to protocol
	/// rules for flood protection.
	///
	/// If this returns `Ok`, the internal state has been altered. After `receive`ing a new
	/// candidate, we are then cleared to send the peer further statements about that candidate.
	///
	/// This returns `Ok(true)` if this is the first time the peer has become aware of a
	/// candidate with given hash.
	fn receive(
		&mut self,
		fingerprint: &(CompactStatement, ValidatorIndex),
		max_message_count: usize,
	) -> std::result::Result<bool, Rep> {
		// We don't check `sent_statements` because a statement could be in-flight from both
		// sides at the same time.
		if self.received_statements.contains(fingerprint) {
			return Err(COST_DUPLICATE_STATEMENT)
		}

		let (candidate_hash, fresh) = match fingerprint.0 {
			CompactStatement::Seconded(ref h) => {
				let allowed_remote = self
					.seconded_counts
					.entry(fingerprint.1)
					.or_insert_with(Default::default)
					.note_remote(*h);

				if !allowed_remote {
					return Err(COST_UNEXPECTED_STATEMENT_REMOTE)
				}

				(h, !self.is_known_candidate(h))
			},
			CompactStatement::Valid(ref h) => {
				if !self.is_known_candidate(h) {
					return Err(COST_UNEXPECTED_STATEMENT_UNKNOWN_CANDIDATE)
				}

				(h, false)
			},
		};

		{
			let received_per_candidate =
				self.received_message_count.entry(*candidate_hash).or_insert(0);

			if *received_per_candidate >= max_message_count {
				return Err(COST_APPARENT_FLOOD)
			}

			*received_per_candidate += 1;
		}

		self.received_statements.insert(fingerprint.clone());
		self.received_candidates.insert(*candidate_hash);
		Ok(fresh)
	}

	/// Note a received large statement metadata.
	fn receive_large_statement(&mut self) -> std::result::Result<(), Rep> {
		if self.large_statement_count >= MAX_LARGE_STATEMENTS_PER_SENDER {
			return Err(COST_APPARENT_FLOOD)
		}
		self.large_statement_count += 1;
		Ok(())
	}

	/// This method does the same checks as `receive` without modifying the internal state.
	/// Returns an error if the peer should not have sent us this message according to protocol
	/// rules for flood protection.
	fn check_can_receive(
		&self,
		fingerprint: &(CompactStatement, ValidatorIndex),
		max_message_count: usize,
	) -> std::result::Result<(), Rep> {
		// We don't check `sent_statements` because a statement could be in-flight from both
		// sides at the same time.
		if self.received_statements.contains(fingerprint) {
			return Err(COST_DUPLICATE_STATEMENT)
		}

		let candidate_hash = match fingerprint.0 {
			CompactStatement::Seconded(ref h) => {
				let allowed_remote = self
					.seconded_counts
					.get(&fingerprint.1)
					.map_or(true, |r| r.is_wanted_candidate(h));

				if !allowed_remote {
					return Err(COST_UNEXPECTED_STATEMENT_REMOTE)
				}

				h
			},
			CompactStatement::Valid(ref h) => {
				if !self.is_known_candidate(&h) {
					return Err(COST_UNEXPECTED_STATEMENT_UNKNOWN_CANDIDATE)
				}

				h
			},
		};

		let received_per_candidate = self.received_message_count.get(candidate_hash).unwrap_or(&0);

		if *received_per_candidate >= max_message_count {
			Err(COST_APPARENT_FLOOD)
		} else {
			Ok(())
		}
	}

	/// Check for candidates that the peer is aware of. This indicates that we can
	/// send other statements pertaining to that candidate.
	fn is_known_candidate(&self, candidate: &CandidateHash) -> bool {
		self.sent_candidates.contains(candidate) || self.received_candidates.contains(candidate)
	}
}

pub struct PeerData {
	view: View,
	protocol_version: ValidationVersion,
	view_knowledge: HashMap<Hash, PeerRelayParentKnowledge>,
	/// Peer might be known as authority with the given ids.
	maybe_authority: Option<HashSet<AuthorityDiscoveryId>>,
}

impl PeerData {
	/// Updates our view of the peer's knowledge with this statement's fingerprint based
	/// on something that we would like to send to the peer.
	///
	/// NOTE: assumes `self.can_send` returned true before this call.
	///
	/// Once the knowledge has incorporated a statement, it cannot be incorporated again.
	///
	/// This returns `true` if this is the first time the peer has become aware of a
	/// candidate with the given hash.
	fn send(
		&mut self,
		relay_parent: &Hash,
		fingerprint: &(CompactStatement, ValidatorIndex),
	) -> bool {
		debug_assert!(
			self.can_send(relay_parent, fingerprint),
			"send is only called after `can_send` returns true; qed",
		);
		self.view_knowledge
			.get_mut(relay_parent)
			.expect("send is only called after `can_send` returns true; qed")
			.send(fingerprint)
	}

	/// This returns `None` if the peer cannot accept this statement, without altering internal
	/// state.
	fn can_send(
		&self,
		relay_parent: &Hash,
		fingerprint: &(CompactStatement, ValidatorIndex),
	) -> bool {
		self.view_knowledge.get(relay_parent).map_or(false, |k| k.can_send(fingerprint))
	}

	/// Attempt to update our view of the peer's knowledge with this statement's fingerprint based
	/// on a message we are receiving from the peer.
	///
	/// Provide the maximum message count that we can receive per candidate. In practice we should
	/// not receive more statements for any one candidate than there are members in the group
	/// assigned to that para, but this maximum needs to be lenient to account for equivocations
	/// that may be cross-group. As such, a maximum of 2 * `n_validators` is recommended.
	///
	/// This returns an error if the peer should not have sent us this message according to protocol
	/// rules for flood protection.
	///
	/// If this returns `Ok`, the internal state has been altered. After `receive`ing a new
	/// candidate, we are then cleared to send the peer further statements about that candidate.
	///
	/// This returns `Ok(true)` if this is the first time the peer has become aware of a
	/// candidate with given hash.
	fn receive(
		&mut self,
		relay_parent: &Hash,
		fingerprint: &(CompactStatement, ValidatorIndex),
		max_message_count: usize,
	) -> std::result::Result<bool, Rep> {
		self.view_knowledge
			.get_mut(relay_parent)
			.ok_or(COST_UNEXPECTED_STATEMENT_MISSING_KNOWLEDGE)?
			.receive(fingerprint, max_message_count)
	}

	/// This method does the same checks as `receive` without modifying the internal state.
	/// Returns an error if the peer should not have sent us this message according to protocol
	/// rules for flood protection.
	fn check_can_receive(
		&self,
		relay_parent: &Hash,
		fingerprint: &(CompactStatement, ValidatorIndex),
		max_message_count: usize,
	) -> std::result::Result<(), Rep> {
		self.view_knowledge
			.get(relay_parent)
			.ok_or(COST_UNEXPECTED_STATEMENT_MISSING_KNOWLEDGE)?
			.check_can_receive(fingerprint, max_message_count)
	}

	/// Receive a notice about out of view statement and returns the value of the old flag
	fn receive_unexpected(&mut self, relay_parent: &Hash) -> usize {
		self.view_knowledge
			.get_mut(relay_parent)
			.map_or(0_usize, |relay_parent_peer_knowledge| {
				let old = relay_parent_peer_knowledge.unexpected_count;
				relay_parent_peer_knowledge.unexpected_count += 1_usize;
				old
			})
	}

	/// Basic flood protection for large statements.
	fn receive_large_statement(&mut self, relay_parent: &Hash) -> std::result::Result<(), Rep> {
		self.view_knowledge
			.get_mut(relay_parent)
			.ok_or(COST_UNEXPECTED_STATEMENT_MISSING_KNOWLEDGE)?
			.receive_large_statement()
	}
}

// A statement stored while a relay chain head is active.
#[derive(Debug, Copy, Clone)]
struct StoredStatement<'a> {
	comparator: &'a StoredStatementComparator,
	statement: &'a SignedFullStatement,
}

// A value used for comparison of stored statements to each other.
//
// The compact version of the statement, the validator index, and the signature of the validator
// is enough to differentiate between all types of equivocations, as long as the signature is
// actually checked to be valid. The same statement with 2 signatures and 2 statements with
// different (or same) signatures wll all be correctly judged to be unequal with this comparator.
#[derive(PartialEq, Eq, Hash, Clone, Debug)]
struct StoredStatementComparator {
	compact: CompactStatement,
	validator_index: ValidatorIndex,
	signature: ValidatorSignature,
}

impl<'a> From<(&'a StoredStatementComparator, &'a SignedFullStatement)> for StoredStatement<'a> {
	fn from(
		(comparator, statement): (&'a StoredStatementComparator, &'a SignedFullStatement),
	) -> Self {
		Self { comparator, statement }
	}
}

impl<'a> StoredStatement<'a> {
	fn compact(&self) -> &'a CompactStatement {
		&self.comparator.compact
	}

	fn fingerprint(&self) -> (CompactStatement, ValidatorIndex) {
		(self.comparator.compact.clone(), self.statement.validator_index())
	}
}

#[derive(Debug)]
enum NotedStatement<'a> {
	NotUseful,
	Fresh(StoredStatement<'a>),
	UsefulButKnown,
}

/// Large statement fetching status.
enum LargeStatementStatus {
	/// We are currently fetching the statement data from a remote peer. We keep a list of other
	/// nodes claiming to have that data and will fallback on them.
	Fetching(FetchingInfo),
	/// Statement data is fetched or we got it locally via `StatementDistributionMessage::Share`.
	FetchedOrShared(CommittedCandidateReceipt),
}

/// Info about a fetch in progress.
struct FetchingInfo {
	/// All peers that send us a `LargeStatement` or a `Valid` statement for the given
	/// `CandidateHash`, together with their originally sent messages.
	///
	/// We use an `IndexMap` here to preserve the ordering of peers sending us messages. This is
	/// desirable because we reward first sending peers with reputation.
	available_peers: IndexMap<PeerId, Vec<net_protocol::StatementDistributionMessage>>,
	/// Peers left to try in case the background task needs it.
	peers_to_try: Vec<PeerId>,
	/// Sender for sending fresh peers to the fetching task in case of failure.
	peer_sender: Option<oneshot::Sender<Vec<PeerId>>>,
	/// Task taking care of the request.
	///
	/// Will be killed once dropped.
	#[allow(dead_code)]
	fetching_task: RemoteHandle<()>,
}

#[derive(Debug, PartialEq, Eq)]
enum DeniedStatement {
	NotUseful,
	UsefulButKnown,
}

pub(crate) struct ActiveHeadData {
	/// All candidates we are aware of for this head, keyed by hash.
	candidates: HashSet<CandidateHash>,
	/// Persisted validation data cache.
	cached_validation_data: HashMap<ParaId, PersistedValidationData>,
	/// Stored statements for circulation to peers.
	///
	/// These are iterable in insertion order, and `Seconded` statements are always
	/// accepted before dependent statements.
	statements: IndexMap<StoredStatementComparator, SignedFullStatement>,
	/// Large statements we are waiting for with associated meta data.
	waiting_large_statements: HashMap<CandidateHash, LargeStatementStatus>,
	/// The parachain validators at the head's child session index.
	validators: IndexedVec<ValidatorIndex, ValidatorId>,
	/// The current session index of this fork.
	session_index: sp_staking::SessionIndex,
	/// How many `Seconded` statements we've seen per validator.
	seconded_counts: HashMap<ValidatorIndex, usize>,
}

impl ActiveHeadData {
	fn new(
		validators: IndexedVec<ValidatorIndex, ValidatorId>,
		session_index: sp_staking::SessionIndex,
	) -> Self {
		ActiveHeadData {
			candidates: Default::default(),
			cached_validation_data: Default::default(),
			statements: Default::default(),
			waiting_large_statements: Default::default(),
			validators,
			session_index,
			seconded_counts: Default::default(),
		}
	}

	/// Fetches the `PersistedValidationData` from the runtime, assuming
	/// that the core is free. The relay parent must match that of the active
	/// head.
	async fn fetch_persisted_validation_data<Sender>(
		&mut self,
		sender: &mut Sender,
		relay_parent: Hash,
		para_id: ParaId,
	) -> Result<Option<&PersistedValidationData>>
	where
		Sender: StatementDistributionSenderTrait,
	{
		if let Entry::Vacant(entry) = self.cached_validation_data.entry(para_id) {
			let persisted_validation_data =
				polkadot_node_subsystem_util::request_persisted_validation_data(
					relay_parent,
					para_id,
					OccupiedCoreAssumption::Free,
					sender,
				)
				.await
				.await
				.map_err(Error::RuntimeApiUnavailable)?
				.map_err(|err| Error::FetchPersistedValidationData(para_id, err))?;

			match persisted_validation_data {
				Some(pvd) => entry.insert(pvd),
				None => return Ok(None),
			};
		}

		Ok(self.cached_validation_data.get(&para_id))
	}

	/// Note the given statement.
	///
	/// If it was not already known and can be accepted,  returns `NotedStatement::Fresh`,
	/// with a handle to the statement.
	///
	/// If it can be accepted, but we already know it, returns `NotedStatement::UsefulButKnown`.
	///
	/// We accept up to `VC_THRESHOLD` (2 at time of writing) `Seconded` statements
	/// per validator. These will be the first ones we see. The statement is assumed
	/// to have been checked, including that the validator index is not out-of-bounds and
	/// the signature is valid.
	///
	/// Any other statements or those that reference a candidate we are not aware of cannot be
	/// accepted and will return `NotedStatement::NotUseful`.
	fn note_statement(&mut self, statement: SignedFullStatement) -> NotedStatement {
		let validator_index = statement.validator_index();
		let comparator = StoredStatementComparator {
			compact: statement.payload().to_compact(),
			validator_index,
			signature: statement.signature().clone(),
		};

		match comparator.compact {
			CompactStatement::Seconded(h) => {
				let seconded_so_far = self.seconded_counts.entry(validator_index).or_insert(0);
				if *seconded_so_far >= VC_THRESHOLD {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						?statement,
						"Extra statement is ignored"
					);
					return NotedStatement::NotUseful
				}

				self.candidates.insert(h);
				if let Some(old) = self.statements.insert(comparator.clone(), statement) {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						statement = ?old,
						"Known statement"
					);
					NotedStatement::UsefulButKnown
				} else {
					*seconded_so_far += 1;

					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						statement = ?self.statements.last().expect("Just inserted").1,
						"Noted new statement"
					);
					// This will always return `Some` because it was just inserted.
					let key_value = self
						.statements
						.get_key_value(&comparator)
						.expect("Statement was just inserted; qed");

					NotedStatement::Fresh(key_value.into())
				}
			},
			CompactStatement::Valid(h) => {
				if !self.candidates.contains(&h) {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						?statement,
						"Statement for unknown candidate"
					);
					return NotedStatement::NotUseful
				}

				if let Some(old) = self.statements.insert(comparator.clone(), statement) {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						statement = ?old,
						"Known statement"
					);
					NotedStatement::UsefulButKnown
				} else {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						statement = ?self.statements.last().expect("Just inserted").1,
						"Noted new statement"
					);
					// This will always return `Some` because it was just inserted.
					NotedStatement::Fresh(
						self.statements
							.get_key_value(&comparator)
							.expect("Statement was just inserted; qed")
							.into(),
					)
				}
			},
		}
	}

	/// Returns an error if the statement is already known or not useful
	/// without modifying the internal state.
	fn check_useful_or_unknown(
		&self,
		statement: &UncheckedSignedStatement,
	) -> std::result::Result<(), DeniedStatement> {
		let validator_index = statement.unchecked_validator_index();
		let compact = statement.unchecked_payload();
		let comparator = StoredStatementComparator {
			compact: compact.clone(),
			validator_index,
			signature: statement.unchecked_signature().clone(),
		};

		match compact {
			CompactStatement::Seconded(_) => {
				let seconded_so_far = self.seconded_counts.get(&validator_index).unwrap_or(&0);
				if *seconded_so_far >= VC_THRESHOLD {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						?statement,
						"Extra statement is ignored",
					);
					return Err(DeniedStatement::NotUseful)
				}

				if self.statements.contains_key(&comparator) {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						?statement,
						"Known statement",
					);
					return Err(DeniedStatement::UsefulButKnown)
				}
			},
			CompactStatement::Valid(h) => {
				if !self.candidates.contains(&h) {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						?statement,
						"Statement for unknown candidate",
					);
					return Err(DeniedStatement::NotUseful)
				}

				if self.statements.contains_key(&comparator) {
					gum::trace!(
						target: LOG_TARGET,
						?validator_index,
						?statement,
						"Known statement",
					);
					return Err(DeniedStatement::UsefulButKnown)
				}
			},
		}
		Ok(())
	}

	/// Get an iterator over all statements for the active head. Seconded statements come first.
	fn statements(&self) -> impl Iterator<Item = StoredStatement<'_>> + '_ {
		self.statements.iter().map(Into::into)
	}

	/// Get an iterator over all statements for the active head that are for a particular candidate.
	fn statements_about(
		&self,
		candidate_hash: CandidateHash,
	) -> impl Iterator<Item = StoredStatement<'_>> + '_ {
		self.statements()
			.filter(move |s| s.compact().candidate_hash() == &candidate_hash)
	}
}

/// Check a statement signature under this parent hash.
fn check_statement_signature(
	head: &ActiveHeadData,
	relay_parent: Hash,
	statement: UncheckedSignedStatement,
) -> std::result::Result<SignedStatement, UncheckedSignedStatement> {
	let signing_context =
		SigningContext { session_index: head.session_index, parent_hash: relay_parent };

	head.validators
		.get(statement.unchecked_validator_index())
		.ok_or_else(|| statement.clone())
		.and_then(|v| statement.try_into_checked(&signing_context, v))
}

/// Places the statement in storage if it is new, and then
/// circulates the statement to all peers who have not seen it yet, and
/// sends all statements dependent on that statement to peers who could previously not receive
/// them but now can.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn circulate_statement_and_dependents<Context>(
	topology_store: &SessionBoundGridTopologyStorage,
	peers: &mut HashMap<PeerId, PeerData>,
	active_heads: &mut HashMap<Hash, ActiveHeadData>,
	ctx: &mut Context,
	relay_parent: Hash,
	statement: SignedFullStatement,
	priority_peers: Vec<PeerId>,
	metrics: &Metrics,
	rng: &mut impl rand::Rng,
) {
	let active_head = match active_heads.get_mut(&relay_parent) {
		Some(res) => res,
		None => return,
	};

	let topology = topology_store
		.get_topology_or_fallback(active_head.session_index)
		.local_grid_neighbors();
	// First circulate the statement directly to all peers needing it.
	// The borrow of `active_head` needs to encompass only this (Rust) statement.
	let outputs: Option<(CandidateHash, Vec<PeerId>)> = {
		match active_head.note_statement(statement) {
			NotedStatement::Fresh(stored) => Some((
				*stored.compact().candidate_hash(),
				circulate_statement(
					RequiredRouting::GridXY,
					topology,
					peers,
					ctx,
					relay_parent,
					stored,
					priority_peers,
					metrics,
					rng,
				)
				.await,
			)),
			_ => None,
		}
	};

	// Now send dependent statements to all peers needing them, if any.
	if let Some((candidate_hash, peers_needing_dependents)) = outputs {
		for peer in peers_needing_dependents {
			if let Some(peer_data) = peers.get_mut(&peer) {
				// defensive: the peer data should always be some because the iterator
				// of peers is derived from the set of peers.
				send_statements_about(
					peer,
					peer_data,
					ctx,
					relay_parent,
					candidate_hash,
					&*active_head,
					metrics,
				)
				.await;
			}
		}
	}
}

/// Create a network message from a given statement.
fn v1_statement_message(
	relay_parent: Hash,
	statement: SignedFullStatement,
	metrics: &Metrics,
) -> protocol_v1::StatementDistributionMessage {
	let (is_large, size) = is_statement_large(&statement);
	if let Some(size) = size {
		metrics.on_created_message(size);
	}

	if is_large {
		protocol_v1::StatementDistributionMessage::LargeStatement(StatementMetadata {
			relay_parent,
			candidate_hash: statement.payload().candidate_hash(),
			signed_by: statement.validator_index(),
			signature: statement.signature().clone(),
		})
	} else {
		protocol_v1::StatementDistributionMessage::Statement(relay_parent, statement.into())
	}
}

/// Check whether a statement should be treated as large statement.
///
/// Also report size of statement - if it is a `Seconded` statement, otherwise `None`.
fn is_statement_large(statement: &SignedFullStatement) -> (bool, Option<usize>) {
	match &statement.payload() {
		Statement::Seconded(committed) => {
			let size = statement.as_unchecked().encoded_size();
			// Runtime upgrades will always be large and even if not - no harm done.
			if committed.commitments.new_validation_code.is_some() {
				return (true, Some(size))
			}

			// Half max size seems to be a good threshold to start not using notifications:
			let threshold =
				PeerSet::Validation.get_max_notification_size(IsAuthority::Yes) as usize / 2;

			(size >= threshold, Some(size))
		},
		Statement::Valid(_) => (false, None),
	}
}

/// Circulates a statement to all peers who have not seen it yet, and returns
/// an iterator over peers who need to have dependent statements sent.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn circulate_statement<'a, Context>(
	required_routing: RequiredRouting,
	topology: &GridNeighbors,
	peers: &mut HashMap<PeerId, PeerData>,
	ctx: &mut Context,
	relay_parent: Hash,
	stored: StoredStatement<'a>,
	mut priority_peers: Vec<PeerId>,
	metrics: &Metrics,
	rng: &mut impl rand::Rng,
) -> Vec<PeerId> {
	let fingerprint = stored.fingerprint();

	let mut peers_to_send: Vec<PeerId> = peers
		.iter()
		.filter_map(
			|(peer, data)| {
				if data.can_send(&relay_parent, &fingerprint) {
					Some(*peer)
				} else {
					None
				}
			},
		)
		.collect();

	let good_peers: HashSet<&PeerId> = peers_to_send.iter().collect();
	// Only take priority peers we can send data to:
	priority_peers.retain(|p| good_peers.contains(p));

	// Avoid duplicates:
	let priority_set: HashSet<&PeerId> = priority_peers.iter().collect();
	peers_to_send.retain(|p| !priority_set.contains(p));

	util::choose_random_subset_with_rng(
		|e| topology.route_to_peer(required_routing, e),
		&mut peers_to_send,
		rng,
		MIN_GOSSIP_PEERS,
	);

	// We don't want to use less peers, than we would without any priority peers:
	let min_size = std::cmp::max(peers_to_send.len(), MIN_GOSSIP_PEERS);
	// Make set full:
	let needed_peers = min_size as i64 - priority_peers.len() as i64;
	if needed_peers > 0 {
		peers_to_send.truncate(needed_peers as usize);
		// Order important here - priority peers are placed first, so will be sent first.
		// This gives backers a chance to be among the first in requesting any large statement
		// data.
		priority_peers.append(&mut peers_to_send);
	}
	peers_to_send = priority_peers;
	// We must not have duplicates:
	debug_assert!(
		peers_to_send.len() == peers_to_send.clone().into_iter().collect::<HashSet<_>>().len(),
		"We filter out duplicates above. qed.",
	);

	let (v1_peers_to_send, non_v1_peers_to_send) = peers_to_send
		.into_iter()
		.map(|peer_id| {
			let peer_data =
				peers.get_mut(&peer_id).expect("a subset is taken above, so it exists; qed");

			let new = peer_data.send(&relay_parent, &fingerprint);

			(peer_id, new, peer_data.protocol_version)
		})
		.partition::<Vec<_>, _>(|(_, _, version)| match version {
			ValidationVersion::V1 => true,
			ValidationVersion::V2 | ValidationVersion::V3 => false,
		}); // partition is handy here but not if we add more protocol versions

	let payload = v1_statement_message(relay_parent, stored.statement.clone(), metrics);

	// Send all these peers the initial statement.
	if !v1_peers_to_send.is_empty() {
		gum::trace!(
			target: LOG_TARGET,
			?v1_peers_to_send,
			?relay_parent,
			statement = ?stored.statement,
			"Sending statement to v1 peers",
		);
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			v1_peers_to_send.iter().map(|(p, _, _)| *p).collect(),
			compatible_v1_message(ValidationVersion::V1, payload.clone()).into(),
		))
		.await;
	}

	let peers_to_send: Vec<(PeerId, ProtocolVersion)> = non_v1_peers_to_send
		.iter()
		.map(|(p, _, version)| (*p, (*version).into()))
		.collect();

	let peer_needs_dependent_statement = v1_peers_to_send
		.into_iter()
		.chain(non_v1_peers_to_send)
		.filter_map(|(peer, needs_dependent, _)| if needs_dependent { Some(peer) } else { None })
		.collect();

	let v2_peers_to_send = filter_by_peer_version(&peers_to_send, ValidationVersion::V2.into());
	let v3_to_send = filter_by_peer_version(&peers_to_send, ValidationVersion::V3.into());

	if !v2_peers_to_send.is_empty() {
		gum::trace!(
			target: LOG_TARGET,
			?v2_peers_to_send,
			?relay_parent,
			statement = ?stored.statement,
			"Sending statement to v2 peers",
		);
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			v2_peers_to_send,
			compatible_v1_message(ValidationVersion::V2, payload.clone()).into(),
		))
		.await;
	}

	if !v3_to_send.is_empty() {
		gum::trace!(
			target: LOG_TARGET,
			?v3_to_send,
			?relay_parent,
			statement = ?stored.statement,
			"Sending statement to v3 peers",
		);
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			v3_to_send,
			compatible_v1_message(ValidationVersion::V3, payload.clone()).into(),
		))
		.await;
	}

	peer_needs_dependent_statement
}

/// Send all statements about a given candidate hash to a peer.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_statements_about<Context>(
	peer: PeerId,
	peer_data: &mut PeerData,
	ctx: &mut Context,
	relay_parent: Hash,
	candidate_hash: CandidateHash,
	active_head: &ActiveHeadData,
	metrics: &Metrics,
) {
	for statement in active_head.statements_about(candidate_hash) {
		let fingerprint = statement.fingerprint();
		if !peer_data.can_send(&relay_parent, &fingerprint) {
			continue
		}
		peer_data.send(&relay_parent, &fingerprint);
		let payload = v1_statement_message(relay_parent, statement.statement.clone(), metrics);

		gum::trace!(
			target: LOG_TARGET,
			?peer,
			?relay_parent,
			?candidate_hash,
			statement = ?statement.statement,
			"Sending statement",
		);
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			vec![peer],
			compatible_v1_message(peer_data.protocol_version, payload).into(),
		))
		.await;

		metrics.on_statement_distributed();
	}
}

/// Send all statements at a given relay-parent to a peer.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn send_statements<Context>(
	peer: PeerId,
	peer_data: &mut PeerData,
	ctx: &mut Context,
	relay_parent: Hash,
	active_head: &ActiveHeadData,
	metrics: &Metrics,
) {
	for statement in active_head.statements() {
		let fingerprint = statement.fingerprint();
		if !peer_data.can_send(&relay_parent, &fingerprint) {
			continue
		}
		peer_data.send(&relay_parent, &fingerprint);
		let payload = v1_statement_message(relay_parent, statement.statement.clone(), metrics);

		gum::trace!(
			target: LOG_TARGET,
			?peer,
			?relay_parent,
			statement = ?statement.statement,
			"Sending statement"
		);
		ctx.send_message(NetworkBridgeTxMessage::SendValidationMessage(
			vec![peer],
			compatible_v1_message(peer_data.protocol_version, payload).into(),
		))
		.await;

		metrics.on_statement_distributed();
	}
}

/// Modify the reputation of a peer based on its behavior.
async fn modify_reputation(
	reputation: &mut ReputationAggregator,
	sender: &mut impl overseer::StatementDistributionSenderTrait,
	peer: PeerId,
	rep: Rep,
) {
	reputation.modify(sender, peer, rep).await;
}

/// If message contains a statement, then retrieve it, otherwise fork task to fetch it.
///
/// This function will also return `None` if the message did not pass some basic checks, in that
/// case no statement will be requested, on the flipside you get `ActiveHeadData` in addition to
/// your statement.
///
/// If the message was large, but the result has been fetched already that one is returned.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn retrieve_statement_from_message<'a, Context>(
	peer: PeerId,
	peer_version: ValidationVersion,
	message: protocol_v1::StatementDistributionMessage,
	active_head: &'a mut ActiveHeadData,
	ctx: &mut Context,
	req_sender: &mpsc::Sender<RequesterMessage>,
	metrics: &Metrics,
) -> Option<UncheckedSignedFullStatement> {
	let fingerprint = message.get_fingerprint();
	let candidate_hash = *fingerprint.0.candidate_hash();

	// Immediately return any Seconded statement:
	let message = if let protocol_v1::StatementDistributionMessage::Statement(h, s) = message {
		if let Statement::Seconded(_) = s.unchecked_payload() {
			return Some(s)
		}
		protocol_v1::StatementDistributionMessage::Statement(h, s)
	} else {
		message
	};

	match active_head.waiting_large_statements.entry(candidate_hash) {
		Entry::Occupied(mut occupied) => {
			match occupied.get_mut() {
				LargeStatementStatus::Fetching(info) => {
					let is_large_statement = message.is_large_statement();

					let is_new_peer = match info.available_peers.entry(peer) {
						IEntry::Occupied(mut occupied) => {
							occupied.get_mut().push(compatible_v1_message(peer_version, message));
							false
						},
						IEntry::Vacant(vacant) => {
							vacant.insert(vec![compatible_v1_message(peer_version, message)]);
							true
						},
					};

					if is_new_peer & is_large_statement {
						info.peers_to_try.push(peer);
						// Answer any pending request for more peers:
						if let Some(sender) = info.peer_sender.take() {
							let to_send = std::mem::take(&mut info.peers_to_try);
							if let Err(peers) = sender.send(to_send) {
								// Requester no longer interested for now, might want them
								// later:
								info.peers_to_try = peers;
							}
						}
					}
				},
				LargeStatementStatus::FetchedOrShared(committed) => {
					match message {
						protocol_v1::StatementDistributionMessage::Statement(_, s) => {
							// We can now immediately return any statements (should only be
							// `Statement::Valid` ones, but we don't care at this point.)
							return Some(s)
						},
						protocol_v1::StatementDistributionMessage::LargeStatement(metadata) =>
							return Some(UncheckedSignedFullStatement::new(
								Statement::Seconded(committed.clone()),
								metadata.signed_by,
								metadata.signature.clone(),
							)),
					}
				},
			}
		},
		Entry::Vacant(vacant) => {
			match message {
				protocol_v1::StatementDistributionMessage::LargeStatement(metadata) => {
					if let Some(new_status) = launch_request(
						metadata,
						peer,
						peer_version,
						req_sender.clone(),
						ctx,
						metrics,
					)
					.await
					{
						vacant.insert(new_status);
					}
				},
				protocol_v1::StatementDistributionMessage::Statement(_, s) => {
					// No fetch in progress, safe to return any statement immediately (we don't
					// bother about normal network jitter which might cause `Valid` statements to
					// arrive early for now.).
					return Some(s)
				},
			}
		},
	}
	None
}

/// Launch request for a large statement and get tracking status.
///
/// Returns `None` if spawning task failed.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn launch_request<Context>(
	meta: StatementMetadata,
	peer: PeerId,
	peer_version: ValidationVersion,
	req_sender: mpsc::Sender<RequesterMessage>,
	ctx: &mut Context,
	metrics: &Metrics,
) -> Option<LargeStatementStatus> {
	let (task, handle) =
		fetch(meta.relay_parent, meta.candidate_hash, vec![peer], req_sender, metrics.clone())
			.remote_handle();

	let result = ctx.spawn("large-statement-fetcher", task.boxed());
	if let Err(err) = result {
		gum::error!(target: LOG_TARGET, ?err, "Spawning task failed.");
		return None
	}
	let available_peers = {
		let mut m = IndexMap::new();
		m.insert(
			peer,
			vec![compatible_v1_message(
				peer_version,
				protocol_v1::StatementDistributionMessage::LargeStatement(meta),
			)],
		);
		m
	};
	Some(LargeStatementStatus::Fetching(FetchingInfo {
		available_peers,
		peers_to_try: Vec::new(),
		peer_sender: None,
		fetching_task: handle,
	}))
}

/// Handle incoming message and circulate it to peers, if we did not know it already.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_incoming_message_and_circulate<'a, Context, R>(
	peer: PeerId,
	topology_storage: &SessionBoundGridTopologyStorage,
	peers: &mut HashMap<PeerId, PeerData>,
	active_heads: &'a mut HashMap<Hash, ActiveHeadData>,
	recent_outdated_heads: &RecentOutdatedHeads,
	ctx: &mut Context,
	message: net_protocol::StatementDistributionMessage,
	req_sender: &mpsc::Sender<RequesterMessage>,
	metrics: &Metrics,
	runtime: &mut RuntimeInfo,
	rng: &mut R,
	reputation: &mut ReputationAggregator,
) where
	R: rand::Rng,
{
	let handled_incoming = match peers.get_mut(&peer) {
		Some(data) =>
			handle_incoming_message(
				peer,
				data,
				active_heads,
				recent_outdated_heads,
				ctx,
				message,
				req_sender,
				metrics,
				reputation,
			)
			.await,
		None => None,
	};

	// if we got a fresh message, we need to circulate it to all peers.
	if let Some((relay_parent, statement)) = handled_incoming {
		// we can ignore the set of peers who this function returns as now expecting
		// dependent statements.
		//
		// we have the invariant in this subsystem that we never store a `Valid` or `Invalid`
		// statement before a `Seconded` statement. `Seconded` statements are the only ones
		// that require dependents. Thus, if this is a `Seconded` statement for a candidate we
		// were not aware of before, we cannot have any dependent statements from the candidate.
		let _ = metrics.time_network_bridge_update("circulate_statement");

		let session_index = runtime.get_session_index_for_child(ctx.sender(), relay_parent).await;
		let topology = match session_index {
			Ok(session_index) =>
				topology_storage.get_topology_or_fallback(session_index).local_grid_neighbors(),
			Err(e) => {
				gum::debug!(
					target: LOG_TARGET,
					%relay_parent,
					"cannot get session index for the specific relay parent: {:?}",
					e
				);

				topology_storage.get_current_topology().local_grid_neighbors()
			},
		};
		let required_routing =
			topology.required_routing_by_index(statement.statement.validator_index(), false);

		let _ = circulate_statement(
			required_routing,
			topology,
			peers,
			ctx,
			relay_parent,
			statement,
			Vec::new(),
			metrics,
			rng,
		)
		.await;
	}
}

// Handle a statement. Returns a reference to a newly-stored statement
// if we were not already aware of it, along with the corresponding relay-parent.
//
// This function checks the signature and ensures the statement is compatible with our
// view. It also notifies candidate backing if the statement was previously unknown.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn handle_incoming_message<'a, Context>(
	peer: PeerId,
	peer_data: &mut PeerData,
	active_heads: &'a mut HashMap<Hash, ActiveHeadData>,
	recent_outdated_heads: &RecentOutdatedHeads,
	ctx: &mut Context,
	message: net_protocol::StatementDistributionMessage,
	req_sender: &mpsc::Sender<RequesterMessage>,
	metrics: &Metrics,
	reputation: &mut ReputationAggregator,
) -> Option<(Hash, StoredStatement<'a>)> {
	let _ = metrics.time_network_bridge_update("handle_incoming_message");

	let message = match message {
		Versioned::V1(m) => m,
		Versioned::V2(protocol_v2::StatementDistributionMessage::V1Compatibility(m)) |
		Versioned::V3(protocol_v3::StatementDistributionMessage::V1Compatibility(m)) => m,
		Versioned::V2(_) | Versioned::V3(_) => {
			// The higher-level subsystem code is supposed to filter out
			// all non v1 messages.
			gum::debug!(
				target: LOG_TARGET,
				"Legacy statement-distribution code received unintended v2 message"
			);

			return None
		},
	};

	let relay_parent = message.get_relay_parent();

	let active_head = match active_heads.get_mut(&relay_parent) {
		Some(h) => h,
		None => {
			gum::debug!(
				target: LOG_TARGET,
				%relay_parent,
				"our view out-of-sync with active heads; head not found",
			);

			if !recent_outdated_heads.is_recent_outdated(&relay_parent) {
				modify_reputation(reputation, ctx.sender(), peer, COST_UNEXPECTED_STATEMENT).await;
			}

			return None
		},
	};

	if let protocol_v1::StatementDistributionMessage::LargeStatement(_) = message {
		if let Err(rep) = peer_data.receive_large_statement(&relay_parent) {
			gum::debug!(target: LOG_TARGET, ?peer, ?message, ?rep, "Unexpected large statement.",);
			modify_reputation(reputation, ctx.sender(), peer, rep).await;
			return None
		}
	}

	let fingerprint = message.get_fingerprint();
	let candidate_hash = *fingerprint.0.candidate_hash();

	let max_message_count = active_head.validators.len() * 2;

	// perform only basic checks before verifying the signature
	// as it's more computationally heavy
	if let Err(rep) = peer_data.check_can_receive(&relay_parent, &fingerprint, max_message_count) {
		// This situation can happen when a peer's Seconded message was lost
		// but we have received the Valid statement.
		// So we check it once and then ignore repeated violation to avoid
		// reputation change flood.
		let unexpected_count = peer_data.receive_unexpected(&relay_parent);

		gum::debug!(
			target: LOG_TARGET,
			?relay_parent,
			?peer,
			?message,
			?rep,
			?unexpected_count,
			"Error inserting received statement"
		);

		match rep {
			// This happens when a Valid statement has been received but there is no corresponding
			// Seconded
			COST_UNEXPECTED_STATEMENT_UNKNOWN_CANDIDATE => {
				metrics.on_unexpected_statement_valid();
				// Report peer merely if this is not a duplicate out-of-view statement that
				// was caused by a missing Seconded statement from this peer
				if unexpected_count == 0_usize {
					modify_reputation(reputation, ctx.sender(), peer, rep).await;
				}
			},
			// This happens when we have an unexpected remote peer that announced Seconded
			COST_UNEXPECTED_STATEMENT_REMOTE => {
				metrics.on_unexpected_statement_seconded();
				modify_reputation(reputation, ctx.sender(), peer, rep).await;
			},
			_ => {
				modify_reputation(reputation, ctx.sender(), peer, rep).await;
			},
		}

		return None
	}

	let checked_compact = {
		let (compact, validator_index) = message.get_fingerprint();
		let signature = message.get_signature();

		let unchecked_compact = UncheckedSignedStatement::new(compact, validator_index, signature);

		match active_head.check_useful_or_unknown(&unchecked_compact) {
			Ok(()) => {},
			Err(DeniedStatement::NotUseful) => return None,
			Err(DeniedStatement::UsefulButKnown) => {
				// Note a received statement in the peer data
				peer_data
					.receive(&relay_parent, &fingerprint, max_message_count)
					.expect("checked in `check_can_receive` above; qed");
				modify_reputation(reputation, ctx.sender(), peer, BENEFIT_VALID_STATEMENT).await;

				return None
			},
		}

		// check the signature on the statement.
		match check_statement_signature(&active_head, relay_parent, unchecked_compact) {
			Err(statement) => {
				gum::debug!(target: LOG_TARGET, ?peer, ?statement, "Invalid statement signature");
				modify_reputation(reputation, ctx.sender(), peer, COST_INVALID_SIGNATURE).await;
				return None
			},
			Ok(statement) => statement,
		}
	};

	// Fetch from the network only after signature and usefulness checks are completed.
	let is_large_statement = message.is_large_statement();
	let statement = retrieve_statement_from_message(
		peer,
		peer_data.protocol_version,
		message,
		active_head,
		ctx,
		req_sender,
		metrics,
	)
	.await?;

	let payload = statement.unchecked_into_payload();

	// Upgrade the `Signed` wrapper from the compact payload to the full payload.
	// This fails if the payload doesn't encode correctly.
	let statement: SignedFullStatement = match checked_compact.convert_to_superpayload(payload) {
		Err((compact, _)) => {
			gum::debug!(
				target: LOG_TARGET,
				?peer,
				?compact,
				is_large_statement,
				"Full statement had bad payload."
			);
			modify_reputation(reputation, ctx.sender(), peer, COST_WRONG_HASH).await;
			return None
		},
		Ok(statement) => statement,
	};

	// Ensure the statement is stored in the peer data.
	//
	// Note that if the peer is sending us something that is not within their view,
	// it will not be kept within their log.
	match peer_data.receive(&relay_parent, &fingerprint, max_message_count) {
		Err(_) => {
			unreachable!("checked in `check_can_receive` above; qed");
		},
		Ok(true) => {
			gum::trace!(target: LOG_TARGET, ?peer, ?statement, "Statement accepted");
			// Send the peer all statements concerning the candidate that we have,
			// since it appears to have just learned about the candidate.
			send_statements_about(
				peer,
				peer_data,
				ctx,
				relay_parent,
				candidate_hash,
				&*active_head,
				metrics,
			)
			.await;
		},
		Ok(false) => {},
	}

	// For `Seconded` statements `None` or `Err` means we couldn't fetch the PVD, which
	// means the statement shouldn't be accepted.
	//
	// In case of `Valid` we should have it cached prior, therefore this performs
	// no Runtime API calls and always returns `Ok(Some(_))`.
	let pvd = if let Statement::Seconded(receipt) = statement.payload() {
		let para_id = receipt.descriptor.para_id();
		// Either call the Runtime API or check that validation data is cached.
		let result = active_head
			.fetch_persisted_validation_data(ctx.sender(), relay_parent, para_id)
			.await;

		match result {
			Ok(Some(pvd)) => Some(pvd.clone()),
			Ok(None) | Err(_) => return None,
		}
	} else {
		None
	};

	// Extend the payload with persisted validation data required by the backing
	// subsystem.
	//
	// Do it in advance before noting the statement because we don't want to borrow active
	// head mutable and use the cache.
	let statement_with_pvd = statement
		.clone()
		.convert_to_superpayload_with(move |statement| match statement {
			Statement::Seconded(receipt) => {
				let persisted_validation_data = pvd
					.expect("PVD is ensured to be `Some` for all `Seconded` messages above; qed");
				StatementWithPVD::Seconded(receipt, persisted_validation_data)
			},
			Statement::Valid(candidate_hash) => StatementWithPVD::Valid(candidate_hash),
		})
		.expect("payload was checked with conversion from compact; qed");

	// Note: `peer_data.receive` already ensures that the statement is not an unbounded equivocation
	// or unpinned to a seconded candidate. So it is safe to place it into the storage.
	match active_head.note_statement(statement) {
		NotedStatement::NotUseful | NotedStatement::UsefulButKnown => {
			unreachable!("checked in `is_useful_or_unknown` above; qed");
		},
		NotedStatement::Fresh(statement) => {
			modify_reputation(reputation, ctx.sender(), peer, BENEFIT_VALID_STATEMENT_FIRST).await;

			// When we receive a new message from a peer, we forward it to the
			// candidate backing subsystem.
			ctx.send_message(CandidateBackingMessage::Statement(relay_parent, statement_with_pvd))
				.await;

			Some((relay_parent, statement))
		},
	}
}

/// Update a peer's view. Sends all newly unlocked statements based on the previous
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
async fn update_peer_view_and_maybe_send_unlocked<Context, R>(
	peer: PeerId,
	topology: &GridNeighbors,
	peer_data: &mut PeerData,
	ctx: &mut Context,
	active_heads: &HashMap<Hash, ActiveHeadData>,
	new_view: View,
	metrics: &Metrics,
	rng: &mut R,
) where
	R: rand::Rng,
{
	let old_view = std::mem::replace(&mut peer_data.view, new_view);

	// Remove entries for all relay-parents in the old view but not the new.
	for removed in old_view.difference(&peer_data.view) {
		let _ = peer_data.view_knowledge.remove(removed);
	}

	// Use both grid directions
	let is_gossip_peer = topology.route_to_peer(RequiredRouting::GridXY, &peer);
	let lucky = is_gossip_peer ||
		util::gen_ratio_rng(
			util::MIN_GOSSIP_PEERS.saturating_sub(topology.len()),
			util::MIN_GOSSIP_PEERS,
			rng,
		);

	// Add entries for all relay-parents in the new view but not the old.
	// Furthermore, send all statements we have for those relay parents.
	let new_view = peer_data.view.difference(&old_view).copied().collect::<Vec<_>>();
	for new in new_view.iter().copied() {
		peer_data.view_knowledge.insert(new, Default::default());
		if !lucky {
			continue
		}
		if let Some(active_head) = active_heads.get(&new) {
			send_statements(peer, peer_data, ctx, new, active_head, metrics).await;
		}
	}
}

/// Handle a local network update.
#[overseer::contextbounds(StatementDistribution, prefix=self::overseer)]
pub(crate) async fn handle_network_update<Context, R>(
	ctx: &mut Context,
	state: &mut State,
	req_sender: &mpsc::Sender<RequesterMessage>,
	update: NetworkBridgeEvent<net_protocol::StatementDistributionMessage>,
	rng: &mut R,
	metrics: &Metrics,
	reputation: &mut ReputationAggregator,
) where
	R: rand::Rng,
{
	let peers = &mut state.peers;
	let topology_storage = &mut state.topology_storage;
	let authorities = &mut state.authorities;
	let active_heads = &mut state.active_heads;
	let recent_outdated_heads = &state.recent_outdated_heads;
	let runtime = &mut state.runtime;

	match update {
		NetworkBridgeEvent::PeerConnected(peer, role, protocol_version, maybe_authority) => {
			gum::trace!(target: LOG_TARGET, ?peer, ?role, ?protocol_version, "Peer connected");

			let protocol_version = match ValidationVersion::try_from(protocol_version).ok() {
				Some(v) => v,
				None => {
					gum::trace!(
						target: LOG_TARGET,
						?peer,
						?protocol_version,
						"unknown protocol version, ignoring"
					);
					return
				},
			};

			peers.insert(
				peer,
				PeerData {
					view: Default::default(),
					protocol_version,
					view_knowledge: Default::default(),
					maybe_authority: maybe_authority.clone(),
				},
			);
			if let Some(authority_ids) = maybe_authority {
				authority_ids.into_iter().for_each(|a| {
					authorities.insert(a, peer);
				});
			}
		},
		NetworkBridgeEvent::PeerDisconnected(peer) => {
			gum::trace!(target: LOG_TARGET, ?peer, "Peer disconnected");
			if let Some(auth_ids) = peers.remove(&peer).and_then(|p| p.maybe_authority) {
				auth_ids.into_iter().for_each(|a| {
					authorities.remove(&a);
				});
			}
		},
		NetworkBridgeEvent::NewGossipTopology(topology) => {
			let _ = metrics.time_network_bridge_update("new_gossip_topology");

			let new_session_index = topology.session;
			let new_topology = topology.topology;
			let old_topology =
				topology_storage.get_current_topology().local_grid_neighbors().clone();
			topology_storage.update_topology(new_session_index, new_topology, topology.local_index);

			let newly_added = topology_storage
				.get_current_topology()
				.local_grid_neighbors()
				.peers_diff(&old_topology);

			for peer in newly_added {
				if let Some(data) = peers.get_mut(&peer) {
					let view = std::mem::take(&mut data.view);
					update_peer_view_and_maybe_send_unlocked(
						peer,
						topology_storage.get_current_topology().local_grid_neighbors(),
						data,
						ctx,
						&*active_heads,
						view,
						metrics,
						rng,
					)
					.await
				}
			}
		},
		NetworkBridgeEvent::PeerMessage(peer, message) => {
			handle_incoming_message_and_circulate(
				peer,
				topology_storage,
				peers,
				active_heads,
				recent_outdated_heads,
				ctx,
				message,
				req_sender,
				metrics,
				runtime,
				rng,
				reputation,
			)
			.await;
		},
		NetworkBridgeEvent::PeerViewChange(peer, view) => {
			let _ = metrics.time_network_bridge_update("peer_view_change");
			gum::trace!(target: LOG_TARGET, ?peer, ?view, "Peer view change");
			match peers.get_mut(&peer) {
				Some(data) =>
					update_peer_view_and_maybe_send_unlocked(
						peer,
						topology_storage.get_current_topology().local_grid_neighbors(),
						data,
						ctx,
						&*active_heads,
						view,
						metrics,
						rng,
					)
					.await,
				None => (),
			}
		},
		NetworkBridgeEvent::OurViewChange(_view) => {
			// handled by `ActiveLeavesUpdate`
		},
		NetworkBridgeEvent::UpdatedAuthorityIds(peer, authority_ids) => {
			gum::trace!(
				target: LOG_TARGET,
				?peer,
				?authority_ids,
				"Updated `AuthorityDiscoveryId`s"
			);
			topology_storage
				.get_current_topology_mut()
				.update_authority_ids(peer, &authority_ids);
			// Remove the authority IDs which were previously mapped to the peer
			// but aren't part of the new set.
			authorities.retain(|a, p| p != &peer || authority_ids.contains(a));

			// Map the new authority IDs to the peer.
			for a in authority_ids.iter().cloned() {
				authorities.insert(a, peer);
			}

			if let Some(data) = peers.get_mut(&peer) {
				data.maybe_authority = Some(authority_ids);
			}
		},
	}
}

/// Handle messages from responder background task.
pub(crate) async fn handle_responder_message(
	state: &mut State,
	message: ResponderMessage,
) -> JfyiErrorResult<()> {
	let peers = &state.peers;
	let active_heads = &mut state.active_heads;

	match message {
		ResponderMessage::GetData { requesting_peer, relay_parent, candidate_hash, tx } => {
			if !requesting_peer_knows_about_candidate(
				peers,
				&requesting_peer,
				&relay_parent,
				&candidate_hash,
			)? {
				return Err(JfyiError::RequestedUnannouncedCandidate(
					requesting_peer,
					candidate_hash,
				))
			}

			let active_head =
				active_heads.get(&relay_parent).ok_or(JfyiError::NoSuchHead(relay_parent))?;

			let committed = match active_head.waiting_large_statements.get(&candidate_hash) {
				Some(LargeStatementStatus::FetchedOrShared(committed)) => committed.clone(),
				_ =>
					return Err(JfyiError::NoSuchFetchedLargeStatement(relay_parent, candidate_hash)),
			};

			tx.send(committed).map_err(|_| JfyiError::ResponderGetDataCanceled)?;
		},
	}
	Ok(())
}

#[overseer::contextbounds(StatementDistribution, prefix = self::overseer)]
pub(crate) async fn handle_requester_message<Context, R: rand::Rng>(
	ctx: &mut Context,
	state: &mut State,
	req_sender: &mpsc::Sender<RequesterMessage>,
	rng: &mut R,
	message: RequesterMessage,
	metrics: &Metrics,
	reputation: &mut ReputationAggregator,
) -> JfyiErrorResult<()> {
	let topology_storage = &state.topology_storage;
	let peers = &mut state.peers;
	let active_heads = &mut state.active_heads;
	let recent_outdated_heads = &state.recent_outdated_heads;
	let runtime = &mut state.runtime;

	match message {
		RequesterMessage::Finished {
			relay_parent,
			candidate_hash,
			from_peer,
			response,
			bad_peers,
		} => {
			for bad in bad_peers {
				modify_reputation(reputation, ctx.sender(), bad, COST_FETCH_FAIL).await;
			}
			modify_reputation(reputation, ctx.sender(), from_peer, BENEFIT_VALID_RESPONSE).await;

			let active_head =
				active_heads.get_mut(&relay_parent).ok_or(JfyiError::NoSuchHead(relay_parent))?;

			let status = active_head.waiting_large_statements.remove(&candidate_hash);

			let info = match status {
				Some(LargeStatementStatus::Fetching(info)) => info,
				Some(LargeStatementStatus::FetchedOrShared(_)) => {
					// We are no longer interested in the data.
					return Ok(())
				},
				None =>
					return Err(JfyiError::NoSuchLargeStatementStatus(relay_parent, candidate_hash)),
			};

			active_head
				.waiting_large_statements
				.insert(candidate_hash, LargeStatementStatus::FetchedOrShared(response));

			// Cache is now populated, send all messages:
			for (peer, messages) in info.available_peers {
				for message in messages {
					handle_incoming_message_and_circulate(
						peer,
						topology_storage,
						peers,
						active_heads,
						recent_outdated_heads,
						ctx,
						message,
						req_sender,
						&metrics,
						runtime,
						rng,
						reputation,
					)
					.await;
				}
			}
		},
		RequesterMessage::SendRequest(req) => {
			ctx.send_message(NetworkBridgeTxMessage::SendRequests(
				vec![req],
				IfDisconnected::ImmediateError,
			))
			.await;
		},
		RequesterMessage::GetMorePeers { relay_parent, candidate_hash, tx } => {
			let active_head =
				active_heads.get_mut(&relay_parent).ok_or(JfyiError::NoSuchHead(relay_parent))?;

			let status = active_head.waiting_large_statements.get_mut(&candidate_hash);

			let info = match status {
				Some(LargeStatementStatus::Fetching(info)) => info,
				Some(LargeStatementStatus::FetchedOrShared(_)) => {
					// This task is going to die soon - no need to send it anything.
					gum::debug!(target: LOG_TARGET, "Zombie task wanted more peers.");
					return Ok(())
				},
				None =>
					return Err(JfyiError::NoSuchLargeStatementStatus(relay_parent, candidate_hash)),
			};

			if info.peers_to_try.is_empty() {
				info.peer_sender = Some(tx);
			} else {
				let peers_to_try = std::mem::take(&mut info.peers_to_try);
				if let Err(peers) = tx.send(peers_to_try) {
					// No longer interested for now - might want them later:
					info.peers_to_try = peers;
				}
			}
		},
		RequesterMessage::ReportPeer(peer, rep) =>
			modify_reputation(reputation, ctx.sender(), peer, rep).await,
	}
	Ok(())
}

/// Handle a deactivated leaf.
pub(crate) fn handle_deactivate_leaf(state: &mut State, deactivated: Hash) {
	if state.active_heads.remove(&deactivated).is_some() {
		gum::trace!(
			target: LOG_TARGET,
			hash = ?deactivated,
			"Deactivating leaf",
		);

		state.recent_outdated_heads.note_outdated(deactivated);
	}
}

/// Handle a new activated leaf. This assumes that the leaf does not
/// support prospective parachains.
#[overseer::contextbounds(StatementDistribution, prefix = self::overseer)]
pub(crate) async fn handle_activated_leaf<Context>(
	ctx: &mut Context,
	state: &mut State,
	activated: ActivatedLeaf,
) -> Result<()> {
	let relay_parent = activated.hash;
	gum::trace!(
		target: LOG_TARGET,
		hash = ?relay_parent,
		"New active leaf",
	);

	// Retrieve the parachain validators at the child of the head we track.
	let session_index =
		state.runtime.get_session_index_for_child(ctx.sender(), relay_parent).await?;
	let info = state
		.runtime
		.get_session_info_by_index(ctx.sender(), relay_parent, session_index)
		.await?;
	let session_info = &info.session_info;

	state
		.active_heads
		.entry(relay_parent)
		.or_insert(ActiveHeadData::new(session_info.validators.clone(), session_index));

	Ok(())
}

/// Share a local statement with the rest of the network.
#[overseer::contextbounds(StatementDistribution, prefix = self::overseer)]
pub(crate) async fn share_local_statement<Context, R: Rng>(
	ctx: &mut Context,
	state: &mut State,
	relay_parent: Hash,
	statement: SignedFullStatement,
	rng: &mut R,
	metrics: &Metrics,
) -> Result<()> {
	// Make sure we have data in cache:
	if is_statement_large(&statement).0 {
		if let Statement::Seconded(committed) = &statement.payload() {
			let active_head = state
				.active_heads
				.get_mut(&relay_parent)
				// This should never be out-of-sync with our view if the view
				// updates correspond to actual `StartWork` messages.
				.ok_or(JfyiError::NoSuchHead(relay_parent))?;
			active_head.waiting_large_statements.insert(
				statement.payload().candidate_hash(),
				LargeStatementStatus::FetchedOrShared(committed.clone()),
			);
		}
	}

	let info = state.runtime.get_session_info(ctx.sender(), relay_parent).await?;
	let session_info = &info.session_info;
	let validator_info = &info.validator_info;

	// Get peers in our group, so we can make sure they get our statement
	// directly:
	let group_peers = {
		if let Some(our_group) = validator_info.our_group {
			let our_group = &session_info
				.validator_groups
				.get(our_group)
				.expect("`our_group` is derived from `validator_groups`; qed");

			our_group
				.into_iter()
				.filter_map(|i| {
					if Some(*i) == validator_info.our_index {
						return None
					}
					let authority_id = &session_info.discovery_keys[i.0 as usize];
					state.authorities.get(authority_id).map(|p| *p)
				})
				.collect()
		} else {
			Vec::new()
		}
	};
	circulate_statement_and_dependents(
		&mut state.topology_storage,
		&mut state.peers,
		&mut state.active_heads,
		ctx,
		relay_parent,
		statement,
		group_peers,
		metrics,
		rng,
	)
	.await;

	Ok(())
}

/// Check whether a peer knows about a candidate from us.
///
/// If not, it is deemed illegal for it to request corresponding data from us.
fn requesting_peer_knows_about_candidate(
	peers: &HashMap<PeerId, PeerData>,
	requesting_peer: &PeerId,
	relay_parent: &Hash,
	candidate_hash: &CandidateHash,
) -> JfyiErrorResult<bool> {
	let peer_data = peers
		.get(requesting_peer)
		.ok_or_else(|| JfyiError::NoSuchPeer(*requesting_peer))?;
	let knowledge = peer_data
		.view_knowledge
		.get(relay_parent)
		.ok_or_else(|| JfyiError::NoSuchHead(*relay_parent))?;
	Ok(knowledge.sent_candidates.get(&candidate_hash).is_some())
}

fn compatible_v1_message(
	version: ValidationVersion,
	message: protocol_v1::StatementDistributionMessage,
) -> net_protocol::StatementDistributionMessage {
	match version {
		ValidationVersion::V1 => Versioned::V1(message),
		ValidationVersion::V2 =>
			Versioned::V2(protocol_v2::StatementDistributionMessage::V1Compatibility(message)),
		ValidationVersion::V3 =>
			Versioned::V3(protocol_v3::StatementDistributionMessage::V1Compatibility(message)),
	}
}
