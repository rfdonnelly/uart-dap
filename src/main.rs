use uart_dap::{Command, Event, Result, UartDap};

use clap::Parser;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};
use tracing::{error, info};
use tracing_subscriber;

#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    #[clap(long, value_enum, default_value_t = ArgEcho::Local)]
    echo: ArgEcho,

    #[clap(long, value_enum, default_value_t = ArgLineEnding::CrLf)]
    line_ending: ArgLineEnding,

    #[clap(short, long, default_value_t = 9600)]
    baud_rate: u32,

    /// Path to serial port device
    path: String,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum ArgEcho {
    Local,
    Remote,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum ArgLineEnding {
    Lf,
    #[clap(name = "crlf")]
    CrLf,
}

impl From<ArgEcho> for uart_dap::Echo {
    fn from(e: ArgEcho) -> Self {
        match e {
            ArgEcho::Local => Self::Local,
            ArgEcho::Remote => Self::Remote,
        }
    }
}

impl From<ArgLineEnding> for uart_dap::LineEnding {
    fn from(e: ArgLineEnding) -> Self {
        match e {
            ArgLineEnding::Lf => Self::Lf,
            ArgLineEnding::CrLf => Self::CrLf,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let args = Args::parse();

    let (app_command_tx, app_command_rx) = mpsc::channel(1);
    let (serial_event_tx, serial_event_rx) = mpsc::channel(1);

    let serial = UartDap::new(
        &args.path,
        args.baud_rate,
        args.echo.into(),
        args.line_ending.into(),
    )?;

    tokio::select! {
        result = process_commands(app_command_tx) => result,
        result = serial.run(app_command_rx, serial_event_tx) => result,
        result = report_events(serial_event_rx) => result,
    }?;

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn process_commands(app_command_tx: mpsc::Sender<Command>) -> Result<()> {
    info!("started");

    let stdin = tokio::io::stdin();
    let mut reader = FramedRead::new(stdin, LinesCodec::new());

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                let tokens = line.split_ascii_whitespace().collect::<Vec<_>>();
                if let Some(command) = Command::from_tokens(&tokens) {
                    app_command_tx.send(command).await?;
                }
            }
            Err(e) => {
                error!(?e);
            }
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn report_events(mut serial_command_rx: mpsc::Receiver<Event>) -> Result<()> {
    info!("started");

    loop {
        let event = serial_command_rx.recv().await;
        info!(?event);
    }
}

#[cfg(test)]
mod test {
    #[tokio::test]
    async fn message() {}
}
