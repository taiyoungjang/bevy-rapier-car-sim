use super::params::*;
use crate::{
    camera::{CameraConfig, CameraFollowMode},
    car::*,
    config::*,
    db_client::DbClientResource,
    nn::{dqn_bevy::*, util::*},
    track::*,
};
use bevy::prelude::*;
use bevy_rapier3d::prelude::*;
use dfdx::prelude::*;
use rand::Rng;
use std::time::Instant;

pub type QNetwork = (
    (Linear<STATE_SIZE, HIDDEN_SIZE>, ReLU),
    (Linear<HIDDEN_SIZE, HIDDEN_SIZE>, ReLU),
    Linear<HIDDEN_SIZE, ACTIONS>,
);
pub type Observation = [f32; STATE_SIZE];
pub const OBSERVATION_ZERO: Observation = [0.; STATE_SIZE];

pub fn dqn_system(
    time: Res<Time>,
    mut dqn: ResMut<DqnResource>,
    mut sgd_res: NonSendMut<SgdResource>,
    mut cars_dqn: NonSendMut<CarsDqnResource>,
    q_name: Query<&Name>,
    mut q_car: Query<(
        &mut Car,
        &Velocity,
        &Transform,
        &Children,
        Entity,
        Option<&HID>,
        &mut CarDqnPrev,
    )>,
    q_colliding_entities: Query<&CollidingEntities, With<CollidingEntities>>,
    mut config: ResMut<Config>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut camera_config: ResMut<CameraConfig>,
    dbres: Res<DbClientResource>,
) {
    let seconds = time.seconds_since_startup();
    if dqn.respawn_at > 0. && seconds > dqn.respawn_at {
        let (transform, init_meters) = config.get_transform_random();
        let new_car_id = spawn_car(
            &mut commands,
            &mut meshes,
            &mut materials,
            &config.car_scene.as_ref().unwrap(),
            dqn.respawn_is_hid,
            transform,
            dqn.respawn_index,
            init_meters,
            config.max_torque,
        );
        // if camera_config.mode.not_none() && dqn.respawn_is_hid {
        camera_config.camera_follow = Some(new_car_id);
        camera_config.mode = CameraFollowMode::Far;
        // }
        dqn.respawn_at = 0.;
        dqn.respawn_is_hid = false;
        dqn.respawn_index = 0;
        config.use_brain = true;
        return;
    };
    let should_act: bool = seconds > dqn.seconds;
    if should_act {
        dqn.seconds = seconds + STEP_DURATION;
        dqn.step += 1;
    }

    for (mut car, v, tr, children, e, hid, mut car_dqn_prev) in q_car.iter_mut() {
        let is_hid = hid.is_some();
        let mut crash: bool = false;
        for &child in children.iter() {
            let colliding_entities = q_colliding_entities.get(child);
            if let Ok(colliding_entities) = colliding_entities {
                for e in colliding_entities.iter() {
                    let colliding_entity = q_name.get(e).unwrap();
                    if !colliding_entity.contains(ASSET_ROAD) {
                        crash = true;
                    }
                }
            }
        }

        let mut vel_angle = car.line_dir.angle_between(v.linvel);
        if vel_angle.is_nan() {
            vel_angle = 0.;
        }
        let pos_dir = tr.rotation.mul_vec3(Vec3::Z);
        let mut pos_angle = car.line_dir.angle_between(pos_dir);
        if pos_angle.is_nan() {
            pos_angle = 0.;
        }
        let vel_cos = vel_angle.cos();
        let pos_cos = pos_angle.cos();
        let mut d_from_center = car.line_pos - tr.translation;
        d_from_center.y = 0.;
        let d = d_from_center.length();

        let shape_reward = || -> f32 {
            if crash {
                return -1.;
            }
            // https://team.inria.fr/rits/files/2018/02/ICRA18_EndToEndDriving_CameraReady.pdf
            // In [13] the reward is computed as a function of the difference of angle α between the road and car’s heading and the speed v.
            // R = v(cos α − d)
            let mut reward = v.linvel.length() / SPEED_LIMIT_MPS * (vel_cos - d / 5.);
            if vel_cos.is_sign_positive() && pos_cos.is_sign_negative() {
                reward = -reward;
            }
            if reward.is_nan() {
                return 0.;
            }
            return reward;
        };
        let reward = shape_reward();
        let mps = v.linvel.length();
        let kmh = mps / 1000. * 3600.;
        let mut obs: Observation = [0.; STATE_SIZE];
        for i in 0..obs.len() {
            obs[i] = match i {
                0 => kmh / 100.,
                1 => vel_cos,
                2 => pos_cos,
                _ => car.sensor_inputs[i - STATE_SIZE_BASE],
            };
        }

        let (prev_action, prev_obs) = (car_dqn_prev.prev_action, car_dqn_prev.prev_obs);
        if config.use_brain && (should_act || crash) && !prev_obs.iter().all(|&x| x == 0.) {
            dqn.rb.store(prev_obs, prev_action, reward, obs, crash);
            if dqn.rb.should_persist() {
                dqn.rb.persist(&dbres.client);
            }
        }

        let (action, exploration) = cars_dqn.act(obs, dqn.eps);
        if should_act && !crash {
            car_dqn_prev.prev_obs = obs;
            car_dqn_prev.prev_action = action;
            car_dqn_prev.prev_reward = reward;
        }
        if crash {
            dqn.crashes += 1;
            dqn.respawn_at = seconds + 0.5;
            dqn.respawn_is_hid = is_hid;
            dqn.respawn_index = car.index;
            commands.entity(e).despawn_recursive();
            car.despawn_wheels(&mut commands);
            config.use_brain = false;
        }
        if !config.use_brain || !should_act || crash {
            return;
        }

        if let Some(_hid) = hid {
            if dqn.rb.len() < BATCH_SIZE {
                log_action_reward(car_dqn_prev.prev_action, reward);
            } else {
                let start = Instant::now();
                let mut rng = rand::thread_rng();
                let batch_indexes = [(); BATCH_SIZE].map(|_| rng.gen_range(0..dqn.rb.len()));
                let (s, a, r, sn, done) = dqn.rb.get_batch_tensors(batch_indexes);
                let mut loss_string: String = String::from("");
                for _i_epoch in 0..EPOCHS {
                    let next_q_values: Tensor2D<BATCH_SIZE, ACTIONS> =
                        cars_dqn.tqn.forward(sn.clone());
                    let max_next_q: Tensor1D<BATCH_SIZE> = next_q_values.max_axis::<-1>();
                    let target_q = 0.99 * mul(max_next_q, &(1.0 - done.clone())) + &r;
                    // forward through model, computing gradients
                    let q_values: Tensor2D<BATCH_SIZE, ACTIONS, OwnedTape> =
                        cars_dqn.qn.forward(s.trace());
                    let action_qs: Tensor1D<BATCH_SIZE, OwnedTape> = q_values.select(&a);
                    let loss = huber_loss(action_qs, &target_q, 1.);
                    let loss_v = *loss.data();
                    // run backprop
                    let gradients = loss.backward();
                    sgd_res
                        .sgd
                        .update(&mut cars_dqn.qn, gradients)
                        .expect("Unused params");
                    if _i_epoch % 4 == 0 {
                        loss_string.push_str(format!("{:.2} ", loss_v).as_str());
                    }
                }
                log_training(exploration, action, reward, &loss_string, start);
                if dqn.step % SYNC_INTERVAL_STEPS == 0 && dqn.rb.len() > BATCH_SIZE * 2 {
                    dbg!("networks sync");
                    cars_dqn.tqn = cars_dqn.qn.clone();
                }
                dqn.eps = if dqn.eps <= dqn.min_eps {
                    dqn.min_eps
                } else {
                    dqn.eps - DECAY
                };
            }
        }

        let (gas, brake, left, right) = map_action_to_car(action);
        car.gas = gas;
        car.brake = brake;
        car.steering = -left + right;
    }
}
