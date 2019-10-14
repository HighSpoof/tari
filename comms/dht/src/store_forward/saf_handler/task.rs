// Copyright 2019, The Tari Project
//
// Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
// following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
// disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
// following disclaimer in the documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
// products derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
// INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
// WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
// USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use crate::{
    config::DhtConfig,
    envelope::{DhtMessageType, NodeDestination},
    inbound::{DecryptedDhtMessage, DhtInboundMessage},
    outbound::{BroadcastStrategy, OutboundEncryption, OutboundMessageRequester},
    store_forward::{
        error::StoreAndForwardError,
        message::{StoredMessage, StoredMessagesRequest, StoredMessagesResponse},
        SAFStorage,
    },
};
use futures::{future, stream, Future, StreamExt};
use log::*;
use std::sync::Arc;
use tari_comms::{
    message::Message,
    peer_manager::{NodeIdentity, PeerManager, PeerManagerError},
    utils::{crypt, signature},
};
use tari_comms_middleware::MiddlewareError;
use tari_utilities::message_format::MessageFormat;
use tokio_executor::blocking;
use tower::{Service, ServiceExt};

const LOG_TARGET: &'static str = "comms::dht::store_forward";

pub struct MessageHandlerTask<S> {
    config: DhtConfig,
    next_service: S,
    peer_manager: Arc<PeerManager>,
    outbound_service: OutboundMessageRequester,
    node_identity: Arc<NodeIdentity>,
    message: Option<DecryptedDhtMessage>,
    store: Arc<SAFStorage>,
}

impl<S> MessageHandlerTask<S>
where S: Service<DecryptedDhtMessage, Response = (), Error = MiddlewareError>
{
    pub fn new(
        config: DhtConfig,
        next_service: S,
        store: Arc<SAFStorage>,
        peer_manager: Arc<PeerManager>,
        outbound_service: OutboundMessageRequester,
        node_identity: Arc<NodeIdentity>,
        message: DecryptedDhtMessage,
    ) -> Self
    {
        Self {
            config,
            store,
            next_service,
            peer_manager,
            outbound_service,
            node_identity,
            message: Some(message),
        }
    }

    pub async fn run(mut self) -> Result<(), MiddlewareError> {
        let message = self
            .message
            .take()
            .expect("DhtInboundMessageTask initialized without message");

        if message.dht_header.message_type.is_dht_message() && message.decryption_failed() {
            debug!(
                target: LOG_TARGET,
                "Received SAFRetrieveMessages message which could not decrypt from NodeId={}. Discarding message.",
                message.source_peer.node_id
            );
            return Ok(());
        }

        match message.dht_header.message_type {
            DhtMessageType::SAFRequestMessages => self.handle_stored_messages_request(message).await?,

            DhtMessageType::SAFStoredMessages => self.handle_stored_messages(message).await?,
            // Not a SAF message, call downstream middleware
            _ => {
                trace!(target: LOG_TARGET, "Passing message onto next service");
                self.next_service.oneshot(message).await?
            },
        }

        Ok(())
    }

    async fn handle_stored_messages_request(
        &mut self,
        message: DecryptedDhtMessage,
    ) -> Result<(), StoreAndForwardError>
    {
        trace!(
            target: LOG_TARGET,
            "Received request for stored message from {}",
            message.comms_header.message_public_key
        );
        let msg = message
            .success()
            .expect("already checked that this message decrypted successfully");
        let retrieve_msgs = StoredMessagesRequest::from_binary(&msg.body)?;

        if !self.peer_manager.in_network_region(
            &message.source_peer.node_id,
            &self.node_identity.identity.node_id,
            self.config.saf_num_closest_nodes,
        )? {
            debug!(
                target: LOG_TARGET,
                "Received store and forward message requests from node outside of this nodes network region"
            );
            return Ok(());
        }

        // Compile a set of stored messages for the requesting peer
        // TODO: compiling the bundle of messages is slow, especially when there are many stored messages, a
        //       better approach should be used.

        let messages = self.store.with_inner(|mut store| {
            store
                .iter()
                // All messages within start_time (if specified)
                .filter(|(_, msg)| {
                    retrieve_msgs.since.map(|since| since <= msg.stored_at).unwrap_or(true)
                })
                .filter(|(_, msg)|{
                    match &msg.dht_header.destination {
                        // The stored message was sent with an undisclosed recipient. Perhaps this node
                        // is interested in it
                        NodeDestination::Undisclosed => true,
                        // Was the stored message sent for the requesting node public key?
                        NodeDestination::PublicKey(dest_public_key) => dest_public_key == &message.source_peer.public_key,
                        // Was the stored message sent for the requesting node node id?
                        NodeDestination::NodeId(dest_node_id) => dest_node_id == &message.source_peer.node_id,
                    }
                })
                .take(self.config.saf_max_returned_messages)
                .map(|(_, msg)| msg)
                .cloned()
                .collect::<Vec<_>>()
        });

        let stored_messages: StoredMessagesResponse = messages.into();

        trace!(
            target: LOG_TARGET,
            "Responding to received message retrieval request with {} message(s)",
            stored_messages.len()
        );
        self.outbound_service
            .send_message(
                BroadcastStrategy::DirectPublicKey(message.source_peer.public_key),
                NodeDestination::Undisclosed,
                OutboundEncryption::EncryptForDestination,
                DhtMessageType::SAFStoredMessages,
                stored_messages,
            )
            .await?;

        Ok(())
    }

    async fn handle_stored_messages(self, message: DecryptedDhtMessage) -> Result<(), StoreAndForwardError> {
        trace!(
            target: LOG_TARGET,
            "Received stored messages from {}",
            message.comms_header.message_public_key
        );
        let msg = message
            .success()
            .expect("already checked that this message decrypted successfully");
        let response = StoredMessagesResponse::from_binary(&msg.body)?;

        debug!(
            target: LOG_TARGET,
            "Received {} stored messages from neighbouring peer",
            response.len()
        );

        let tasks = response
            .messages
            .into_iter()
            // Map to futures which process the stored message
            .map(|msg| self.process_incoming_stored_message(msg));

        let successful_msgs_iter = future::join_all(tasks)
            .await
            .into_iter()
            .map(|result| {
                match &result {
                    // Decryption can fail, the message wasn't for this node
                    Err(err @ StoreAndForwardError::DecryptionFailed) => {
                        debug!(
                            target: LOG_TARGET,
                            "Unable to decrypt stored message sent by {}: {}", message.source_peer.node_id, err
                        );
                    },
                    Err(StoreAndForwardError::PeerManagerError(PeerManagerError::PeerNotFoundError)) => {
                        debug!(target: LOG_TARGET, "Origin peer not found. Discarding stored message.");
                    },

                    // Every other error shouldn't happen if the sending node is behaving
                    Err(err) => {
                        // TODO: Ban node?
                        warn!(
                            target: LOG_TARGET,
                            "SECURITY: invalid store and forward message was discarded from NodeId={}. Reason: {}. \
                             These messages should never have been forwarded. This is a sign of a badly behaving node.",
                            message.source_peer.node_id,
                            err
                        );
                    },
                    _ => {},
                }

                result
            })
            .filter(Result::is_ok)
            .map(Result::unwrap);

        self.next_service
            .call_all(stream::iter(successful_msgs_iter))
            .unordered()
            .for_each(|service_result| {
                if let Err(err) = service_result {
                    error!(target: LOG_TARGET, "Error when calling next service: {}", err);
                }
                future::ready(())
            })
            .await;

        Ok(())
    }

    fn process_incoming_stored_message(
        &self,
        message: StoredMessage,
    ) -> impl Future<Output = Result<DecryptedDhtMessage, StoreAndForwardError>>
    {
        let node_identity = Arc::clone(&self.node_identity);
        let peer_manager = Arc::clone(&self.peer_manager);
        blocking::run(move || {
            Self::check_destination(&node_identity, &message)?;
            // Check signature
            Self::check_signature(&message)?;
            // Decrypt
            let decrypted_message = Self::try_decrypt(&node_identity, &message)?;

            let peer = peer_manager.find_with_public_key(&message.dht_header.origin_public_key)?;

            let inbound_msg = DhtInboundMessage::new(message.dht_header, peer, message.comms_header, vec![]);

            Ok(DecryptedDhtMessage::succeeded(decrypted_message, inbound_msg))
        })
    }

    fn check_destination(node_identity: &NodeIdentity, msg: &StoredMessage) -> Result<(), StoreAndForwardError> {
        Some(&msg.dht_header.destination)
            .filter(|destination| match destination {
                NodeDestination::Undisclosed => true,
                NodeDestination::PublicKey(pk) => node_identity.public_key() == pk,
                NodeDestination::NodeId(node_id) => node_identity.node_id() == node_id,
            })
            .map(|_| ())
            .ok_or(StoreAndForwardError::InvalidDestination)
    }

    fn check_signature(msg: &StoredMessage) -> Result<(), StoreAndForwardError> {
        signature::verify(
            &msg.dht_header.origin_public_key,
            &msg.dht_header.origin_signature,
            &msg.encrypted_body,
        )
        .map_err(|_| StoreAndForwardError::InvalidSignature)
        .and_then(|is_valid| match is_valid {
            true => Ok(()),
            false => Err(StoreAndForwardError::InvalidSignature),
        })
    }

    fn try_decrypt(node_identity: &NodeIdentity, msg: &StoredMessage) -> Result<Message, StoreAndForwardError> {
        let shared_secret = crypt::generate_ecdh_secret(&node_identity.secret_key, &msg.dht_header.origin_public_key);
        let decrypted_bytes = crypt::decrypt(&shared_secret, &msg.encrypted_body)?;
        Message::from_binary(&decrypted_bytes).map_err(|_| StoreAndForwardError::DecryptionFailed)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        envelope::DhtMessageFlags,
        test_utils::{make_dht_inbound_message, make_node_identity, make_peer_manager, service_spy},
    };
    use bitflags::_core::time::Duration;
    use chrono::Utc;
    use futures::channel::mpsc;
    use tari_comms::message::MessageHeader;
    use tari_test_utils::runtime;

    // TODO: unit tests for static functions (check_signature, etc)

    #[test]
    fn request_stored_messages() {
        runtime::test_async(|rt| {
            let spy = service_spy();
            let storage = Arc::new(SAFStorage::new(10));

            let peer_manager = make_peer_manager();
            let (oms_tx, mut oms_rx) = mpsc::channel(1);

            let node_identity = make_node_identity();

            // Recent message
            let inbound_msg = make_dht_inbound_message(&node_identity, vec![], DhtMessageFlags::empty());
            storage.insert(
                vec![0],
                StoredMessage::new(0, inbound_msg.comms_header, inbound_msg.dht_header, b"A".to_vec()),
                Duration::from_secs(60),
            );

            // Expired message
            let inbound_msg = make_dht_inbound_message(&node_identity, vec![], DhtMessageFlags::empty());
            storage.insert(
                vec![1],
                StoredMessage::new(0, inbound_msg.comms_header, inbound_msg.dht_header, vec![]),
                Duration::from_secs(0),
            );

            // Out of time range
            let inbound_msg = make_dht_inbound_message(&node_identity, vec![], DhtMessageFlags::empty());
            let mut msg = StoredMessage::new(0, inbound_msg.comms_header, inbound_msg.dht_header, vec![]);
            msg.stored_at = Utc::now().checked_sub_signed(chrono::Duration::days(1)).unwrap();

            let mut message = DecryptedDhtMessage::succeeded(
                Message::from_message_format(
                    MessageHeader::new(()).unwrap(),
                    StoredMessagesRequest::since(Utc::now().checked_sub_signed(chrono::Duration::seconds(60)).unwrap()),
                )
                .unwrap(),
                make_dht_inbound_message(&node_identity, vec![], DhtMessageFlags::ENCRYPTED),
            );
            message.dht_header.message_type = DhtMessageType::SAFRequestMessages;

            let task = MessageHandlerTask::new(
                Default::default(),
                spy.service::<MiddlewareError>(),
                storage,
                peer_manager,
                OutboundMessageRequester::new(oms_tx),
                node_identity,
                message,
            );
            rt.spawn(async move {
                task.run().await.unwrap();
                let msg = unwrap_oms_send_msg!(oms_rx.next().await.unwrap());
                let msg = Message::from_binary(&msg.body).unwrap();
                let msg = StoredMessagesResponse::from_binary(&msg.body).unwrap();
                assert_eq!(msg.messages.len(), 1);
                assert_eq!(msg.messages[0].encrypted_body, b"A");
                assert!(!spy.is_called());
            });
        });
    }

    #[test]
    fn receive_stored_messages() {
        runtime::test_async(|rt| {
            let spy = service_spy();
            let storage = Arc::new(SAFStorage::new(10));

            let peer_manager = make_peer_manager();
            let (oms_tx, _) = mpsc::channel(1);

            let node_identity = make_node_identity();

            let shared_key = crypt::generate_ecdh_secret(&node_identity.secret_key, node_identity.public_key());
            let msg_a = crypt::encrypt(
                &shared_key,
                &Message::from_message_format((), b"A".to_vec())
                    .unwrap()
                    .to_binary()
                    .unwrap(),
            )
            .unwrap();

            let inbound_msg_a = make_dht_inbound_message(&node_identity, msg_a.clone(), DhtMessageFlags::ENCRYPTED);
            // Need to know the peer to process a stored message
            peer_manager.add_peer(inbound_msg_a.source_peer.clone()).unwrap();
            let msg_b = crypt::encrypt(
                &shared_key,
                &Message::from_message_format((), b"B".to_vec())
                    .unwrap()
                    .to_binary()
                    .unwrap(),
            )
            .unwrap();

            let inbound_msg_b = make_dht_inbound_message(&node_identity, msg_b.clone(), DhtMessageFlags::ENCRYPTED);
            // Need to know the peer to process a stored message
            peer_manager.add_peer(inbound_msg_b.source_peer.clone()).unwrap();

            let mut message = DecryptedDhtMessage::succeeded(
                Message::from_message_format(MessageHeader::new(()).unwrap(), StoredMessagesResponse {
                    messages: vec![
                        StoredMessage::new(
                            0,
                            inbound_msg_a.comms_header.clone(),
                            inbound_msg_a.dht_header.clone(),
                            msg_a,
                        ),
                        StoredMessage::new(0, inbound_msg_b.comms_header, inbound_msg_b.dht_header, msg_b),
                    ],
                })
                .unwrap(),
                make_dht_inbound_message(&node_identity, vec![], DhtMessageFlags::ENCRYPTED),
            );
            message.dht_header.message_type = DhtMessageType::SAFStoredMessages;

            let task = MessageHandlerTask::new(
                Default::default(),
                spy.service::<MiddlewareError>(),
                storage,
                peer_manager,
                OutboundMessageRequester::new(oms_tx),
                node_identity,
                message,
            );
            rt.spawn(async move {
                task.run().await.unwrap();
                assert_eq!(spy.call_count(), 2);
                let requests = spy.take_requests();
                assert_eq!(requests.len(), 2);
                // Deserialize each request into the message (a vec of a single byte in this case)
                let msgs = requests
                    .into_iter()
                    .map(|req| req.success().unwrap().deserialize_message().unwrap())
                    .collect::<Vec<Vec<u8>>>();
                assert!(msgs.contains(&b"A".to_vec()));
                assert!(msgs.contains(&b"B".to_vec()));
            });
        });
    }
}
