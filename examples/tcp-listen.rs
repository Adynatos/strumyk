use std::env;
use strumyk::{receive_handshake, send_handshake, Connection, Result};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let addr = format!("127.0.0.1:{}", args[1]);
    let listener = TcpListener::bind(addr).await?;
    let (mut soc, _addr) = listener.accept().await?;

    let hand_shake = receive_handshake(&mut soc).await?;
    //TODO: in future, verify their handshake and parse torrent to send our
    send_handshake(&mut soc, hand_shake).await?;

    let mut connection = Connection::new(soc);
    let msg = connection.read_msg().await?;
    println!("{:?}", msg);
    Ok(())
}
