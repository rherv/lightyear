//! This module contains the shared code between the client and the server.
//!
//! The simulation logic (movement, etc.) should be shared between client and server to guarantee that there won't be
//! mispredictions/rollbacks.
use crate::protocol::*;
use bevy::prelude::*;
use lightyear::input::bei::prelude::{Action, ActionOf, Bindings, Cardinal};
use lightyear_examples_common::shared::SharedSettings;

pub struct SharedPlugin;

impl Plugin for SharedPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ProtocolPlugin);
    }
}

pub const SHARED_SETTINGS: SharedSettings = SharedSettings {
    protocol_id: 0,
    private_key: [0; 32],
};

/// Deterministic hash for PreSpawned action entities.
/// Uses the client's PeerId and a salt to produce the same hash on both client and server,
/// regardless of spawn tick.
pub(crate) fn action_prespawn_hash(client_id: PeerId, salt: u64) -> u64 {
    client_id
        .to_bits()
        .wrapping_mul(6364136223846793005)
        .wrapping_add(salt)
}

#[derive(Component)]
pub(crate) struct ServerAction;

/// Spawn action entities for a player. Called on both client and server.
pub(crate) fn spawn_action_entities(
    commands: &mut Commands,
    player_entity: Entity,
    client_id: PeerId,
    is_server: bool,
) {
    let hash = action_prespawn_hash(client_id, 1);
    let prespawned = if is_server {
        PreSpawned::new(hash)
    } else {
        // The local action entity uses the hash as a stable input target, but it
        // is not a predicted gameplay object that should be cleaned up if the
        // server action replication arrived before the local action was spawned.
        PreSpawned::new(hash).for_receiver(player_entity)
    };
    let mut action = commands.spawn((
        ActionOf::<Player>::new(player_entity),
        Action::<MovePlayer>::new(),
        Bindings::spawn(Cardinal::wasd_keys()),
        prespawned,
    ));
    if is_server {
        #[cfg(feature = "server")]
        action.insert((
            Replicate::to_clients(NetworkTarget::Single(client_id)),
            ServerAction,
        ));
    } else {
        action.insert(lightyear::prelude::input::bei::InputMarker::<Player>::default());
    }
}

// This system defines how we update the player's positions when we receive an input
pub(crate) fn shared_movement_behaviour(mut position: Mut<PlayerPosition>, input: &Inputs) {
    const MOVE_SPEED: f32 = 10.0;
    let Inputs::Direction(direction) = input;
    if direction.up {
        position.y += MOVE_SPEED;
    }
    if direction.down {
        position.y -= MOVE_SPEED;
    }
    if direction.left {
        position.x -= MOVE_SPEED;
    }
    if direction.right {
        position.x += MOVE_SPEED;
    }
}

pub(crate) fn apply_player_input(mut velocity: Mut<PlayerVelocity>, input: Vec2) {
    const MOVE_SPEED: f32 = 10.0;
    velocity.x += input.x * MOVE_SPEED;
    velocity.y += input.y * MOVE_SPEED;
}
