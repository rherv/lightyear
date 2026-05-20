use bevy_app::prelude::*;
use bevy_ecs::prelude::*;
use bevy_state::prelude::*;

use bevy_replicon::prelude::*;
use bevy_replicon::server::visibility::client_visibility::ClientVisibility;
use bevy_replicon::shared::backend::connected_client::NetworkId;
use lightyear_connection::client::Connected;
use lightyear_connection::client_of::ClientOf;
use lightyear_connection::host::HostClient;
use lightyear_connection::server::{Started, Stopped};
use lightyear_core::id::RemoteId;
use lightyear_core::prelude::LocalTimeline;
use lightyear_link::prelude::Server;
use lightyear_transport::channel::receivers::ChannelReceive;
use lightyear_transport::plugin::TransportSystems;
use lightyear_transport::prelude::Transport;

use crate::channels::RepliconChannelMap;
use crate::checkpoint::wrap_server_payload;
use lightyear_messages::plugin::MessageSystems;
use tracing::{error, info, trace};

/// Adds the replicon server-side backend bridge for lightyear.
///
/// Handles:
/// - `ServerState` transitions (Running when server starts or client connects)
/// - `ConnectedClient` insertion for replicon visibility
/// - Sending `ServerMessages` (replication) and receiving `ClientMessages` (acks) via transport
pub struct RepliconServerPlugin;

impl Plugin for RepliconServerPlugin {
    fn build(&self, app: &mut App) {
        // When Connected is added to a link entity, add replicon's ConnectedClient + NetworkId
        app.add_observer(on_client_connected);

        // State management
        app.add_systems(
            PreUpdate,
            sync_server_state.before(ServerSystems::ReceivePackets),
        );

        // Packet bridge: replicon <-> lightyear transport
        app.add_systems(
            PreUpdate,
            receive_server_packets.in_set(ServerSystems::ReceivePackets),
        );
        app.add_systems(
            PostUpdate,
            send_server_packets.in_set(ServerSystems::SendPackets),
        );

        app.configure_sets(
            PreUpdate,
            ServerSystems::ReceivePackets
                .after(TransportSystems::Receive)
                // Replicon bridge must read its channels before lightyear's MessagePlugin::recv
                // drains ALL transport receivers (including replicon channels)
                .before(MessageSystems::Receive),
        );
        app.configure_sets(
            PostUpdate,
            ServerSystems::SendPackets.before(TransportSystems::Send),
        );
    }
}

/// When `Connected` is added to a remote client link entity, insert replicon's
/// `ConnectedClient` and `NetworkId` so replicon's packet path can target it.
///
/// Host-clients intentionally do not become replicon `ConnectedClient`s because they share the
/// same world as the server and may otherwise collide with a real remote client's `NetworkId`.
/// They only need `ClientVisibility` for lightyear's same-app visibility hooks.
fn on_client_connected(
    _trigger: On<Add, Connected>,
    remotes: Query<(Entity, &RemoteId), (Added<Connected>, With<ClientOf>, Without<HostClient>)>,
    hosts: Query<Entity, (Added<Connected>, With<HostClient>)>,
    mut commands: Commands,
) {
    for (entity, remote_id) in remotes.iter() {
        commands.entity(entity).insert((
            ConnectedClient {
                max_size: lightyear_transport::packet::packet_builder::MAX_PACKET_SIZE,
            },
            NetworkId::new(remote_id.to_bits()),
        ));
    }

    for entity in hosts.iter() {
        commands.entity(entity).insert(ClientVisibility::default());
    }
}

/// Sync replicon's `ServerState` with lightyear lifecycle.
///
/// Sets `Running` when `Started` is present (server app).
fn sync_server_state(
    started: Query<(), (With<Server>, With<Started>)>,
    stopped: Query<(), (With<Server>, With<Stopped>)>,
    state: Res<State<ServerState>>,
    mut next_state: ResMut<NextState<ServerState>>,
) {
    if !started.is_empty() && *state.get() != ServerState::Running {
        next_state.set(ServerState::Running);
    }
    if started.is_empty() && !stopped.is_empty() && *state.get() != ServerState::Stopped {
        next_state.set(ServerState::Stopped);
    }
}

/// Receive packets from transports and populate `ServerMessages` (ack data from peers).
///
/// Reads from client_channels (MutationAcks) on each transport and puts into `ServerMessages`.
fn receive_server_packets(
    channel_map: Res<RepliconChannelMap>,
    mut server_messages: ResMut<ServerMessages>,
    mut transports: Query<(Entity, &mut Transport), With<ClientOf>>,
) {
    for (entity, mut transport) in transports.iter_mut() {
        for (idx, &(_, channel_id)) in channel_map.client_channels.iter().enumerate() {
            if let Some(receiver) = transport.receivers.get_mut(&channel_id) {
                while let Some((_, message, _)) = receiver.receiver.read_message() {
                    server_messages.insert_received(entity, idx, message);
                }
            }
        }
    }
}

/// Send `ServerMessages` (replication data) via transport to peers.
///
/// Drains `ServerMessages` and sends on server_channels (Updates, Mutations).
fn send_server_packets(
    channel_map: Res<RepliconChannelMap>,
    timeline: Res<LocalTimeline>,
    mut server_messages: ResMut<ServerMessages>,
    mut transports: Query<&mut Transport, With<ClientOf>>,
) {
    for (client, channel_idx, message) in server_messages.drain_sent() {
        let (channel_kind, _) = channel_map.server_channels[channel_idx];
        let message = match channel_idx {
            // Replicon channels 0/1 feed ConfirmHistory / ServerMutateTicks on the client.
            // Prefix them with the authoritative server Lightyear tick so prediction can later
            // translate Replicon checkpoint ticks back into simulation time.
            0 | 1 => wrap_server_payload(timeline.tick(), message),
            _ => message,
        };
        if matches!(channel_idx, 0 | 1)
            && message.len() > lightyear_transport::packet::packet_builder::MAX_PACKET_SIZE
        {
            info!(
                channel_idx,
                client = ?client,
                timeline_tick = ?timeline.tick(),
                wrapped_len = message.len(),
                max_packet_size = lightyear_transport::packet::packet_builder::MAX_PACKET_SIZE,
                "wrapped replicon payload exceeds packet budget; relying on transport fragmentation"
            );
        }
        trace!(
            "send_server_packets: sending {} bytes on channel_idx={} to {:?}",
            message.len(),
            channel_idx,
            client
        );
        if let Ok(mut transport) = transports.get_mut(client) {
            if let Err(error_kind) = transport.send_mut_erased(channel_kind, message, 1.0) {
                error!(
                    ?error_kind,
                    channel_idx,
                    client = ?client,
                    "failed to queue replicon server payload into transport sender"
                );
            }
        } else {
            trace!("send_server_packets: no transport for client {:?}", client);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{send_server_packets, sync_server_state};
    use bevy_app::{App, Update};
    use bevy_ecs::system::RunSystemOnce;
    use bevy_replicon::core::server_entity_map::ServerEntityMap;
    use bevy_replicon::prelude::ServerState;
    use bevy_replicon::server::ServerMessages;
    use bevy_state::app::StatesPlugin;
    use bevy_state::state::State;
    use bytes::Bytes;
    use lightyear_connection::client::PeerMetadata;
    use lightyear_connection::client_of::ClientOf;
    use lightyear_connection::server::Stopped;
    use lightyear_core::prelude::LocalTimeline;
    use lightyear_link::prelude::Server;
    use lightyear_transport::channel::senders::ChannelSend;
    use lightyear_transport::plugin::TransportPlugin;
    use lightyear_transport::prelude::Transport;
    use test_log::test;

    use crate::channels::{
        RepliconChannelMap, RepliconChannelRegistrationPlugin, RepliconMutationsChannel,
        RepliconUpdatesChannel,
    };

    #[test]
    fn non_server_stopped_marker_does_not_stop_local_sender() {
        let mut app = App::new();
        app.add_plugins(StatesPlugin)
            .init_resource::<PeerMetadata>()
            .init_state::<ServerState>()
            .add_systems(Update, sync_server_state)
            .insert_state(ServerState::Running);

        app.world_mut().spawn(Stopped);

        app.update();
        app.update();

        assert_eq!(
            *app.world().resource::<State<ServerState>>().get(),
            ServerState::Running
        );
    }

    #[test]
    fn stopped_server_entity_transitions_state_to_stopped() {
        let mut app = App::new();
        app.add_plugins(StatesPlugin)
            .init_resource::<PeerMetadata>()
            .init_state::<ServerState>()
            .add_systems(Update, sync_server_state)
            .insert_state(ServerState::Running);

        app.world_mut().spawn((Server::default(), Stopped));

        app.update();
        app.update();

        assert_eq!(
            *app.world().resource::<State<ServerState>>().get(),
            ServerState::Stopped
        );
    }

    #[test]
    fn send_bridge_fragments_channel_zero_payload_that_would_overflow_after_wrapping() {
        let mut app = App::new();
        app.add_plugins(TransportPlugin)
            .add_plugins(RepliconChannelRegistrationPlugin)
            .insert_resource(LocalTimeline::default())
            .insert_resource(ServerMessages::new(ServerEntityMap::default()));

        let registry = app.world().resource::<lightyear_transport::prelude::ChannelRegistry>();
        let mut transport = Transport::default();
        transport.add_sender_from_registry::<RepliconUpdatesChannel>(registry);
        transport.add_sender_from_registry::<RepliconMutationsChannel>(registry);

        let client = app.world_mut().spawn((ClientOf, transport)).id();

        // Channel 0 is wrapped in send_server_packets; this wrapped payload exceeds 1200 bytes
        // and must be sent through the transport fragmentation path.
        let payload = Bytes::from(vec![0_u8; 1778]);
        app.world_mut()
            .resource_mut::<ServerMessages>()
            .insert_sent(client, 0, payload);

        app.world_mut().run_system_once(send_server_packets).unwrap();

        let channel_kind = app.world().resource::<RepliconChannelMap>().server_channels[0].0;
        let mut transport = app.world_mut().get_mut::<Transport>(client).unwrap();
        let sender = transport.senders.get_mut(&channel_kind).unwrap();
        let (single, fragmented) = sender.sender.send_packet();
        assert!(single.is_empty());
        assert!(!fragmented.is_empty());
    }

    #[test]
    fn send_bridge_keeps_channel_zero_payload_when_wrapper_fits_budget() {
        let mut app = App::new();
        app.add_plugins(TransportPlugin)
            .add_plugins(RepliconChannelRegistrationPlugin)
            .insert_resource(LocalTimeline::default())
            .insert_resource(ServerMessages::new(ServerEntityMap::default()));

        let registry = app.world().resource::<lightyear_transport::prelude::ChannelRegistry>();
        let mut transport = Transport::default();
        transport.add_sender_from_registry::<RepliconUpdatesChannel>(registry);
        transport.add_sender_from_registry::<RepliconMutationsChannel>(registry);

        let client = app.world_mut().spawn((ClientOf, transport)).id();
        let payload = Bytes::from(vec![0_u8; 1193]);
        app.world_mut()
            .resource_mut::<ServerMessages>()
            .insert_sent(client, 0, payload);

        app.world_mut().run_system_once(send_server_packets).unwrap();

        let channel_kind = app.world().resource::<RepliconChannelMap>().server_channels[0].0;
        let mut transport = app.world_mut().get_mut::<Transport>(client).unwrap();
        let sender = transport.senders.get_mut(&channel_kind).unwrap();
        let (single, fragmented) = sender.sender.send_packet();
        assert!(!single.is_empty() || !fragmented.is_empty());
    }
}
