// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! NBD server: newstyle fixed handshake, simple replies.
//!
//! Supported options: EXPORT_NAME, GO, INFO, LIST, ABORT. Everything else
//! (STARTTLS, STRUCTURED_REPLY, ...) is answered with NBD_REP_ERR_UNSUP.
//! Supported commands: READ, WRITE (+FUA), FLUSH, TRIM, WRITE_ZEROES, DISC.
//! Requests on one connection are processed strictly in order.

use crate::backend::BlockBackend;
use crate::limiter::Limiter;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const NBDMAGIC: u64 = 0x4e42444d41474943; // "NBDMAGIC"
const IHAVEOPT: u64 = 0x49484156454f5054; // "IHAVEOPT"
const REP_MAGIC: u64 = 0x0003e889045565a9;
const REQ_MAGIC: u32 = 0x25609513;
const SIMPLE_REPLY_MAGIC: u32 = 0x67446698;

const HS_FIXED_NEWSTYLE: u16 = 1;
const HS_NO_ZEROES: u16 = 2;
const CF_FIXED_NEWSTYLE: u32 = 1;
const CF_NO_ZEROES: u32 = 2;

pub const OPT_EXPORT_NAME: u32 = 1;
pub const OPT_ABORT: u32 = 2;
pub const OPT_LIST: u32 = 3;
pub const OPT_INFO: u32 = 6;
pub const OPT_GO: u32 = 7;

pub const REP_ACK: u32 = 1;
pub const REP_SERVER: u32 = 2;
pub const REP_INFO: u32 = 3;
pub const REP_ERR_UNSUP: u32 = 0x8000_0001;
pub const REP_ERR_INVALID: u32 = 0x8000_0003;
pub const REP_ERR_UNKNOWN: u32 = 0x8000_0006;

pub const INFO_EXPORT: u16 = 0;
pub const INFO_BLOCK_SIZE: u16 = 3;

pub const FLAG_HAS_FLAGS: u16 = 1 << 0;
pub const FLAG_READ_ONLY: u16 = 1 << 1;
pub const FLAG_SEND_FLUSH: u16 = 1 << 2;
pub const FLAG_SEND_FUA: u16 = 1 << 3;
pub const FLAG_SEND_TRIM: u16 = 1 << 5;
pub const FLAG_SEND_WRITE_ZEROES: u16 = 1 << 6;

pub const CMD_READ: u16 = 0;
pub const CMD_WRITE: u16 = 1;
pub const CMD_DISC: u16 = 2;
pub const CMD_FLUSH: u16 = 3;
pub const CMD_TRIM: u16 = 4;
pub const CMD_WRITE_ZEROES: u16 = 6;
const CMDF_FUA: u16 = 1 << 0;

pub const EPERM: u32 = 1;
pub const EIO: u32 = 5;
pub const EINVAL: u32 = 22;
pub const ENOSPC: u32 = 28;

const MAX_REQ: u32 = 32 * 1024 * 1024;
const MAX_OPT: u32 = 64 * 1024;

pub struct Export {
    pub name: String,
    pub backend: Arc<dyn BlockBackend>,
    pub read_only: bool,
    pub limiter: Option<Arc<Limiter>>,
}

impl Export {
    pub fn new(name: impl Into<String>, backend: Arc<dyn BlockBackend>) -> Self {
        Self { name: name.into(), backend, read_only: false, limiter: None }
    }
    pub fn read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }
    pub fn limiter(mut self, l: Limiter) -> Self {
        self.limiter = Some(Arc::new(l));
        self
    }
    fn flags(&self) -> u16 {
        let mut f = FLAG_HAS_FLAGS | FLAG_SEND_FLUSH | FLAG_SEND_FUA;
        if self.read_only {
            f |= FLAG_READ_ONLY;
        } else {
            f |= FLAG_SEND_TRIM | FLAG_SEND_WRITE_ZEROES;
        }
        f
    }
}

pub struct Server {
    exports: Vec<Arc<Export>>,
}

impl Server {
    pub fn new(exports: Vec<Export>) -> Arc<Self> {
        Arc::new(Self { exports: exports.into_iter().map(Arc::new).collect() })
    }

    /// Resolve a requested name; the empty name selects the only/first export.
    fn find(&self, name: &str) -> Option<Arc<Export>> {
        if name.is_empty() {
            return self.exports.first().cloned();
        }
        self.exports.iter().find(|e| e.name == name).cloned()
    }

    /// Accept connections until the listener errors.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        loop {
            let (s, _) = listener.accept().await?;
            let _ = s.set_nodelay(true);
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle(s).await {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("fluxvm-blockd: connection ended: {e}");
                    }
                }
            });
        }
    }

    pub async fn handle(&self, mut s: TcpStream) -> io::Result<()> {
        let export = match self.negotiate(&mut s).await? {
            Some(e) => e,
            None => return Ok(()),
        };
        transmit(&mut s, export).await
    }

    async fn negotiate(&self, s: &mut TcpStream) -> io::Result<Option<Arc<Export>>> {
        let mut hello = Vec::with_capacity(18);
        hello.extend_from_slice(&NBDMAGIC.to_be_bytes());
        hello.extend_from_slice(&IHAVEOPT.to_be_bytes());
        hello.extend_from_slice(&(HS_FIXED_NEWSTYLE | HS_NO_ZEROES).to_be_bytes());
        s.write_all(&hello).await?;
        let cflags = s.read_u32().await?;
        if cflags & CF_FIXED_NEWSTYLE == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "client lacks fixed newstyle"));
        }
        let no_zeroes = cflags & CF_NO_ZEROES != 0;
        loop {
            let magic = s.read_u64().await?;
            if magic != IHAVEOPT {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad option magic"));
            }
            let opt = s.read_u32().await?;
            let len = s.read_u32().await?;
            if len > MAX_OPT {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "option too large"));
            }
            let mut data = vec![0u8; len as usize];
            s.read_exact(&mut data).await?;
            match opt {
                OPT_EXPORT_NAME => {
                    let name = String::from_utf8_lossy(&data).into_owned();
                    // No error channel for EXPORT_NAME: just drop the connection.
                    let Some(e) = self.find(&name) else { return Ok(None) };
                    let mut r = Vec::new();
                    r.extend_from_slice(&e.backend.size().to_be_bytes());
                    r.extend_from_slice(&e.flags().to_be_bytes());
                    if !no_zeroes {
                        r.extend_from_slice(&[0u8; 124]);
                    }
                    s.write_all(&r).await?;
                    return Ok(Some(e));
                }
                OPT_ABORT => {
                    reply(s, opt, REP_ACK, &[]).await?;
                    return Ok(None);
                }
                OPT_LIST => {
                    for e in &self.exports {
                        let mut p = (e.name.len() as u32).to_be_bytes().to_vec();
                        p.extend_from_slice(e.name.as_bytes());
                        reply(s, opt, REP_SERVER, &p).await?;
                    }
                    reply(s, opt, REP_ACK, &[]).await?;
                }
                OPT_INFO | OPT_GO => {
                    if data.len() < 6 {
                        reply(s, opt, REP_ERR_INVALID, b"short").await?;
                        continue;
                    }
                    let nlen = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
                    if data.len() < 4 + nlen + 2 {
                        reply(s, opt, REP_ERR_INVALID, b"short").await?;
                        continue;
                    }
                    let name = String::from_utf8_lossy(&data[4..4 + nlen]).into_owned();
                    let Some(e) = self.find(&name) else {
                        reply(s, opt, REP_ERR_UNKNOWN, b"no such export").await?;
                        continue;
                    };
                    let mut p = INFO_EXPORT.to_be_bytes().to_vec();
                    p.extend_from_slice(&e.backend.size().to_be_bytes());
                    p.extend_from_slice(&e.flags().to_be_bytes());
                    reply(s, opt, REP_INFO, &p).await?;
                    let mut bs = INFO_BLOCK_SIZE.to_be_bytes().to_vec();
                    bs.extend_from_slice(&1u32.to_be_bytes()); // min
                    bs.extend_from_slice(&4096u32.to_be_bytes()); // preferred
                    bs.extend_from_slice(&MAX_REQ.to_be_bytes()); // max
                    reply(s, opt, REP_INFO, &bs).await?;
                    reply(s, opt, REP_ACK, &[]).await?;
                    if opt == OPT_GO {
                        return Ok(Some(e));
                    }
                }
                _ => reply(s, opt, REP_ERR_UNSUP, &[]).await?,
            }
        }
    }
}

async fn reply(s: &mut TcpStream, opt: u32, rep: u32, payload: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(20 + payload.len());
    b.extend_from_slice(&REP_MAGIC.to_be_bytes());
    b.extend_from_slice(&opt.to_be_bytes());
    b.extend_from_slice(&rep.to_be_bytes());
    b.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    b.extend_from_slice(payload);
    s.write_all(&b).await
}

async fn send_reply(s: &mut TcpStream, err: u32, handle: u64, data: &[u8]) -> io::Result<()> {
    let mut b = Vec::with_capacity(16 + data.len());
    b.extend_from_slice(&SIMPLE_REPLY_MAGIC.to_be_bytes());
    b.extend_from_slice(&err.to_be_bytes());
    b.extend_from_slice(&handle.to_be_bytes());
    b.extend_from_slice(data);
    s.write_all(&b).await
}

fn errno(e: &io::Error) -> u32 {
    match e.kind() {
        io::ErrorKind::PermissionDenied => EPERM,
        io::ErrorKind::InvalidInput => EINVAL,
        _ => e.raw_os_error().map_or(EIO, |c| c as u32),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| io::Error::other(e.to_string()))?
}

async fn transmit(s: &mut TcpStream, e: Arc<Export>) -> io::Result<()> {
    let size = e.backend.size();
    loop {
        let magic = s.read_u32().await?;
        if magic != REQ_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request magic"));
        }
        let cflags = s.read_u16().await?;
        let cmd = s.read_u16().await?;
        let handle = s.read_u64().await?;
        let off = s.read_u64().await?;
        let len = s.read_u32().await?;
        let in_range = off.checked_add(len as u64).is_some_and(|end| end <= size);

        match cmd {
            CMD_DISC => {
                let b = e.backend.clone();
                let _ = blocking(move || b.flush()).await;
                return Ok(());
            }
            CMD_READ => {
                if len > MAX_REQ || !in_range {
                    send_reply(s, EINVAL, handle, &[]).await?;
                    continue;
                }
                if let Some(l) = &e.limiter {
                    l.acquire(len as u64).await;
                }
                let b = e.backend.clone();
                let r = blocking(move || {
                    let mut buf = vec![0u8; len as usize];
                    b.read_at(off, &mut buf)?;
                    Ok(buf)
                })
                .await;
                match r {
                    Ok(buf) => send_reply(s, 0, handle, &buf).await?,
                    Err(er) => send_reply(s, errno(&er), handle, &[]).await?,
                }
            }
            CMD_WRITE => {
                if len > MAX_REQ {
                    // Cannot resync without reading the payload; drop the link.
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "write too large"));
                }
                let mut data = vec![0u8; len as usize];
                s.read_exact(&mut data).await?;
                if e.read_only {
                    send_reply(s, EPERM, handle, &[]).await?;
                    continue;
                }
                if !in_range {
                    send_reply(s, ENOSPC, handle, &[]).await?;
                    continue;
                }
                if let Some(l) = &e.limiter {
                    l.acquire(len as u64).await;
                }
                let b = e.backend.clone();
                let fua = cflags & CMDF_FUA != 0;
                let r = blocking(move || {
                    b.write_at(off, &data)?;
                    if fua { b.flush() } else { Ok(()) }
                })
                .await;
                send_reply(s, r.err().map_or(0, |x| errno(&x)), handle, &[]).await?;
            }
            CMD_FLUSH => {
                let b = e.backend.clone();
                let r = blocking(move || b.flush()).await;
                send_reply(s, r.err().map_or(0, |x| errno(&x)), handle, &[]).await?;
            }
            CMD_TRIM | CMD_WRITE_ZEROES => {
                if e.read_only {
                    send_reply(s, EPERM, handle, &[]).await?;
                    continue;
                }
                if !in_range {
                    send_reply(s, EINVAL, handle, &[]).await?;
                    continue;
                }
                if let Some(l) = &e.limiter {
                    l.acquire(0).await;
                }
                let b = e.backend.clone();
                let fua = cflags & CMDF_FUA != 0;
                let r = blocking(move || {
                    if cmd == CMD_TRIM {
                        b.trim(off, len as u64)?;
                    } else {
                        b.write_zeroes(off, len as u64)?;
                    }
                    if fua { b.flush() } else { Ok(()) }
                })
                .await;
                send_reply(s, r.err().map_or(0, |x| errno(&x)), handle, &[]).await?;
            }
            _ => {
                // Unknown command: if it carried a payload we cannot know; reply EINVAL.
                send_reply(s, EINVAL, handle, &[]).await?;
            }
        }
    }
}
