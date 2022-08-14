use clap::Parser;
use futures::stream::StreamExt;
use futures::SinkExt;
use tokio::sync::{broadcast, oneshot};
use tokio_serial::SerialPortBuilderExt;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
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
    let mut serial_writer = FramedWrite::new(serial_tx_port, LinesCodec::new());
    let mut serial_reader = FramedRead::new(serial_rx_port, LinesCodec::new());

    let (command_tx, mut command_rx0) = broadcast::channel(2);
    let mut command_rx1 = command_tx.subscribe();

    let (response_tx, mut response_rx) = oneshot::channel();

    tokio::join! {
        process_stdin(command_tx),
        process_serial_tx(command_rx0, serial_writer),
        process_serial_rx(serial_reader, response_tx),
        process_serial_buffer(&mut command_rx1, &mut response_rx),
    };

    Ok(())
}

async fn process_stdin(command_tx: broadcast::Sender<String>) {
    let stdin = tokio::io::stdin();
    let mut reader = FramedRead::new(stdin, LinesCodec::new());

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                info!(?line);
                command_tx.send(line).unwrap();
            }
            Err(e) => {
                error!(?e);
            }
        }
    }
}

async fn process_serial_tx(command_rx: broadcast::Receiver<String>, writer: impl SinkExt<String>) {
}

async fn process_serial_rx(reader: impl StreamExt, response_tx: oneshot::Sender<String>) {
}

enum LineType {
    Command(String),
    Response(String),
}

enum BufferState {
    WaitForCommand,
    WaitForResponse(String),
}

async fn process_serial_buffer(
    command_rx: &mut broadcast::Receiver<String>,
    response_rx: &mut oneshot::Receiver<String>,
) {
    let mut state = BufferState::WaitForCommand;

    loop {
        let line = tokio::select!{
            line = command_rx.recv() => LineType::Command(line.unwrap()),
            line = (&mut *response_rx) => LineType::Response(line.unwrap()),
        };

        match line {
            LineType::Command(command) => {
                info!(?command);
                if command.starts_with("rd ") {
                    state = BufferState::WaitForResponse(command);
                }
            }
            LineType::Response(response) => {
                if let BufferState::WaitForResponse(command) = state {
                    state = BufferState::WaitForCommand;
                    info!(?command, ?response);
                }
            }
        }
    }

}

#[cfg(test)]
mod test {
    #[tokio::test]
    async fn message() {}
}
