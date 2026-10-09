//! Just enough HTTP/2 to record a client's opening frames and answer its
//! first request.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const PRIORITY: u8 = 0x2;
const SETTINGS: u8 = 0x4;
const WINDOW_UPDATE: u8 = 0x8;
const CONTINUATION: u8 = 0x9;

const END_STREAM: u8 = 0x1;
const ACK: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY_FLAG: u8 = 0x20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Http2 {
    /// SETTINGS as sent, in order.
    pub settings: Vec<(u16, u32)>,
    /// The connection-level WINDOW_UPDATE increment, 0 if none was sent.
    pub window_update: u32,
    /// PRIORITY frames before the first request: (stream, exclusive, depends on, weight).
    pub priority_frames: Vec<(u32, bool, u32, u8)>,
    /// Priority carried on the first HEADERS frame, if any.
    pub headers_priority: Option<(bool, u32, u8)>,
    /// The Akamai HTTP/2 fingerprint string.
    pub akamai: String,
}

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

async fn read_frame<S: AsyncRead + Unpin>(s: &mut S) -> io::Result<Frame> {
    let mut head = [0u8; 9];
    s.read_exact(&mut head).await?;
    let len = (head[0] as usize) << 16 | (head[1] as usize) << 8 | head[2] as usize;
    if len > 1 << 20 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).await?;
    Ok(Frame {
        kind: head[3],
        flags: head[4],
        stream: u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff,
        payload,
    })
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = vec![(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags];
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn dependency(bytes: &[u8]) -> (bool, u32, u8) {
    let raw = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    // The wire carries weight - 1; the Akamai format prints the wire value + 1.
    (raw >> 31 == 1, raw & 0x7fff_ffff, bytes[4])
}

/// Reads the preface and frames up to the end of the first request's
/// headers.
pub async fn read_request<S: AsyncRead + Unpin>(
    s: &mut S,
) -> io::Result<(Http2, Vec<(String, String)>)> {
    let mut preface = [0u8; 24];
    s.read_exact(&mut preface).await?;
    if preface != PREFACE {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut settings = Vec::new();
    let mut window_update = 0;
    let mut priority_frames = Vec::new();
    let mut headers_priority = None;
    let mut block = Vec::new();
    loop {
        let f = read_frame(s).await?;
        match f.kind {
            SETTINGS if f.flags & ACK == 0 => {
                for c in f.payload.as_chunks::<6>().0 {
                    settings.push((
                        u16::from_be_bytes([c[0], c[1]]),
                        u32::from_be_bytes([c[2], c[3], c[4], c[5]]),
                    ));
                }
            }
            WINDOW_UPDATE if f.stream == 0 && f.payload.len() == 4 => {
                window_update =
                    u32::from_be_bytes([f.payload[0], f.payload[1], f.payload[2], f.payload[3]])
                        & 0x7fff_ffff;
            }
            PRIORITY if f.payload.len() == 5 => {
                let (excl, dep, w) = dependency(&f.payload);
                priority_frames.push((f.stream, excl, dep, w));
            }
            HEADERS => {
                let mut p = &f.payload[..];
                let mut pad = 0;
                if f.flags & PADDED != 0 {
                    pad = p[0] as usize;
                    p = &p[1..];
                }
                if f.flags & PRIORITY_FLAG != 0 {
                    headers_priority = Some(dependency(&p[..5]));
                    p = &p[5..];
                }
                block.extend_from_slice(&p[..p.len() - pad]);
                if f.flags & END_HEADERS != 0 {
                    break;
                }
            }
            CONTINUATION => {
                block.extend_from_slice(&f.payload);
                if f.flags & END_HEADERS != 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    let headers: Vec<(String, String)> = fluke_hpack::Decoder::new()
        .decode(&block)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?
        .into_iter()
        .map(|(k, v)| {
            (
                String::from_utf8_lossy(&k).into_owned(),
                String::from_utf8_lossy(&v).into_owned(),
            )
        })
        .collect();
    let akamai = akamai(&settings, window_update, &priority_frames, &headers);
    Ok((
        Http2 {
            settings,
            window_update,
            priority_frames,
            headers_priority,
            akamai,
        },
        headers,
    ))
}

fn akamai(
    settings: &[(u16, u32)],
    window_update: u32,
    priority: &[(u32, bool, u32, u8)],
    headers: &[(String, String)],
) -> String {
    let settings = settings
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(";");
    let priority = if priority.is_empty() {
        "0".to_string()
    } else {
        priority
            .iter()
            .map(|(s, e, d, w)| format!("{s}:{}:{d}:{}", *e as u8, *w as u32 + 1))
            .collect::<Vec<_>>()
            .join(",")
    };
    let pseudo = headers
        .iter()
        .filter_map(|(k, _)| k.strip_prefix(':').and_then(|n| n.chars().next()))
        .map(String::from)
        .collect::<Vec<_>>()
        .join(",");
    format!("{settings}|{window_update}|{priority}|{pseudo}")
}

/// Answers stream 1 with 200 and `body`.
pub async fn respond<S: AsyncWrite + Unpin>(s: &mut S, body: &[u8]) -> io::Result<()> {
    let mut out = frame(SETTINGS, 0, 0, &[]);
    out.extend(frame(SETTINGS, ACK, 0, &[]));
    // 0x88 is ":status: 200" from the HPACK static table.
    out.extend(frame(HEADERS, END_HEADERS, 1, &[0x88]));
    for (i, chunk) in body.chunks(16_384).enumerate() {
        let last = (i + 1) * 16_384 >= body.len();
        out.extend(frame(DATA, if last { END_STREAM } else { 0 }, 1, chunk));
    }
    if body.is_empty() {
        out.extend(frame(DATA, END_STREAM, 1, &[]));
    }
    s.write_all(&out).await?;
    s.flush().await
}
