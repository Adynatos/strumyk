extern crate serde;
extern crate serde_bencode;
extern crate serde_bytes;
#[macro_use]
extern crate serde_derive;

use std::path::Path;
use std::error::Error;
use serde_bencode::de;
use serde_bytes::ByteBuf;
use sha1::{Sha1, Digest};
use bytes::{BufMut, Bytes, BytesMut, Buf};
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc};
use std::{fmt, io::Cursor};
use tokio::fs::File;
use hex_string::HexString;
use bitvec::prelude::*;


pub type Result<T> = std::result::Result<T, Box<dyn Error+Send+Sync>>;

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

    let handshake = Handshake{info_hash: Bytes::copy_from_slice(&buffer[29..49]),
         peer_id: Bytes::copy_from_slice(&buffer[49..])};
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

    return Ok(())
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
enum PeerMessage
{
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield{
        bits: BitVec<u8, Msb0>
    },
    Request{
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece{
        index: u32,
        begin: u32,
        data: Bytes, 
    },
    Cancel{
        index: u32,
        begin: u32,
        length: u32,
    },

}

impl From<Message> for PeerMessage {
    fn from(msg: Message) -> Self {
        match msg.id {
            0 => PeerMessage::Choke{},
            1 => PeerMessage::Unchoke{},
            2 => PeerMessage::Interested{},
            5 => {
                let bytes = msg.payload.expect("Missing payload for bitfield");
                let bv = BitVec::from_slice(&bytes);
                PeerMessage::Bitfield{bits: bv}
            }
            7 => {
                let mut bytes = msg.payload.expect("Missing payload for pieces");
                let index = bytes.get_u32();
                let begin = bytes.get_u32();

                PeerMessage::Piece{index, begin, data: bytes}
            }
            _ => unimplemented!()
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
        if input.remaining() < len as usize {
            return Err(MsgError::Incomplete);
        }
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
            let bytes = Bytes::copy_from_slice(&input.get_ref()[5..(length+4) as usize]);
            println!("id: {}, payload: {}", id, bytes.len());
            input.advance(bytes.remaining());
            payload = Some(bytes);
        } else {
            payload = None;
        }

        return Ok(Message{length, id, payload})
    }
}

impl From<PeerMessage> for Message {
    fn from(message: PeerMessage) -> Self {
        match message {
            PeerMessage::Choke => Message{id: 0, length: 1, payload: None},
            PeerMessage::Unchoke => Message{id: 1, length: 1, payload: None},
            PeerMessage::Interested => Message{id: 2, length: 1, payload: None},
            PeerMessage::NotInterested => Message{id: 3, length: 1, payload: None},
            PeerMessage::Request{index, begin, length} => {
                let mut buf = BytesMut::new();
                buf.put_u32(index);
                buf.put_u32(begin);
                buf.put_u32(length);
                Message{id:6 , length: 13, payload: Some(buf.freeze())}
            },
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
        Self{
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
            },
            Err(MsgError::Incomplete) => Ok(None),
            Err(e) => {
                Err(e.to_string().into())
            }
        }
    }
}

#[derive(Debug)]
pub enum PeerCommand {
    RequestPiece{
        index: u32,
    },
    Exit,
}
#[derive(Debug)]
pub enum PeerUpdate {
    Unchoked,
    Choked,
    FinishedPiece{index: u32},
}

pub async fn peer_handler(mut stream: TcpStream, info: MetaInfo,
     mut commands: mpsc::Receiver<PeerCommand>, updates: mpsc::Sender<PeerUpdate>) -> Result<()> {
    // receive bitfield,  send interested,
    // wait for unchoke, then request block and wait for piece
    let handshake = Handshake{
        info_hash: Bytes::from(hash_info(&info)),
        peer_id: Bytes::copy_from_slice(b"AAAAAAAAAAAAAAAAAAAB")};
    send_handshake(&mut stream, handshake).await?;
    receive_handshake(&mut stream).await?;
    println!("Exchanged hanshake");
    let mut connection = Connection::new(stream);
    let mut _choked = true;
    let mut _bitfield: BitVec<u8, Msb0>;
    let piece_size = info.info.piece_length;
    println!("Piece length: {}", piece_size);
    let mut buffer = BytesMut::with_capacity(piece_size as usize);
    const BLOCK_SIZE: u32 = 16384;
    let reqs_per_piece = piece_size / BLOCK_SIZE as i64;
    let mut block_idx = 1;
    loop {
        tokio::select!{
            msg =  connection.read_msg() => {
                match msg? {
                    None => {},
                    Some(msg) => {
                        match PeerMessage::from(msg) {
                            PeerMessage::Choke => _choked = true,
                            PeerMessage::Bitfield{bits} => {
                                _bitfield = bits;
                                connection.write_msg(&Message::from(PeerMessage::Interested{})).await?;
                            },
                            PeerMessage::Unchoke => {
                                updates.send(PeerUpdate::Unchoked{}).await?;
                            }
                            PeerMessage::Piece{index,  begin: _begin, data} => {
                                buffer.extend_from_slice(&data);
                                println!("Received block with index: {} and id: {}", index, block_idx);
                                if block_idx < reqs_per_piece {
                                //TODO: index should be saved from request 
                                connection.write_msg(&Message::from(PeerMessage::Request{index: index, begin: BLOCK_SIZE * block_idx as u32, length: BLOCK_SIZE})).await?;
                                block_idx += 1;
                                } else {
                                    let filename = format!("piece{}", index);
                                    let mut file = File::create(filename).await?;
                                    file.write_all(&buffer).await?;
                                    let hs_idx = (20 * index) as usize;
                                    let hs = HexString::from_bytes(&Vec::from(&info.info.pieces[hs_idx..hs_idx+20]));
                                    println!("Expected hash: {:?}", hs);
                                    let mut hasher = Sha1::new();
                                    hasher.update(buffer);
                                    let hs2 = HexString::from_bytes(&hasher.finalize()[..].into());
                                    println!("Buffer hash: {:?}", hs2);
                                    assert_eq!(hs, hs2);
                                    buffer = BytesMut::with_capacity(piece_size as usize);
                                    block_idx = 1;
                                    updates.send(PeerUpdate::FinishedPiece{index}).await?;
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
                        connection.write_msg(&Message::from(PeerMessage::Request{index: index, begin: 0, length: BLOCK_SIZE})).await?;
                    },
                    PeerCommand::Exit => { break; }
                }
            }
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}
