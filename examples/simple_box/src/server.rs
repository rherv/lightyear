//! The server side of the example.
//! It is possible (and recommended) to run the server in headless mode (without any rendering plugins).
//!
//! The server will:
//! - spawn a new player entity for each client that connects
//! - read inputs from the clients and move the player entities accordingly
//!
//! Lightyear will handle the replication of entities automatically if you add a `Replicate` component to them.
use crate::automation::AutomationServerPlugin;
use crate::protocol::*;
use crate::shared;
use bevy::prelude::*;
use bevy_enhanced_input::prelude::{Action, Fire};
use lightyear::connection::client::Connected;
use lightyear::connection::host::{HostClient, HostServer};
use lightyear::prelude::server::*;
use lightyear::prelude::*;
use lightyear_examples_common::shared::SEND_INTERVAL;

pub struct ExampleServerPlugin;

impl Plugin for ExampleServerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(AutomationServerPlugin);
        app.insert_resource(ReplicationMetadata::new(SEND_INTERVAL));
        // the physics/FixedUpdates systems that consume inputs should be run in this set.
        app.add_observer(move_player);
        app.add_observer(handle_new_client);
        app.add_observer(handle_connected);
        app.add_systems(Update, send_message);
    }
}

/// When a new client tries to connect to a server, an entity is created for it with the `LinkOf` component.
/// This entity represents the link between the server and that client.
///
/// You can add additional components to update the link. In this case we will add a `ReplicationSender` that
/// will enable us to replicate local entities to that client.
pub(crate) fn handle_new_client(trigger: On<Add, LinkOf>, mut commands: Commands) {
    commands
        .entity(trigger.entity)
        .insert((ReplicationSender, Name::from("Client")));
}

/// If the new client connects to the server, we want to spawn a new player entity for it.
///
/// We have to react specifically on `Connected` because there is no guarantee that the connection request we
/// received was valid. The server could reject the connection attempt for many reasons (server is full, packet is invalid,
/// DDoS attempt, etc.). We want to start the replication only when the client is confirmed as connected.
pub(crate) fn handle_connected(
    trigger: On<Add, Connected>,
    query: Query<&RemoteId, With<ClientOf>>,
    mut commands: Commands,
) {
    let Ok(client_id) = query.get(trigger.entity) else {
        return;
    };
    let client_id = client_id.0;
    let player_entity = commands
        .spawn((
            Player,
            PlayerBundle::new(client_id, Vec2::ZERO),
            // we replicate the Player entity to all clients that are connected to this server
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(NetworkTarget::Single(client_id)),
            InterpolationTarget::to_clients(NetworkTarget::AllExceptSingle(client_id)),
            ControlledBy {
                owner: trigger.entity,
                lifetime: Default::default(),
            },
        ))
        .id();
    info!(
        "Create player entity {:?} for client {:?}",
        player_entity, client_id
    );
    shared::spawn_action_entities(&mut commands, player_entity, client_id, true);
}

/// Read client inputs and move players in server therefore giving a basis for other clients
fn move_player(
    trigger: On<Fire<MovePlayer>>,
    host_server: Query<(), With<HostServer>>,
    server_actions: Query<(), (With<Action<MovePlayer>>, With<shared::ServerAction>)>,
    controlled_by: Query<&ControlledBy>,
    host_clients: Query<(), With<HostClient>>,
    mut position_query: Query<&mut PlayerPosition>,
) {
    let is_host_server = !host_server.is_empty();
    if is_host_server && !server_actions.contains(trigger.action) {
        return;
    }
    if is_host_server {
        if let Ok(controlled_by) = controlled_by.get(trigger.context) {
            if host_clients.get(controlled_by.owner).is_ok() {
                return;
            }
        }
    }
    if let Ok(position) = position_query.get_mut(trigger.context) {
        shared::shared_movement_behaviour(position, &Inputs::Direction(Direction {
            up: trigger.value.y > 0.0,
            down: trigger.value.y < 0.0,
            left: trigger.value.x < 0.0,
            right: trigger.value.x > 0.0,
        }));
    }
}

/// Send messages from server to clients (only in non-headless mode, because otherwise we run with minimal plugins
/// and cannot do input handling)
pub(crate) fn send_message(
    mut sender: ServerMultiMessageSender,
    server: Single<&Server>,
    input: Option<Res<ButtonInput<KeyCode>>>,
) {
    if input.is_some_and(|input| input.just_pressed(KeyCode::KeyM)) {
        let message = Message1(5);
        info!("Sending message: {:?}", message);
        sender
            .send::<_, Channel1>(&message, server.into_inner(), &NetworkTarget::All)
            .unwrap_or_else(|e| {
                error!("Failed to send message: {:?}", e);
            });
    }
}
