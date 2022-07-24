use tokio::net::TcpStream;
use std::{error::Error, path::Path};
use std::env;
use std::fs::{self, File};
use std::io;
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
    println!("Last received: {}", piece_idx);

    let mut max_outstanding_reqs = 8;
    let mut queued_requests = 0;

    while let Some(update) = updates_rx.recv().await {
      match update {
        PeerUpdate::Choked => { choked = true; },
        PeerUpdate::Unchoked => {
          choked = false;
          if queued_requests < max_outstanding_reqs {
            queued_requests += 1;
            commands_tx.send(PeerCommand::RequestPiece{index: piece_idx}).await?;
          }
        }
        PeerUpdate::FinishedPiece{index} => {
          piece_idx = index + 1;
          queued_requests -= 1;
          if piece_idx == num_of_pieces {
            commands_tx.send(PeerCommand::Exit{}).await?;
            break;
          }
          if !choked {
            if queued_requests < max_outstanding_reqs {
              queued_requests += 1;
              commands_tx.send(PeerCommand::RequestPiece{index: piece_idx}).await?;
            }
          }
        }
        PeerUpdate::Downloaded{bytes} => {
          println!("Download speed: {}kB/s", bytes/1024);
          println!("Piece len: {}", torrent.info.piece_length as u32);
          let queue_size = bytes / 0x400; //From https://blog.libtorrent.org/2011/11/requesting-pieces/
          max_outstanding_reqs = std::cmp::max(queue_size / torrent.info.piece_length as u32, 4);
          println!("Queue size: {}", max_outstanding_reqs);
        }
      }
    }
    let paths = fs::read_dir("./").unwrap();
    let mut paths = paths.flatten().flat_map(|e| e.file_name().into_string()).filter(|e| e.contains(&"piece")).collect::<Vec<String>>();
    paths.sort_by_key(|e| {
      e.chars().filter(|c| c.is_digit(10)).collect::<String>().parse::<u32>().expect("unexpected piece index")
    }); 
    let mut writer = File::create("resulting-file")?;
    for path in paths {
      let mut reader = File::open(path)?;
      io::copy(&mut reader, &mut writer)?;
    }

    Ok(())
}