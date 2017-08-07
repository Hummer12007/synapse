use std::net::{TcpStream};
use std::{time, str, result, mem};
use std::io::{self, Write};

use base64;
use httparse;
use ring::digest;

use super::reader::Reader;
use super::writer::Writer;
use super::proto::ws::{Message, Frame, Opcode};
use super::{Result, ResultExt, ErrorKind};
use util::{IOR, aread};

pub struct Client {
    conn: TcpStream,
    r: Reader,
    w: Writer,
    buf: FragBuf,
    last_action: time::Instant,
}

pub struct Incoming {
    conn: TcpStream,
    key: Option<String>,
    buf: [u8; 1024],
    pos: usize,
    last_action: time::Instant,
}

pub enum IncomingStatus {
    Incomplete,
    Upgrade,
    Transfer { data: Vec<u8>, token: String },
}

enum FragBuf {
    None,
    Text(Vec<u8>),
    Binary(Vec<u8>),
}

impl Client {
    pub fn read(&mut self) -> Result<Option<Frame>> {
        loop {
            match self.read_frame()? {
                Ok(f) => return Ok(Some(f)),
                Err(true) => return Ok(None),
                Err(false) => continue,
            }
        }
    }

    fn read_frame(&mut self) -> Result<result::Result<Frame, bool>> {
        let m = match self.r.read(&mut self.conn).chain_err(|| ErrorKind::IO)? {
            Some(m) => m,
            None => return Ok(Err(true)),
        };
        if m.opcode().is_control() && m.len > 125 {
            return Err(ErrorKind::BadPayload("Control frame too long!").into())
        }
        if m.opcode().is_control() && !m.fin() {
            return Err(ErrorKind::BadPayload("Control frame must not be fragmented!").into())
        }
        if m.opcode().is_other() {
            return Err(ErrorKind::BadPayload("Non standard opcodes unsupported!").into())
        }
        if m.extensions() {
            return Err(ErrorKind::BadPayload("Connection should not contain RSV bits!").into())
        }
        match m.opcode() {
            Opcode::Close => {
                self.send_msg(Message::close())?;
                return Err(ErrorKind::Complete.into());
            }
            Opcode::Text
                | Opcode::Binary
                | Opcode::Continuation => {
                    if let Some(f) = self.buf.process(m)? {
                        #[cfg(feature = "autobahn")]
                        self.send(f)?;
                        #[cfg(not(feature = "autobahn"))]
                        return Ok(Ok(f));
                    }
                }
            Opcode::Ping => {
                self.send_msg(Message::pong(m.data))?;
            }
            _ => { }
        }
        Ok(Err(false))
    }

    pub fn write(&mut self) -> Result<()> {
        self.w.write(&mut self.conn).chain_err(|| ErrorKind::IO)
    }

    pub fn send(&mut self, f: Frame) -> Result<()> {
        self.send_msg(f.into())
    }

    fn send_msg(&mut self, msg: Message) -> Result<()> {
        self.w.enqueue(msg);
        self.write()
    }
}

impl Into<TcpStream> for Client {
    fn into(self) -> TcpStream {
        self.conn
    }
}

impl Into<Client> for Incoming {
    fn into(mut self) -> Client {
        let mut ctx = digest::Context::new(&digest::SHA1);
        let magic = self.key.unwrap() + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
        ctx.update(magic.as_bytes());
        let digest = ctx.finish();
        let accept = base64::encode(digest.as_ref());
        let lines = vec![
            format!("HTTP/1.1 101 Switching Protocols"),
            format!("Connection: upgrade"),
            format!("Upgrade: websocket"),
            format!("Sec-WebSocket-Accept: {}", accept),
        ];
        let data = lines.join("\r\n") + "\r\n\r\n";
        // Ignore error, it'll pop up again anyways
        self.conn.write(data.as_bytes());

        Client {
            r: Reader::new(),
            w: Writer::new(),
            buf: FragBuf::None,
            conn: self.conn,
            last_action: time::Instant::now(),
        }
    }
}

impl Into<TcpStream> for Incoming {
    fn into(self) -> TcpStream {
        self.conn
    }
}

impl Incoming {
    pub fn new(conn: TcpStream) -> Incoming {
        conn.set_nonblocking(true).unwrap();
        Incoming {
            conn,
            buf: [0; 1024],
            pos: 0,
            last_action: time::Instant::now(),
            key: None,
        }
    }

    /// Result indicates if the Incoming connection is
    /// valid to be upgraded into a Client
    pub fn readable(&mut self) -> io::Result<IncomingStatus> {
        loop {
            match aread(&mut self.buf[self.pos..], &mut self.conn) {
                // TODO: Consider more
                IOR::Complete => return Err(io::ErrorKind::InvalidData.into()),
                IOR::Incomplete(a) => {
                    self.pos += a;
                    let mut headers = [httparse::EMPTY_HEADER; 24];
                    let mut req = httparse::Request::new(&mut headers);
                    match req.parse(&self.buf[..self.pos]) {
                        Ok(httparse::Status::Partial) => continue,
                        Ok(httparse::Status::Complete(idx)) => {
                            if let Ok(k) = validate_upgrade(&req) {
                                self.key = Some(k);
                                return Ok(IncomingStatus::Upgrade);
                            } else if let Some(token) = validate_tx(&req) {
                                return Ok(IncomingStatus::Transfer { data: self.buf[idx..self.pos].to_owned(), token });
                            } else {
                                // Probably some dumb CORS OPTION shit, just tell the client
                                // everyting's cool and close up

                                let lines = vec![
                                    format!("HTTP/1.1 200 OK"),
                                    format!("Connection: Closed"),
                                    format!("Access-Control-Allow-Origin: {}", "*"),
                                    format!("Access-Control-Allow-Methods: {}", "OPTIONS, POST, GET"),
                                    format!("Access-Control-Allow-Headers: {}", "Access-Control-Allow-Headers, Origin, Accept, X-Requested-With, Content-Type, Access-Control-Request-Method, Access-Control-Request-Headers, Authorization"),
                                ];
                                let data = lines.join("\r\n") + "\r\n\r\n";
                                // Ignore error, it'll pop up again anyways
                                self.conn.write(data.as_bytes());
                                return Err(io::ErrorKind::InvalidData.into());
                            }
                        }
                        Err(_) => return Err(io::ErrorKind::InvalidData.into()),
                    }
                }
                IOR::Blocked => return Ok(IncomingStatus::Incomplete),
                IOR::EOF => return Err(io::ErrorKind::UnexpectedEof.into()),
                IOR::Err(e) => return Err(e),
            }
        }
    }
}

impl FragBuf {
    fn process(&mut self, msg: Message) -> Result<Option<Frame>> {
        let fin = msg.fin();
        let s = mem::replace(self, FragBuf::None);
        *self = match (s, msg.opcode()) {
            (FragBuf::None, Opcode::Text) => {
                FragBuf::Text(msg.data)
            }
            (FragBuf::None, Opcode::Binary) => {
                FragBuf::Binary(msg.data)
            }
            (FragBuf::None, Opcode::Continuation) => {
                return Err(ErrorKind::BadPayload("Invalid continuation frame").into());
            }
            (FragBuf::Text(mut b), Opcode::Continuation) => {
                b.extend(msg.data.into_iter());
                FragBuf::Text(b)
            }
            (FragBuf::Binary(mut b), Opcode::Continuation) => {
                b.extend(msg.data.into_iter());
                FragBuf::Binary(b)
            }
            (FragBuf::Text(_), Opcode::Text)
                | (FragBuf::Text(_), Opcode::Binary)
                | (FragBuf::Binary(_), Opcode::Text)
                | (FragBuf::Binary(_), Opcode::Binary) => {
                    return Err(ErrorKind::BadPayload("Expected continuation of data frame").into());
                }
            _ => return Ok(None),
        };
        if fin {
            match mem::replace(self, FragBuf::None) {
                FragBuf::Text(b) => {
                    let t = String::from_utf8(b).chain_err(|| ErrorKind::BadPayload("Invalid Utf8 in text!"))?;
                    Ok(Some(Frame::Text(t)))
                }
                FragBuf::Binary(b) => Ok(Some(Frame::Binary(b))),
                FragBuf::None => unreachable!(),
            }
        } else {
            Ok(None)
        }
    }
}

// TODO: We're not really checking HTTP semantics here, might be worth
// considering.
fn validate_tx(req: &httparse::Request) -> Option<String> {
    for header in req.headers.iter() {
        if header.name.to_lowercase() == "authorization" {
            return str::from_utf8(header.value)
                .ok()
                .and_then(|v| {
                    if v.starts_with("Bearer ") {
                        let (_, tok) = v.split_at(7);
                        Some(tok.to_owned())
                    } else {
                        None
                    }
                });
        }
    }
    None
}

fn validate_upgrade(req: &httparse::Request) -> result::Result<String, ()> {
    if !req.method.map(|m| m == "GET").unwrap_or(false) {
        return Err(());
    }

    let mut conn = None;
    let mut upgrade = None;
    let mut key = None;
    let mut version = None;

    for header in req.headers.iter() {
        if header.name == "Connection" {
            conn = str::from_utf8(header.value).ok();
        }
        if header.name == "Upgrade" {
            upgrade = str::from_utf8(header.value).ok();
        }
        if header.name == "Sec-WebSocket-Key" {
            key = str::from_utf8(header.value).ok();
        }
        if header.name == "Sec-WebSocket-Version" {
            version = str::from_utf8(header.value).ok();
        }
    }

    if conn != Some("Upgrade") {
        return Err(());
    }
    if upgrade.map(|s| s.to_lowercase()) != Some("websocket".to_owned()) {
        return Err(());
    }

    if version != Some("13") {
        return Err(());
    }

    if let Some(k) = key {
        return Ok(k.to_owned());
    }
    return Err(());
}
