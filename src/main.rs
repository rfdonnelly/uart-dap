use std::str::FromStr;

use clap::Parser;
use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{broadcast, mpsc};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec, LinesCodecError};
use tracing::{error, info};
use tracing_subscriber;

#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    #[clap(short, long, default_value_t = 9600)]
    baud_rate: u32,

    path: String,
}

type Error = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let port = tokio_serial::new(args.path, args.baud_rate).open_native_async()?;

    let (serial_rx_port, serial_tx_port) = tokio::io::split(port);
    let serial_writer = FramedWrite::new(serial_tx_port, LinesCodec::new());
    let serial_reader = FramedRead::new(serial_rx_port, LinesCodec::new());

    let (command_tx, command_rx0) = broadcast::channel(2);
    let mut command_rx1 = command_tx.subscribe();

    let (response_tx, mut response_rx) = mpsc::channel(1);

    tokio::try_join! {
        process_stdin(command_tx),
        process_serial_tx(command_rx0, serial_writer),
        process_serial_rx(serial_reader, response_tx),
        process_serial_buffer(&mut command_rx1, &mut response_rx),
    }?;

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn process_stdin(command_tx: broadcast::Sender<String>) -> Result<()> {
    info!("started");

    let stdin = tokio::io::stdin();
    let mut reader = FramedRead::new(stdin, LinesCodec::new());

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                info!(?line);
                command_tx.send(line)?;
            }
            Err(e) => {
                error!(?e);
            }
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn process_serial_tx(
    mut command_rx: broadcast::Receiver<String>,
    mut writer: impl Sink<String> + Unpin,
) -> Result<()> {
    info!("started");

    loop {
        let command = command_rx.recv().await?;
        writer
            .send(command)
            .await
            .map_err(|_| Error::from("could not send"))?;
    }
}

#[tracing::instrument(skip_all)]
async fn process_serial_rx(
    mut reader: impl Stream<Item = core::result::Result<String, LinesCodecError>> + Unpin,
    response_tx: mpsc::Sender<String>,
) -> Result<()> {
    info!("started");

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                info!(?line, "forwarding");
                response_tx.send(line).await?;
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
    WaitForResponse(ReadCommand),
}

#[derive(Debug, Clone, Copy)]
struct ReadCommand {
    addr: u32,
}

impl FromStr for ReadCommand {
    type Err = Error;

    fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
        let (command, args) = s.split_once(char::is_whitespace).ok_or_else(|| Error::from("not a command"))?;
        match command {
            "rd" => {
                let addr = parse_based_int(args)?;
                Ok(Self { addr })
            }
            _ => Err(format!("failed to parse command from '{s}'"))?
         }
    }
}

#[derive(Debug, Clone, Copy)]
struct WriteCommand {
    addr: u32,
    data: u32,
}

impl FromStr for WriteCommand {
    type Err = Error;

    fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
        let (command, args) = s.split_once(char::is_whitespace).ok_or_else(|| Error::from("not a command"))?;
        match command {
            "wr" => {
                let (addr, data) = args.split_once(char::is_whitespace).ok_or_else(|| Error::from("expected 2 arguments"))?;
                let addr = parse_based_int(&addr)?;
                let data = parse_based_int(&data)?;
                Ok(Self { addr, data })
            }
            _ => Err(format!("failed to parse command from '{s}'"))?
         }
    }
}

#[tracing::instrument(skip_all)]
async fn process_serial_buffer(
    command_rx: &mut broadcast::Receiver<String>,
    response_rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let mut state = BufferState::WaitForCommand;

    info!("started");

    loop {
        info!("waiting on select");
        let line = tokio::select! {
            line = command_rx.recv() => line?,
            line = response_rx.recv() => line.ok_or_else(|| Error::from("channel closed"))?,
        };
        info!(?line, "done waiting on select");

        if let BufferState::WaitForResponse(ref command) = state {
            info!(?state, ?command);
            let data = parse_based_int(&line)?;
            process_read_command(command.addr, data).await;
        } else if line.starts_with("wr ") {
            let command = WriteCommand::from_str(&line)?;
            process_write_command(command.addr, command.data).await;
        }

        let next_state = if let BufferState::WaitForResponse(_) = state {
            BufferState::WaitForCommand
        } else if line.starts_with("rd ") {
            let command = ReadCommand::from_str(&line)?;
            BufferState::WaitForResponse(command)
        } else {
            state.clone()
        };
        info!("done next_state processing");

        info!(?state, ?next_state, ?line);

        state = next_state;
    }
}

#[tracing::instrument]
async fn process_write_command(addr: u32, data: u32) {
    info!("called");
}

#[tracing::instrument]
async fn process_read_command(addr: u32, data: u32) {
    info!("called");
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

#[cfg(test)]
mod test {
    #[tokio::test]
    async fn message() {}
}
