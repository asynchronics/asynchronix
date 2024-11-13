//! Example: a model that reads data external to the simulation.
//!
//! This example demonstrates in particular:
//!
//! * processing of external inputs (useful in co-simulation),
//! * system clock,
//! * periodic scheduling.
//!
//! ```text
//!                                                 ┏━━━━━━━━━━━━━━━━━━━━━━━━┓
//!                                                 ┃ Simulation             ┃
//! ┌╌╌╌╌╌╌╌╌╌╌╌╌┐         ┌╌╌╌╌╌╌╌╌╌╌╌╌┐           ┃   ┌──────────┐         ┃
//! ┆            ┆ message ┆            ┆  message  ┃   │          │ message ┃
//! ┆ UDP Client ├╌╌╌╌╌╌╌╌►┆ UDP Server ├╌╌╌╌╌╌╌╌╌╌╌╂╌╌►│ Listener ├─────────╂─►
//! ┆            ┆  [UDP]  ┆            ┆ [channel] ┃   │          │         ┃
//! └╌╌╌╌╌╌╌╌╌╌╌╌┘         └╌╌╌╌╌╌╌╌╌╌╌╌┘           ┃   └──────────┘         ┃
//!                                                 ┗━━━━━━━━━━━━━━━━━━━━━━━━┛
//! ```

use std::io::ErrorKind;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;

use asynchronix::model::{BuildContext, Context, InitializedModel, Model, ProtoModel};
use asynchronix::ports::{EventBuffer, Output};
use asynchronix::simulation::{Mailbox, SimInit, SimulationError};
use asynchronix::time::{AutoSystemClock, MonotonicTime};

const DELTA: Duration = Duration::from_millis(2);
const PERIOD: Duration = Duration::from_millis(20);
const N: usize = 10;
const SHUTDOWN_SIGNAL: &str = "<SHUTDOWN>";
const SENDER: (Ipv4Addr, u16) = (Ipv4Addr::new(127, 0, 0, 1), 8000);
const RECEIVER: (Ipv4Addr, u16) = (Ipv4Addr::new(127, 0, 0, 1), 9000);

/// Prototype for the `Listener` Model.
pub struct ProtoListener {
    /// Received message.
    pub message: Output<String>,

    /// Notifier to start the UDP client.
    start: Notifier,
}

impl ProtoListener {
    fn new(start: Notifier) -> Self {
        Self {
            message: Output::default(),
            start,
        }
    }
}

impl ProtoModel for ProtoListener {
    type Model = Listener;

    /// Start the UDP Server immediately upon model construction.
    fn build(self, _: &mut BuildContext<Self>) -> Listener {
        let (tx, rx) = channel();

        let external_handle = thread::spawn(move || {
            Listener::listen(tx, self.start);
        });

        Listener::new(self.message, rx, external_handle)
    }
}

/// Model that asynchronously receives messages external to the simulation.
pub struct Listener {
    /// Received message.
    message: Output<String>,

    /// Receiver of external messages.
    rx: Receiver<String>,

    /// Handle to UDP Server.
    server_handle: Option<JoinHandle<()>>,
}

impl Listener {
    /// Creates a Listener.
    pub fn new(
        message: Output<String>,
        rx: Receiver<String>,
        server_handle: JoinHandle<()>,
    ) -> Self {
        Self {
            message,
            rx,
            server_handle: Some(server_handle),
        }
    }

    /// Periodically scheduled function that processes external events.
    async fn process(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.message.send(message).await;
        }
    }

    /// Starts the UDP server.
    fn listen(tx: Sender<String>, start: Notifier) {
        let socket = UdpSocket::bind(RECEIVER).unwrap();
        let mut buf = [0; 1 << 16];

        // Wake up the client.
        start.notify();

        loop {
            match socket.recv_from(&mut buf) {
                Ok((packet_size, _)) => {
                    if let Ok(message) = std::str::from_utf8(&buf[..packet_size]) {
                        if message == SHUTDOWN_SIGNAL {
                            break;
                        }
                        // Inject external message into simulation.
                        if tx.send(message.into()).is_err() {
                            break;
                        }
                    };
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {
                    continue;
                }
                _ => {
                    break;
                }
            }
        }
    }
}

impl Model for Listener {
    /// Initialize model.
    async fn init(self, context: &Context<Self>) -> InitializedModel<Self> {
        // Schedule periodic function that processes external events.
        context
            .scheduler
            .schedule_periodic_event(DELTA, PERIOD, Listener::process, ())
            .unwrap();

        self.into()
    }
}

impl Drop for Listener {
    /// Wait for UDP Server shutdown.
    fn drop(&mut self) {
        self.server_handle.take().map(|handle| {
            let _ = handle.join();
        });
    }
}

/// A synchronization barrier that can be unblocked by a notifier.
struct WaitBarrier(Arc<(Mutex<bool>, Condvar)>);

impl WaitBarrier {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }
    fn notifier(&self) -> Notifier {
        Notifier(self.0.clone())
    }
    fn wait(self) {
        let _unused = self
            .0
             .1
            .wait_while(self.0 .0.lock().unwrap(), |pending| *pending)
            .unwrap();
    }
}

/// A notifier for the associated synchronization barrier.
struct Notifier(Arc<(Mutex<bool>, Condvar)>);

impl Notifier {
    fn notify(self) {
        *self.0 .0.lock().unwrap() = false;
        self.0 .1.notify_one();
    }
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.

    // Synchronization barrier for the UDP client.
    let start = WaitBarrier::new();

    // Prototype of the listener model.
    let mut listener = ProtoListener::new(start.notifier());

    // Mailboxes.
    let listener_mbox = Mailbox::new();

    // Model handles for simulation.
    let mut message = EventBuffer::with_capacity(N + 1);
    listener.message.connect_sink(&message);

    // Start time (arbitrary since models do not depend on absolute time).
    let t0 = MonotonicTime::EPOCH;

    // Assembly and initialization.
    let mut simu = SimInit::new()
        .add_model(listener, listener_mbox, "listener")
        .set_clock(AutoSystemClock::new())
        .init(t0)?;

    // ----------
    // Simulation.
    // ----------

    // External client that sends UDP messages.
    let sender_handle = thread::spawn(move || {
        let socket = UdpSocket::bind(SENDER).unwrap();

        // Wait until the UDP Server is ready.
        start.wait();

        for i in 0..N {
            socket.send_to(i.to_string().as_bytes(), RECEIVER).unwrap();
            if i % 3 == 0 {
                sleep(PERIOD * i as u32)
            }
        }

        socket
    });

    // Advance simulation, external messages will be collected.
    simu.step_by(Duration::from_secs(2))?;

    // Shut down the server.
    let socket = sender_handle.join().unwrap();
    socket
        .send_to(SHUTDOWN_SIGNAL.as_bytes(), RECEIVER)
        .unwrap();

    // Check collected external messages.
    let mut packets = 0_u32;
    for _ in 0..N {
        // Check all messages accounting for possible UDP packet re-ordering,
        // but assuming no packet loss.
        packets |= 1 << message.next().unwrap().parse::<u8>().unwrap();
    }
    assert_eq!(packets, u32::MAX >> 22);
    assert_eq!(message.next(), None);

    Ok(())
}
