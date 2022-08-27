use std::fmt;
use std::str::FromStr;

use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_serial::SerialPortBuilderExt;
use tokio_serial::SerialStream;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec, LinesCodecError};
use tracing::{error, info};

pub type Error = Box<dyn std::error::Error>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    On,
    Off,
}

#[derive(Debug, Clone, Copy)]
pub enum Command {
    Read { addr: u32 },
    Write { addr: u32, data: u32 },
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Read { addr: u32, data: u32 },
    Write { addr: u32, data: u32 },
}

// UART Debug Access Port
pub struct UartDap {
    port: SerialStream,
    echo: Echo,
}

impl UartDap {
    pub fn new(
        path: &str,
        baud_rate: u32,
        echo: Echo,
    ) -> Result<Self> {
        let port = tokio_serial::new(path, baud_rate).open_native_async()?;

        Ok(Self {
            port,
            echo,
        })
    }

    pub async fn run(
        self,
        app_command_rx: mpsc::Receiver<Command>,
        serial_event_tx: mpsc::Sender<Event>,
    ) -> Result<()> {
        let (serial_rx_port, serial_tx_port) = tokio::io::split(self.port);
        let serial_writer = FramedWrite::new(serial_tx_port, LinesCodec::new());
        let serial_reader = FramedRead::new(serial_rx_port, LinesCodec::new());

        let (app_command_echo_tx, mut app_command_echo_rx) = mpsc::channel(1);
        let (app_command_serial_tx, app_command_serial_rx) = mpsc::channel(1);

        let (serial_forward_tx, mut serial_forward_rx) = mpsc::channel(1);

        tokio::select! {
            result = process_input(app_command_rx, app_command_echo_tx, app_command_serial_tx, self.echo) => result,
            result = process_serial_tx(app_command_serial_rx, serial_writer) => result,
            result = process_serial_rx(serial_reader, serial_forward_tx) => result,
            result = process_serial_buffer(&mut app_command_echo_rx, &mut serial_forward_rx, serial_event_tx) => result,
        }?;

        Ok(())
    }
}

impl FromStr for Command {
    type Err = Error;

    fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
        let (command, args) = s
            .split_once(char::is_whitespace)
            .ok_or_else(|| "unrecognized command")?;
        match command {
            "rd" => {
                let addr = parse_based_int(args)?;
                Ok(Self::Read { addr })
            }
            "wr" => {
                let (addr, data) = args
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| "expected 2 arguments")?;
                let addr = parse_based_int(&addr)?;
                let data = parse_based_int(&data)?;
                Ok(Self::Write { addr, data })
            }
            _ => Err("unrecognized command")?,
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Read { addr } => write!(f, "rd {addr}"),
            Self::Write { addr, data } => write!(f, "wr {addr} {data}"),
        }
    }
}

pub async fn process_input(
    mut app_command_rx: mpsc::Receiver<Command>,
    echo_tx: mpsc::Sender<Command>,
    serial_tx: mpsc::Sender<Command>,
    echo: Echo,
) -> Result<()> {
    while let Some(command) = app_command_rx.recv().await {
        if echo == Echo::On {
            echo_tx.send(command).await?;
        }
        serial_tx.send(command).await?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
pub async fn process_serial_tx(
    mut app_command_rx: mpsc::Receiver<Command>,
    mut writer: impl Sink<String> + Unpin,
) -> Result<()> {
    while let Some(command) = app_command_rx.recv().await {
        writer
            .send(command.to_string())
            .await
            .map_err(|_| Error::from("could not send"))?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
pub async fn process_serial_rx(
    mut reader: impl Stream<Item = core::result::Result<String, LinesCodecError>> + Unpin,
    serial_forward_tx: mpsc::Sender<String>,
) -> Result<()> {
    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                serial_forward_tx.send(line).await?;
            }
            Err(e) => {
                error!(?e);
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum BufferState {
    WaitForCommand,
    WaitForResponse(Command),
}

#[tracing::instrument(skip_all)]
pub async fn process_serial_buffer(
    app_command_rx: &mut mpsc::Receiver<Command>,
    serial_forward_rx: &mut mpsc::Receiver<String>,
    serial_event_tx: mpsc::Sender<Event>,
) -> Result<()> {
    let mut state = BufferState::WaitForCommand;

    loop {
        let line = tokio::select! {
            command = app_command_rx.recv() => command.ok_or_else(|| "channel closed")?.to_string(),
            line = serial_forward_rx.recv() => line.ok_or_else(|| "channel closed")?,
        };

        // Skip blank lines
        if line == "" {
            continue;
        }

        let command = Command::from_str(&line).ok();

        let next_state = if let BufferState::WaitForResponse(_) = state {
            BufferState::WaitForCommand
        } else if let Some(Command::Read { .. }) = command {
            BufferState::WaitForResponse(command.unwrap())
        } else {
            state.clone()
        };

        info!(?state, ?next_state, ?line);

        if let BufferState::WaitForResponse(Command::Read { addr }) = state {
            let data = parse_based_int(&line)?;
            serial_event_tx.send(Event::Read { addr, data }).await?;
        } else if let Some(Command::Write { addr, data }) = command {
            serial_event_tx.send(Event::Write { addr, data }).await?;
        }

        state = next_state;
    }
}

fn parse_based_int(s: &str) -> Result<u32> {
    if s.starts_with("0x") || s.starts_with("0X") {
        let (_prefix, value) = s.split_at(2);
        Ok(u32::from_str_radix(value, 16)?)
    } else if s.starts_with("0b") || s.starts_with("0B") {
        let (_prefix, value) = s.split_at(2);
        Ok(u32::from_str_radix(value, 2)?)
    } else {
        Ok(u32::from_str(s)?)
    }
}
