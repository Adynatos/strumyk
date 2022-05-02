use std::path::Path;
use reqwest;
use form_urlencoded;
use strumyk::{hash_info, parse_torrent};

fn main() {
    let path = Path::new("ubuntu-21.10-desktop-amd64.iso.torrent");
    let torrent = parse_torrent(path);
    let hash = hash_info(&torrent);
    let announce = torrent.announce;
    let announce = announce.replace("announce", "scrape");

    let enc: String = form_urlencoded::byte_serialize(&hash).collect();
    let url = format!("{}?hash_info={}", announce, enc);
    //println!("url {}", url);
    let client = reqwest::blocking::Client::new();
    let req = client.get(url).query(&[("uploaded", 0)]).build().unwrap();
   println!("{:?}", req);
   
}