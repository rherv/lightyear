use crate::client_server::prediction::{
    register_rollback_check_helper, trigger_rollback_check, trigger_state_rollback,
};
use crate::protocol::{CompFull, CompNotNetworked, NativeInput};
use crate::stepper::*;
#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use bevy::prelude::*;
use bevy_replicon::prelude::RepliconTick;
use core::time::Duration;
use lightyear::input::native::prelude::InputMarker;
use lightyear::prediction::Predicted;
use lightyear::prediction::predicted_history::PredictionHistory;
use lightyear::prelude::input::native::ActionState;
use lightyear_connection::prelude::NetworkTarget;
use lightyear_core::id::PeerId;
use lightyear_core::prelude::LocalTimeline;
use lightyear_core::tick::Tick;
use lightyear_messages::MessageManager;
use lightyear_prediction::despawn::{PredictionDespawnCommandsExt, PredictionDisable};
use lightyear_prediction::manager::{LastConfirmedInput, RollbackMode};
use lightyear_prediction::prelude::*;
use lightyear_prediction::rollback::{DeterministicPredicted, reset_input_rollback_tracker};
use lightyear_replication::prelude::*;
use test_log::test;

fn setup() -> (ClientServerStepper, Entity) {
    let mut stepper = ClientServerStepper::from_config(StepperConfig::single());
    register_rollback_check_helper(stepper.client_app());

    // add predicted/confirmed entities
    let predicted = stepper
        .client_app()
        .world_mut()
        .spawn((Predicted, CompFull(1.0)))
        .id();
    // run one frame to initialize prediction history for the entity
    stepper.frame_step(1);
    (stepper, predicted)
}

// =============================================================================
// Scenario 1: Component predicted-inserted but not from replication → removed on rollback
// =============================================================================

/// Client prediction adds a component, but server never has it.
/// On rollback, the component should be removed.
#[test]
fn test_predicted_insert_reverted_on_rollback() {
    let (mut stepper, predicted) = setup();

    stepper.frame_step(1);
    let rollback_tick = stepper.client_tick(0);
    stepper.frame_step(1);

    // Client prediction adds CompNotNetworked (not replicated, server doesn't have it)
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .insert(CompNotNetworked(1.0));

    // Simulate confirmed state at rollback_tick: CompFull still 1.0 (no change from server)
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(rollback_tick, Some(CompFull(1.0)));

    trigger_rollback_check(&mut stepper, rollback_tick);
    stepper.frame_step(1);

    // CompNotNetworked should be removed: it wasn't in the history at rollback_tick
    assert!(
        stepper
            .client_app()
            .world()
            .get::<CompNotNetworked>(predicted)
            .is_none(),
        "Predicted-inserted component should be removed on rollback"
    );
}

// =============================================================================
// Scenario 2: Component predicted-removed → re-inserted on rollback
// =============================================================================

/// Client prediction removes a component, but server still has it.
/// On rollback, the component should be restored.
#[test]
fn test_predicted_remove_restored_on_rollback() {
    let (mut stepper, predicted) = setup();

    fn increment_and_remove(
        mut commands: Commands,
        mut query: Query<(Entity, &mut CompFull), With<Predicted>>,
    ) {
        for (entity, mut comp) in query.iter_mut() {
            comp.0 += 1.0;
            if comp.0 == 5.0 {
                commands.entity(entity).remove::<CompFull>();
            }
        }
    }
    stepper
        .client_app()
        .add_systems(FixedUpdate, increment_and_remove);

    // Run until CompFull is removed (1.0 → 2.0 → 3.0 → 4.0 → 5.0 → removed)
    stepper.frame_step(5);
    assert!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .is_none(),
        "CompFull should have been removed by prediction"
    );

    let tick = stepper.client_tick(0);
    // Simulate server confirms CompFull = -10.0 at tick-3 (server says it still exists)
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .insert(CompFull(-10.0));
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(tick - 3, Some(CompFull(-10.0)));

    trigger_rollback_check(&mut stepper, tick - 3);
    stepper.frame_step(1);

    // CompFull should be re-added and re-simulated: -10 + 4 increments = -6
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .unwrap()
            .0,
        -6.0,
        "Predicted-removed component should be restored and re-simulated on rollback"
    );
}

// =============================================================================
// Scenario 3: Entity predicted-despawned → re-enabled on rollback
// =============================================================================

/// Client uses `prediction_despawn` on an entity.
/// On rollback (to before the despawn), the entity should be re-enabled.
#[test]
fn test_predicted_despawn_restored_on_rollback() {
    let (mut stepper, predicted) = setup();

    stepper.frame_step(1);
    let rollback_tick = stepper.client_tick(0);
    stepper.frame_step(1);

    // Predicted-despawn the entity (adds PredictionDisable, doesn't actually despawn)
    stepper
        .client_app()
        .world_mut()
        .commands()
        .entity(predicted)
        .prediction_despawn();
    stepper.frame_step(1);

    assert!(
        stepper.client_app().world().get_entity(predicted).is_ok(),
        "Entity should still exist after prediction_despawn"
    );
    assert!(
        stepper
            .client_app()
            .world()
            .get::<PredictionDisable>(predicted)
            .is_some(),
        "Entity should have PredictionDisable marker"
    );

    // Trigger rollback to before the despawn
    trigger_rollback_check(&mut stepper, rollback_tick);
    stepper.frame_step(1);

    // PredictionDisable should be removed, entity re-enabled
    assert!(
        stepper.client_app().world().get_entity(predicted).is_ok(),
        "Entity should still exist after rollback"
    );
    assert!(
        stepper
            .client_app()
            .world()
            .get::<PredictionDisable>(predicted)
            .is_none(),
        "PredictionDisable should be removed after rollback"
    );
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .unwrap(),
        &CompFull(1.0),
        "Component should be restored to value at rollback tick"
    );
}

// =============================================================================
// Scenario 4: Entity predicted-spawned → despawned on rollback
// =============================================================================

/// A DeterministicPredicted entity spawned during prediction should be despawned
/// if rollback goes back to before the spawn tick.
#[test]
fn test_predicted_spawn_despawned_on_rollback() {
    let (mut stepper, _) = setup();
    stepper.frame_step(1);

    let tick = stepper.client_tick(0);
    let predicted_a = stepper
        .client_app()
        .world_mut()
        .spawn((Predicted, DeterministicPredicted::default(), CompFull(1.0)))
        .id();

    // trigger a rollback to before the entity was spawned
    trigger_state_rollback(&mut stepper, tick - 1);
    stepper.frame_step(1);

    assert!(
        stepper
            .client_app()
            .world()
            .get_entity(predicted_a)
            .is_err(),
        "Predicted-spawned entity should be despawned on rollback to before spawn tick"
    );
}

// =============================================================================
// Scenario 5: Component modified → corrected on rollback
// =============================================================================

/// Client prediction modifies a component value differently than the server.
/// On rollback, the component should snap to the confirmed value and re-simulate.
#[test]
fn test_predicted_modify_corrected_on_rollback() {
    let (mut stepper, predicted) = setup();

    fn increment_component(mut query: Query<&mut CompFull, With<Predicted>>) {
        for mut comp in query.iter_mut() {
            comp.0 += 1.0;
        }
    }
    stepper
        .client_app()
        .add_systems(FixedUpdate, increment_component);

    // Run 3 frames: CompFull goes 1.0 → 2.0 → 3.0 → 4.0
    stepper.frame_step(3);
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .unwrap()
            .0,
        4.0
    );

    let tick = stepper.client_tick(0);
    // Server says CompFull was actually 10.0 at tick-2 (different from predicted 2.0)
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(tick - 2, Some(CompFull(10.0)));

    trigger_rollback_check(&mut stepper, tick - 2);
    stepper.frame_step(1);

    // Snap to 10.0 at tick-2, re-simulate 3 ticks: 10.0 + 3 = 13.0
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .unwrap()
            .0,
        13.0,
        "Modified component should be corrected to confirmed value and re-simulated"
    );
}

/// If one predicted entity triggers a rollback from an older tick while another
/// predicted entity already has a newer confirmed tick, the newer confirmed
/// value must be preserved during replay.
#[test]
fn test_rollback_preserves_later_confirmed_values_on_other_entities() {
    fn increment_component(mut query: Query<&mut CompFull, With<Predicted>>) {
        for mut comp in query.iter_mut() {
            comp.0 += 1.0;
        }
    }

    let (mut stepper, predicted_a) = setup();
    let predicted_b = stepper
        .client_app()
        .world_mut()
        .spawn((Predicted, CompFull(10.0)))
        .id();

    // Initialize prediction history for the second entity.
    stepper.frame_step(1);
    stepper
        .client_app()
        .add_systems(FixedUpdate, increment_component);

    // Build enough history so rollback and later confirmed ticks are distinct.
    stepper.frame_step(4);
    let current_tick = stepper.client_tick(0);
    let rollback_tick = current_tick - 3;
    let later_confirmed_tick = current_tick - 1;

    let rollback_replicon_tick = RepliconTick::new(u32::from(rollback_tick.0));
    let later_replicon_tick = RepliconTick::new(u32::from(later_confirmed_tick.0));

    let world = stepper.client_app().world_mut();
    world
        .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
        .record(RepliconTick::default(), rollback_tick);
    world
        .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
        .record(rollback_replicon_tick, rollback_tick);
    world
        .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
        .record(later_replicon_tick, later_confirmed_tick);
    world
        .entity_mut(predicted_a)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(rollback_tick, Some(CompFull(100.0)));
    world
        .entity_mut(predicted_a)
        .insert(ConfirmHistory::new(rollback_replicon_tick));

    world
        .entity_mut(predicted_b)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(later_confirmed_tick, Some(CompFull(200.0)));
    world
        .entity_mut(predicted_b)
        .insert(ConfirmHistory::new(later_replicon_tick));

    trigger_state_rollback(&mut stepper, rollback_tick);
    stepper.frame_step(1);

    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted_a)
            .unwrap()
            .0,
        104.0,
        "Rollback initiator should replay from the older confirmed tick through the current frame tick"
    );
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted_b)
            .unwrap()
            .0,
        203.0,
        "Later confirmed value on another entity should be preserved and replayed across the remaining rollback ticks and the current frame tick"
    );
}

/// Replicated helper/input entities can carry `ConfirmHistory` without having any
/// prediction history. A stale confirm tick for those entities should not make
/// the unchanged-entity rollback pass resolve an evicted checkpoint.
#[test]
fn test_stale_confirm_history_without_prediction_history_is_ignored() {
    let (mut stepper, _) = setup();
    stepper.frame_step(5);

    let server_confirmed_tick = stepper.client_tick(0) - 1;
    let current_replicon_tick = RepliconTick::new(500);
    let stale_replicon_tick = RepliconTick::new(1);

    let world = stepper.client_app().world_mut();
    world.spawn((Predicted, ConfirmHistory::new(stale_replicon_tick)));
    world
        .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
        .record(current_replicon_tick, server_confirmed_tick);
    world
        .resource_mut::<ServerMutateTicks>()
        .confirm(current_replicon_tick, 1);

    stepper.frame_step(1);
}

#[test]
fn test_missing_confirm_history_checkpoint_mapping_does_not_request_rollback() {
    #[derive(Resource, Default)]
    struct RollbackObserved(bool);

    fn record_rollback(
        manager: Single<&PredictionManager>,
        mut observed: ResMut<RollbackObserved>,
    ) {
        observed.0 |= manager.get_rollback_start_tick().is_some();
    }

    let (mut stepper, predicted) = setup();
    stepper.frame_step(5);

    let server_confirmed_tick = stepper.client_tick(0) - 1;
    let current_replicon_tick = RepliconTick::new(500);
    let stale_replicon_tick = RepliconTick::new(1);

    stepper
        .client_app()
        .insert_resource(RollbackObserved::default());
    stepper.client_app().add_systems(
        PreUpdate,
        record_rollback
            .after(RollbackSystems::Check)
            .before(RollbackSystems::Prepare),
    );

    let world = stepper.client_app().world_mut();
    world
        .entity_mut(predicted)
        .insert(ConfirmHistory::new(stale_replicon_tick));
    world
        .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
        .record(current_replicon_tick, server_confirmed_tick);
    world
        .resource_mut::<ServerMutateTicks>()
        .confirm(current_replicon_tick, 1);

    stepper.frame_step(1);

    assert!(
        !stepper
            .client_app()
            .world()
            .resource::<RollbackObserved>()
            .0
    );
}

// =============================================================================
// Scenario 6: Component inserted from remote → applied on rollback
// =============================================================================

/// Server sends a new component that the client didn't have.
/// On rollback, the component should be present at the confirmed tick.
#[test]
fn test_remote_insert_applied_on_rollback() {
    let (mut stepper, predicted) = setup();

    stepper.frame_step(2);
    let tick = stepper.client_tick(0);
    stepper.frame_step(1);

    // Simulate server sending CompNotNetworked(5.0) at `tick` (component client didn't predict)
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .insert(CompNotNetworked(5.0));
    // Create prediction history for CompNotNetworked with confirmed value
    let mut history = PredictionHistory::<CompNotNetworked>::default();
    history.add_confirmed(tick, Some(CompNotNetworked(5.0)));
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .insert(history);

    trigger_rollback_check(&mut stepper, tick);
    stepper.frame_step(1);

    // The remotely-inserted component should be present after rollback
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompNotNetworked>(predicted)
            .unwrap(),
        &CompNotNetworked(5.0),
        "Remotely-inserted component should be present after rollback"
    );
}

// =============================================================================
// Scenario 7: Component removed from remote → removed on rollback
// =============================================================================

/// Server removes a component that the client still had.
/// On rollback, the component should be absent.
#[test]
fn test_remote_remove_applied_on_rollback() {
    let (mut stepper, predicted) = setup();

    stepper.frame_step(1);
    let rollback_tick = stepper.client_tick(0);
    stepper.frame_step(1);

    // Simulate server removing CompFull at rollback_tick
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(rollback_tick, None);
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted)
        .remove::<CompFull>();

    trigger_rollback_check(&mut stepper, rollback_tick);
    stepper.frame_step(1);

    // CompFull should be absent after rollback: server confirmed removal
    assert!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .is_none(),
        "Remotely-removed component should be absent after rollback"
    );
}

// =============================================================================
// Other rollback tests
// =============================================================================

/// If we have disable_rollback (DeterministicPredicted):
/// 1) the entity alone doesn't trigger rollback
/// 2) if a rollback happens (from another entity), we reset to the predicted history value
#[test]
fn test_disable_rollback() {
    let (mut stepper, predicted_b) = setup();

    // add a DeterministicPredicted entity (disable state rollback for it)
    let predicted_a = stepper
        .client_app()
        .world_mut()
        .spawn((Predicted, DeterministicPredicted::default(), CompFull(1.0)))
        .id();

    // value gets synced and added to PredictionHistory
    stepper.frame_step(1);

    // 2. If a rollback happens (triggered by predicted_b), DeterministicPredicted entity
    //    gets reset to its historical value
    let tick = stepper.client_tick(0);

    // Set up history for predicted_a with a known confirmed value
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted_a)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(tick, Some(CompFull(10.0)));

    // Simulate confirmed update for predicted_b with a different value to trigger mismatch
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted_b)
        .get_mut::<PredictionHistory<CompFull>>()
        .unwrap()
        .add_confirmed(tick, Some(CompFull(3.0)));
    stepper
        .client_app()
        .world_mut()
        .entity_mut(predicted_b)
        .get_mut::<CompFull>()
        .unwrap()
        .0 = 3.0;

    // step once to avoid a 0-tick rollback
    stepper.frame_step(1);

    trigger_rollback_check(&mut stepper, tick);
    stepper.frame_step(1);

    // the DeterministicPredicted entity was rolled back to the past PredictionHistory value
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted_a)
            .unwrap()
            .0,
        10.0
    );
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted_b)
            .unwrap()
            .0,
        3.0
    );
}

/// Test that:
/// - the `Time` resource's elapsed is rollbacked to the first tick of the rollback
/// - the `Time` resource's elapsed time is advanced correctly during the rollback
/// - the `Time` resource's delta during a rollback is the `Time<Fixed>`'s delta
#[test]
fn test_rollback_time_resource() {
    #[derive(Debug, PartialEq)]
    struct TimeSnapshot {
        is_rollback: bool,
        delta: Duration,
        elapsed: Duration,
    }

    #[derive(Resource, Default, Debug)]
    struct TimeTracker {
        snapshots: Vec<TimeSnapshot>,
    }

    // Record the time resource's values for each tick.
    fn track_time(
        time: Res<Time>,
        mut time_tracker: ResMut<TimeTracker>,
        rollback: Single<&PredictionManager>,
    ) {
        time_tracker.snapshots.push(TimeSnapshot {
            is_rollback: rollback.is_rollback(),
            delta: time.delta(),
            elapsed: time.elapsed(),
        });
    }

    let (mut stepper, predicted) = setup();
    // Build up enough prediction history so rollback tick is within range
    stepper.frame_step(2);

    // Add time tracking AFTER building history to only capture the rollback frame
    stepper.client_app().insert_resource(TimeTracker::default());
    stepper.client_app().add_systems(FixedUpdate, track_time);
    let time_before_next_tick = *stepper.client_app().world().resource::<Time<Fixed>>();

    // Trigger 2 rollback ticks
    let tick = stepper.client_tick(0);
    trigger_rollback_check(&mut stepper, tick - 2);
    stepper.frame_step(1);

    // Check that the component got synced.
    assert_eq!(
        stepper
            .client_app()
            .world()
            .get::<CompFull>(predicted)
            .unwrap(),
        &CompFull(1.0)
    );

    // Verify that the 2 rollback ticks and regular tick occurred with the
    // correct delta times and elapsed times.
    let tick_duration = stepper.tick_duration;
    let time_tracker = stepper.client_app().world().resource::<TimeTracker>();
    assert_eq!(
        time_tracker.snapshots,
        vec![
            TimeSnapshot {
                is_rollback: true,
                delta: tick_duration,
                elapsed: time_before_next_tick.elapsed() - tick_duration
            },
            TimeSnapshot {
                is_rollback: true,
                delta: tick_duration,
                elapsed: time_before_next_tick.elapsed()
            },
            TimeSnapshot {
                is_rollback: false,
                delta: tick_duration,
                elapsed: time_before_next_tick.elapsed() + tick_duration
            }
        ]
    );
}

/// Clients 1 and 2 have inputs and send them to the Server, who rebroadcasts to client 0
fn setup_stepper_for_input_rollback(
    mode: RollbackMode,
) -> (ClientServerStepper, Entity, Entity, Entity, Entity) {
    let mut stepper = ClientServerStepper::from_config(StepperConfig::with_netcode_clients(3));

    let mut client_mut = stepper.client_mut(0);
    let mut prediction_manager = client_mut.get_mut::<PredictionManager>().unwrap();
    prediction_manager.rollback_policy.input = mode;
    prediction_manager.rollback_policy.state = RollbackMode::Disabled;

    let server_entity_1 = stepper
        .server_app
        .world_mut()
        .spawn(Replicate::to_clients(NetworkTarget::AllExceptSingle(
            PeerId::Netcode(2),
        )))
        .id();
    let server_entity_2 = stepper
        .server_app
        .world_mut()
        .spawn(Replicate::to_clients(NetworkTarget::AllExceptSingle(
            PeerId::Netcode(1),
        )))
        .id();
    stepper.frame_step_server_first(1);

    // Check that in PostUpdate, the LastConfirmedInput is reset if no input messages were received
    let client = stepper.client(0);
    assert!(!client.get::<LastConfirmedInput>().unwrap().received_input());

    // add input-markers on client 1/2 so that they can send remote input messages
    let client_entity_1 = stepper
        .client(1)
        .get::<MessageManager>()
        .unwrap()
        .entity_mapper
        .get_local(server_entity_1)
        .expect("entity was not replicated to client");
    stepper.client_apps[1]
        .world_mut()
        .entity_mut(client_entity_1)
        .insert((InputMarker::<NativeInput>::default(),));

    let client_entity_2 = stepper
        .client(2)
        .get::<MessageManager>()
        .unwrap()
        .entity_mapper
        .get_local(server_entity_2)
        .expect("entity was not replicated to client");
    stepper.client_apps[2]
        .world_mut()
        .entity_mut(client_entity_2)
        .insert((InputMarker::<NativeInput>::default(),));

    let client_entity_a = stepper
        .client(0)
        .get::<MessageManager>()
        .unwrap()
        .entity_mapper
        .get_local(server_entity_1)
        .expect("entity was not replicated to client");
    // we want to predict this entity
    stepper.client_apps[0]
        .world_mut()
        .entity_mut(client_entity_a)
        .insert((CompNotNetworked(1.0), DeterministicPredicted::default()));
    let client_entity_b = stepper
        .client(0)
        .get::<MessageManager>()
        .unwrap()
        .entity_mapper
        .get_local(server_entity_2)
        .expect("entity was not replicated to client");

    // build a steady state where we have already received an input
    stepper.frame_step(2);

    (
        stepper,
        client_entity_1,
        client_entity_2,
        client_entity_a,
        client_entity_b,
    )
}

/// Test that we rollback from the last confirmed input when RollbackMode::Always for inputs
#[test]
fn test_input_rollback_always_mode() {
    let (mut stepper, _, _, client_entity, _) =
        setup_stepper_for_input_rollback(RollbackMode::Always);

    // build a steady state where have already received an input
    stepper.frame_step(2);

    // send input message from client 1/2 to server
    stepper.frame_step(1);
    let input_tick = stepper.client_tick(1);

    info!("Will check rollback at tick: {input_tick:?}");

    let check_rollback_start =
        move |timeline: Res<LocalTimeline>,
              manager: Single<(&LastConfirmedInput, &PredictionManager)>| {
            let (last_confirmed_input, manager) = manager.into_inner();
            let tick = timeline.tick();
            if tick == input_tick {
                assert!(last_confirmed_input.received_input());
                let rollback_start = manager.get_rollback_start_tick();
                // We receive the input message for tick `input_tick`, but we rollback at the previous LastConfirmedInput tick,
                // which is `input_tick - 1`
                assert_eq!(rollback_start.unwrap(), input_tick - 1);
            }
        };
    stepper.client_apps[0].add_systems(
        PreUpdate,
        check_rollback_start
            .after(RollbackSystems::Check)
            .before(reset_input_rollback_tracker),
    );

    // modify the CompNotNetworked component
    stepper.client_apps[0]
        .world_mut()
        .get_mut::<CompNotNetworked>(client_entity)
        .unwrap()
        .0 = 2.0;

    // server broadcast input message to clients (including client 0)
    stepper.frame_step_server_first(1);

    // after the rollback, the last_confirmed_input is reset
    assert_eq!(
        stepper
            .client(0)
            .get::<LastConfirmedInput>()
            .unwrap()
            .tick
            .get(),
        input_tick
    );
    // also check that the component was reset to the value it had in the history
    assert_eq!(
        stepper.client_apps[0]
            .world()
            .get::<CompNotNetworked>(client_entity)
            .unwrap()
            .0,
        1.0
    );
}

/// Test that LastConfirmedInput computes the earliest input across multiple clients
#[test]
fn test_last_confirmed_input_multiple_clients() {
    let (mut stepper, client_entity_1, _, _, _) =
        setup_stepper_for_input_rollback(RollbackMode::Always);

    // only client 2 will send an input message to the server
    stepper.client_apps[1]
        .world_mut()
        .entity_mut(client_entity_1)
        .remove::<InputMarker<NativeInput>>();
    stepper.frame_step(1);
    let input_tick = stepper.client_tick(1);

    // server broadcast input message to clients
    stepper.frame_step_server_first(1);

    // after the rollback, the last_confirmed_input is updated. It's updated to `input_tick - 1` and not `input_tick`
    // because we didn't receive a new input message from client 1
    assert_eq!(
        stepper
            .client(0)
            .get::<LastConfirmedInput>()
            .unwrap()
            .tick
            .get(),
        input_tick - 1
    );
}

/// Test that rollback tick is set to the earliest mismatch when RollbackMode::Check for inputs
#[test]
fn test_input_rollback_check_mode_earliest_mismatch() {
    let (mut stepper, client_entity_1, _, client_entity_a, _) =
        setup_stepper_for_input_rollback(RollbackMode::Check);

    // build a steady state where we have already received an input
    stepper.frame_step(2);

    // client 1 and client 2 send an input message to the server
    // client 1's input will cause a mismatch
    stepper.client_apps[1]
        .world_mut()
        .get_mut::<ActionState<NativeInput>>(client_entity_1)
        .unwrap()
        .0 = NativeInput(1);
    stepper.frame_step(1);
    let input_tick = stepper.client_tick(1);

    let check_rollback_start =
        move |timeline: Res<LocalTimeline>, manager: Single<&PredictionManager>| {
            let manager = manager.into_inner();
            let tick = timeline.tick();
            if tick == input_tick {
                assert!(manager.earliest_mismatch_input.has_mismatches());
                let rollback_start = manager.get_rollback_start_tick();
                // there is a mismatch only for client 1, which is enough to trigger a rollback.
                // we trigger a rollback to the earliest mismatch, which is `input_tick`
                assert_eq!(rollback_start.unwrap(), input_tick - 1);
            }
        };
    stepper.client_apps[0].add_systems(
        PreUpdate,
        check_rollback_start
            .after(RollbackSystems::Check)
            .before(reset_input_rollback_tracker),
    );

    // server broadcast input message to clients
    stepper.frame_step_server_first(1);
}

/// Test that we don't rollback if there are no input mismatches in Check mode
#[test]
fn test_no_rollback_without_input_mismatches() {
    let (mut stepper, _, _, _, _) = setup_stepper_for_input_rollback(RollbackMode::Check);

    // build a steady state where we have already received an input
    stepper.frame_step(2);

    // client 1 and client 2 send an input message to the server
    // there will be no mismatches
    stepper.frame_step(1);
    let input_tick = stepper.client_tick(1);

    let check_rollback_start =
        move |timeline: Res<LocalTimeline>, manager: Single<&PredictionManager>| {
            let manager = manager.into_inner();
            let tick = timeline.tick();
            if tick == input_tick {
                assert!(!manager.earliest_mismatch_input.has_mismatches());
                let rollback_start = manager.get_rollback_start_tick();
                assert!(rollback_start.is_none());
            }
        };
    stepper.client_apps[0].add_systems(
        PreUpdate,
        check_rollback_start
            .after(RollbackSystems::Check)
            .before(RollbackSystems::Prepare),
    );

    // server broadcast input message to clients
    stepper.frame_step_server_first(1);
}

/// Test that if we spawn a DeterministicPredicted entity with skip_despawn = true
/// We only start enabling rollback for this entity a few ticks after it was spawned.
#[test]
fn test_deterministic_predicted_skip_despawn() {
    let (mut stepper, _) = setup();

    // add predicted/confirmed entities
    let receiver = stepper.client(0).id();
    let tick = stepper.client_tick(0);
    let predicted_a = stepper
        .client_app()
        .world_mut()
        .spawn((
            Predicted,
            DeterministicPredicted {
                skip_despawn: true,
                enable_rollback_after: 2,
            },
            CompFull(1.0),
        ))
        .id();

    // Rollback: the entity should have DisableRollback added until the
    // configured enable_rollback_after tick.
    trigger_state_rollback(&mut stepper, tick);
    stepper.frame_step(1);
    assert!(
        stepper
            .client_app()
            .world()
            .get::<DisableRollback>(predicted_a)
            .is_some()
    );

    // trigger a rollback at tick + 2, we should re-enable rollback
    // since it's the spawn_tick of DeterministicPredicted + 2
    trigger_state_rollback(&mut stepper, tick + 2);
    stepper.frame_step(1);
    assert!(
        stepper
            .client_app()
            .world()
            .get::<DisableRollback>(predicted_a)
            .is_none()
    );
}

/// Test that if we spawn a DeterministicPredicted entity with skip_despawn = false
/// The entity is despawned it was spawned before the rollback tick.
#[test]
fn test_deterministic_predicted_despawn() {
    let (mut stepper, _) = setup();
    stepper.frame_step(1);

    // add predicted/confirmed entities
    let receiver = stepper.client(0).id();
    let tick = stepper.client_tick(0);

    let predicted_a = stepper
        .client_app()
        .world_mut()
        .spawn((Predicted, DeterministicPredicted::default(), CompFull(1.0)))
        .id();

    // trigger a rollback at tick - 2, we should despawn the DeterministicPredicted
    // since it was spawned before the rollback
    trigger_state_rollback(&mut stepper, tick - 1);
    stepper.frame_step(1);
    assert!(
        stepper
            .client_app()
            .world()
            .get_entity(predicted_a)
            .is_err()
    )
}

// =============================================================================
// State rollback request churn diagnostics
// =============================================================================
//
// These tests document a rollback churn pattern observed after the Replicon
// rollback changes.
//
// There are two independent state rollback request paths:
//
// 1. confirmed-update path:
//    A replicated component update is written into PredictionHistory and, when
//    it mismatches prediction history, records
//    StateRollbackMetadata::earliest_mismatch_tick.
//
// 2. unchanged-entity path:
//    ServerMutateTicks advances, but an entity's ConfirmHistory is older. The
//    rollback checker infers that the entity was unchanged at the newer
//    server-confirmed tick and compares its last confirmed value against
//    prediction history.
//
// The problematic request-chain is:
//
//     unchanged_entity rollback
//     -> rollback/replay
//     -> confirmed_update mismatch for the same or adjacent server window
//     -> second rollback/replay
//
// The model tests below document the request lifecycle. The ignored same-window
// diagnostic documents the suspected bad invariant violation. The real
// replication stepper proves that the two request paths can chain through actual
// server->client Replicon delivery.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeledRollbackRequestCause {
    ConfirmedUpdate,
    UnchangedEntity,
}

#[derive(Debug, Default)]
struct ModeledStateRollbackMetadata {
    last_processed_tick: Option<Tick>,
    earliest_mismatch_tick: Option<Tick>,
}

impl ModeledStateRollbackMetadata {
    fn record_mismatch(&mut self, tick: Tick) {
        match self.earliest_mismatch_tick {
            None => self.earliest_mismatch_tick = Some(tick),
            Some(existing) if tick < existing => self.earliest_mismatch_tick = Some(tick),
            _ => {}
        }
    }

    fn take_ready_mismatch_tick(&mut self, local_tick: Tick) -> Option<Tick> {
        let mismatch_tick = self.earliest_mismatch_tick?;
        if mismatch_tick >= local_tick {
            return None;
        }
        self.earliest_mismatch_tick = None;
        Some(mismatch_tick)
    }

    fn has_server_mutate_ticks_advanced(&self, server_confirmed_tick: Tick) -> bool {
        match self.last_processed_tick {
            None => true,
            Some(last_processed_tick) => server_confirmed_tick > last_processed_tick,
        }
    }

    fn set_last_processed_tick(&mut self, server_confirmed_tick: Tick) {
        self.last_processed_tick = Some(server_confirmed_tick);
    }
}

/// Minimal model of the two state rollback request sites in `check_rollback`.
/// This helper models request emission only, not ECS execution.
fn simulate_state_check_pass(
    metadata: &mut ModeledStateRollbackMetadata,
    local_tick: Tick,
    server_confirmed_tick: Tick,
    unchanged_entity_mismatch_count: usize,
) -> Vec<(ModeledRollbackRequestCause, Tick)> {
    let mut requests = Vec::new();

    // confirmed-update path
    if let Some(mismatch_tick) = metadata.take_ready_mismatch_tick(local_tick) {
        requests.push((ModeledRollbackRequestCause::ConfirmedUpdate, mismatch_tick));
    }

    // unchanged-entity path
    if metadata.has_server_mutate_ticks_advanced(server_confirmed_tick) {
        for _ in 0..unchanged_entity_mismatch_count {
            requests.push((
                ModeledRollbackRequestCause::UnchangedEntity,
                server_confirmed_tick,
            ));
        }
    }

    metadata.set_last_processed_tick(server_confirmed_tick);
    requests
}

/// Documents unchanged-entity fanout: more than one stale predicted entity can
/// emit an unchanged-entity rollback request in a single state check pass.
#[test]
fn test_state_rollback_request_model_allows_unchanged_entity_fanout() {
    let mut metadata = ModeledStateRollbackMetadata::default();

    let requests = simulate_state_check_pass(&mut metadata, Tick(904), Tick(903), 2);

    assert_eq!(
        requests,
        vec![
            (ModeledRollbackRequestCause::UnchangedEntity, Tick(903)),
            (ModeledRollbackRequestCause::UnchangedEntity, Tick(903)),
        ]
    );
}

/// Documents the minimal churn mechanism:
///
/// Pass 1 emits an unchanged-entity rollback request for a server-confirmed
/// tick. After rollback/replay, state diffing can record a confirmed-update
/// mismatch for that same tick. Pass 2 then emits a confirmed-update rollback
/// request for the same rollback window.
///
/// This is not necessarily wrong by itself, but it is the request-lifecycle
/// shape seen in the live churn logs.
#[test]
fn test_state_rollback_request_model_allows_unchanged_then_confirmed_follow_up_same_window() {
    let mut metadata = ModeledStateRollbackMetadata::default();

    let mismatch_tick = Tick(903);
    let local_tick = Tick(904);

    let pass1 = simulate_state_check_pass(&mut metadata, local_tick, mismatch_tick, 1);
    assert_eq!(
        pass1,
        vec![(ModeledRollbackRequestCause::UnchangedEntity, mismatch_tick)]
    );

    // Between passes, mismatch is re-recorded by state diffing / write_history.
    metadata.record_mismatch(mismatch_tick);

    let pass2 = simulate_state_check_pass(&mut metadata, local_tick, mismatch_tick, 0);
    assert_eq!(
        pass2,
        vec![(ModeledRollbackRequestCause::ConfirmedUpdate, mismatch_tick)]
    );
}

/// Control: if unchanged-entity mismatch is absent, a confirmed-update mismatch
/// produces a single confirmed-update rollback request.
#[test]
fn test_state_rollback_request_model_confirmed_update_only_is_single_request() {
    let mut metadata = ModeledStateRollbackMetadata::default();

    metadata.record_mismatch(Tick(50));

    let requests = simulate_state_check_pass(&mut metadata, Tick(51), Tick(50), 0);

    assert_eq!(
        requests,
        vec![(ModeledRollbackRequestCause::ConfirmedUpdate, Tick(50))]
    );
}

/// Diagnostic reproduction of the exact same-local-tick / same-window churn
/// shape seen in live logs.
///
/// Phase 1 uses the real unchanged-entity path:
/// - ServerMutateTicks advances.
/// - The entity ConfirmHistory is stale.
/// - The unchanged-entity path requests rollback.
///
/// Phase 2 manually injects the confirmed-update mismatch with
/// trigger_rollback_check(), standing in for PredictionRegistry::write_history.
/// This is intentional: it isolates the rollback scheduler/lifecycle behavior
/// from packet delivery timing.
///
/// The real-replication companion test below proves that write_history can
/// produce the phase-2 confirmed-update rollback through actual server->client
/// replication. This diagnostic proves that if that mismatch is recorded before
/// the local tick advances, the same rollback window can be executed twice.
///
/// This test is ignored because the final assertion describes the desired fixed
/// invariant, not the current behavior.
#[test]
#[ignore = "documents suspected rollback churn bug: same local tick/window can rollback twice"]
fn test_state_rollback_should_coalesce_same_window_follow_up_request() {
    use lightyear_prediction::diagnostics::PredictionMetrics;

    #[derive(Resource, Default, Debug)]
    struct RollbackProbe {
        hits: Vec<(Tick, Tick)>, // (local_tick, rollback_tick)
    }

    fn increment_component(mut query: Query<&mut CompFull, With<Predicted>>) {
        for mut comp in query.iter_mut() {
            comp.0 += 1.0;
        }
    }

    fn record_rollback_after_check(
        timeline: Res<LocalTimeline>,
        manager: Single<&PredictionManager>,
        mut probe: ResMut<RollbackProbe>,
    ) {
        let local_tick = timeline.tick();
        let rollback_start = manager.get_rollback_start_tick();

        info!(
            ?local_tick,
            ?rollback_start,
            is_rollback = manager.is_rollback(),
            "same-window-churn probe: after RollbackSystems::Check"
        );

        if let Some(rollback_tick) = rollback_start {
            probe.hits.push((local_tick, rollback_tick));
        }
    }

    fn log_metrics(label: &'static str, stepper: &mut ClientServerStepper) {
        let local_tick = stepper.client_tick(0);
        let metrics = stepper
            .client_app()
            .world()
            .get_resource::<PredictionMetrics>()
            .map(|m| (m.rollbacks, m.rollback_ticks))
            .unwrap_or((0, 0));

        let hits = stepper
            .client_app()
            .world()
            .get_resource::<RollbackProbe>()
            .map(|p| p.hits.clone())
            .unwrap_or_default();

        info!(
            label,
            ?local_tick,
            rollbacks = metrics.0,
            rollback_ticks = metrics.1,
            ?hits,
            "same-window-churn metrics"
        );
    }

    let (mut stepper, predicted_a) = setup();

    // Add a second predicted entity so unchanged-entity fanout has the same
    // shape as the live logs, where two entities requested unchanged rollback
    // for the same server-confirmed window.
    let predicted_b = stepper
        .client_app()
        .world_mut()
        .spawn((Predicted, CompFull(10.0)))
        .id();

    // Initialize prediction history for predicted_b.
    stepper.frame_step(1);

    stepper
        .client_app()
        .add_systems(FixedUpdate, increment_component);

    stepper.client_app().insert_resource(RollbackProbe::default());
    stepper.client_app().add_systems(
        PreUpdate,
        record_rollback_after_check
            .after(RollbackSystems::Check)
            .before(RollbackSystems::Prepare),
    );

    // Build enough predicted history that stale-confirmed values and newer
    // predicted values are distinct.
    stepper.frame_step(3);

    let local_tick = stepper.client_tick(0);
    let stale_tick = local_tick - 2;
    let mismatch_tick = local_tick - 1;

    let stale_replicon_tick = RepliconTick::new(700);
    let server_replicon_tick = RepliconTick::new(701);

    info!(
        ?local_tick,
        ?stale_tick,
        ?mismatch_tick,
        ?predicted_a,
        ?predicted_b,
        "same-window-churn: seed begin"
    );

    // Phase 1 seed:
    //
    // Do NOT call trigger_rollback_check() here. The first rollback must come
    // only from the unchanged-entity path.
    {
        let world = stepper.client_app().world_mut();

        world
            .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
            .record(stale_replicon_tick, stale_tick);
        world
            .resource_mut::<lightyear_replication::checkpoint::ReplicationCheckpointMap>()
            .record(server_replicon_tick, mismatch_tick);

        world
            .resource_mut::<ServerMutateTicks>()
            .confirm(server_replicon_tick, 1);

        world
            .entity_mut(predicted_a)
            .insert(ConfirmHistory::new(stale_replicon_tick));
        world
            .entity_mut(predicted_a)
            .get_mut::<PredictionHistory<CompFull>>()
            .expect("predicted_a should have PredictionHistory<CompFull>")
            .add_confirmed(stale_tick, Some(CompFull(-100.0)));

        world
            .entity_mut(predicted_b)
            .insert(ConfirmHistory::new(stale_replicon_tick));
        world
            .entity_mut(predicted_b)
            .get_mut::<PredictionHistory<CompFull>>()
            .expect("predicted_b should have PredictionHistory<CompFull>")
            .add_confirmed(stale_tick, Some(CompFull(-200.0)));
    }

    let (a_live, b_live) = {
        let world = stepper.client_app().world();
        (
            world.get::<CompFull>(predicted_a).cloned(),
            world.get::<CompFull>(predicted_b).cloned(),
        )
    };

    info!(
        ?stale_replicon_tick,
        ?server_replicon_tick,
        ?stale_tick,
        ?mismatch_tick,
        ?a_live,
        ?b_live,
        "same-window-churn: phase 1 seeded stale ConfirmHistory + advanced ServerMutateTicks"
    );

    log_metrics("before first manual client update", &mut stepper);

    // Manually update only the client app. This runs another check cycle
    // without using frame_step(), so the local tick is intentionally not
    // advanced by the harness.
    let tick_before_first_update = stepper.client_tick(0);
    stepper.client_app().update();
    let tick_after_first_update = stepper.client_tick(0);

    log_metrics("after first manual client update", &mut stepper);

    let hits_after_first = stepper
        .client_app()
        .world()
        .resource::<RollbackProbe>()
        .hits
        .clone();

    assert!(
        !hits_after_first.is_empty(),
        "expected first manual client update to request rollback from unchanged-entity path; \
         tick_before_first_update={tick_before_first_update:?}, \
         tick_after_first_update={tick_after_first_update:?}"
    );

    let (first_local_tick, first_rollback_tick) = hits_after_first[0];

    assert_eq!(
        first_rollback_tick, mismatch_tick,
        "expected first rollback to target the ServerMutateTicks-confirmed mismatch window"
    );

    info!(
        ?first_local_tick,
        ?first_rollback_tick,
        ?tick_before_first_update,
        ?tick_after_first_update,
        "same-window-churn: first rollback observed"
    );

    // Phase 2 seed:
    //
    // Mimic the follow-up confirmed update for the same server window being
    // recorded after unchanged rollback completed, but before local tick
    // advances. This is the part that the real-replication companion test proves
    // can come from write_history; here we inject it to isolate same-tick
    // scheduler behavior.
    {
        let world = stepper.client_app().world_mut();

        world
            .entity_mut(predicted_a)
            .get_mut::<PredictionHistory<CompFull>>()
            .expect("predicted_a should still have PredictionHistory<CompFull>")
            .add_confirmed(mismatch_tick, Some(CompFull(1234.0)));

        world
            .entity_mut(predicted_b)
            .get_mut::<PredictionHistory<CompFull>>()
            .expect("predicted_b should still have PredictionHistory<CompFull>")
            .add_confirmed(mismatch_tick, Some(CompFull(5678.0)));
    }

    trigger_rollback_check(&mut stepper, mismatch_tick);

    let (a_live, b_live) = {
        let world = stepper.client_app().world();
        (
            world.get::<CompFull>(predicted_a).cloned(),
            world.get::<CompFull>(predicted_b).cloned(),
        )
    };

    info!(
        ?mismatch_tick,
        ?a_live,
        ?b_live,
        "same-window-churn: phase 2 seeded confirmed-update follow-up mismatch"
    );

    log_metrics("before second manual client update", &mut stepper);

    let tick_before_second_update = stepper.client_tick(0);
    stepper.client_app().update();
    let tick_after_second_update = stepper.client_tick(0);

    log_metrics("after second manual client update", &mut stepper);

    let final_hits = stepper
        .client_app()
        .world()
        .resource::<RollbackProbe>()
        .hits
        .clone();

    assert!(
        final_hits.len() >= 2,
        "expected current code to reproduce the same-window follow-up shape before checking the desired invariant; \
         final_hits={final_hits:?}, \
         tick_before_second_update={tick_before_second_update:?}, \
         tick_after_second_update={tick_after_second_update:?}"
    );

    let (second_local_tick, second_rollback_tick) = final_hits[1];

    assert_eq!(
        second_rollback_tick, first_rollback_tick,
        "diagnostic did not reproduce same-window churn; final_hits={final_hits:?}"
    );

    assert_eq!(
        second_local_tick, first_local_tick,
        "diagnostic did not reproduce same-local-tick churn; final_hits={final_hits:?}"
    );

    let same_window_hits = final_hits
        .iter()
        .filter(|(local_tick, rollback_tick)| {
            *local_tick == first_local_tick && *rollback_tick == first_rollback_tick
        })
        .count();

    assert_eq!(
        same_window_hits, 1,
        "rollback requests for the same local tick/window should be coalesced; \
         observed final_hits={final_hits:?}"
    );
}

/// Reproduces the rollback request-chain through real server->client
/// replication.
///
/// Phase 1:
/// - The server mutates a helper entity.
/// - This advances ServerMutateTicks through real Replicon delivery.
/// - The predicted entity does not receive an explicit component update, so it
///   is checked by the unchanged-entity path.
/// - The unchanged-entity path requests rollback.
///
/// Phase 2:
/// - The server then mutates the predicted entity itself.
/// - The client receives the update through the normal Replicon
///   PredictionRegistry::write_history path.
/// - The confirmed value mismatches PredictionHistory.
/// - StateRollbackMetadata records the mismatch.
/// - A later rollback check consumes it as a confirmed-update rollback.
///
/// This test proves the real pipeline can produce:
///
///     unchanged_entity rollback -> confirmed_update rollback
///
/// It does not require the second rollback to occur at the same local tick or
/// for the same rollback tick. The ignored diagnostic above captures that
/// stricter same-local-tick/same-window shape.
#[test]
fn test_state_rollback_real_replication_can_chain_unchanged_then_confirmed_update() {
    use lightyear_prediction::diagnostics::PredictionMetrics;

    #[derive(Resource, Default, Debug)]
    struct RollbackProbe {
        hits: Vec<(Tick, Tick)>, // (local_tick, rollback_tick)
    }

    fn increment_component(mut query: Query<&mut CompFull, With<Predicted>>) {
        for mut comp in query.iter_mut() {
            comp.0 += 1.0;
        }
    }

    fn record_rollback_after_check(
        timeline: Res<LocalTimeline>,
        manager: Single<&PredictionManager>,
        mut probe: ResMut<RollbackProbe>,
    ) {
        let local_tick = timeline.tick();
        let rollback_start = manager.get_rollback_start_tick();

        info!(
            ?local_tick,
            ?rollback_start,
            is_rollback = manager.is_rollback(),
            "rollback-chain-repro probe: after RollbackSystems::Check"
        );

        if let Some(rollback_tick) = rollback_start {
            probe.hits.push((local_tick, rollback_tick));
        }
    }

    fn log_client_state(
        label: &'static str,
        stepper: &mut ClientServerStepper,
        predicted: Entity,
    ) {
        let local_tick = stepper.client_tick(0);
        let server_tick = stepper.server_tick();

        let metrics = stepper
            .client_app()
            .world()
            .get_resource::<PredictionMetrics>()
            .map(|m| (m.rollbacks, m.rollback_ticks))
            .unwrap_or((0, 0));

        let (
            hits,
            live,
            history_len,
            confirm_history_replicon_tick,
            server_mutate_replicon_tick,
        ) = {
            let world = stepper.client_app().world();

            let hits = world
                .get_resource::<RollbackProbe>()
                .map(|p| p.hits.clone())
                .unwrap_or_default();

            let live = world.get::<CompFull>(predicted).cloned();

            let history_len = world
                .get::<PredictionHistory<CompFull>>(predicted)
                .map(|h| h.len());

            let confirm_history_replicon_tick =
                world.get::<ConfirmHistory>(predicted).map(|c| c.last_tick());

            let server_mutate_replicon_tick =
                world.resource::<ServerMutateTicks>().last_tick();

            (
                hits,
                live,
                history_len,
                confirm_history_replicon_tick,
                server_mutate_replicon_tick,
            )
        };

        info!(
            label,
            ?local_tick,
            ?server_tick,
            rollbacks = metrics.0,
            rollback_ticks = metrics.1,
            ?hits,
            ?live,
            ?history_len,
            ?confirm_history_replicon_tick,
            ?server_mutate_replicon_tick,
            "rollback-chain-repro client state"
        );
    }

    let mut stepper = ClientServerStepper::from_config(StepperConfig::single());

    // This is the predicted entity we care about. It is server-backed, so later
    // server mutations can reach the client through Replicon and the normal
    // PredictionRegistry::write_history path.
    let server_predicted_entity = stepper
        .server_app
        .world_mut()
        .spawn((
            Replicate::to_clients(NetworkTarget::All),
            PredictionTarget::to_clients(NetworkTarget::All),
            CompFull(1.0),
        ))
        .id();

    // Helper entity used only to advance ServerMutateTicks with a real server
    // mutation while leaving server_predicted_entity unchanged.
    let server_helper_entity = stepper
        .server_app
        .world_mut()
        .spawn((
            Replicate::to_clients(NetworkTarget::All),
            CompFull(100.0),
        ))
        .id();

    info!(
        ?server_predicted_entity,
        ?server_helper_entity,
        "rollback-chain-repro: spawned server entities"
    );

    stepper.frame_step(4);

    let predicted = stepper
        .client(0)
        .get::<MessageManager>()
        .unwrap()
        .entity_mapper
        .get_local(server_predicted_entity)
        .expect("server predicted entity should be mapped to a client entity");

    let helper_client_entity = stepper
        .client(0)
        .get::<MessageManager>()
        .unwrap()
        .entity_mapper
        .get_local(server_helper_entity)
        .expect("server helper entity should be mapped to a client entity");

    info!(
        ?server_predicted_entity,
        ?server_helper_entity,
        ?predicted,
        ?helper_client_entity,
        "rollback-chain-repro: mapped server entities to client entities"
    );

    {
        let world = stepper.client_app().world();
        assert!(
            world.get::<Predicted>(predicted).is_some(),
            "client entity should be Predicted"
        );
        assert!(
            world.get::<PredictionHistory<CompFull>>(predicted).is_some(),
            "client entity should have PredictionHistory<CompFull>"
        );
        assert!(
            world.get::<ConfirmHistory>(predicted).is_some(),
            "client entity should have ConfirmHistory"
        );
    }

    stepper
        .client_app()
        .add_systems(FixedUpdate, increment_component);

    stepper.client_app().insert_resource(RollbackProbe::default());
    stepper.client_app().add_systems(
        PreUpdate,
        record_rollback_after_check
            .after(RollbackSystems::Check)
            .before(RollbackSystems::Prepare),
    );

    // Build prediction history so the client has predicted values ahead of the
    // server-confirmed region.
    stepper.frame_step(3);

    // Clear probe noise from setup/history-building frames.
    stepper
        .client_app()
        .world_mut()
        .resource_mut::<RollbackProbe>()
        .hits
        .clear();

    // Use a real world tick near the server timeline, not a fake Replicon tick.
    // The client is ahead of the server, so prediction history should contain
    // this tick.
    let stale_confirmed_tick = stepper.server_tick();

    info!(
        ?stale_confirmed_tick,
        client_tick = ?stepper.client_tick(0),
        server_tick = ?stepper.server_tick(),
        "rollback-chain-repro: seeding stale confirmed value into prediction history"
    );

    {
        let world = stepper.client_app().world_mut();

        world
            .entity_mut(predicted)
            .get_mut::<PredictionHistory<CompFull>>()
            .expect("predicted entity should have PredictionHistory<CompFull>")
            .add_confirmed(stale_confirmed_tick, Some(CompFull(-100.0)));
    }

    log_client_state(
        "after stale confirmed value seed, before helper mutation",
        &mut stepper,
        predicted,
    );

    // Phase 1: mutate only the helper entity on the server. This advances
    // ServerMutateTicks through real replication while the predicted entity does
    // not receive an explicit update, making it eligible for unchanged-entity
    // rollback checking.
    {
        let mut helper = stepper
            .server_app
            .world_mut()
            .entity_mut(server_helper_entity);
        helper.get_mut::<CompFull>().unwrap().0 = 101.0;
    }

    info!(
        server_tick = ?stepper.server_tick(),
        client_tick = ?stepper.client_tick(0),
        "rollback-chain-repro: phase 1 helper server mutation applied"
    );

    let phase_1_start_len = stepper
        .client_app()
        .world()
        .resource::<RollbackProbe>()
        .hits
        .len();

    for attempt in 1..=6 {
        let before_client_tick = stepper.client_tick(0);
        let before_server_tick = stepper.server_tick();

        info!(
            attempt,
            ?before_client_tick,
            ?before_server_tick,
            "rollback-chain-repro: running server-first frame for phase 1 unchanged rollback"
        );

        stepper.frame_step_server_first(1);

        log_client_state(
            "after phase 1 server-first attempt",
            &mut stepper,
            predicted,
        );

        let hits = stepper
            .client_app()
            .world()
            .resource::<RollbackProbe>()
            .hits
            .clone();

        info!(
            attempt,
            before_client_tick = ?before_client_tick,
            after_client_tick = ?stepper.client_tick(0),
            before_server_tick = ?before_server_tick,
            after_server_tick = ?stepper.server_tick(),
            ?hits,
            "rollback-chain-repro: phase 1 attempt complete"
        );

        if hits.len() > phase_1_start_len {
            break;
        }
    }

    let hits_after_phase_1 = stepper
        .client_app()
        .world()
        .resource::<RollbackProbe>()
        .hits
        .clone();

    assert!(
        hits_after_phase_1.len() > phase_1_start_len,
        "expected phase 1 helper mutation to cause unchanged-entity rollback; \
         hits_after_phase_1={hits_after_phase_1:?}"
    );

    let (first_local_tick, first_rollback_tick) = hits_after_phase_1[phase_1_start_len];

    info!(
        ?first_local_tick,
        ?first_rollback_tick,
        ?hits_after_phase_1,
        "rollback-chain-repro: phase 1 unchanged rollback observed"
    );

    // Phase 2: mutate the actual predicted server entity. This should go
    // through Replicon receive/write_history on the client. If the confirmed
    // value mismatches prediction history, write_history records a mismatch,
    // and a later rollback check consumes it as confirmed_update.
    {
        let mut server_entity = stepper
            .server_app
            .world_mut()
            .entity_mut(server_predicted_entity);
        server_entity.get_mut::<CompFull>().unwrap().0 = 1234.0;
    }

    info!(
        server_tick = ?stepper.server_tick(),
        client_tick = ?stepper.client_tick(0),
        ?first_local_tick,
        ?first_rollback_tick,
        "rollback-chain-repro: phase 2 predicted server entity mutation applied"
    );

    let phase_2_start_len = stepper
        .client_app()
        .world()
        .resource::<RollbackProbe>()
        .hits
        .len();

    for attempt in 1..=8 {
        let before_client_tick = stepper.client_tick(0);
        let before_server_tick = stepper.server_tick();

        info!(
            attempt,
            ?before_client_tick,
            ?before_server_tick,
            "rollback-chain-repro: running server-first frame for phase 2 confirmed-update rollback"
        );

        stepper.frame_step_server_first(1);

        log_client_state(
            "after phase 2 server-first attempt",
            &mut stepper,
            predicted,
        );

        let hits = stepper
            .client_app()
            .world()
            .resource::<RollbackProbe>()
            .hits
            .clone();

        info!(
            attempt,
            before_client_tick = ?before_client_tick,
            after_client_tick = ?stepper.client_tick(0),
            before_server_tick = ?before_server_tick,
            after_server_tick = ?stepper.server_tick(),
            ?hits,
            "rollback-chain-repro: phase 2 attempt complete"
        );

        if hits.len() > phase_2_start_len {
            break;
        }
    }

    let final_hits = stepper
        .client_app()
        .world()
        .resource::<RollbackProbe>()
        .hits
        .clone();

    assert!(
        final_hits.len() > phase_2_start_len,
        "expected phase 2 predicted server mutation to produce confirmed-update rollback via real write_history path; \
         final_hits={final_hits:?}. \
         If this fails, the mutation did not reach PredictionRegistry::write_history as a mismatch-producing update."
    );

    let (second_local_tick, second_rollback_tick) = final_hits[phase_2_start_len];

    info!(
        ?final_hits,
        ?first_local_tick,
        ?first_rollback_tick,
        ?second_local_tick,
        ?second_rollback_tick,
        "rollback-chain-repro: observed real unchanged_entity -> real confirmed_update rollback sequence"
    );

    assert!(
        second_local_tick >= first_local_tick,
        "second rollback should not occur before first rollback; final_hits={final_hits:?}"
    );

    // Diagnostic only. The real-replication path may use a later rollback tick
    // because this harness lets normal server frames pass.
    if second_rollback_tick != first_rollback_tick {
        info!(
            ?first_rollback_tick,
            ?second_rollback_tick,
            "rollback-chain-repro: phase 2 rollback tick differs from phase 1 rollback tick"
        );
    }

    if second_local_tick != first_local_tick {
        info!(
            ?first_local_tick,
            ?second_local_tick,
            "rollback-chain-repro: phase 2 occurred on a later local tick; same-tick reproduction is covered by the ignored diagnostic"
        );
    }
}