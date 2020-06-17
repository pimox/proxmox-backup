use anyhow::{Error};
use futures::*;

// Simple H2 server to test H2 speed with h2client.rs

use tokio::net::TcpListener;
use tokio::io::{AsyncRead, AsyncWrite};

use proxmox_backup::client::pipe_to_stream::PipeToSendStream;

fn main() -> Result<(), Error> {
    proxmox_backup::tools::runtime::main(run())
}

async fn run() -> Result<(), Error> {
    let mut listener = TcpListener::bind(std::net::SocketAddr::from(([127,0,0,1], 8008))).await?;

    println!("listening on {:?}", listener.local_addr());

    loop {
        let (socket, _addr) = listener.accept().await?;
        tokio::spawn(handle_connection(socket)
            .map(|res| {
                if let Err(err) = res {
                    eprintln!("Error: {}", err);
                }
            }));
    }
}

async fn handle_connection<T: AsyncRead + AsyncWrite + Unpin>(socket: T) -> Result<(), Error> {
    let mut conn = h2::server::handshake(socket).await?;

    println!("H2 connection bound");

    while let Some((request, mut respond)) = conn.try_next().await? {
        println!("GOT request: {:?}", request);

        let response = http::Response::builder()
            .status(http::StatusCode::OK)
            .body(())
            .unwrap();

        let send = respond.send_response(response, false).unwrap();
        let data = vec![65u8; 1024*1024];
        PipeToSendStream::new(bytes::Bytes::from(data), send).await?;
        println!("DATA SENT");
    }

    Ok(())
}