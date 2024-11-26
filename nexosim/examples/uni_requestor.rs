//! Example: sensor reading data from the environment model.
//!
//! This example demonstrates in particular:
//!
//! * cyclical self-scheduling methods,
//! * model initialization,
//! * simulation monitoring with buffered event sinks,
//! * connection with mapping,
//! * UniRequestor port.
//!
//! ```text
//!                        ┌─────────────┐               ┌──────────┐
//!                        │             │ temperature   │          │ overheat
//! Temperature ●─────────►│ Environment ├──────────────►│ Sensor   ├──────────►
//!                        │             │               │          │ state
//!                        └─────────────┘               └──────────┘
//! ```

use std::time::Duration;

use nexosim_util::observables::ObservableValue;

use nexosim::model::{Context, InitializedModel, Model};
use nexosim::ports::{EventBuffer, Output, UniRequestor};
use nexosim::simulation::{Mailbox, SimInit, SimulationError};
use nexosim::time::MonotonicTime;

/// Sensor model
pub struct Sensor {
    /// Temperature [deg C] -- requestor port.
    pub temp: UniRequestor<(), f64>,

    /// Overheat detection [-] -- output port.
    pub overheat: Output<bool>,

    /// Temperature threshold [deg C] -- parameter.
    threshold: f64,

    /// Overheat detection [-] -- observable state.
    oh: ObservableValue<bool>,
}

impl Sensor {
    /// Creates new Sensor with overheat threshold set [deg C].
    pub fn new(threshold: f64, temp: UniRequestor<(), f64>) -> Self {
        let overheat = Output::new();
        Self {
            temp,
            overheat: overheat.clone(),
            threshold,
            oh: ObservableValue::new(overheat),
        }
    }

    /// Cyclically scheduled method that reads data from environment and
    /// avaluates overheat state.
    pub async fn tick(&mut self) {
        let temp = self.temp.send(()).await.unwrap();
        if temp > self.threshold {
            if !self.oh.get() {
                self.oh.set(true).await;
            }
        } else if *self.oh.get() {
            self.oh.set(false).await;
        }
    }
}

impl Model for Sensor {
    /// Propagate state and schedule cyclic method.
    async fn init(mut self, context: &mut Context<Self>) -> InitializedModel<Self> {
        self.oh.propagate().await;

        context
            .schedule_periodic_event(
                Duration::from_millis(500),
                Duration::from_millis(500),
                Self::tick,
                (),
            )
            .unwrap();

        self.into()
    }
}

/// Environment model.
pub struct Env {
    /// Temperature [deg F] -- internal state.
    temp: f64,
}

impl Env {
    /// Creates new environment model with the temperature [deg F] set.
    pub fn new(temp: f64) -> Self {
        Self { temp }
    }

    /// Sets temperature [deg F].
    pub async fn set_temp(&mut self, temp: f64) {
        self.temp = temp;
    }

    /// Gets temperature [deg F].
    pub async fn get_temp(&mut self, _: ()) -> f64 {
        self.temp
    }
}

impl Model for Env {}

/// Converts Fahrenheit to Celsius.
pub fn fahr_to_cels(t: f64) -> f64 {
    5.0 * (t - 32.0) / 9.0
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Mailboxes.
    let sensor_mbox = Mailbox::new();
    let env_mbox = Mailbox::new();

    // Connect data line and convert Fahrenheit degrees to Celsius.
    let temp_req = UniRequestor::with_map(|x| *x, fahr_to_cels, Env::get_temp, &env_mbox);

    // Models.
    let mut sensor = Sensor::new(100.0, temp_req);
    let env = Env::new(0.0);

    // Model handles for simulation.
    let env_addr = env_mbox.address();

    let mut overheat = EventBuffer::new();
    sensor.overheat.connect_sink(&overheat);

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let (mut simu, scheduler) = SimInit::new()
        .add_model(sensor, sensor_mbox, "sensor")
        .add_model(env, env_mbox, "env")
        .init(t0)?;

    // ----------
    // Simulation.
    // ----------

    // Check initial conditions.
    assert_eq!(simu.time(), t0);
    assert_eq!(overheat.next(), Some(false));
    assert!(overheat.next().is_none());

    // Change temperature in 2s.
    scheduler
        .schedule_event(Duration::from_secs(2), Env::set_temp, 105.0, &env_addr)
        .unwrap();

    // Change temperature in 4s.
    scheduler
        .schedule_event(Duration::from_secs(4), Env::set_temp, 213.0, &env_addr)
        .unwrap();

    simu.step_until(Duration::from_secs(3))?;
    assert!(overheat.next().is_none());

    simu.step_until(Duration::from_secs(5))?;
    assert_eq!(overheat.next(), Some(true));

    Ok(())
}
