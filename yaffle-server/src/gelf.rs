use std::collections::HashMap;
use std::error::Error;
use std::io;
use std::net::SocketAddr;

use bytes::{buf::ext::BufExt, Buf, Bytes};
use flate2::bufread::{GzDecoder, ZlibDecoder};
use log::{debug, info, trace, warn};
use serde::{self, Deserialize};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{channel, Sender};
use tokio::task;
use tokio::time::{self, Duration};

enum PacketType<'a> {
    Uncompressed(&'a mut dyn Buf),
    Chunk {
        id: u64,
        seqno: u8,
        count: u8,
        content: &'a mut dyn Buf,
    },
    GzipCompressed(&'a mut dyn Buf),
    ZlibCompressed(&'a mut dyn Buf),
}

#[derive(Deserialize, Debug)]
pub struct GELFMessage {
    pub version: String,
    pub host: String,
    pub short_message: String,
    #[serde(flatten)]
    pub other: HashMap<String, serde_json::Value>,
}

fn classify(buf: &mut dyn Buf) -> Result<PacketType, io::Error> {
    let next_bytes = buf.bytes();
    if next_bytes.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Buffer available bytes too small",
        ));
    }
    match next_bytes[0..2] {
        [0x1e, 0x0f] => {
            buf.advance(2);
            if buf.remaining() < 10 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Packet too small",
                ));
            }
            let mut h = [0; 10];
            buf.copy_to_slice(&mut h);
            Ok(PacketType::Chunk {
                id: u64::from_be_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]),
                seqno: h[8],
                count: h[9],
                content: buf,
            })
        }
        [0x78, 0x01]
        | [0x78, 0x5e]
        | [0x78, 0x9c]
        | [0x78, 0xda]
        | [0x78, 0x20]
        | [0x78, 0x7d]
        | [0x78, 0xbb]
        | [0x78, 0xf9] => Ok(PacketType::ZlibCompressed(buf)),
        [0x1f, 0x8b, ..] => Ok(PacketType::GzipCompressed(buf)),
        _ => Ok(PacketType::Uncompressed(buf)),
    }
}

fn assemble_chunked(
    partials: &mut HashMap<u64, Vec<Option<Bytes>>>,
    id: u64,
    seqno: u8,
    count: u8,
    content: &mut dyn Buf,
    expiry_tx: &Sender<u64>,
) -> Option<Vec<Bytes>> {
    let entry = partials.entry(id).or_insert_with(|| {
        let mut tx2 = expiry_tx.clone();
        task::spawn(async move {
            // In 5s, send a message via the channel to expire this entry
            time::delay_for(Duration::from_secs(5)).await;
            tx2.send(id).await.unwrap();
        });
        vec![None; count.into()]
    });
    if let Some(opt) = entry.get_mut(seqno as usize) {
        if let Some(old) = opt.replace(content.to_bytes()) {
            warn!("Replaced packet {:?}!", old)
        }
    } else {
        warn!(
            "Chunk seqno: {} > len: {}, count: {}",
            seqno,
            entry.len(),
            count
        );
    }
    if !entry.contains(&None) {
        debug!(
            "Full chunked message received, {} chunks, count {}",
            entry.len(),
            count
        );
        let mut pieces = partials.remove(&id).unwrap();
        Some(pieces.drain(..).map(|piece| piece.unwrap()).collect())
    } else {
        None
    }
}

fn parse_packet(
    partials: &mut HashMap<u64, Vec<Option<Bytes>>>,
    buf: &mut dyn Buf,
    expiry_tx: &Sender<u64>,
) -> Result<Option<GELFMessage>, Box<dyn Error>> {
    trace!("Packet with {} remaining bytes", buf.remaining());
    match classify(buf)? {
        PacketType::Chunk {
            id,
            seqno,
            count,
            content,
        } => {
            trace!("Chunk {}: {} of {} chunks", id, seqno, count);
            if count < 2 {
                warn!("Chunked packet with only {} chunks", count);
            } else if let Some(mut complete) =
                assemble_chunked(partials, id, seqno, count, content, expiry_tx)
            {
                let mut drainer = complete.drain(..);
                let mut chain: Box<dyn Buf> =
                    Box::new(drainer.next().unwrap().chain(drainer.next().unwrap()));
                for item in drainer {
                    chain = Box::new(chain.chain(item));
                }
                return parse_packet(partials, &mut chain, expiry_tx);
            }
        }
        PacketType::Uncompressed(content) => {
            trace!(
                "Uncompressed packet with {} remaining bytes, content: {:?}",
                content.remaining(),
                std::str::from_utf8(content.bytes())
            );
            let m: GELFMessage = serde_json::from_reader(content.reader())?;
            debug!("Received {:?}", m);
            return Ok(Some(m));
        }
        PacketType::GzipCompressed(content) => {
            trace!(
                "GzipCompressed packet with {} remaining bytes",
                content.remaining()
            );
            let m: GELFMessage = serde_json::from_reader(GzDecoder::new(content.reader()))?;
            debug!("Received {:?}", m);
            return Ok(Some(m));
        }
        PacketType::ZlibCompressed(content) => {
            trace!(
                "ZlibCompressed packet with {} remaining bytes",
                content.remaining()
            );
            let m: GELFMessage = serde_json::from_reader(ZlibDecoder::new(content.reader()))?;
            debug!("Received {:?}", m);
            return Ok(Some(m));
        }
    };
    Ok(None)
}

pub async fn run_recv_loop(
    mut socket: UdpSocket,
    mut gelf_pipe: Sender<(SocketAddr, GELFMessage)>,
) -> Result<(), Box<dyn Error>> {
    let mut buf = [0; 65536];
    let mut partials: HashMap<u64, Vec<Option<Bytes>>> = HashMap::new();
    let (expiry_tx, mut expiry_rx) = channel(10);
    loop {
        tokio::select! {
            something = socket.recv_from(&mut buf) => {
                match something {
                    Ok((size, src)) => {
                        debug!("Read {} bytes from socket", size);
                        let parsed = parse_packet(&mut partials, &mut buf.take(size), &expiry_tx).unwrap_or_else(|e| {warn!("Packet handle error: {}", e); None});
                        if let Some(gelf_msg) = parsed {
                            gelf_pipe.send((src, gelf_msg)).await?
                        }
                    }
                    Err(e) => {
                       warn!("Packet receive error: {}", e);
                    }
                }
            }
            Some(to_expire) = expiry_rx.recv() => {
                if let Some(remaining) = partials.remove(&to_expire) {
                    info!("Chunks expired: {:?}", remaining);
                }
            }
        }
    }
}
