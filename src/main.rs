use stargzr::player;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    player::initialize(PathBuf::from("D:/Skola/.projekti/stargzr/music")).await;
}