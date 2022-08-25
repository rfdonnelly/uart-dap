use uart_dap::{UartDap, Command, Event, Echo, Result};

use std::str::FromStr;

use clap::Parser;
use futures::{StreamExt};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};
use tracing::{error, info};
use tracing_subscriber;

#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    /// Disable local echo
    #[clap(long)]
    no_echo: bool,

    #[clap(short, long, default_value_t = 9600)]
    baud_rate: u32,

    /// Path to serial port device
    path: String,
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
        if args.no_echo { Echo::Off } else { Echo::On },
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
    let stdin = tokio::io::stdin();
    let mut reader = FramedRead::new(stdin, LinesCodec::new());

    while let Some(result) = reader.next().await {
        match result {
            Ok(line) => {
                let command = Command::from_str(&line)?;
                app_command_tx.send(command).await?;
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
