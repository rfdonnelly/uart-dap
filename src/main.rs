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
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let args = Args::parse();

    let port = tokio_serial::new(args.path, args.baud_rate).open_native_async()?;

    let (serial_rx_port, serial_tx_port) = tokio::io::split(port);
    let serial_writer = FramedWrite::new(serial_tx_port, LinesCodec::new());
    let serial_reader = FramedRead::new(serial_rx_port, LinesCodec::new());

    let (app_command_tx, app_command_rx0) = broadcast::channel(2);
    let mut app_command_rx1 = app_command_tx.subscribe();

    let (serial_forward_tx, mut serial_forward_rx) = mpsc::channel(1);

    tokio::select! {
        result = process_commands(app_command_tx) => result,
        result = process_serial_tx(app_command_rx0, serial_writer) => result,
        result = process_serial_rx(serial_reader, serial_forward_tx) => result,
        result = process_serial_buffer(&mut app_command_rx1, &mut serial_forward_rx) => result,
    }?;

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn process_commands(app_command_tx: broadcast::Sender<String>) -> Result<()> {
    info!("started");

    let stdin = tokio::io::stdin();
    let mut reader = FramedRead::new(stdin, LinesCodec::new());

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                info!(?line);
                app_command_tx.send(line)?;
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
    mut app_command_rx: broadcast::Receiver<String>,
    mut writer: impl Sink<String> + Unpin,
) -> Result<()> {
    info!("started");

    loop {
        let command = app_command_rx.recv().await?;
        writer
            .send(command)
            .await
            .map_err(|_| Error::from("could not send"))?;
    }
}

#[tracing::instrument(skip_all)]
async fn process_serial_rx(
    mut reader: impl Stream<Item = core::result::Result<String, LinesCodecError>> + Unpin,
    serial_forward_tx: mpsc::Sender<String>,
) -> Result<()> {
    info!("started");

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                info!(?line, "forwarding");
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

#[derive(Debug, Clone, Copy)]
enum Command {
    Read { addr: u32 },
    Write { addr: u32, data: u32 },
}

impl FromStr for Command {
    type Err = Error;

    fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
        let (command, args) = s.split_once(char::is_whitespace).ok_or_else(|| Error::from("unrecognized command"))?;
        match command {
            "rd" => {
                let addr = parse_based_int(args)?;
                Ok(Self::Read { addr })
            }
            "wr" => {
                let (addr, data) = args.split_once(char::is_whitespace).ok_or_else(|| Error::from("expected 2 arguments"))?;
                let addr = parse_based_int(&addr)?;
                let data = parse_based_int(&data)?;
                Ok(Self::Write { addr, data })
            }
            _ => Err("unrecognized command")?
         }
    }
}

#[tracing::instrument(skip_all)]
async fn process_serial_buffer(
    app_command_rx: &mut broadcast::Receiver<String>,
    serial_forward_rx: &mut mpsc::Receiver<String>,
) -> Result<()> {
    let mut state = BufferState::WaitForCommand;

    info!("started");

    loop {
        let line = tokio::select! {
            line = app_command_rx.recv() => line?,
            line = serial_forward_rx.recv() => line.ok_or_else(|| Error::from("channel closed"))?,
        };

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
            process_read_command(addr, data).await;
        } else if let Some(Command::Write { addr, data }) = command {
            process_write_command(addr, data).await;
        }

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
