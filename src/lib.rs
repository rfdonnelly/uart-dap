use std::fmt;
use std::num::ParseIntError;
use std::ops::Deref;
use std::str::{self, FromStr};

use bytes::{BufMut, BytesMut};
use if_chain::if_chain;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::sync::mpsc;
use tokio_serial::SerialPortBuilderExt;
use tokio_serial::SerialStream;
use tracing::{info, trace};

pub type Error = Box<dyn std::error::Error>;
pub type Result<T> = std::result::Result<T, Error>;

const LINE_BUFFER_SIZE: usize = 4096;
const MAX_BYTES_PER_LINE: u32 = 16;
const READ_DEFAULT_NBYTES: u32 = 16;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Read { addr: u32, nbytes: u32 },
    Write { addr: u32, data: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
            result = command_splitter(app_command_rx, command_echo_tx, command_serial_tx, self.echo) => result,
            result = serial_transmitter(self.line_ending, command_serial_rx, serial_tx) => result,
            result = serial_combiner(prompt, self.line_ending, &mut command_echo_rx, &mut serial_rx, serial_event_tx) => result,
        }?;

        Ok(())
    }
}

impl Command {
    pub fn from_tokens(tokens: &[&str]) -> Option<Self> {
        match tokens {
            ["mr", "kernel", addr, nbytes] => {
                let addr = parse_based_int(&addr).ok()?;
                let nbytes = parse_based_int(&nbytes).ok()?;
                Some(Self::Read { addr, nbytes })
            }
            ["mr", "kernel", addr] => {
                let addr = parse_based_int(&addr).ok()?;
                Some(Self::Read {
                    addr,
                    nbytes: READ_DEFAULT_NBYTES,
                })
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
            Self::Read { addr, nbytes } => write!(f, "mr kernel {addr:#x} {nbytes}"),
            Self::Write { addr, data } => write!(f, "mw kernel {addr:#x} {data:#x}"),
        }
    }
}

#[tracing::instrument(skip_all)]
async fn command_splitter(
    mut app_command_rx: mpsc::Receiver<Command>,
    command_echo_tx: mpsc::Sender<Command>,
    command_serial_tx: mpsc::Sender<Command>,
    echo: Echo,
) -> Result<()> {
    while let Some(command) = app_command_rx.recv().await {
        info!(?command, ?echo, "Received command");
        if echo == Echo::Local {
            command_echo_tx.send(command).await?;
        }
        command_serial_tx.send(command).await?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn serial_transmitter(
    line_ending: LineEnding,
    mut command_serial_rx: mpsc::Receiver<Command>,
    mut serial_tx: WriteHalf<SerialStream>,
) -> Result<()> {
    while let Some(command) = command_serial_rx.recv().await {
        info!(
            data = format!("{command}{line_ending}").as_str(),
            "Transmitting serial"
        );
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

    loop {
        tokio::select! {
            result = command_echo_rx.recv() => {
                let command = result.ok_or_else(|| "channel closed")?;
                let message = format!("{}{}", command, line_ending);
                line_buffer.put_slice(&message.as_bytes());
                info!(?line_buffer, "Received command");
                Result::<()>::Ok(())
            }
            result = serial_rx.read_buf(&mut line_buffer) => {
                result.map_err(|_| "failed to read from serial port")?;
                info!(?line_buffer, "Received serial");
                Result::<()>::Ok(())
            }
        }?;

        trace!(line_buffer = str::from_utf8(&line_buffer)?, "recevied data");

        if let Some(b'\n') = line_buffer.last() {
            let lines = line_buffer.deref().split(|b| b == &b'\n');
            for line in lines {
                let line = str::from_utf8(&line)?.trim();
                // TODO: remove this hack that accomodates for split with newline at end creating
                // an empty array
                if line != "" {
                    state = process_line(prompt, state, line, &mut event_tx).await?;
                }
            }
            line_buffer.clear();
        } else {
            // Case where we have some number of full lines and a partial line
            //
            // Process the full lines, clear the buffer, and copy the partial line back into the
            // buffer
            //
            // TODO: This could be optimized to prevent unnecessary data movement (e.g. circular buffer)
            let nlines = line_buffer.iter().filter(|&&b| b == b'\n').count();
            if nlines > 0 {
                let lines = line_buffer
                    .deref()
                    .split(|&b| b == b'\n')
                    .collect::<Vec<&[u8]>>();
                let (full_lines, partial_lines) = lines.split_at(lines.len() - 1);
                let partial_line = partial_lines
                    .first()
                    .unwrap()
                    .iter()
                    .copied()
                    .collect::<Vec<u8>>();
                for line in full_lines {
                    let line = str::from_utf8(&line)?.trim();
                    state = process_line(prompt, state, line, &mut event_tx).await?;
                }
                line_buffer.clear();
                line_buffer.put_slice(&partial_line);
            }
        }

        trace!(line = str::from_utf8(&line_buffer)?, "processed lines");
    }
}

// Parses reads like:
//
// [20220204T044316] DEBUG> mr kernel 0xC0000010
// [20220204T044316] c0000010: 03 0a 30 18  00 00 00 00  00 00 00 80  00 07 00 00 |..0.............|
#[tracing::instrument(skip_all)]
async fn process_line(
    prompt: &str,
    state: BufferState,
    line: &str,
    event_tx: &mut mpsc::Sender<Event>,
) -> Result<BufferState> {
    info!(?state, ?line, "Processing line");
    match state {
        BufferState::WaitForCommand => {
            let tokens = line.split_ascii_whitespace().collect::<Vec<_>>();

            // Guard against panic on split_at when tokens is empty
            if tokens.is_empty() {
                return Ok(state);
            }

            match tokens.split_at(1) {
                (first, user_tokens) if first == [prompt] => {
                    if let Some(command) = Command::from_tokens(&user_tokens) {
                        match command {
                            Command::Write { addr, data } => {
                                let event = Event::Write { addr, data };
                                info!(?event, "Sending event");
                                event_tx.send(event).await?;

                                Ok(state)
                            }
                            Command::Read { addr: _, nbytes: _ } => {
                                Ok(BufferState::WaitForResponse(command))
                            }
                        }
                    } else {
                        Ok(state)
                    }
                }
                _ => Ok(state),
            }
        }
        BufferState::WaitForResponse(command) => {
            if_chain! {
                if let Command::Read { addr, nbytes } = command;
                if let Some((_, remaining)) = line.split_once(": ");
                if let Some((remaining, _)) = remaining.split_once(" |");
                then {
                    let read_bytes = remaining
                        .split_ascii_whitespace()
                        .map(|token| u32::from_str_radix(token, 16))
                        .collect::<std::result::Result<Vec<_>, ParseIntError>>()?;
                    let dwords = read_bytes.chunks(4).map(|dword_bytes| {
                        dword_bytes
                            .iter()
                            .rev()
                            .enumerate()
                            .fold(0u32, |dword, (idx, byte)| dword | (byte << (idx * 8)))
                    });

                    for (idx, dword) in dwords.enumerate() {
                        let addr = addr + (idx as u32 * 4);
                        let data = dword;
                        let event = Event::Read { addr, data };
                        info!(?event, "Sending event");
                        event_tx.send(event).await?;
                    }

                    if nbytes > MAX_BYTES_PER_LINE {
                        let addr = addr + MAX_BYTES_PER_LINE;
                        let nbytes = nbytes - MAX_BYTES_PER_LINE;
                        let command = Command::Read { addr, nbytes };
                        Ok(BufferState::WaitForResponse(command))
                    } else {
                        Ok(BufferState::WaitForCommand)
                    }
                } else {
                    Ok(BufferState::WaitForCommand)
                }
            }
        }
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
