//! Prints every message a controller sends, as it arrives, and keeps a
//! running total for the jog controls (CC 0x21 side, 0x22 top with vinyl,
//! 0x23 top without). Turn a jog once around to measure its resolution, or
//! nudge it gently to see what small movements send.
//! Usage: `jog_steps [port]` (default "DDJ-GRV6"); Ctrl-C ends.
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let port = std::env::args().nth(1).unwrap_or_else(|| "DDJ-GRV6".to_owned());
    let mut input = midir::MidiInput::new("deckhand jog steps")?;
    input.ignore(midir::Ignore::None);
    let in_port = input
        .ports()
        .into_iter()
        .find(|p| input.port_name(p).is_ok_and(|n| n.contains(&port)))
        .ok_or_else(|| anyhow::anyhow!("no input port matching {port}"))?;
    let (tx, rx) = mpsc::channel();
    let _connection = input
        .connect(&in_port, "jog steps", move |_, message, _| drop(tx.send(message.to_vec())), ())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Listening on {port}");
    let start = Instant::now();
    let mut totals = BTreeMap::new();
    for message in rx {
        let hex = message.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
        let ms = start.elapsed().as_millis();
        match message[..] {
            [status, control @ 0x21..=0x23, value] if status & 0xF0 == 0xB0 => {
                let steps = i64::from(value) - 64;
                let total = totals.entry((status & 0x0F, control)).or_insert(0i64);
                *total += steps;
                let name = match control {
                    0x21 => "side",
                    0x22 => "top (vinyl)",
                    _ => "top (no vinyl)",
                };
                let deck = (status & 0x0F) + 1;
                println!("{ms:>8} ms  {hex}  deck {deck} {name} {steps:+} (total {total})");
            }
            _ => println!("{ms:>8} ms  {hex}"),
        }
    }
    Ok(())
}
