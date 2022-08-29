use std::fmt;
use std::str::{self, FromStr};

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::sync::mpsc;
use tokio_serial::SerialPortBuilderExt;
use tokio_serial::SerialStream;
use tracing::{info, trace};

pub type Error = Box<dyn std::error::Error>;
pub type Result<T> = std::result::Result<T, Error>;

const LINE_BUFFER_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

impl fmt::Display for LineEnding {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LineEnding::Lf => write!(f, "\n"),
            LineEnding::CrLf => write!(f, "\r\n"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Wind River VxWorks
    VxWorks,
    /// Green Hills Integrity
    Integrity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Read { addr: u32 },
    Write { addr: u32, data: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Read { addr: u32, data: u32 },
    Write { addr: u32, data: u32 },
}

// UART Debug Access Port
pub struct UartDap {
    port: SerialStream,
    echo: Echo,
    line_ending: LineEnding,
}

impl UartDap {
    pub fn new(path: &str, baud_rate: u32, echo: Echo, line_ending: LineEnding) -> Result<Self> {
        let port = tokio_serial::new(path, baud_rate).open_native_async()?;

        Ok(Self {
            port,
            echo,
            line_ending,
        })
    }

    pub async fn run(
        self,
        app_command_rx: mpsc::Receiver<Command>,
        serial_event_tx: mpsc::Sender<Event>,
    ) -> Result<()> {
        let (mut serial_rx, serial_tx) = tokio::io::split(self.port);

        let (command_echo_tx, mut command_echo_rx) = mpsc::channel(1);
        let (command_serial_tx, command_serial_rx) = mpsc::channel(1);

        let prompt = "DEBUG>";

        tokio::select! {
            result = command_input_splitter(app_command_rx, command_echo_tx, command_serial_tx, self.echo) => result,
            result = process_serial_tx(self.line_ending, command_serial_rx, serial_tx) => result,
            result = serial_combiner(prompt, self.line_ending, &mut command_echo_rx, &mut serial_rx, serial_event_tx) => result,
        }?;

        Ok(())
    }
}

impl Command {
    pub fn from_tokens(tokens: &[&str]) -> Option<Self> {
        match tokens {
            ["mr", "kernel", addr] => {
                let addr = parse_based_int(&addr).ok()?;
                Some(Self::Read { addr })
            }
            ["mw", "kernel", addr, data] => {
                let addr = parse_based_int(&addr).ok()?;
                let data = parse_based_int(&data).ok()?;
                Some(Self::Write { addr, data })
            }
            _ => None,
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Read { addr } => write!(f, "mr kernel {addr:#x}"),
            Self::Write { addr, data } => write!(f, "mw kernel {addr:#x} {data:#x}"),
        }
    }
}

#[tracing::instrument(skip_all)]
async fn command_input_splitter(
    mut app_command_rx: mpsc::Receiver<Command>,
    command_echo_tx: mpsc::Sender<Command>,
    command_serial_tx: mpsc::Sender<Command>,
    echo: Echo,
) -> Result<()> {
    info!("started");

    while let Some(command) = app_command_rx.recv().await {
        info!(?command);
        if echo == Echo::Local {
            command_echo_tx.send(command).await?;
        }
        command_serial_tx.send(command).await?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn process_serial_tx(
    line_ending: LineEnding,
    mut command_serial_rx: mpsc::Receiver<Command>,
    mut serial_tx: WriteHalf<SerialStream>,
) -> Result<()> {
    info!("started");

    while let Some(command) = command_serial_rx.recv().await {
        info!(?command);
        serial_tx
            .write_all(command.to_string().as_bytes())
            .await
            .map_err(|_| "could not send")?;
        serial_tx
            .write_all(line_ending.to_string().as_bytes())
            .await
            .map_err(|_| "could not send")?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum BufferState {
    WaitForCommand,
    WaitForResponse(Command),
}

#[tracing::instrument(skip_all)]
async fn serial_combiner(
    prompt: &str,
    line_ending: LineEnding,
    command_echo_rx: &mut mpsc::Receiver<Command>,
    mut serial_rx: impl AsyncRead + Unpin,
    mut event_tx: mpsc::Sender<Event>,
) -> Result<()> {
    let mut state = BufferState::WaitForCommand;
    let mut line_buffer = BytesMut::with_capacity(LINE_BUFFER_SIZE);

    info!("started");

    loop {
        tokio::select! {
            result = command_echo_rx.recv() => {
                let command = result.ok_or_else(|| "channel closed")?;
                let message = format!("{}{}", command, line_ending);
                line_buffer.put_slice(&message.as_bytes());
                Result::<()>::Ok(())
            }
            result = serial_rx.read_buf(&mut line_buffer) => {
                result.map_err(|_| "failed to read from serial port")?;
                Result::<()>::Ok(())
            }
        }?;

        trace!(line = str::from_utf8(&line_buffer)?);

        // NOTE: Assumes IO is line based
        if let Some(b'\n') = line_buffer.last() {
            let line = str::from_utf8(&line_buffer)?.trim();
            state = process_line(prompt, state, line, &mut event_tx).await?;
            line_buffer.clear();
        }
    }
}

#[tracing::instrument(skip_all)]
async fn process_line(
    prompt: &str,
    state: BufferState,
    line: &str,
    event_tx: &mut mpsc::Sender<Event>,
) -> Result<BufferState> {
    info!(?state, ?line);
    match state {
        BufferState::WaitForCommand => {
            let tokens = line.split_ascii_whitespace().collect::<Vec<_>>();
            match tokens.split_at(1) {
                (first, user_tokens) if first == [prompt] => {
                    if let Some(command) = Command::from_tokens(&user_tokens) {
                        match command {
                            Command::Write { addr, data } => {
                                let event = Event::Write { addr, data };
                                info!(?event);
                                event_tx.send(event).await?;
                            }
                            Command::Read { addr: _ } => {
                                return Ok(BufferState::WaitForResponse(command));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        BufferState::WaitForResponse(command) => {
            if let Command::Read { addr } = command {
                let data = parse_based_int(&line)?;
                let event = Event::Read { addr, data };
                info!(?event);
                event_tx.send(event).await?;
            }

            return Ok(BufferState::WaitForCommand);
        }
    }

    Ok(state)
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
