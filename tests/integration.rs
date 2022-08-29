// Based on: https://github.com/berkowski/tokio-serial/blob/master/tests/test_serialstream.rs

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{process, sync::mpsc, time};
use tokio_serial::SerialPortBuilderExt;
use tracing::{info, trace};
use tracing_subscriber;

use uart_dap::{Command, Echo, Event, LineEnding, UartDap};

#[cfg(unix)]
const DEFAULT_TEST_PORT_NAMES: &str = concat!(
    env!("CARGO_TARGET_TMPDIR"),
    "/ttyUSB0;",
    env!("CARGO_TARGET_TMPDIR"),
    "/ttyUSB1"
);

struct Fixture {
    #[cfg(unix)]
    process: process::Child,
    pub port_a: &'static str,
    pub port_b: &'static str,
}

#[cfg(unix)]
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(id) = self.process.id() {
            trace!("stopping socat process (id: {})...", id);

            self.process.start_kill().ok();
            std::thread::sleep(Duration::from_millis(250));
            trace!("removing link: {}", self.port_a);
            std::fs::remove_file(self.port_a).ok();
            trace!("removing link: {}", self.port_b);
            std::fs::remove_file(self.port_b).ok();
        }
    }
}

impl Fixture {
    #[cfg(unix)]
    pub async fn new(port_a: &'static str, port_b: &'static str) -> Self {
        let args = [
            format!("PTY,link={}", port_a),
            format!("PTY,link={}", port_b),
        ];
        trace!("starting process: socat {} {}", args[0], args[1]);

        let process = process::Command::new("socat")
            .args(&args)
            .spawn()
            .expect("unable to spawn socat process");
        trace!(".... done! (pid: {:?})", process.id().unwrap());

        time::sleep(Duration::from_millis(500)).await;

        Self {
            process,
            port_a,
            port_b,
        }
    }

    #[cfg(not(unix))]
    pub async fn new(port_a: &'static str, port_b: &'static str) -> Self {
        Self { port_a, port_b }
    }
}

async fn setup_virtual_serial_ports() -> Fixture {
    let port_names: Vec<&str> = std::option_env!("TEST_PORT_NAMES")
        .unwrap_or(DEFAULT_TEST_PORT_NAMES)
        .split(';')
        .collect();

    assert_eq!(port_names.len(), 2);
    Fixture::new(port_names[0], port_names[1]).await
}

#[tokio::test]
async fn performs_write_command() {
    let _ = tracing_subscriber::fmt::try_init();

    let fixture = setup_virtual_serial_ports().await;

    let dap = UartDap::new(fixture.port_a, 115200, Echo::Local, LineEnding::Lf).unwrap();
    let model = tokio_serial::new(fixture.port_b, 115200)
        .open_native_async()
        .unwrap();
    let (mut model_rx, mut model_tx) = tokio::io::split(model);

    let (command_tx, command_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(1);

    let join_handle = tokio::spawn(async move { dap.run(command_rx, event_tx).await.unwrap() });

    info!("Sending serial prompt");
    model_tx.write_all(b"DEBUG> ").await.unwrap();
    time::sleep(Duration::from_millis(500)).await;

    let command = Command::Write {
        addr: 0x600df00d,
        data: 0xa5a5a5a5,
    };
    info!("Sending command");
    command_tx.send(command).await.unwrap();

    info!("Awaiting event");
    assert_eq!(
        event_rx.recv().await.unwrap(),
        Event::Write {
            addr: 0x600df00d,
            data: 0xa5a5a5a5
        }
    );
    let mut buf = [0u8; 32];
    info!("Awaiting serial");
    let n = model_rx.read(&mut buf).await.unwrap();
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        "mw kernel 0x600df00d 0xa5a5a5a5\n"
    );

    if join_handle.is_finished() {
        join_handle.await.unwrap();
    } else {
        join_handle.abort();
    }
}

#[tokio::test]
async fn performs_read_command() {
    let _ = tracing_subscriber::fmt::try_init();

    let fixture = setup_virtual_serial_ports().await;

    let dap = UartDap::new(fixture.port_a, 115200, Echo::Local, LineEnding::Lf).unwrap();
    let model = tokio_serial::new(fixture.port_b, 115200)
        .open_native_async()
        .unwrap();
    let (mut model_rx, mut model_tx) = tokio::io::split(model);

    let (command_tx, command_rx) = mpsc::channel(1);
    let (event_tx, mut event_rx) = mpsc::channel(1);

    let join_handle = tokio::spawn(async move { dap.run(command_rx, event_tx).await.unwrap() });

    info!("Sending serial prompt");
    model_tx.write_all(b"DEBUG> ").await.unwrap();
    time::sleep(Duration::from_millis(500)).await;

    let command = Command::Read {
        addr: 0x600df00d,
        nbytes: 20,
    };
    info!("Sending command");
    command_tx.send(command).await.unwrap();

    let mut buf = [0u8; 32];
    info!("Awaiting serial");
    let n = model_rx.read(&mut buf).await.unwrap();
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        "mr kernel 0x600df00d 20\n"
    );

    model_tx
        .write_all(b"600df00d: 5a 5a 5a 5a  01 02 03 04  05 06 07 08  09 0a 0b 0c |-------|\n")
        .await
        .unwrap();
    info!("Awaiting events");
    assert_eq!(
        event_rx.recv().await.unwrap(),
        Event::Read {
            addr: 0x600df00d,
            data: 0x5a5a5a5a,
        }
    );
    assert_eq!(
        event_rx.recv().await.unwrap(),
        Event::Read {
            addr: 0x600df011,
            data: 0x04030201,
        }
    );
    assert_eq!(
        event_rx.recv().await.unwrap(),
        Event::Read {
            addr: 0x600df015,
            data: 0x08070605,
        }
    );
    assert_eq!(
        event_rx.recv().await.unwrap(),
        Event::Read {
            addr: 0x600df019,
            data: 0x0c0b0a09,
        }
    );
    model_tx
        .write_all(b"600df01d: 0d 0e 0f 10                                        |-------|\n")
        .await
        .unwrap();
    assert_eq!(
        event_rx.recv().await.unwrap(),
        Event::Read {
            addr: 0x600df01d,
            data: 0x100f0e0d
        }
    );

    if join_handle.is_finished() {
        join_handle.await.unwrap();
    } else {
        join_handle.abort();
    }
}
