use tokio::net::TcpStream;
use std::{error::Error, path::Path};
use std::env;
use strumyk::{parse_torrent, peer_handler};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>>
{
    let args: Vec<String> = env::args().collect();
    let addr = format!("127.0.0.1:{}", args[1]);
    let stream = TcpStream::connect(addr).await?;
    let path = Path::new("ubuntu-21.10-desktop-amd64.iso.torrent");
    let torrent = parse_torrent(path);

    peer_handler(stream, torrent).await?;

    Ok(())
}