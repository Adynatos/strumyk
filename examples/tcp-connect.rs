use bitvec::prelude::*;
use socket2::{Domain, Socket, Type};
use std::env;
use std::fs::{self, File};
use std::io;
use std::net::SocketAddr;
use std::{error::Error, path::Path};
use strumyk::{parse_torrent, peer_handler, MetaInfo, PeerCommand, PeerUpdate, PiecePicker};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;

#[derive(Clone)]
struct TorrentState {
    meta: MetaInfo,
    workdir: String,
}

impl TorrentState {
    fn new(meta: MetaInfo, workdir: String) -> Self {
        Self { meta, workdir }
    }

    fn initial_download_progress(&self) -> BitVec<u8, Msb0> {
        let paths = fs::read_dir(self.workdir.clone()).unwrap();
        let pieces = paths
            .flatten()
            .flat_map(|e| e.file_name().into_string())
            .filter(|e| e.contains(&"piece"))
            .collect::<Vec<String>>();
        let pieces_ids = pieces
            .iter()
            .map(|s| s.chars().filter(|c| c.is_digit(10)).collect::<String>());
        let mut pieces_id = pieces_ids
            .flat_map(|e| e.parse::<u32>())
            .collect::<Vec<u32>>();
        pieces_id.sort();

        let mut bs = BitVec::new();
        let num_of_pieces: u32 = (self.meta.info.pieces.len() / 20) as u32;
        bs.resize(num_of_pieces as usize, false);
        for idx in pieces_id.iter() {
            bs.set(*idx as usize, true);
        }
        bs
    }
}

#[allow(dead_code)]
struct PeerState {
    pub choked: bool,
    interested: bool,
    im_choking: bool,
    im_interested: bool,
}

impl Default for PeerState {
    fn default() -> Self {
        Self {
            choked: true,
            interested: true,
            im_choking: true,
            im_interested: true,
        }
    }
}

struct PeerHandle {
    state: PeerState,
    commands: Sender<PeerCommand>,
}

impl PeerHandle {
    fn connect(
        id: u32,
        address: String,
        torrent: TorrentState,
        updates: Sender<PeerUpdate>,
    ) -> Self {
        let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(32);
        let s = Self {
            state: Default::default(),
            commands: commands_tx,
        };

        let bs = torrent.initial_download_progress();
        let workdir = torrent.workdir.clone();
        let meta = torrent.meta.clone();
        tokio::spawn(async move {
            let stream = TcpStream::connect(address.clone()).await?;
            peer_handler(id as u8, stream, meta, commands_rx, updates, bs, workdir).await
        });
        s
    }

    fn accept(
        id: u32,
        torrent: TorrentState,
        updates: Sender<PeerUpdate>,
        stream: TcpStream,
    ) -> Self {
        let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(32);
        let s = Self {
            state: Default::default(),
            commands: commands_tx,
        };

        let bs = torrent.initial_download_progress();
        let workdir = torrent.workdir.clone();
        let meta = torrent.meta.clone();
        tokio::spawn(async move {
            peer_handler(id as u8, stream, meta, commands_rx, updates, bs, workdir).await
        });
        s
    }

    //TODO read up on await, why double await?
    async fn exit(&self) -> Result<(), tokio::sync::mpsc::error::SendError<PeerCommand>> {
        self.commands.send(PeerCommand::Exit {}).await
    }

    async fn send_have(
        &self,
        index: u32,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<PeerCommand>> {
        self.commands.send(PeerCommand::Have { index }).await
    }
    async fn request_piece(
        &self,
        piece_id: u32,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<PeerCommand>> {
        self.commands
            .send(PeerCommand::RequestPiece { index: piece_id })
            .await
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let addresses: Vec<String> = args[3..].into();
    let path = Path::new("ubuntu-20.04.5-desktop-amd64.iso.torrent");
    let torrent = parse_torrent(path);
    let workdir = &args[1];
    let listen_port = &args[2];

    let torrent_state = TorrentState::new(torrent, workdir.to_string());
    let bs = torrent_state.initial_download_progress();

    let mut max_outstanding_reqs = 12;
    let mut queued_requests = 0;

    let mut piece_picker = PiecePicker::new(bs.clone()); //TODO: get bitset, update bitset
    let mut piece_idx;

    let mut last_peer_id = 0;
    //let mut commands: HashMap<u8, tokio::sync::mpsc::Sender<PeerCommand>> = HashMap::new();
    // TODO: commands should be PeerManager
    let (updates_tx, mut updates_rx) = tokio::sync::mpsc::channel(32);
    let mut peers = Vec::new();
    for (id, address) in addresses.into_iter().enumerate() {
        let peer = PeerHandle::connect(id as u32, address, torrent_state.clone(), updates_tx.clone());
        peers.push(peer);
        last_peer_id = id;
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
        tokio::select! {
            Ok((stream, _address)) = listener.accept() => {
                last_peer_id += 1;
                let peer = PeerHandle::accept(last_peer_id as u32, torrent_state.clone(), updates_tx.clone(), stream);
                peers.push(peer);
            },
            Some(update) = updates_rx.recv() => {
                match update {
                    PeerUpdate::Choked{peer_id} => {
                        peers[peer_id as usize].state.choked = true;
                    }
                    PeerUpdate::Unchoked{peer_id} => {
                        peers[peer_id as usize].state.choked = false;

                        if queued_requests < max_outstanding_reqs {
                            piece_idx = piece_picker.next(peer_id);
                            if let Some(piece_idx) = piece_idx {
                                queued_requests += 1;
                                peers[peer_id as usize].commands
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
                            for peer in peers {
                                peer.exit().await?;
                            }
                            break;
                        }
                        //TODO: Remove command channels for disconnected peers
                        for peer in &peers {
                            peer.send_have(index).await?;
                        }
                            if queued_requests < max_outstanding_reqs {
                                if let Some(piece_idx) = piece_idx {
                                    queued_requests += 1;
                                    let peer = &peers[peer_id as usize];
                                    if !peer.state.choked {
                                        peer.request_piece(piece_idx).await?;
                                    }
                                }
                            }
                    }
                    PeerUpdate::Downloaded { peer_id, bytes } => {
                        //TODO: keep this on peer-by-peer basis
                        println!("Download speed: {}kB/s from peer: {}", bytes / 1024, peer_id);
                        println!("Piece len: {}", torrent_state.meta.info.piece_length as u32);
                        let queue_size = bytes / 0x400; //From https://blog.libtorrent.org/2011/11/requesting-pieces/
                        max_outstanding_reqs =
                            std::cmp::max(queue_size / torrent_state.meta.info.piece_length as u32, 16);
                        println!("Queue size: {}", max_outstanding_reqs);
                    }
                    PeerUpdate::Bitfield { peer_id, bits } => piece_picker.update(peer_id, bits),
                    PeerUpdate::Have{peer_id, index} => {
                        piece_picker.have(peer_id, index);
                        //TODO: deduplicate!
                            if queued_requests < max_outstanding_reqs {
                                if let Some(piece_idx) = piece_picker.next(peer_id) {
                                    queued_requests += 1;
                                    let peer = &peers[peer_id as usize];
                                    if !peer.state.choked {
                                        peer.request_piece(piece_idx).await?;
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
