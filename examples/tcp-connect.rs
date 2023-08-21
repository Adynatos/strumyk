use bitvec::prelude::*;
use std::env;
use std::fs::{self, File};
use std::io;
use tokio::net::TcpListener;
use std::{error::Error, path::Path};
use strumyk::{parse_torrent, peer_handler, PeerCommand, PeerUpdate, PiecePicker};
use tokio::net::TcpStream;
use std::collections::HashMap;
use socket2::{Socket, Domain, Type};
use std::net::SocketAddr;


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let addresses: Vec<String>= args[3..].into();
    let path = Path::new("ubuntu-20.04.5-desktop-amd64.iso.torrent");
    let torrent = parse_torrent(path);
    let workdir = &args[1];
    let listen_port = &args[2];

    let mut choked = true;
    let paths = fs::read_dir(workdir).unwrap();
    let paths = paths
        .flatten()
        .flat_map(|e| e.file_name().into_string())
        .filter(|e| e.contains(&"piece"))
        .collect::<Vec<String>>();
    let paths = paths
        .iter()
        .map(|s| s.chars().filter(|c| c.is_digit(10)).collect::<String>());
    let mut paths = paths.flat_map(|e| e.parse::<u32>()).collect::<Vec<u32>>();
    paths.sort();

    let num_of_pieces: u32 = (torrent.info.pieces.len() / 20) as u32;
    println!("Num of pieces: {}", num_of_pieces);

    let mut bs = BitVec::new();
    bs.resize(num_of_pieces as usize, false);
    for path in paths.iter() {
        bs.set(*path as usize, true);
    }

    let mut max_outstanding_reqs = 12;
    let mut queued_requests = 0;

    let mut piece_picker = PiecePicker::new(num_of_pieces, bs.clone()); //TODO: get bitset, update bitset
    let mut piece_idx;

    let mut last_peer_id = 0;
    let mut commands: HashMap<u8, tokio::sync::mpsc::Sender<PeerCommand>> = HashMap::new();
    let (updates_tx, mut updates_rx) = tokio::sync::mpsc::channel(32);
    for (peer_id, address) in addresses.into_iter().enumerate() {
        let torrent = torrent.clone();
        let updates_tx = updates_tx.clone();
        let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(32);
        last_peer_id = peer_id;
        let peer_id: u8 = peer_id as u8;
        commands.insert(peer_id, commands_tx);
        let bs = bs.clone();
        let workdir = workdir.clone();
        tokio::spawn(async move {
            let stream = TcpStream::connect(address.clone()).await?;
            peer_handler(peer_id as u8, stream, torrent, commands_rx, updates_tx, bs, workdir).await
        });
    }
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
    let address: SocketAddr = format!("127.0.0.1:{}", listen_port).parse().unwrap();
    let address = address.into();
    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&address)?;
    socket.listen(128)?;
    let listener = TcpListener::from_std(socket.into())?;

    loop {
    tokio::select!{
        Ok((stream, _address)) = listener.accept() => {
            let torrent = torrent.clone();
            let updates_tx = updates_tx.clone();
            let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(32);
            last_peer_id += 1;
            let peer_id: u8 = last_peer_id as u8;
            commands.insert(peer_id, commands_tx);
            let bs = bs.clone();
            let workdir = workdir.clone();
            tokio::spawn(async move {
                peer_handler(peer_id as u8, stream, torrent, commands_rx, updates_tx, bs, workdir).await
            });
        },
        Some(update) = updates_rx.recv() => {
            match update {
                PeerUpdate::Choked{peer_id: _} => {
                    choked = true;
                }
                PeerUpdate::Unchoked{peer_id} => {
                    choked = false;
                    if queued_requests < max_outstanding_reqs {
                        piece_idx = piece_picker.next(peer_id);
                        if let Some(piece_idx) = piece_idx {
                            queued_requests += 1;
                            commands[&peer_id]
                                .send(PeerCommand::RequestPiece { index: piece_idx })
                                .await?;
                        }    
                    }
                }
                PeerUpdate::FinishedPiece { peer_id, index } => {
                    piece_idx = piece_picker.next(peer_id);
                    queued_requests -= 1;
                    //TODO: new condition for end - we no longer download stuff sequentially
                    if piece_picker.completed() {
                        for peer in commands.values() {
                            peer.send(PeerCommand::Exit {}).await?;
                        }
                        break;
                    }
                    //TODO: Remove command channels for disconnected peers
                    for tx in commands.values() {
                        tx.send(PeerCommand::Have{index}).await?;
                    }
                    if !choked {
                        if queued_requests < max_outstanding_reqs {
                            if let Some(piece_idx) = piece_idx {
                                queued_requests += 1;
                                commands[&peer_id]
                                    .send(PeerCommand::RequestPiece { index: piece_idx })
                                    .await?;
                            }
                        }
                    }
                }
                PeerUpdate::Downloaded { peer_id, bytes } => {
                    //TODO: keep this on peer-by-peer basis
                    println!("Download speed: {}kB/s from peer: {}", bytes / 1024, peer_id);
                    println!("Piece len: {}", torrent.info.piece_length as u32);
                    let queue_size = bytes / 0x400; //From https://blog.libtorrent.org/2011/11/requesting-pieces/
                    max_outstanding_reqs =
                        std::cmp::max(queue_size / torrent.info.piece_length as u32, 16);
                    println!("Queue size: {}", max_outstanding_reqs);
                }
                PeerUpdate::Bitfield { peer_id, bits } => piece_picker.update(peer_id, bits),
                PeerUpdate::Have{peer_id, index} => {
                    piece_picker.have(peer_id, index);
                    //TODO: deduplicate!
                    if !choked {
                        if queued_requests < max_outstanding_reqs {
                            if let Some(piece_idx) = piece_picker.next(peer_id) {
                                queued_requests += 1;
                                commands[&peer_id]
                                    .send(PeerCommand::RequestPiece { index: piece_idx })
                                    .await?;
                            }
                        }
                    }
                }
            }
        },
        else => break,
    }
    }

    let paths = fs::read_dir(workdir).unwrap();
    let mut paths = paths
        .flatten()
        .flat_map(|e| e.file_name().into_string())
        .filter(|e| e.contains(&"piece"))
        .collect::<Vec<String>>();
    paths.sort_by_key(|e| {
        e.chars()
            .filter(|c| c.is_digit(10))
            .collect::<String>()
            .parse::<u32>()
            .expect("unexpected piece index")
    });
    let mut writer = File::create("resulting-file")?;
    for path in paths {
        let mut reader = File::open(path)?;
        io::copy(&mut reader, &mut writer)?;
    }

    Ok(())
}
