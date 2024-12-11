//! Example: RIU acquiring data from sensor.
//!
//! This example demonstrates in particular:
//!
//! * the use of replier port adaptor,
//! * periodic model self-scheduling.
//!
//! ```text
//!                       ┌────────┐                ┌─────────┐  Sensor TC  ┌─────┐
//! Set temperature ●────►│        │  ◄Sensor TC    │         │◄────────────┤     │
//!                       │ Sensor │◄►────────────►◄│ Adaptor │  Sensor TM  │ RIU ├────► RIU TM
//! Set illuminance ●────►│        │   Sensor TM►   │         ├────────────►│     │
//!                       └────────┘                └─────────┘             └─────┘
//! ```

use std::fmt::Debug;
use std::time::Duration;

use nexosim::model::{Context, InitializedModel, Model};
use nexosim::ports::{EventBuffer, Output};
use nexosim::simulation::{Mailbox, SimInit, SimulationError};
use nexosim::time::MonotonicTime;
use nexosim_util::combinators::ReplierAdaptor;

const DELTA: Duration = Duration::from_millis(2);
const PERIOD: Duration = Duration::from_secs(1);

/// Sensor TC.
#[derive(Clone, Debug, PartialEq)]
pub enum SensorTc {
    GetTemp,
    GetIllum,
}

/// Sensor TM.
#[derive(Clone, Debug, PartialEq)]
pub enum SensorTm {
    Temp(f64),
    Illum(f64),
}

/// Sensor model.
pub struct Sensor {
    /// Temperature [deg C] -- internal state.
    temp: f64,

    /// Illuminance [lx] -- internal state.
    illum: f64,
}

impl Sensor {
    /// Creates a sensor model.
    pub fn new() -> Self {
        Self {
            temp: 0.0,
            illum: 0.0,
        }
    }

    /// Sets sensor temperature [deg C].
    pub async fn set_temp(&mut self, temp: f64) {
        self.temp = temp;
    }

    /// Sets sensor illuminance [lx].
    pub async fn set_illum(&mut self, illum: f64) {
        self.illum = illum;
    }

    /// Processes sensor TC -- input port.
    pub async fn process_tc(&mut self, tc: SensorTc) -> SensorTm {
        match tc {
            SensorTc::GetTemp => SensorTm::Temp(self.temp),
            SensorTc::GetIllum => SensorTm::Illum(self.illum),
        }
    }
}

impl Model for Sensor {}

/// Internal TM field.
#[derive(Clone, Debug, PartialEq)]
pub struct TmField<T>
where
    T: Clone + Debug + PartialEq,
{
    /// TM value.
    pub value: T,

    /// TM readiness flag.
    pub ready: bool,
}

/// RIU TM.
#[derive(Clone, Debug, PartialEq)]
pub struct RiuTm {
    /// Temperature [deg C].
    temp: f64,

    /// Iluminance [lx].
    illum: f64,
}

/// RIU model.
pub struct Riu {
    /// Sensor TC -- output port.
    pub sensor_tc: Output<SensorTc>,

    /// RIU TM -- output port.
    pub tm: Output<RiuTm>,

    /// Temperature [deg C] -- internal state.
    temp: TmField<f64>,

    /// Illuminance [lx] -- internal state.
    illum: TmField<f64>,
}

impl Riu {
    /// Creates an RIU model.
    pub fn new() -> Self {
        Self {
            sensor_tc: Output::new(),
            tm: Output::new(),
            temp: TmField {
                value: 0.0,
                ready: true,
            },
            illum: TmField {
                value: 0.0,
                ready: true,
            },
        }
    }

    /// Processes sensor TM -- input port.
    pub async fn sensor_tm(&mut self, tm: SensorTm) {
        match tm {
            SensorTm::Temp(temp) => {
                self.temp = TmField {
                    value: temp,
                    ready: true,
                }
            }
            SensorTm::Illum(illum) => {
                self.illum = TmField {
                    value: illum,
                    ready: true,
                }
            }
        }

        if self.temp.ready && self.illum.ready {
            self.report().await
        }
    }

    /// Starts sensor TM acquisition -- periodic activity.
    async fn acquire(&mut self) {
        self.temp.ready = false;
        self.illum.ready = false;
        self.sensor_tc.send(SensorTc::GetTemp).await;
        self.sensor_tc.send(SensorTc::GetIllum).await
    }

    /// Reports RIU TM.
    async fn report(&mut self) {
        self.tm
            .send(RiuTm {
                temp: self.temp.value,
                illum: self.illum.value,
            })
            .await
    }
}

impl Model for Riu {
    /// Initializes model.
    async fn init(self, cx: &mut Context<Self>) -> InitializedModel<Self> {
        // Schedule periodic acquisition.
        cx.schedule_periodic_event(DELTA, PERIOD, Riu::acquire, ())
            .unwrap();

        self.into()
    }
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.
    let sensor = Sensor::new();
    let mut riu = Riu::new();
    let mut sensor_adaptor = ReplierAdaptor::new();

    // Mailboxes.
    let sensor_mbox = Mailbox::new();
    let riu_mbox = Mailbox::new();
    let sensor_adaptor_mbox = Mailbox::new();

    // Connections.
    riu.sensor_tc
        .connect(ReplierAdaptor::input, &sensor_adaptor_mbox);
    sensor_adaptor.output.connect(Riu::sensor_tm, &riu_mbox);
    sensor_adaptor
        .requestor
        .connect(Sensor::process_tc, &sensor_mbox);

    // Model handles for simulation.
    let mut tm = EventBuffer::new();
    let sensor_addr = sensor_mbox.address();

    riu.tm.connect_sink(&tm);

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let mut simu = SimInit::new()
        .add_model(sensor, sensor_mbox, "sensor")
        .add_model(riu, riu_mbox, "riu")
        .add_model(sensor_adaptor, sensor_adaptor_mbox, "sensor_adaptor")
        .init(t0)?
        .0;

    // ----------
    // Simulation.
    // ----------

    // Initial state: no RIU TM.
    assert_eq!(tm.next(), None);

    simu.step_until(Duration::from_millis(1200))?;

    // RIU TM generated.
    assert_eq!(
        tm.next(),
        Some(RiuTm {
            temp: 0.0,
            illum: 0.0
        })
    );

    // Consume all RIU TM generated so far.
    while tm.next().is_some() {}

    // Set temperature and wait for RIU TM.
    simu.process_event(Sensor::set_temp, 2.0, &sensor_addr)?;

    simu.step_until(Duration::from_millis(1000))?;

    assert_eq!(
        tm.next(),
        Some(RiuTm {
            temp: 2.0,
            illum: 0.0
        })
    );

    // Set illuminance and wait for RIU TM.
    simu.process_event(Sensor::set_illum, 3.0, &sensor_addr)?;

    simu.step_until(Duration::from_millis(1000))?;

    assert_eq!(
        tm.next(),
        Some(RiuTm {
            temp: 2.0,
            illum: 3.0
        })
    );

    Ok(())
}
