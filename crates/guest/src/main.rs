use ox::io;
use ox::iter::AsyncIterator;
use ox::net::TcpListener;

fn main() -> io::Result<()> {
    ox::runtime::block_on(|reactor| async move {
        let listener = TcpListener::bind(&reactor, "127.0.0.1:8080").await?;
        println!("Listening on {}", listener.local_addr()?);
        println!("type `nc localhost 8080` to create a TCP client");

        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            let stream = stream?;
            println!("Accepted from: {}", stream.peer_addr()?);
            io::copy(&stream, &stream).await?;
        }
        Ok(())
    })
}
