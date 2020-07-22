// Copyright 2020 Parity Technologies (UK) Ltd.
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

//! The bitfield distribution
//!
//! In case this node is a validator, gossips its own signed availability bitfield
//! for a particular relay parent.
//! Independently of that, gossips on received messages from peers to other interested peers.

use codec::{Decode, Encode};
use futures::{channel::oneshot, FutureExt};

use node_primitives::{ProtocolId, View};

use log::{debug, info, trace, warn};
use polkadot_node_subsystem::messages::*;
use polkadot_node_subsystem::{
	FromOverseer, OverseerSignal, SpawnedSubsystem, Subsystem, SubsystemContext, SubsystemResult,
};
use polkadot_primitives::v1::{SigningContext, ValidatorId, Hash, SignedAvailabilityBitfield};
use sc_network::ReputationChange;
use std::collections::{HashMap, HashSet};

const COST_SIGNATURE_INVALID: ReputationChange =
	ReputationChange::new(-100, "Bitfield signature invalid");
const COST_VALIDATOR_INDEX_INVALID: ReputationChange =
	ReputationChange::new(-100, "Bitfield validator index invalid");
const COST_MISSING_PEER_SESSION_KEY: ReputationChange =
	ReputationChange::new(-133, "Missing peer session key");
const COST_NOT_INTERESTED: ReputationChange =
	ReputationChange::new(-51, "Not intersted in that parent hash");
const COST_MESSAGE_NOT_DECODABLE: ReputationChange =
	ReputationChange::new(-100, "Not intersted in that parent hash");
const GAIN_VALID_MESSAGE_FIRST: ReputationChange =
	ReputationChange::new(15, "Valid message with new information");
const GAIN_VALID_MESSAGE: ReputationChange =
	ReputationChange::new(5, "Valid message");

/// Checked signed availability bitfield that is distributed
/// to other peers.
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
pub struct BitfieldGossipMessage {
	/// The relay parent this message is relative to.
	pub relay_parent: Hash,
	/// The actual signed availability bitfield.
	pub signed_availability: SignedAvailabilityBitfield,
}

/// Data used to track information of peers and relay parents the
/// overseer ordered us to work on.
#[derive(Default, Clone)]
struct Tracker {
	/// track all active peers and their views
	/// to determine what is relevant to them.
	peer_views: HashMap<PeerId, View>,

	/// Our current view.
	view: View,

	/// Additional data particular to a relay parent.
	per_relay_parent: HashMap<Hash, PerRelayParentData>,
}

/// Data for a particular relay parent.
#[derive(Debug, Clone, Default)]
struct PerRelayParentData {
	/// Signing context for a particular relay parent.
	signing_context: SigningContext,

	/// Set of validators for a particular relay parent.
	validator_set: Vec<ValidatorId>,

	/// Set of validators for a particular relay parent for which we
	/// received a valid `BitfieldGossipMessage`.
	/// Also serves as the list of known messages for peers connecting
	/// after bitfield gossips were already received.
	one_per_validator: HashMap<ValidatorId, BitfieldGossipMessage>,

	/// which messages of which validators were already sent
	message_sent_to_peer: HashMap<PeerId, HashSet<ValidatorId>>,
}

impl PerRelayParentData {
	/// Determines if that particular message signed by a validator is needed by the given peer.
	fn message_from_validator_needed_by_peer(
		&self,
		peer: &PeerId,
		validator: &ValidatorId,
	) -> bool {
		if let Some(set) = self.message_sent_to_peer.get(peer) {
			!set.contains(validator)
		} else {
			false
		}
	}
}

fn network_update_message(n: NetworkBridgeEvent) -> AllMessages {
	AllMessages::BitfieldDistribution(BitfieldDistributionMessage::NetworkBridgeUpdate(n))
}

/// The bitfield distribution subsystem.
pub struct BitfieldDistribution;

impl BitfieldDistribution {
	/// The protocol identifier for bitfield distribution.
	const PROTOCOL_ID: ProtocolId = *b"bitd";

	/// Start processing work as passed on from the Overseer.
	async fn run<Context>(mut ctx: Context) -> SubsystemResult<()>
	where
		Context: SubsystemContext<Message = BitfieldDistributionMessage>,
	{
		// startup: register the network protocol with the bridge.
		ctx.send_message(AllMessages::NetworkBridge(
			NetworkBridgeMessage::RegisterEventProducer(Self::PROTOCOL_ID, network_update_message),
		))
		.await?;

		// work: process incoming messages from the overseer and process accordingly.
		let mut tracker = Tracker::default();
		loop {
			let message = ctx.recv().await?;
			match message {
				FromOverseer::Communication { msg } => {
					// another subsystem created this signed availability bitfield messages
					match msg {
						// relay_message a bitfield via gossip to other validators
						BitfieldDistributionMessage::DistributeBitfield(
							hash,
							signed_availability,
						) => {
							trace!(target: "bitd", "Processing DistributeBitfield");
							let job_data = &mut tracker.per_relay_parent.get_mut(&hash).expect("Overseer does not send work items related to relay parents that are not part of our workset. qed");

							let validator = {
								job_data
									.validator_set
									.get(signed_availability.validator_index() as usize)
									.expect("Our own validation index exists. qed")
							}
							.clone();

							let peer_views = &mut tracker.peer_views;
							let msg = BitfieldGossipMessage {
								relay_parent: hash,
								signed_availability,
							};

							relay_message(&mut ctx, job_data, peer_views, validator, msg)
								.await?;
						}
						BitfieldDistributionMessage::NetworkBridgeUpdate(event) => {
							trace!(target: "bitd", "Processing NetworkMessage");
							// a network message was received
							if let Err(e) =
								handle_network_msg(&mut ctx, &mut tracker, event).await
							{
								warn!(target: "bitd", "Failed to handle incomming network messages: {:?}", e);
							}
						}
					}
				}
				FromOverseer::Signal(OverseerSignal::StartWork(relay_parent)) => {
					trace!(target: "bitd", "Start {:?}", relay_parent);
					// query basic system parameters once
					// @todo assumption: these cannot change within a session
					let (validator_set, signing_context) =
						query_basics(&mut ctx, relay_parent).await?;

					let _ = tracker.per_relay_parent.insert(
						relay_parent,
						PerRelayParentData {
							signing_context,
							validator_set,
							..Default::default()
						},
					);
				}
				FromOverseer::Signal(OverseerSignal::StopWork(relay_parent)) => {
					trace!(target: "bitd", "Stop {:?}", relay_parent);
					// @todo assumption: it is good enough to prevent additional work from being
					// scheduled, the individual futures are supposedly completed quickly
					let _ = tracker.per_relay_parent.remove(&relay_parent);
				}
				FromOverseer::Signal(OverseerSignal::Conclude) => {
					trace!(target: "bitd", "Conclude");
					tracker.per_relay_parent.clear();
					return Ok(());
				}
			}
		}
	}
}

/// Modify the reputation of peer based on their behaviour.
async fn modify_reputation<Context>(
	ctx: &mut Context,
	peer: PeerId,
	rep: ReputationChange,
) -> SubsystemResult<()>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	trace!(target: "bitd", "Reputation change of {:?} for peer {:?}", rep, peer);
	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::ReportPeer(peer, rep),
	))
	.await
}

/// Distribute a given valid and signature checked bitfield message.
///
/// Can be originated by another subsystem or received via network from another peer.
async fn relay_message<Context>(
	ctx: &mut Context,
	job_data: &mut PerRelayParentData,
	peer_views: &mut HashMap<PeerId, View>,
	validator: ValidatorId,
	message: BitfieldGossipMessage,
) -> SubsystemResult<()>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{

	// notify the overseer about a new and valid signed bitfield
	ctx.send_message(
		AllMessages::Provisioner(
			ProvisionerMessage::ProvisionableData(
				ProvisionableData::Bitfield(
					message.relay_parent.clone(),
					message.signed_availability.clone(),
				)
			)
		)
	).await;

	let message_sent_to_peer = &mut (job_data.message_sent_to_peer);

	// concurrently pass on the bitfield distribution to all interested peers
	let interested_peers = peer_views
		.iter()
		.filter_map(|(peer, view)| {
			// check interest in the peer in this message's relay parent
			if view.contains(&message.relay_parent) {
				// track the message as sent for this peer
				message_sent_to_peer
					.entry(peer.clone())
					.or_default()
					.insert(validator.clone());

				Some(peer.clone())
			} else {
				None
			}
		})
		.collect::<Vec<PeerId>>();

	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::SendMessage(
			interested_peers,
			BitfieldDistribution::PROTOCOL_ID,
			Encode::encode(&message),
		),
	))
	.await?;
	Ok(())
}

/// Handle an incoming message from a peer.
async fn process_incoming_peer_message<Context>(
	ctx: &mut Context,
	tracker: &mut Tracker,
	origin: PeerId,
	message: BitfieldGossipMessage,
) -> SubsystemResult<()>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	// we don't care about this, not part of our view
	if !tracker.view.contains(&message.relay_parent) {
		return modify_reputation(ctx, origin, COST_NOT_INTERESTED).await;
	}

	// Ignore anything the overseer did not tell this subsystem to work on
	let mut job_data = tracker.per_relay_parent.get_mut(&message.relay_parent);
	let job_data: &mut _ = if let Some(ref mut job_data) = job_data {
		job_data
	} else {
		return modify_reputation(ctx, origin, COST_NOT_INTERESTED).await;
	};

	let validator_set = &job_data.validator_set;
	if validator_set.len() == 0 {
		return modify_reputation(ctx, origin, COST_MISSING_PEER_SESSION_KEY).await;
	}

	// use the (untrusted) validator index provided by the signed payload
	// and see if that one actually signed the availability bitset
	let signing_context = job_data.signing_context.clone();
	let validator_index = message.signed_availability.validator_index() as usize;
	let validator = if let Some(validator) = validator_set.get(validator_index) {
		validator.clone()
	} else {
		return modify_reputation(ctx, origin, COST_VALIDATOR_INDEX_INVALID).await;
	};

	if message
		.signed_availability
		.check_signature(&signing_context, &validator)
		.is_ok()
	{
		let one_per_validator = &mut (job_data.one_per_validator);
		// only relay_message a message of a validator once
		if one_per_validator.get(&validator).is_some() {
			trace!(target: "bitd", "Alrady received a message for validator at index {}", validator_index);
			return Ok(());
		}
		one_per_validator.insert(validator.clone(), message.clone());

		relay_message(ctx, job_data, &mut tracker.peer_views, validator, message).await;

		modify_reputation(ctx, origin, GAIN_VALID_MESSAGE).await
	} else {
		modify_reputation(ctx, origin, COST_SIGNATURE_INVALID).await
	}
}

/// Deal with network bridge updates and track what needs to be tracked
/// which depends on the message type received.
async fn handle_network_msg<Context>(
	ctx: &mut Context,
	tracker: &mut Tracker,
	bridge_message: NetworkBridgeEvent,
) -> SubsystemResult<()>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	match bridge_message {
		NetworkBridgeEvent::PeerConnected(peerid, _role) => {
			// insert if none already present
			tracker.peer_views.entry(peerid).or_insert(View::default());
		}
		NetworkBridgeEvent::PeerDisconnected(peerid) => {
			// get rid of superfluous data
			tracker.peer_views.remove(&peerid);
		}
		NetworkBridgeEvent::PeerViewChange(peerid, view) => {
			catch_up_messages(ctx, tracker, peerid, view).await?;
		}
		NetworkBridgeEvent::OurViewChange(view) => {
			let old_view = std::mem::replace(&mut (tracker.view), view);

			for new in tracker.view.difference(&old_view) {
				if !tracker.per_relay_parent.contains_key(&new) {
					warn!(target: "bitd", "Our view contains {} but the overseer never told use we should work on this", &new);
				}
			}
		}
		NetworkBridgeEvent::PeerMessage(remote, bytes) => {
			if let Ok(gossiped_bitfield) = BitfieldGossipMessage::decode(&mut (bytes.as_slice())) {
				trace!(target: "bitd", "Received bitfield gossip from peer {:?}", &remote);
				process_incoming_peer_message(ctx, tracker, remote, gossiped_bitfield).await?;
			} else {
				return modify_reputation(ctx, remote, COST_MESSAGE_NOT_DECODABLE).await;
			}
		}
	}
	Ok(())
}

// Send the difference between two views which were not sent
// to that particular peer.
async fn catch_up_messages<Context>(
	ctx: &mut Context,
	tracker: &mut Tracker,
	origin: PeerId,
	view: View,
) -> SubsystemResult<()>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	use std::collections::hash_map::Entry;
	let current = tracker.peer_views.entry(origin.clone()).or_default();

	let delta_vec: Vec<Hash> = (*current).difference(&view).cloned().collect();

	*current = view;

	// Send all messages we've seen before and the peer is now interested
	// in to that peer.

	let delta_set: HashMap<ValidatorId, BitfieldGossipMessage> = delta_vec
		.into_iter()
		.filter_map(|new_relay_parent_interest| {
			if let Some(job_data) = (&*tracker).per_relay_parent.get(&new_relay_parent_interest) {
				// send all messages
				let one_per_validator = job_data.one_per_validator.clone();
				let origin = origin.clone();
				Some(
					one_per_validator
						.into_iter()
						.filter(move |(validator, _message)| {
							// except for the ones the peer already has
							job_data.message_from_validator_needed_by_peer(&origin, validator)
						}),
				)
			} else {
				// A relay parent is in the peers view, which is not in ours, ignore those.
				None
			}
		})
		.flatten()
		.collect();

	for (validator, message) in delta_set.into_iter() {
		send_tracked_gossip_message(ctx, tracker, origin.clone(), validator, message).await?;
	}

	Ok(())
}

/// Send a gossip message and track it in the per relay parent data.
async fn send_tracked_gossip_message<Context>(
	ctx: &mut Context,
	tracker: &mut Tracker,
	dest: PeerId,
	validator: ValidatorId,
	message: BitfieldGossipMessage,
) -> SubsystemResult<()>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let job_data = if let Some(job_data) = tracker.per_relay_parent.get_mut(&message.relay_parent) {
		job_data
	} else {
		// @todo punishing here seems unreasonable
		return Ok(());
	};

	let message_sent_to_peer = &mut (job_data.message_sent_to_peer);
	message_sent_to_peer
		.entry(dest.clone())
		.or_default()
		.insert(validator.clone());

	let bytes = Encode::encode(&message);
	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::SendMessage(vec![dest], BitfieldDistribution::PROTOCOL_ID, bytes),
	))
	.await?;
	Ok(())
}

impl<C> Subsystem<C> for BitfieldDistribution
where
	C: SubsystemContext<Message = BitfieldDistributionMessage> + Sync + Send,
{
	fn start(self, ctx: C) -> SpawnedSubsystem {
		SpawnedSubsystem {
			name: "bitfield-distribution",
			future: Box::pin(async move { Self::run(ctx) }.map(|_| ())),
		}
	}
}

/// Query our validator set and signing context for a particular relay parent.
async fn query_basics<Context>(
	ctx: &mut Context,
	relay_parent: Hash,
) -> SubsystemResult<(Vec<ValidatorId>, SigningContext)>
where
	Context: SubsystemContext<Message = BitfieldDistributionMessage>,
{
	let (validators_tx, validators_rx) = oneshot::channel();
	let (signing_tx, signing_rx) = oneshot::channel();

	let query_validators = AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		relay_parent.clone(),
		RuntimeApiRequest::Validators(validators_tx),
	));

	let query_signing = AllMessages::RuntimeApi(RuntimeApiMessage::Request(
		relay_parent.clone(),
		RuntimeApiRequest::SigningContext(signing_tx),
	));

	ctx.send_messages(std::iter::once(query_validators).chain(std::iter::once(query_signing)))
		.await?;

	Ok((validators_rx.await?, signing_rx.await?))
}

#[cfg(test)]
mod test {
	use super::*;
	use bitvec::{bitvec, vec::BitVec};
	use futures::executor;
	use polkadot_primitives::v0::{Signed, ValidatorPair};
	use polkadot_primitives::v1::AvailabilityBitfield;
	use smol_timeout::TimeoutExt;
	use sp_core::crypto::Pair;
	use std::time::Duration;
	use maplit::{hashmap, hashset};

	macro_rules! msg_sequence {
		($( $input:expr ),+ $(,)? ) => [
			vec![ $( FromOverseer::Communication { msg: $input } ),+ ]
		];
	}

	macro_rules! view {
		( $( $hash:expr ),+ $(,)? ) => [
			View(vec![ $( $hash.clone() ),+ ])
		];
	}

	#[test]
	#[ignore]
	fn boundary_to_boundary() {
		let hash_a: Hash = [0; 32].into(); // us
		let hash_b: Hash = [1; 32].into(); // other

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();

		let signing_context = SigningContext {
			session_index: 1,
			parent_hash: hash_a.clone(),
		};

		// validator 0 key pair
		let (validator_pair, _seed) = ValidatorPair::generate();
		let validator = validator_pair.public();

		let payload = AvailabilityBitfield(bitvec![bitvec::order::Lsb0, u8; 1u8; 32]);
		let signed =
			Signed::<AvailabilityBitfield>::sign(payload, &signing_context, 0, &validator_pair);

		let input =
			msg_sequence![
				BitfieldDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::OurViewChange(view![hash_a, hash_b])
				),
				BitfieldDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerConnected(peer_b.clone(), ObservedRole::Full)
				),
				BitfieldDistributionMessage::NetworkBridgeUpdate(
					NetworkBridgeEvent::PeerViewChange(peer_b.clone(), view![hash_a, hash_b])
				),
				BitfieldDistributionMessage::DistributeBitfield(hash_b.clone(), signed.clone()),
			];

		// empty initial state
		let mut tracker = Tracker::default();

		let pool = sp_core::testing::SpawnBlockingExecutor::new();
		let (mut ctx, mut handle) =
			subsystem_test::make_subsystem_context::<BitfieldDistributionMessage, _>(pool);

		executor::block_on(async move {
			for input in input.into_iter() {
				handle.send(input.into());
			}

			// @todo cannot clone or move `ctx`
			//
			// let completion = BitfieldDistribution::start(BitfieldDistribution, ctx)
			//     .future
			//     .timeout(Duration::from_millis(1000))
			//     .await;

			// while let Ok(rxd) = ctx.recv().await {
			//     // @todo impl expectation checks against a hashmap
			//     dbg!(rxd);
			// }
		});
	}

	/// A very limited tracker, only interested in the relay parent of the
	/// given message, which must be signed by `validator` and a set of peers
	/// which are also only interested in that relay parent.
	fn prewarmed_tracker(
		validator: ValidatorId,
		signing_context: SigningContext,
		known_message: BitfieldGossipMessage,
		peers: Vec<PeerId>,
	) -> Tracker {
		let relay_parent = known_message.relay_parent.clone();
		Tracker {
			per_relay_parent: hashmap! {
				relay_parent.clone() =>
					PerRelayParentData {
						signing_context,
						validator_set: vec![validator.clone()],
						one_per_validator: hashmap! {
							validator.clone() => known_message.clone(),
						},
						message_sent_to_peer: hashmap! {},
					},
			},
			peer_views: peers
				.into_iter()
				.map(|peer| (peer, view!(relay_parent)))
				.collect(),
			view: view!(relay_parent),
		}
	}

	#[test]
	fn receive_invalid_signature() {
		let _ = env_logger::builder()
			.filter(None, log::LevelFilter::Trace)
			.is_test(true)
			.try_init();

		let hash_a: Hash = [0; 32].into();
		let hash_b: Hash = [1; 32].into(); // other

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();
		assert_ne!(peer_a, peer_b);

		let signing_context = SigningContext {
			session_index: 1,
			parent_hash: hash_a.clone(),
		};

		// validator 0 key pair
		let (validator_pair, _seed) = ValidatorPair::generate();
		let validator = validator_pair.public();

		// another validator not part of the validatorset
		let (mallicious, _seed) = ValidatorPair::generate();

		let payload = AvailabilityBitfield(bitvec![bitvec::order::Lsb0, u8; 1u8; 32]);
		let signed =
			Signed::<AvailabilityBitfield>::sign(payload, &signing_context, 0, &mallicious);

		let msg = BitfieldGossipMessage {
			relay_parent: hash_a.clone(),
			signed_availability: signed.clone(),
		};

		let pool = sp_core::testing::SpawnBlockingExecutor::new();
		let (mut ctx, mut handle) =
			subsystem_test::make_subsystem_context::<BitfieldDistributionMessage, _>(pool);

		let mut tracker = prewarmed_tracker(
			validator.clone(),
			signing_context.clone(),
			msg.clone(),
			vec![peer_b.clone()],
		);

		executor::block_on(async move {
			let _res = handle_network_msg(&mut ctx, &mut tracker, NetworkBridgeEvent::PeerMessage(peer_b.clone(), msg.encode()))
				.timeout(Duration::from_millis(10))
				.await
				.expect("10ms is more than enough for sending messages. qed")
				.expect("There are no error values. qed");

			// we should have a reputiation change due to invalid signature
			// @todo assess if Eq+PartialEq are viable to simplify this kind of code
			if let AllMessages::NetworkBridge(NetworkBridgeMessage::ReportPeer(peer, rep)) = handle.recv().await {
				assert_eq!(peer, peer_b);
				assert_eq!(rep, COST_SIGNATURE_INVALID);
			} else {
				panic!("Received unexpected message type.");
			}
		});
	}


	#[test]
	fn receive_invalid_validator_index() {
		let _ = env_logger::builder()
			.filter(None, log::LevelFilter::Trace)
			.is_test(true)
			.try_init();

		let hash_a: Hash = [0; 32].into();
		let hash_b: Hash = [1; 32].into(); // other

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();
		assert_ne!(peer_a, peer_b);

		let signing_context = SigningContext {
			session_index: 1,
			parent_hash: hash_a.clone(),
		};

		// validator 0 key pair
		let (validator_pair, _seed) = ValidatorPair::generate();
		let validator = validator_pair.public();

		let payload = AvailabilityBitfield(bitvec![bitvec::order::Lsb0, u8; 1u8; 32]);
		let signed =
			Signed::<AvailabilityBitfield>::sign(payload, &signing_context, 42, &validator_pair);

		let msg = BitfieldGossipMessage {
			relay_parent: hash_a.clone(),
			signed_availability: signed.clone(),
		};

		let pool = sp_core::testing::SpawnBlockingExecutor::new();
		let (mut ctx, mut handle) =
			subsystem_test::make_subsystem_context::<BitfieldDistributionMessage, _>(pool);

		let mut tracker = prewarmed_tracker(
			validator.clone(),
			signing_context.clone(),
			msg.clone(),
			vec![peer_b.clone()],
		);

		executor::block_on(async move {
			let _res = handle_network_msg(&mut ctx, &mut tracker, NetworkBridgeEvent::PeerMessage(peer_b.clone(), msg.encode()))
				.timeout(Duration::from_millis(10))
				.await
				.expect("10ms is more than enough for sending messages. qed")
				.expect("There are no error values. qed");

			// we should have a reputiation change due to invalid signature
			// @todo assess if Eq+PartialEq are viable to simplify this kind of code
			if let AllMessages::NetworkBridge(NetworkBridgeMessage::ReportPeer(peer, rep)) = handle.recv().await {
				assert_eq!(peer, peer_b);
				assert_eq!(rep, COST_VALIDATOR_INDEX_INVALID);
			} else {
				panic!("Received unexpected message type.");
			}
		});
	}





	#[test]
	fn duplicate_message() {
		let _ = env_logger::builder()
			.filter(None, log::LevelFilter::Trace)
			.is_test(true)
			.try_init();

		let hash_a: Hash = [0; 32].into();
		let hash_b: Hash = [1; 32].into(); // other

		let peer_a = PeerId::random();
		let peer_b = PeerId::random();
		assert_ne!(peer_a, peer_b);

		let signing_context = SigningContext {
			session_index: 1,
			parent_hash: hash_a.clone(),
		};

		// validator 0 key pair
		let (validator_pair, _seed) = ValidatorPair::generate();
		let validator = validator_pair.public();

		let payload = AvailabilityBitfield(bitvec![bitvec::order::Lsb0, u8; 1u8; 32]);
		let signed =
			Signed::<AvailabilityBitfield>::sign(payload, &signing_context, 42, &validator_pair);

		let msg = BitfieldGossipMessage {
			relay_parent: hash_a.clone(),
			signed_availability: signed.clone(),
		};

		let pool = sp_core::testing::SpawnBlockingExecutor::new();
		let (mut ctx, mut handle) =
			subsystem_test::make_subsystem_context::<BitfieldDistributionMessage, _>(pool);

		let mut tracker = prewarmed_tracker(
			validator.clone(),
			signing_context.clone(),
			msg.clone(),
			vec![peer_b.clone()],
		);

		executor::block_on(async move {
			let _res = handle_network_msg(&mut ctx, &mut tracker, NetworkBridgeEvent::PeerMessage(peer_b.clone(), msg.encode()))
				.timeout(Duration::from_millis(10))
				.await
				.expect("10ms is more than enough for sending messages. qed")
				.expect("There are no error values. qed");

			if let AllMessages::NetworkBridge(NetworkBridgeMessage::ReportPeer(peer, rep)) = handle.recv().await {
				assert_eq!(peer, peer_b);
				assert_eq!(rep, COST_VALIDATOR_INDEX_INVALID);
			} else {
				panic!("Received unexpected message type.");
			}

			let _res = handle_network_msg(&mut ctx, &mut tracker, NetworkBridgeEvent::PeerMessage(peer_b.clone(), msg.encode()))
				.timeout(Duration::from_millis(10))
				.await
				.expect("10ms is more than enough for sending messages. qed")
				.expect("There are no error values. qed");

			// we should have a reputiation change due to invalid signature
			// @todo assess if Eq+PartialEq are viable to simplify this kind of code
			if let AllMessages::NetworkBridge(NetworkBridgeMessage::ReportPeer(peer, rep)) = handle.recv().await {
				assert_eq!(peer, peer_b);
				assert_eq!(rep, COST_VALIDATOR_INDEX_INVALID);
			} else {
				panic!("Received unexpected message type.");
			}
		});
	}
}
