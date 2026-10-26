//! Sends SysEx to a controller and prints everything it sends back.
//!
//! `cargo run -p deckhand-control --example sysex_probe -- PORT [HEX BYTES]`
//! Without bytes it sends the universal identity request (F0 7E 7F 06 01 F7).

use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port = args.first().map(String::as_str).unwrap_or("DDJ-GRV6");
    let bytes: Vec<u8> = if args.len() > 1 {
        args[1..].iter().map(|b| u8::from_str_radix(b, 16)).collect::<Result<_, _>>()?
    } else {
        vec![0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]
    };

    let mut input = midir::MidiInput::new("deckhand probe")?;
    input.ignore(midir::Ignore::None);
    let in_port = input
        .ports()
        .into_iter()
        .find(|p| input.port_name(p).is_ok_and(|n| n.contains(port)))
        .ok_or_else(|| anyhow::anyhow!("no input port matching {port}"))?;
    let (tx, rx) = mpsc::channel();
    let _connection = input
        .connect(&in_port, "probe", move |_, message, _| drop(tx.send(message.to_vec())), ())
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let output = midir::MidiOutput::new("deckhand probe")?;
    let out_port = output
        .ports()
        .into_iter()
        .find(|p| output.port_name(p).is_ok_and(|n| n.contains(port)))
        .ok_or_else(|| anyhow::anyhow!("no output port matching {port}"))?;
    let mut out = output.connect(&out_port, "probe").map_err(|e| anyhow::anyhow!("{e}"))?;

    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ");
    println!("> {}", hex(&bytes));
    out.send(&bytes)?;
    let until = Instant::now() + Duration::from_millis(1500);
    let mut count = 0;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        let Ok(message) = rx.recv_timeout(left) else { break };
        count += 1;
        if count <= 400 {
            println!("< {}", hex(&message));
        }
    }
    println!("{count} messages");
    Ok(())
}
