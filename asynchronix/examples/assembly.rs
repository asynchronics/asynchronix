//! Example: an assembly consisting of a current-controlled stepper motor and
//! its driver.
//!
//! This example demonstrates in particular:
//!
//! * submodels,
//! * outputs cloning,
//! * self-scheduling methods,
//! * model setup,
//! * model initialization,
//! * simulation monitoring with event streams.
//!
//! ```text
//!                     ┌──────────────────────────────────────────────┐
//!                     │ Assembly                                     │
//!                     │   ┌──────────┐                ┌──────────┐   │
//!                PPS  │   │          │ coil currents  │          │   │position
//! Pulse rate ●───────►│──►│  Driver  ├───────────────►│  Motor   ├──►│─────────►
//!              (±freq)│   │          │    (IA, IB)    │          │   │(0:199)
//!                     │   └──────────┘                └──────────┘   │
//!                     └──────────────────────────────────────────────┘
//! ```

use std::time::Duration;

use asynchronix::model::{Model, SetupContext};
use asynchronix::ports::{EventBuffer, Output};
use asynchronix::simulation::{Mailbox, SimInit, SimulationError};
use asynchronix::time::MonotonicTime;

mod stepper_motor;

pub use stepper_motor::{Driver, Motor};

pub struct MotorAssembly {
    pub position: Output<u16>,
    init_pos: u16,
    load: Output<f64>,
    pps: Output<f64>,
}

impl MotorAssembly {
    pub fn new(init_pos: u16) -> Self {
        Self {
            position: Default::default(),
            init_pos,
            load: Default::default(),
            pps: Default::default(),
        }
    }

    /// Sets the pulse rate (sign = direction) [Hz] -- input port.
    pub async fn pulse_rate(&mut self, pps: f64) {
        self.pps.send(pps).await;
    }

    /// Torque applied by the load [N·m] -- input port.
    pub async fn load(&mut self, torque: f64) {
        self.load.send(torque).await;
    }
}

impl Model for MotorAssembly {
    fn setup(&mut self, setup_context: &SetupContext<Self>) {
        let mut motor = Motor::new(self.init_pos);
        let mut driver = Driver::new(1.0);

        // Mailboxes.
        let motor_mbox = Mailbox::new();
        let driver_mbox = Mailbox::new();

        // Connections.
        self.pps.connect(Driver::pulse_rate, &driver_mbox);
        self.load.connect(Motor::load, &motor_mbox);
        driver.current_out.connect(Motor::current_in, &motor_mbox);
        // Note: it is important to clone `position` from the parent to the
        // submodel so that all connections made by the user to the parent model
        // are preserved. Connections added after cloning are reflected in all
        // clones.
        motor.position = self.position.clone();

        setup_context.add_model(driver, driver_mbox, "driver");
        setup_context.add_model(motor, motor_mbox, "motor");
    }
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.
    let init_pos = 123;
    let mut assembly = MotorAssembly::new(init_pos);

    // Mailboxes.
    let assembly_mbox = Mailbox::new();
    let assembly_addr = assembly_mbox.address();

    // Model handles for simulation.
    let mut position = EventBuffer::new();
    assembly.position.connect_sink(&position);

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let mut simu = SimInit::new()
        .add_model(assembly, assembly_mbox, "assembly")
        .init(t0)?;

    let scheduler = simu.scheduler();

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
    simu.step_by(Duration::new(0, 900_000_000))?;
    t += Duration::new(0, 900_000_000);
    assert_eq!(simu.time(), t);
    for _ in 0..9 {
        pos = (pos + 1) % Motor::STEPS_PER_REV;
        assert_eq!(position.next(), Some(pos));
    }
    assert!(position.next().is_none());

    Ok(())
}
