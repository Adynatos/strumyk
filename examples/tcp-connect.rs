use tokio::net::TcpStream;
use std::{error::Error, path::Path};
use std::env;
use std::fs;
use strumyk::{parse_torrent, peer_handler, PeerUpdate, PeerCommand};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>>
{
    let args: Vec<String> = env::args().collect();
    let addr = format!("127.0.0.1:{}", args[1]);
    let stream = TcpStream::connect(addr).await?;
    let path = Path::new("ubuntu-21.10-desktop-amd64.iso.torrent");
    let torrent = parse_torrent(path);

    let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(32);
    let (updates_tx, mut updates_rx) = tokio::sync::mpsc::channel(32);

    let torrent_clone = torrent.clone();
    tokio::spawn(async move {
      let _ = peer_handler(stream, torrent_clone, commands_rx, updates_tx).await;
    });

    let mut choked = true;
    let paths = fs::read_dir("./").unwrap();
    let paths = paths.flatten().flat_map(|e| e.file_name().into_string()).filter(|e| e.contains(&"piece")).collect::<Vec<String>>();
    let paths = paths.iter().map(|s| s.chars().filter(|c| c.is_digit(10)).collect::<String>());
    let mut paths = paths.flat_map(|e| e.parse::<u32>()).collect::<Vec<u32>>();
    paths.sort();

    let mut piece_idx = if let Some(&v) = paths.last() { v } else { 0u32 };
    let num_of_pieces: u32 = (torrent.info.pieces.len() / 20) as u32;
    println!("Num of pieces: {}", num_of_pieces);
    while let Some(update) = updates_rx.recv().await {
      match update {
        PeerUpdate::Choked => { choked = true; },
        PeerUpdate::Unchoked => {
          choked = false;
          commands_tx.send(PeerCommand::RequestPiece{index: piece_idx}).await?;
        }
        PeerUpdate::FinishedPiece{index} => {
          piece_idx = index + 1;
          if piece_idx == num_of_pieces {
            commands_tx.send(PeerCommand::Exit{}).await?;
            break;
          }
          if !choked {
            commands_tx.send(PeerCommand::RequestPiece{index: piece_idx}).await?;
          }
        }
      }
    }

    Ok(())
}