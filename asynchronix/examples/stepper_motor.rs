//! Example: current-controlled stepper motor and its driver.
//!
//! This example demonstrates in particular:
//!
//! * self-scheduling methods,
//! * model initialization,
//! * simulation monitoring with buffered event sinks.
//!
//! ```text
//!                       ┌──────────┐
//!                PPS    │          │ coil currents  ┌─────────┐
//! Pulse rate ●─────────►│  Driver  ├───────────────►│         │
//!              (±freq)  │          │    (IA, IB)    │         │ position
//!                       └──────────┘                │  Motor  ├──────────►
//!                                       torque      │         │ (0:199)
//!       Load ●─────────────────────────────────────►│         │
//!                                                   └─────────┘
//! ```

use std::future::Future;
use std::time::Duration;

use asynchronix::model::{Context, InitializedModel, Model};
use asynchronix::ports::{EventBuffer, Output};
use asynchronix::simulation::{Mailbox, SimInit};
use asynchronix::time::MonotonicTime;

/// Stepper motor.
pub struct Motor {
    /// Position [-] -- output port.
    pub position: Output<u16>,

    /// Position [-] -- internal state.
    pos: u16,
    /// Torque applied by the load [N·m] -- internal state.
    torque: f64,
}

impl Motor {
    /// Number of steps per revolution.
    pub const STEPS_PER_REV: u16 = 200;
    /// Torque constant of the motor [N·m·A⁻¹].
    pub const TORQUE_CONSTANT: f64 = 1.0;

    /// Creates a motor with the specified initial position.
    pub fn new(position: u16) -> Self {
        Self {
            position: Default::default(),
            pos: position % Self::STEPS_PER_REV,
            torque: 0.0,
        }
    }

    /// Coil currents [A] -- input port.
    ///
    /// For the sake of simplicity, we do as if the rotor rotates
    /// instantaneously. If the current is too weak to overcome the load or when
    /// attempting to move to an opposite phase, the position remains unchanged.
    pub async fn current_in(&mut self, current: (f64, f64)) {
        assert!(!current.0.is_nan() && !current.1.is_nan());

        let (target_phase, abs_current) = match (current.0 != 0.0, current.1 != 0.0) {
            (false, false) => return,
            (true, false) => (if current.0 > 0.0 { 0 } else { 2 }, current.0.abs()),
            (false, true) => (if current.1 > 0.0 { 1 } else { 3 }, current.1.abs()),
            _ => panic!("current detected in both coils"),
        };

        if abs_current < Self::TORQUE_CONSTANT * self.torque {
            return;
        }
        let pos_delta = match target_phase - (self.pos % 4) as i8 {
            0 | 2 | -2 => return,
            1 | -3 => 1,
            -1 | 3 => Self::STEPS_PER_REV - 1,
            _ => unreachable!(),
        };

        self.pos = (self.pos + pos_delta) % Self::STEPS_PER_REV;
        self.position.send(self.pos).await;
    }

    /// Torque applied by the load [N·m] -- input port.
    pub fn load(&mut self, torque: f64) {
        assert!(torque >= 0.0);

        self.torque = torque;
    }
}

impl Model for Motor {
    /// Broadcasts the initial position of the motor.
    async fn init(mut self, _: &Context<Self>) -> InitializedModel<Self> {
        self.position.send(self.pos).await;
        self.into()
    }
}

/// Stepper motor driver.
pub struct Driver {
    /// Coil A and coil B currents [A] -- output port.
    pub current_out: Output<(f64, f64)>,

    /// Requested pulse rate (pulse per second) [Hz] -- internal state.
    pps: f64,
    /// Phase for the next pulse (= 0, 1, 2 or 3) -- internal state.
    next_phase: u8,
    /// Nominal coil current (absolute value) [A] -- constant.
    current: f64,
}

impl Driver {
    /// Minimum supported pulse rate [Hz].
    const MIN_PPS: f64 = 1.0;
    /// Maximum supported pulse rate [Hz].
    const MAX_PPS: f64 = 1_000.0;

    /// Creates a new driver with the specified nominal current.
    pub fn new(nominal_current: f64) -> Self {
        Self {
            current_out: Default::default(),
            pps: 0.0,
            next_phase: 0,
            current: nominal_current,
        }
    }

    /// Pulse rate (sign = direction) [Hz] -- input port.
    pub async fn pulse_rate(&mut self, pps: f64, context: &Context<Self>) {
        let pps = pps.signum() * pps.abs().clamp(Self::MIN_PPS, Self::MAX_PPS);
        if pps == self.pps {
            return;
        }

        let is_idle = self.pps == 0.0;
        self.pps = pps;

        // Trigger the rotation if the motor is currently idle. Otherwise the
        // new value will be accounted for at the next pulse.
        if is_idle {
            self.send_pulse((), context).await;
        }
    }

    /// Sends a pulse and schedules the next one.
    ///
    /// Note: self-scheduling async methods must be for now defined with an
    /// explicit signature instead of `async fn` due to a rustc issue.
    fn send_pulse<'a>(
        &'a mut self,
        _: (),
        context: &'a Context<Self>,
    ) -> impl Future<Output = ()> + Send + 'a {
        async move {
            let current_out = match self.next_phase {
                0 => (self.current, 0.0),
                1 => (0.0, self.current),
                2 => (-self.current, 0.0),
                3 => (0.0, -self.current),
                _ => unreachable!(),
            };
            self.current_out.send(current_out).await;

            if self.pps == 0.0 {
                return;
            }

            self.next_phase = (self.next_phase + (self.pps.signum() + 4.0) as u8) % 4;

            let pulse_duration = Duration::from_secs_f64(1.0 / self.pps.abs());

            // Schedule the next pulse.
            context
                .scheduler
                .schedule_event(pulse_duration, Self::send_pulse, ())
                .unwrap();
        }
    }
}

impl Model for Driver {}

#[allow(dead_code)]
fn main() -> Result<(), asynchronix::simulation::SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.
    let init_pos = 123;
    let mut motor = Motor::new(init_pos);
    let mut driver = Driver::new(1.0);

    // Mailboxes.
    let motor_mbox = Mailbox::new();
    let driver_mbox = Mailbox::new();

    // Connections.
    driver.current_out.connect(Motor::current_in, &motor_mbox);

    // Model handles for simulation.
    let mut position = EventBuffer::new();
    motor.position.connect_sink(&position);
    let motor_addr = motor_mbox.address();
    let driver_addr = driver_mbox.address();

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let mut simu = SimInit::new()
        .add_model(driver, driver_mbox, "driver")
        .add_model(motor, motor_mbox, "motor")
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
            Driver::pulse_rate,
            10.0,
            &driver_addr,
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

    // Increase the load beyond the torque limit for a 1A driver current.
    simu.process_event(Motor::load, 2.0, &motor_addr)?;

    // Advance simulation time and check that the motor is blocked.
    simu.step()?;
    t += Duration::new(0, 100_000_000);
    assert_eq!(simu.time(), t);
    assert!(position.next().is_none());

    // Do it again.
    simu.step()?;
    t += Duration::new(0, 100_000_000);
    assert_eq!(simu.time(), t);
    assert!(position.next().is_none());

    // Decrease the load below the torque limit for a 1A driver current and
    // advance simulation time.
    simu.process_event(Motor::load, 0.5, &motor_addr)?;
    simu.step()?;
    t += Duration::new(0, 100_000_000);

    // The motor should start moving again, but since the phase was incremented
    // 3 times (out of 4 phases) while the motor was blocked, the motor actually
    // makes a step backward before it moves forward again.
    assert_eq!(simu.time(), t);
    pos = (pos + Motor::STEPS_PER_REV - 1) % Motor::STEPS_PER_REV;
    assert_eq!(position.next(), Some(pos));

    // Advance simulation time by 0.7s, which with a 10Hz PPS should correspond to
    // 7 position increments.
    simu.step_until(Duration::new(0, 700_000_000))?;
    t += Duration::new(0, 700_000_000);
    assert_eq!(simu.time(), t);
    for _ in 0..7 {
        pos = (pos + 1) % Motor::STEPS_PER_REV;
        assert_eq!(position.next(), Some(pos));
    }
    assert!(position.next().is_none());

    // Now make the motor rotate in the opposite direction. Note that this
    // driver only accounts for a new PPS at the next pulse.
    simu.process_event(Driver::pulse_rate, -10.0, &driver_addr)?;
    simu.step()?;
    t += Duration::new(0, 100_000_000);
    assert_eq!(simu.time(), t);
    pos = (pos + 1) % Motor::STEPS_PER_REV;
    assert_eq!(position.next(), Some(pos));

    // Advance simulation time by 1.9s, which with a -10Hz PPS should correspond
    // to 19 position decrements.
    simu.step_until(Duration::new(1, 900_000_000))?;
    t += Duration::new(1, 900_000_000);
    assert_eq!(simu.time(), t);
    pos = (pos + Motor::STEPS_PER_REV - 19) % Motor::STEPS_PER_REV;
    assert_eq!(position.by_ref().last(), Some(pos));

    Ok(())
}
