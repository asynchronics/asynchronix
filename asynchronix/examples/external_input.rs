//! Example: a model that reads data from the external world.
//!
//! This example demonstrates in particular:
//!
//! * external world inputs (useful in cosimulation),
//! * system clock,
//! * periodic scheduling.
//!
//! ```text
//!                                                  ┌────────────────────────────────┐
//!                                                  │ Simulation                     │
//! ┌────────────┐          ┌────────────┐           │          ┌──────────┐          │
//! │            │   UDP    │            │ message   │ message  │          │ message  │   ┌─────────────┐
//! │ UDP Client ├─────────►│ UDP Server ├──────────►├─────────►│ Listener ├─────────►├──►│ EventBuffer │
//! │            │ message  │            │           │          │          │          │   └─────────────┘
//! └────────────┘          └────────────┘           │          └──────────┘          │
//!                                                  └────────────────────────────────┘
//! ```

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, sleep, JoinHandle};
use std::time::Duration;

use atomic_wait::{wait, wake_one};

use mio::net::UdpSocket as MioUdpSocket;
use mio::{Events, Interest, Poll, Token};

use asynchronix::model::{Context, InitializedModel, Model, SetupContext};
use asynchronix::ports::{EventBuffer, Output};
use asynchronix::simulation::{Mailbox, SimInit, SimulationError};
use asynchronix::time::{AutoSystemClock, MonotonicTime};

const DELTA: Duration = Duration::from_millis(2);
const PERIOD: Duration = Duration::from_millis(20);
const N: u32 = 10;
const SENDER: &str = "127.0.0.1:8000";
const RECEIVER: &str = "127.0.0.1:9000";

/// Model that receives external input.
pub struct Listener {
    /// Received message.
    pub message: Output<String>,

    /// Receiver of external messages.
    rx: Receiver<String>,

    /// External sender.
    tx: Option<Sender<String>>,

    /// Synchronization with client.
    start: Arc<AtomicU32>,

    /// Synchronization with simulation.
    stop: Arc<AtomicBool>,

    /// Handle to UDP Server.
    external_handle: Option<JoinHandle<()>>,
}

impl Listener {
    /// Creates a Listener.
    pub fn new(start: Arc<AtomicU32>) -> Self {
        start.store(0, Ordering::Relaxed);

        let (tx, rx) = channel();
        Self {
            message: Output::default(),
            rx,
            tx: Some(tx),
            start,
            stop: Arc::new(AtomicBool::new(false)),
            external_handle: None,
        }
    }

    /// Periodically scheduled function that processes external events.
    async fn process(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.message.send(message).await;
        }
    }

    /// UDP server.
    ///
    /// Code is based on the MIO UDP example.
    fn listener(tx: Sender<String>, start: Arc<AtomicU32>, stop: Arc<AtomicBool>) {
        const UDP_SOCKET: Token = Token(0);
        let mut poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(10);
        let mut socket = MioUdpSocket::bind(RECEIVER.parse().unwrap()).unwrap();
        poll.registry()
            .register(&mut socket, UDP_SOCKET, Interest::READABLE)
            .unwrap();
        let mut buf = [0; 1 << 16];

        // Wake up the client.
        start.store(1, Ordering::Relaxed);
        wake_one(&*start);

        'process: loop {
            // Wait for UDP packet or end of simulation.
            if let Err(err) = poll.poll(&mut events, Some(Duration::from_secs(1))) {
                if err.kind() == ErrorKind::Interrupted {
                    // Exit if simulation is finished.
                    if stop.load(Ordering::Relaxed) {
                        break 'process;
                    }
                    continue;
                }
                break 'process;
            }

            for event in events.iter() {
                match event.token() {
                    UDP_SOCKET => loop {
                        match socket.recv_from(&mut buf) {
                            Ok((packet_size, _)) => {
                                if let Ok(message) = std::str::from_utf8(&buf[..packet_size]) {
                                    // Inject external message into simulation.
                                    if tx.send(message.into()).is_err() {
                                        break 'process;
                                    }
                                };
                            }
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                break;
                            }
                            _ => {
                                break 'process;
                            }
                        }
                    },
                    _ => {
                        panic!("Got event for unexpected token: {:?}", event);
                    }
                }
            }
            // Exit if simulation is finished.
            if stop.load(Ordering::Relaxed) {
                break 'process;
            }
        }

        poll.registry().deregister(&mut socket).unwrap();
    }
}

impl Model for Listener {
    /// Start UDP Server on model setup.
    fn setup(&mut self, _: &SetupContext<Self>) {
        let tx = self.tx.take().unwrap();
        let start = Arc::clone(&self.start);
        let stop = Arc::clone(&self.stop);
        self.external_handle = Some(thread::spawn(move || {
            Self::listener(tx, start, stop);
        }));
    }

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
    /// Notify UDP Server that simulation is over and wait for server shutdown.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let handle = self.external_handle.take();
        if let Some(handle) = handle {
            handle.join().unwrap();
        }
    }
}

fn main() -> Result<(), SimulationError> {
    // ---------------
    // Bench assembly.
    // ---------------

    // Models.

    // Client-server synchronization.
    let start = Arc::new(AtomicU32::new(0));

    let mut listener = Listener::new(Arc::clone(&start));

    // Mailboxes.
    let listener_mbox = Mailbox::new();

    // Model handles for simulation.
    let mut message = EventBuffer::new();
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
        // Wait until UDP Server is ready.
        wait(&start, 0);

        for i in 0..N {
            let socket = UdpSocket::bind(SENDER).unwrap();
            socket.send_to(i.to_string().as_bytes(), RECEIVER).unwrap();
            if i % 3 == 0 {
                sleep(PERIOD * i)
            }
        }
    });

    // Advance simulation, external messages will be collected.
    simu.step_by(Duration::from_secs(2))?;

    // Check collected external messages.
    let mut packets = 0_u32;
    for _ in 0..N {
        // UDP can reorder packages, we are expecting that on not too loaded
        // localhost packages would not be dropped
        packets |= 1 << message.next().unwrap().parse::<u8>().unwrap();
    }
    assert_eq!(packets, u32::MAX >> 22);
    assert_eq!(message.next(), None);

    sender_handle.join().unwrap();

    Ok(())
}
