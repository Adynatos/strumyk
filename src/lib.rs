extern crate serde;
extern crate serde_bencode;
extern crate serde_bytes;
#[macro_use]
extern crate serde_derive;

use bitvec::prelude::*;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use hex_string::HexString;
use serde_bencode::de;
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::error::Error;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use std::u8;
use std::{fmt, io::Cursor};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const BLOCK_SIZE: u32 = 16384;

#[derive(Debug, Deserialize, Clone)]
pub struct MetaInfo {
    pub announce: String,
    pub info: Info,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Info {
    pub name: String,
    #[serde(rename = "piece length")]
    pub piece_length: i64,
    pub pieces: ByteBuf,
    pub length: Option<i64>,
    pub path: Option<ByteBuf>,
}

//TODO: Move file handling out of lib to example
pub fn parse_torrent(path: &Path) -> MetaInfo {
    let mut contents = std::fs::read(path).expect("Unable to read file");

    let res = de::from_bytes(&mut contents);
    if res.is_ok() {
        return res.unwrap();
    } else {
        panic!();
    }
}

pub fn hash_info(metainfo: &MetaInfo) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(serde_bencode::to_bytes(&metainfo.info).expect("Failed to serialize info"));
    hasher.finalize()[..].into()
}

pub struct Handshake {
    pub info_hash: Bytes,
    pub peer_id: Bytes,
}

pub const HANDSHAKE_LEN: usize = 68;
pub async fn receive_handshake(stream: &mut TcpStream) -> Result<Handshake> {
    let mut buffer = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut buffer).await?;

    //let header = buffer.copy_to_bytes(29);
    assert_eq!(b"\x13BitTorrent protocol", &buffer[..20]);

    let handshake = Handshake {
        info_hash: Bytes::copy_from_slice(&buffer[29..49]),
        peer_id: Bytes::copy_from_slice(&buffer[49..]),
    };
    Ok(handshake)
}

pub async fn send_handshake(stream: &mut TcpStream, handshake: Handshake) -> Result<()> {
    let mut header = BytesMut::with_capacity(32);
    header.put_u8(19);
    header.put_slice(b"BitTorrent protocol");
    header.put_bytes(0, 8);

    stream.write_all(&header).await?;
    stream.write_all(&handshake.info_hash).await?;
    stream.write_all(&handshake.peer_id).await?;

    return Ok(());
}

#[derive(Debug)]
pub enum MsgError {
    /// Not enough data is available to parse a message
    Incomplete,

    /// Invalid message encoding
    Other(Box<dyn Error>),
}
impl std::error::Error for MsgError {}
impl fmt::Display for MsgError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MsgError::Incomplete => "stream ended early".fmt(fmt),
            MsgError::Other(err) => err.fmt(fmt),
        }
    }
}
#[allow(dead_code)]
#[derive(Debug)]
enum PeerMessage {
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield { bits: BitVec<u8, Msb0> },
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, data: Bytes },
    Cancel { index: u32, begin: u32, length: u32 },
}

impl From<Message> for PeerMessage {
    fn from(msg: Message) -> Self {
        match msg.id {
            0 => PeerMessage::Choke {},
            1 => PeerMessage::Unchoke {},
            2 => PeerMessage::Interested {},
            5 => {
                let bytes = msg.payload.expect("Missing payload for bitfield");
                let bv = BitVec::from_slice(&bytes);
                PeerMessage::Bitfield { bits: bv }
            }
            7 => {
                let mut bytes = msg.payload.expect("Missing payload for pieces");
                let index = bytes.get_u32();
                let begin = bytes.get_u32();

                PeerMessage::Piece {
                    index,
                    begin,
                    data: bytes,
                }
            }
            _ => unimplemented!(),
        }
    }
}

#[derive(Debug)]
pub struct Message {
    length: i32,
    id: u8,
    payload: Option<Bytes>,
}

impl Message {
    pub fn check(input: &mut Cursor<&[u8]>) -> std::result::Result<(), MsgError> {
        if input.remaining() < 4 {
            return Err(MsgError::Incomplete);
        }
        let len = input.get_i32();
        println!("check length: {}", len);
        if input.remaining() < len as usize {
            return Err(MsgError::Incomplete);
        }
        //todo: != 0
        input.advance(len as usize);
        Ok(())
    }
    pub fn parse(input: &mut Cursor<&[u8]>) -> Result<Message> {
        let length = input.get_i32();
        let id = input.get_u8();
        //TODO: check if bytes length is correct
        let payload: Option<_>;
        if length > 5 {
            println!("Length: {}", length);
            let bytes = Bytes::copy_from_slice(&input.get_ref()[5..(length + 4) as usize]);
            println!("id: {}, payload: {}", id, bytes.len());
            input.advance(bytes.remaining());
            payload = Some(bytes);
        } else {
            payload = None;
        }

        return Ok(Message {
            length,
            id,
            payload,
        });
    }
}

impl From<PeerMessage> for Message {
    fn from(message: PeerMessage) -> Self {
        match message {
            PeerMessage::Choke => Message {
                id: 0,
                length: 1,
                payload: None,
            },
            PeerMessage::Unchoke => Message {
                id: 1,
                length: 1,
                payload: None,
            },
            PeerMessage::Interested => Message {
                id: 2,
                length: 1,
                payload: None,
            },
            PeerMessage::NotInterested => Message {
                id: 3,
                length: 1,
                payload: None,
            },
            PeerMessage::Request {
                index,
                begin,
                length,
            } => {
                let mut buf = BytesMut::new();
                buf.put_u32(index);
                buf.put_u32(begin);
                buf.put_u32(length);
                Message {
                    id: 6,
                    length: 13,
                    payload: Some(buf.freeze()),
                }
            }
            _ => unimplemented!(),
        }
    }
}

pub struct Connection {
    stream: TcpStream,
    buffer: BytesMut,
}

impl Connection {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: BytesMut::with_capacity(4096),
        }
    }
    pub async fn read_msg(&mut self) -> Result<Option<Message>> {
        loop {
            if let Some(msg) = self.parse_msg()? {
                return Ok(Some(msg));
            }

            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                if self.buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err("Connection reset by peer".into());
                }
            }
        }
    }

    pub async fn write_msg(&mut self, msg: &Message) -> Result<()> {
        let mut buf = BytesMut::new();
        buf.put_i32(msg.length);
        buf.put_u8(msg.id);
        if let Some(payload) = &msg.payload {
            buf.put_slice(&payload);
        }
        self.stream.write_all(&buf).await?;
        Ok(())
    }

    fn parse_msg(&mut self) -> Result<Option<Message>> {
        let mut buf = Cursor::new(&self.buffer[..]);
        match Message::check(&mut buf) {
            Ok(_) => {
                let len = buf.position() as usize;
                buf.set_position(0);
                let msg = Message::parse(&mut buf)?;
                self.buffer.advance(len);

                Ok(Some(msg))
            }
            Err(MsgError::Incomplete) => Ok(None),
            Err(e) => Err(e.to_string().into()),
        }
    }
}

#[derive(Debug)]
pub enum PeerCommand {
    RequestPiece { index: u32 },
    Exit,
}
#[derive(Debug)]
pub enum PeerUpdate {
    Unchoked { peer_id: u8 },
    Choked { peer_id: u8 },
    Bitfield { peer_id: u8, bits: BitVec<u8, Msb0> },
    FinishedPiece { peer_id: u8, index: u32 },
    Downloaded { peer_id: u8, bytes: u32 },
}

//TODO: make generic over nr of blocks
pub struct BlockStorage {
    blocks: HashMap<u32, [Bytes; 16]>,
    completed: HashMap<u32, [bool; 16]>,
    info: Info,
}

impl BlockStorage {
    pub fn new(info: Info) -> BlockStorage {
        BlockStorage {
            blocks: HashMap::new(),
            completed: HashMap::new(),
            info,
        }
    }

    pub fn insert_block(&mut self, piece_idx: u32, block_idx: usize, data: Bytes) {
        if !self.blocks.contains_key(&piece_idx) {
            const EMPTY_BYTES: Bytes = Bytes::new();
            self.blocks.insert(piece_idx, [EMPTY_BYTES; 16]);
            self.completed.insert(piece_idx, [false; 16]);
        }
        self.blocks.get_mut(&piece_idx).unwrap()[block_idx] = data;
        self.completed.get_mut(&piece_idx).unwrap()[block_idx] = true;
    }

    pub fn is_completed(&self, piece_idx: u32) -> bool {
        //TODO handle non-existence gracefully
        if piece_idx == last_piece_index(&self.info) {
            self.completed
                .get(&piece_idx)
                .unwrap()
                .iter()
                .take(blocks_in_last_piece(&self.info))
                .all(|&e| e == true)
        } else {
            self.completed
                .get(&piece_idx)
                .unwrap()
                .iter()
                .all(|&e| e == true)
        }
    }

    pub async fn flush_piece(&mut self, piece_idx: u32) -> Result<()> {
        if !self.is_completed(piece_idx) {
            return Err("Flushing incomplete piece".to_string().into());
        }

        let buffer = self.blocks.remove(&piece_idx).unwrap();

        let hs_idx = (20 * piece_idx) as usize;
        let hs = HexString::from_bytes(&Vec::from(&self.info.pieces[hs_idx..hs_idx + 20]));
        println!("Expected hash: {:?}", hs);
        let mut hasher = Sha1::new();
        for block in buffer.iter() {
            hasher.update(&block);
        }
        let hs2 = HexString::from_bytes(&hasher.finalize()[..].into());
        println!("Buffer hash: {:?}", hs2);

        //TODO: return result-failure, clear blocks
        assert_eq!(hs, hs2);

        let filename = format!("piece{}", piece_idx);
        tokio::task::spawn_blocking(move || {
            let mut file = std::io::BufWriter::new(File::create(filename).unwrap());
            for block in buffer.iter() {
                file.write_all(&block).unwrap();
            }
            file.flush().unwrap();
        });
        Ok(())
    }
}

fn last_piece_index(info: &Info) -> u32 {
    (info.pieces.len() / 20 - 1) as u32
}

fn blocks_in_last_piece(info: &Info) -> usize {
    let size_of_last_piece =
        info.length.unwrap() - (info.piece_length * last_piece_index(&info) as i64);
    ((size_of_last_piece / BLOCK_SIZE as i64) + 1) as usize
}

pub async fn peer_handler(
    peer_id: u8,
    mut stream: TcpStream,
    info: MetaInfo,
    mut commands: mpsc::Receiver<PeerCommand>,
    updates: mpsc::Sender<PeerUpdate>,
) -> Result<()> {
    // receive bitfield,  send interested,
    // wait for unchoke, then request block and wait for piece
    let handshake = Handshake {
        info_hash: Bytes::from(hash_info(&info)),
        peer_id: Bytes::copy_from_slice(b"AAAAAAAAAAAAAAAAAAAB"),
    };
    send_handshake(&mut stream, handshake).await?;
    receive_handshake(&mut stream).await?;
    println!("Exchanged hanshake");
    let mut connection = Connection::new(stream);
    let mut _choked = true;
    let mut _bitfield: BitVec<u8, Msb0>;
    let piece_size = info.info.piece_length;
    println!("Piece length: {}", piece_size);

    let reqs_per_piece = piece_size / BLOCK_SIZE as i64;
    let mut downloaded_bytes = 0;
    let mut interval_timer = time::interval(Duration::from_secs(1));

    let mut block_storage = BlockStorage::new(info.info.clone());
    loop {
        tokio::select! {
            msg =  connection.read_msg() => {
                match msg? {
                    None => {},
                    Some(msg) => {
                        match PeerMessage::from(msg) {
                            PeerMessage::Choke => _choked = true,
                            PeerMessage::Bitfield{bits} => {
                                updates.send(PeerUpdate::Bitfield{peer_id, bits}).await?;
                                connection.write_msg(&Message::from(PeerMessage::Interested{})).await?;
                            },
                            PeerMessage::Unchoke => {
                                updates.send(PeerUpdate::Unchoked{peer_id}).await?;
                            }
                            PeerMessage::Piece{index, begin, data} => {
                                downloaded_bytes += data.len();
                                let block_idx = (begin / BLOCK_SIZE) as usize;
                                println!("Received block with index: {} and id: {}", index, block_idx);

                                block_storage.insert_block(index, block_idx, data);
                                if block_storage.is_completed(index) {
                                    block_storage.flush_piece(index).await?;
                                    updates.send(PeerUpdate::FinishedPiece{peer_id, index}).await?;
                                }
                            }
                            _ => unimplemented!()
                        }
                    }
                }
            },
            Some(cmd) = commands.recv() => {
                match cmd {
                    PeerCommand::RequestPiece{index} => {
                        //TODO: this is quite inefficient - move to dedicated block level picker that will drive this
                        for i in 0..(reqs_per_piece+1) as u32 {
                            connection.write_msg(&Message::from(PeerMessage::Request{index: index, begin: i * BLOCK_SIZE, length: BLOCK_SIZE})).await?;
                        }
                    },
                    PeerCommand::Exit => { break; }
                }
            },
            _ = interval_timer.tick() => {
                updates.send(PeerUpdate::Downloaded{peer_id, bytes:downloaded_bytes as u32}).await?;
                downloaded_bytes = 0;
            }
        }
    }
    Ok(())
}

type PeerId = u8;
type PieceIdx = u32;

pub struct PiecePicker {
    //TODO: sort by rarest first when we got more than one peer
    pieces: HashMap<PieceIdx, Vec<PeerId>>,
    pieces_by_rarity: VecDeque<PieceIdx>,
    completed: BitVec<u8, Msb0>,
}

impl PiecePicker {
    pub fn new(num_of_pieces: u32, completed: BitVec<u8, Msb0>) -> Self {
        let pieces = HashMap::new();
        let pieces_by_rarity = VecDeque::with_capacity(num_of_pieces as usize);
        PiecePicker {
            pieces,
            pieces_by_rarity,
            completed,
        }
    }

    pub fn update(&mut self, peer_id: u8, bits: BitVec<u8, Msb0>) {
        for (idx, bit) in bits.iter().enumerate() {
            if bit == true && !self.completed[idx] {
                if let Some(v) = self.pieces.get_mut(&(idx as u32)) {
                    v.push(peer_id)
                } else {
                    self.pieces.insert(idx as u32, vec![peer_id]);
                    self.pieces_by_rarity.push_back(idx as u32);
                }
            }
        }
        self.sort()
    }

    fn sort(&mut self) {
        self.pieces_by_rarity
            .make_contiguous()
            .sort_by_cached_key(|k| {
                self.pieces[k].len();
            });
    }

    pub fn finished(&mut self, _index: u32) {
        //This will be usefull when we will keep track of in progress downloads, now we assume we always succeed :<
    }

    pub fn completed(&self) -> bool {
        self.completed.all()
    }

    pub fn next(&mut self, peer: PeerId) -> u32 {
        for (idx, piece_idx) in self.pieces_by_rarity.clone().iter().enumerate() {
            let piece = self.pieces.get(&piece_idx);
            if let Some(piece) = piece {
                if piece.contains(&peer) {
                    self.pieces_by_rarity.remove(idx);
                    //todo: move to finished when we add timeouts
                    self.completed.set(*piece_idx as usize, true);
                    return *piece_idx;
                }
            }
        }
        panic!("Error somehow we don't have piece for peer!")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
