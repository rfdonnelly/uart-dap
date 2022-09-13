use std::collections::HashMap;
use std::str::FromStr;

use byteorder::{BigEndian, ByteOrder};
use clap::Parser;
use derive_more::Display;
use rand::prelude::*;
use rand_pcg::Pcg32;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_serial::SerialPortBuilderExt;
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[clap(author, version, about)]
struct Args {
    #[clap(short, long, default_value_t = 9600)]
    baud_rate: u32,

    #[clap(value_enum, long, default_value_t = Os::VxWorks)]
    os: Os,

    #[clap(long)]
    echo: bool,

    path: String,
}

#[derive(Copy, Clone, Display, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum Os {
    #[clap(name = "vxworks")]
    VxWorks,
    Integrity,
}

type Result<T> = core::result::Result<T, Box<dyn std::error::Error>>;

struct State {
    rng: Pcg32,
    mem: HashMap<u32, u32>,
}

impl State {
    fn new() -> Self {
        Self {
            rng: Pcg32::from_entropy(),
            mem: HashMap::new(),
        }
    }
}

type Request<'a> = &'a str;
type Response = String;

enum Action {
    None,
    Exit,
    Err(Response),
    Respond(Response),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let port = tokio_serial::new(args.path, args.baud_rate).open_native_async()?;

    let (rx_port, tx_port) = tokio::io::split(port);
    let writer = tx_port;
    let reader = FramedRead::new(rx_port, LinesCodec::new());

    let shutdown_token = CancellationToken::new();
    let shutdown_token_clone = shutdown_token.clone();

    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => println!("received ctrl-c, shutting down"),
            Err(_) => eprintln!("unable to listen for ctrl-c"),
        }

        shutdown_token.cancel();
    });

    listen(reader, writer, args.echo, &args.os, shutdown_token_clone).await?;

    Ok(())
}

async fn listen<R, W>(
    mut reader: FramedRead<R, LinesCodec>,
    mut writer: W,
    echo: bool,
    os: &Os,
    shutdown_token: CancellationToken,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut state = State::new();

    println!("Modeling {}", os);
    writer
        .write(format!("Modeling {}\r\n", os).as_bytes())
        .await?;
    writer.write(prompt(os).as_bytes()).await?;

    loop {
        tokio::select! {
            option = reader.next() => {
                match option {
                    Some(result) => {
                        match result {
                            Ok(req) => {
                                println!("Received: {}", req);
                                if echo {
                                    writer.write(format!("{}\r\n", req).as_bytes()).await?;
                                }
                                let action = process_request(&mut state, &req);
                                match action {
                                    Action::None => {
                                        writer.write(prompt(os).as_bytes()).await?;
                                    }
                                    Action::Exit => return Ok(()),
                                    Action::Err(rsp) => {
                                        writer
                                            .write(format!("Error: {}\r\n", rsp).as_bytes())
                                            .await?;
                                        writer.write(prompt(os).as_bytes()).await?;
                                    }
                                    Action::Respond(rsp) => {
                                        writer.write(format!("{}\r\n", rsp).as_bytes()).await?;
                                        writer.write(prompt(os).as_bytes()).await?;
                                    }
                                }
                            }
                            Err(e) => {
                                Err(e)?;
                            }
                        }
                    }
                    None => {
                        Err("serial port closed")?;
                    }
                }
            }
            _ = shutdown_token.cancelled() => {
                return Ok(());
            }
        }
    }
}

// The prompt for user input.
//
// Flight Software Testbed Example:
//
//  [20220131T220813] DEBUG> mr kernel 0xc0e04004 4^M
//  [20220131T220813] c0e04004: 00 40 04 a0                                         |.@..|^M
//
// The prompt in this example is "[20220131T220813] DEBUG> "
fn prompt(os: &Os) -> &'static str {
    match os {
        Os::VxWorks => "-> ",
        Os::Integrity => "DEBUG> ",
    }
}

fn process_request(state: &mut State, req: Request) -> Action {
    let tokens = req.split_ascii_whitespace().collect::<Vec<_>>();
    match tokens[..] {
        ["exit"] => Action::Exit,
        ["?" | "h" | "help"] => Action::Respond(
            "Available Commands\r
\r
    exit\r
\r
        Gracefully terminate the model.\r
\r
    mw kernel <addr> <data>\r
\r
        Write data to an address.\r
\r
    mr kernel <addr>\r
\r
        Read data from an address.\r
\r
    help\r
\r
        Displays available commands.\r
"
            .to_string(),
        ),
        ["mw", "kernel", addr, data] => {
            let addr = match parse_based_int(&addr) {
                Ok(value) => value,
                Err(_) => return Action::Err(format!("unable to parse addr: {}", addr)),
            };
            let data = match parse_based_int(&data) {
                Ok(value) => value,
                Err(_) => return Action::Err(format!("unable to parse data: {}", addr)),
            };
            println!("Writing addr:{:#} data:{:#}", addr, data);
            state.mem.insert(addr, data);
            Action::None
        }
        ["mr", "kernel", addr, nbytes] => {
            let addr = match parse_based_int(&addr) {
                Ok(value) => value,
                Err(_) => return Action::Err(format!("unable to parse addr: {}", addr)),
            };
            let nbytes = match parse_based_int(&nbytes) {
                Ok(value) => value,
                Err(_) => return Action::Err(format!("unable to parse nbytes: {}", nbytes)),
            };
            process_read_request(state, addr, nbytes)
        }
        ["mr", "kernel", addr] => {
            let addr = match parse_based_int(&addr) {
                Ok(value) => value,
                Err(_) => return Action::Err(format!("unable to parse addr: {}", addr)),
            };
            process_read_request(state, addr, 16)
        }
        _ => Action::Respond("".to_string()),
    }
}

fn div_ceil(lhs: u32, rhs: u32) -> u32 {
    (lhs + rhs - 1) / rhs
}

fn process_read_request(state: &mut State, addr: u32, nbytes: u32) -> Action {
    let ndwords = div_ceil(nbytes, 4);
    let dwords = (0..ndwords).map(|dword_idx| {
        let dword_addr = addr + dword_idx;
        let dword = match state.mem.get(&dword_addr) {
            Some(&data) => data,
            None => state.rng.gen::<u32>(),
        };
        dword
    });
    let bytes = dwords.flat_map(|dword| {
        let mut bytes = [0; 4];
        BigEndian::write_u32(&mut bytes, dword);
        bytes
    });
    let byte_string = bytes
        .map(|byte| format!("{byte:x}"))
        .collect::<Vec<String>>()
        .join(" ");
    let message = format!("{addr:x}: {byte_string} |--------|");

    Action::Respond(message)
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
