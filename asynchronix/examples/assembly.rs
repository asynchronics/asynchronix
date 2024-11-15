//! Example: an assembly consisting of a current-controlled stepper motor and
//! its driver.
//!
//! This example demonstrates in particular:
//!
//! * model prototypes,
//! * submodels,
//! * self-scheduling methods,
//! * model initialization,
//! * simulation monitoring with buffered event sinks.
//!
//! ```text
//!                       ┌────────────────────────────────────────────┐
//!                       │ Assembly                                   │
//!                       │   ┌──────────┐                             │
//!                PPS    │   │          │ coil currents  ┌─────────┐  │
//! Pulse rate ●──────────┼──►│  Driver  ├───────────────►│         │  │
//!              (±freq)  │   │          │    (IA, IB)    │         │  │ position
//!                       │   └──────────┘                │  Motor  ├──┼──────────►
//!              torque   │                               │         │  │ (0:199)
//!       Load ●──────────┼──────────────────────────────►│         │  │
//!                       │                               └─────────┘  │
//!                       └────────────────────────────────────────────┘
//! ```

use std::time::Duration;

use asynchronix::model::{BuildContext, Model, ProtoModel};
use asynchronix::ports::{EventBuffer, Output};
use asynchronix::simulation::{Mailbox, SimInit, SimulationError};
use asynchronix::time::MonotonicTime;

mod stepper_motor;

pub use stepper_motor::{Driver, Motor};

/// A prototype for `MotorAssembly`.
pub struct ProtoMotorAssembly {
    pub position: Output<u16>,
    init_pos: u16,
}

impl ProtoMotorAssembly {
    /// The prototype has a public constructor.
    pub fn new(init_pos: u16) -> Self {
        Self {
            position: Default::default(),
            init_pos,
        }
    }

    // Input methods are in the model itself.
}

/// The parent model which submodels are the driver and the motor.
pub struct MotorAssembly {
    /// Private output for submodel connection.
    pps: Output<f64>,
    /// Private output for submodel connection.
    load: Output<f64>,
}

impl MotorAssembly {
    /// The model now has a module-private constructor.
    fn new() -> Self {
        Self {
            pps: Default::default(),
            load: Default::default(),
        }
    }

    /// Pulse rate (sign = direction) [Hz] -- input port.
    pub async fn pulse_rate(&mut self, pps: f64) {
        self.pps.send(pps).await
    }

    /// Torque applied by the load [N·m] -- input port.
    pub async fn load(&mut self, torque: f64) {
        self.load.send(torque).await
    }
}

impl Model for MotorAssembly {}

impl ProtoModel for ProtoMotorAssembly {
    type Model = MotorAssembly;

    fn build(self, cx: &mut BuildContext<Self>) -> MotorAssembly {
        let mut assembly = MotorAssembly::new();
        let mut motor = Motor::new(self.init_pos);
        let mut driver = Driver::new(1.0);

        // Mailboxes.
        let motor_mbox = Mailbox::new();
        let driver_mbox = Mailbox::new();

        // Connections.
        assembly.pps.connect(Driver::pulse_rate, &driver_mbox);
        assembly.load.connect(Motor::load, &motor_mbox);
        driver.current_out.connect(Motor::current_in, &motor_mbox);

        // Move the prototype's output to the submodel. The `self.position`
        // output can be cloned if necessary if several submodels need access to
        // it.
        motor.position = self.position;

        // Add the submodels to the simulation.
        cx.add_submodel(driver, driver_mbox, "driver");
        cx.add_submodel(motor, motor_mbox, "motor");

        assembly
    }
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.
    let init_pos = 123;
    let mut assembly = ProtoMotorAssembly::new(init_pos);

    // Mailboxes.
    let assembly_mbox = Mailbox::new();
    let assembly_addr = assembly_mbox.address();

    // Model handles for simulation.
    let mut position = EventBuffer::new();
    assembly.position.connect_sink(&position);

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let (mut simu, scheduler) = SimInit::new()
        .add_model(assembly, assembly_mbox, "assembly")
        .init(t0)?;

    // ----------
    // Simulation.
    // ----------

    // Check initial conditions.
    let mut t = t0;
    assert_eq!(simu.time(), t);
    assert_eq!(position.next(), Some(init_pos));
    assert!(position.next().is_none());

    // Start the motor in 2s with a PPS of 10Hz.
    scheduler
        .schedule_event(
            Duration::from_secs(2),
            MotorAssembly::pulse_rate,
            10.0,
            &assembly_addr,
        )
        .unwrap();

    // Advance simulation time to two next events.
    simu.step()?;
    t += Duration::new(2, 0);
    assert_eq!(simu.time(), t);
    simu.step()?;
    t += Duration::new(0, 100_000_000);
    assert_eq!(simu.time(), t);

    // Whichever the starting position, after two phase increments from the
    // driver the rotor should have synchronized with the driver, with a
    // position given by this beautiful formula.
    let mut pos = (((init_pos + 1) / 4) * 4 + 1) % Motor::STEPS_PER_REV;
    assert_eq!(position.by_ref().last().unwrap(), pos);

    // Advance simulation time by 0.9s, which with a 10Hz PPS should correspond to
    // 9 position increments.
    simu.step_until(Duration::new(0, 900_000_000))?;
    t += Duration::new(0, 900_000_000);
    assert_eq!(simu.time(), t);
    for _ in 0..9 {
        pos = (pos + 1) % Motor::STEPS_PER_REV;
        assert_eq!(position.next(), Some(pos));
    }
    assert!(position.next().is_none());

    Ok(())
}
