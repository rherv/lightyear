//! The client plugin.
//! The client will be responsible for:
//! - connecting to the server at Startup
//! - sending inputs to the server
//! - applying inputs to the locally predicted player (for prediction to work, inputs have to be applied to both the
//!   predicted entity and the server entity)

use crate::automation::AutomationClientPlugin;
use crate::protocol::*;
use crate::shared;
use bevy::prelude::*;
use lightyear::connection::host::HostServer;
use lightyear::input::bei::prelude::{Action, ActionOf, Fire};
use lightyear::prelude::client::{InputDelayConfig, InputTimelineConfig};
use lightyear::prelude::input::bei::InputMarker;
use lightyear::prelude::*;

pub struct ExampleClientPlugin;

impl Plugin for ExampleClientPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(AutomationClientPlugin);
        app.add_systems(Startup, configure_input_delay);
        app.add_systems(Update, receive_message1);
        app.add_observer(handle_predicted_spawn);
        app.add_observer(handle_controlled_spawn);
        app.add_observer(handle_interpolated_spawn);
        app.add_observer(player_movement);
    }
}

fn configure_input_delay(client: Single<Entity, With<Client>>, mut commands: Commands) {
    commands.entity(client.into_inner()).insert(
        InputTimelineConfig::default().with_input_delay(InputDelayConfig::no_input_delay()),
    );
}

/// The client input only gets applied to predicted entities that we own
/// This works because we only predict the user's controlled entity.
/// If we were predicting more entities, we would have to only apply movement to the player owned one.
fn player_movement(
    trigger: On<Fire<MovePlayer>>,
    synced_client: Query<(), (With<Client>, With<IsSynced<InputTimeline>>)>,
    host_server: Query<(), With<HostServer>>,
    server_actions: Query<(), (With<Action<MovePlayer>>, With<Replicate>)>,
    mut velocity_query: Query<&mut PlayerVelocity, With<Predicted>>,
) {
    if synced_client.is_empty() {
        return;
    }
    if !host_server.is_empty() && server_actions.contains(trigger.action) {
        return;
    }
    if let Ok(velocity) = velocity_query.get_mut(trigger.context) {
        shared::apply_player_input(velocity, trigger.value);
    }
}

/// System to receive messages on the client
pub(crate) fn receive_message1(mut receiver: Single<&mut MessageReceiver<Message1>>) {
    for message in receiver.receive() {
        info!("Received message: {:?}", message);
    }
}

/// When the predicted copy of the client-owned entity is spawned, do stuff
/// - assign it a different saturation
/// - keep track of it in the Global resource
///
/// Note that this will be triggered multiple times: for the locally-controlled entity,
/// but also for the remote-controlled entities that are spawned with [`Interpolated`].
/// The `With<Predicted>` filter ensures we only add the `InputMarker` once.
pub(crate) fn handle_predicted_spawn(
    trigger: On<Add, (PlayerId, Predicted)>,
    mut predicted: Query<&mut PlayerColor, With<Predicted>>,
) {
    let entity = trigger.entity;
    if let Ok(mut color) = predicted.get_mut(entity) {
        let hsva = Hsva {
            saturation: 0.4,
            ..Hsva::from(color.0)
        };
        color.0 = Color::from(hsva);
    }
}

fn handle_controlled_spawn(
    trigger: On<Add, Controlled>,
    players: Query<(&PlayerId, Has<InputMarker<Player>>, Option<&ControlledBy>), With<Player>>,
    clients: Query<(), With<Client>>,
    actions: Query<&ActionOf<Player>, With<Action<MovePlayer>>>,
    mut commands: Commands,
) {
    let entity = trigger.entity;
    let Ok((player_id, has_input_marker, controlled_by)) = players.get(entity) else {
        return;
    };
    if let Some(controlled_by) = controlled_by {
        if clients.get(controlled_by.owner).is_err() {
            return;
        }
    }
    if has_input_marker {
        return;
    }
    commands
        .entity(entity)
        .insert(InputMarker::<Player>::default());
    if !actions.iter().any(|action_of| action_of.get() == entity) {
        shared::spawn_action_entities(&mut commands, entity, player_id.0, false);
    }
}

/// When the predicted copy of the client-owned entity is spawned, do stuff
/// - assign it a different saturation
/// - keep track of it in the Global resource
pub(crate) fn handle_interpolated_spawn(
    trigger: On<Add, Interpolated>,
    mut interpolated: Query<&mut PlayerColor>,
) {
    if let Ok(mut color) = interpolated.get_mut(trigger.entity) {
        let hsva = Hsva {
            saturation: 0.1,
            ..Hsva::from(color.0)
        };
        color.0 = Color::from(hsva);
    }
}
