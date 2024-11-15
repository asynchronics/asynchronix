//! Example: processor with observable states.
//!
//! This example demonstrates in particular:
//!
//! * the use of observable states,
//! * state machine with delays.
//!
//! ```text
//!                     ┌───────────┐
//! Switch ON/OFF ●────►│           ├────► Mode
//!                     │ Processor │
//! Process data  ●────►│           ├────► Value
//!                     │           │
//!                     │           ├────► House Keeping
//!                     └───────────┘
//! ```

use std::time::Duration;

use asynchronix::model::{Context, InitializedModel, Model};
use asynchronix::ports::{EventBuffer, Output};
use asynchronix::simulation::{AutoActionKey, Mailbox, SimInit, SimulationError};
use asynchronix::time::MonotonicTime;
use asynchronix_util::observables::{Observable, ObservableState, ObservableValue};

/// House keeping TM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hk {
    pub voltage: f64,
    pub current: f64,
}

impl Default for Hk {
    fn default() -> Self {
        Self {
            voltage: 0.0,
            current: 0.0,
        }
    }
}

/// Processor mode ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeId {
    Off,
    Idle,
    Processing,
}

/// Processor state.
pub enum State {
    Off,
    Idle,
    Processing(AutoActionKey),
}

impl Default for State {
    fn default() -> Self {
        State::Off
    }
}

impl Observable<ModeId> for State {
    fn observe(&self) -> ModeId {
        match *self {
            State::Off => ModeId::Off,
            State::Idle => ModeId::Idle,
            State::Processing(_) => ModeId::Processing,
        }
    }
}

/// Processor model.
pub struct Processor {
    /// Mode output.
    pub mode: Output<ModeId>,

    /// Calculated value output.
    pub value: Output<u16>,

    /// HK output.
    pub hk: Output<Hk>,

    /// Internal state.
    state: ObservableState<State, ModeId>,

    /// Accumulator.
    acc: ObservableValue<u16>,

    /// Electrical data.
    elc: ObservableValue<Hk>,
}

impl Processor {
    /// Create a new processor.
    pub fn new() -> Self {
        let mode = Output::new();
        let value = Output::new();
        let hk = Output::new();
        Self {
            mode: mode.clone(),
            value: value.clone(),
            hk: hk.clone(),
            state: ObservableState::new(mode),
            acc: ObservableValue::new(value),
            elc: ObservableValue::new(hk),
        }
    }

    /// Switch processor ON/OFF.
    pub async fn switch_power(&mut self, on: bool) {
        if on {
            self.state.set(State::Idle).await;
            self.elc
                .set(Hk {
                    voltage: 5.5,
                    current: 0.1,
                })
                .await;
            self.acc.set(0).await;
        } else {
            self.state.set(State::Off).await;
            self.elc.set(Hk::default()).await;
            self.acc.set(0).await;
        }
    }

    /// Process data for dt milliseconds.
    pub async fn process(&mut self, dt: u64, cx: &mut Context<Self>) {
        if matches!(self.state.observe(), ModeId::Idle | ModeId::Processing) {
            self.state
                .set(State::Processing(
                    cx.schedule_keyed_event(Duration::from_millis(dt), Self::finish_processing, ())
                        .unwrap()
                        .into_auto(),
                ))
                .await;
            self.elc.modify(|hk| hk.current = 1.0).await;
        }
    }

    /// Finish processing.
    async fn finish_processing(&mut self) {
        self.state.set(State::Idle).await;
        self.acc.modify(|a| *a += 1).await;
        self.elc.modify(|hk| hk.current = 0.1).await;
    }
}

impl Model for Processor {
    /// Propagate all internal states.
    async fn init(mut self, _: &mut Context<Self>) -> InitializedModel<Self> {
        self.state.propagate().await;
        self.acc.propagate().await;
        self.elc.propagate().await;
        self.into()
    }
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.
    let mut proc = Processor::new();

    // Mailboxes.
    let proc_mbox = Mailbox::new();

    // Model handles for simulation.
    let mut mode = EventBuffer::new();
    let mut value = EventBuffer::new();
    let mut hk = EventBuffer::new();

    proc.mode.connect_sink(&mode);
    proc.value.connect_sink(&value);
    proc.hk.connect_sink(&hk);
    let proc_addr = proc_mbox.address();

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let mut simu = SimInit::new()
        .add_model(proc, proc_mbox, "proc")
        .init(t0)?
        .0;

    // ----------
    // Simulation.
    // ----------

    // Initial state.
    expect(
        &mut mode,
        Some(ModeId::Off),
        &mut value,
        Some(0),
        &mut hk,
        0.0,
        0.0,
    );

    // Switch processor on.
    simu.process_event(Processor::switch_power, true, &proc_addr)?;
    expect(
        &mut mode,
        Some(ModeId::Idle),
        &mut value,
        Some(0),
        &mut hk,
        5.5,
        0.1,
    );

    // Trigger processing.
    simu.process_event(Processor::process, 100, &proc_addr)?;
    expect(
        &mut mode,
        Some(ModeId::Processing),
        &mut value,
        None,
        &mut hk,
        5.5,
        1.0,
    );

    // All data processed.
    simu.step_until(Duration::from_millis(101))?;
    expect(
        &mut mode,
        Some(ModeId::Idle),
        &mut value,
        Some(1),
        &mut hk,
        5.5,
        0.1,
    );

    // Trigger long processing.
    simu.process_event(Processor::process, 100, &proc_addr)?;
    expect(
        &mut mode,
        Some(ModeId::Processing),
        &mut value,
        None,
        &mut hk,
        5.5,
        1.0,
    );

    // Trigger short processing, it cancels the previous one.
    simu.process_event(Processor::process, 10, &proc_addr)?;
    expect(
        &mut mode,
        Some(ModeId::Processing),
        &mut value,
        None,
        &mut hk,
        5.5,
        1.0,
    );

    // Wait for short processing to finish, check results.
    simu.step_until(Duration::from_millis(11))?;
    expect(
        &mut mode,
        Some(ModeId::Idle),
        &mut value,
        Some(2),
        &mut hk,
        5.5,
        0.1,
    );

    // Wait long enough, no state change as the long processing has been
    // cancelled.
    simu.step_until(Duration::from_millis(100))?;
    assert_eq!(mode.next(), None);
    assert_eq!(value.next(), None);
    assert_eq!(hk.next(), None);

    Ok(())
}

// Check observable state.
fn expect(
    mode: &mut EventBuffer<ModeId>,
    mode_ex: Option<ModeId>,
    value: &mut EventBuffer<u16>,
    value_ex: Option<u16>,
    hk: &mut EventBuffer<Hk>,
    voltage_ex: f64,
    current_ex: f64,
) {
    assert_eq!(mode.next(), mode_ex);
    assert_eq!(value.next(), value_ex);
    let hk_value = hk.next().unwrap();
    assert!(same(hk_value.voltage, voltage_ex));
    assert!(same(hk_value.current, current_ex));
}

// Compare two voltages or currents.
fn same(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}
