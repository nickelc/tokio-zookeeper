use super::error::ZkError;
use super::request::{MultiHeader, OpCode};
use crate::{Acl, KeeperState, Permission, Stat, WatchedEvent, WatchedEventType};
use byteorder::{BigEndian, ReadBytesExt};
use failure;
use std::io::{self, Read};

#[derive(Debug)]
pub(crate) enum Response {
    Connect {
        protocol_version: i32,
        timeout: i32,
        session_id: i64,
        password: Vec<u8>,
        read_only: bool,
    },
    Stat(Stat),
    GetData {
        bytes: Vec<u8>,
        stat: Stat,
    },
    GetAcl {
        acl: Vec<Acl>,
        stat: Stat,
    },
    Empty,
    Strings(Vec<String>),
    String(String),
    Multi(Vec<Result<Response, ZkError>>),
}

pub trait ReadFrom: Sized {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Self>;
}

impl ReadFrom for Vec<String> {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Self> {
        let len = r#try!(read.read_i32::<BigEndian>());
        let mut items = Vec::with_capacity(len as usize);
        for _ in 0..len {
            items.push(r#try!(read.read_string()));
        }
        Ok(items)
    }
}

impl ReadFrom for Stat {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Stat> {
        Ok(Stat {
            czxid: r#try!(read.read_i64::<BigEndian>()),
            mzxid: r#try!(read.read_i64::<BigEndian>()),
            ctime: r#try!(read.read_i64::<BigEndian>()),
            mtime: r#try!(read.read_i64::<BigEndian>()),
            version: r#try!(read.read_i32::<BigEndian>()),
            cversion: r#try!(read.read_i32::<BigEndian>()),
            aversion: r#try!(read.read_i32::<BigEndian>()),
            ephemeral_owner: r#try!(read.read_i64::<BigEndian>()),
            data_length: r#try!(read.read_i32::<BigEndian>()),
            num_children: r#try!(read.read_i32::<BigEndian>()),
            pzxid: r#try!(read.read_i64::<BigEndian>()),
        })
    }
}

impl ReadFrom for WatchedEvent {
    fn read_from<R: Read>(read: &mut R) -> io::Result<WatchedEvent> {
        let wtype = read.read_i32::<BigEndian>()?;
        let state = read.read_i32::<BigEndian>()?;
        let path = read.read_string()?;
        Ok(WatchedEvent {
            event_type: WatchedEventType::from(wtype),
            keeper_state: KeeperState::from(state),
            path,
        })
    }
}

impl ReadFrom for Vec<Acl> {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Self> {
        let len = r#try!(read.read_i32::<BigEndian>());
        let mut items = Vec::with_capacity(len as usize);
        for _ in 0..len {
            items.push(r#try!(Acl::read_from(read)));
        }
        Ok(items)
    }
}

impl ReadFrom for Acl {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Self> {
        let perms = r#try!(Permission::read_from(read));
        let scheme = r#try!(read.read_string());
        let id = r#try!(read.read_string());
        Ok(Acl { perms, scheme, id })
    }
}

impl ReadFrom for Permission {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Self> {
        Ok(Permission::from_raw(r#try!(read.read_u32::<BigEndian>())))
    }
}

impl ReadFrom for MultiHeader {
    fn read_from<R: Read>(read: &mut R) -> io::Result<Self> {
        let opcode = read.read_i32::<BigEndian>()?;
        let done = read.read_u8()? != 0;
        let err = read.read_i32::<BigEndian>()?;
        if done {
            Ok(MultiHeader::Done)
        } else if opcode == -1 {
            Ok(MultiHeader::NextErr(err.into()))
        } else {
            Ok(MultiHeader::NextOk(opcode.into()))
        }
    }
}

pub trait BufferReader: Read {
    fn read_buffer(&mut self) -> io::Result<Vec<u8>>;
}

impl<R: Read> BufferReader for R {
    fn read_buffer(&mut self) -> io::Result<Vec<u8>> {
        let len = r#try!(self.read_i32::<BigEndian>());
        let len = if len < 0 { 0 } else { len as usize };
        let mut buf = vec![0; len];
        let read = r#try!(self.read(&mut buf));
        if read == len {
            Ok(buf)
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "read_buffer failed",
            ))
        }
    }
}

trait StringReader: Read {
    fn read_string(&mut self) -> io::Result<String>;
}

impl<R: Read> StringReader for R {
    fn read_string(&mut self) -> io::Result<String> {
        let raw = r#try!(self.read_buffer());
        Ok(String::from_utf8(raw).unwrap())
    }
}

impl Response {
    pub(super) fn parse(opcode: OpCode, reader: &mut &[u8]) -> Result<Self, failure::Error> {
        match opcode {
            OpCode::CreateSession => Ok(Response::Connect {
                protocol_version: reader.read_i32::<BigEndian>()?,
                timeout: reader.read_i32::<BigEndian>()?,
                session_id: reader.read_i64::<BigEndian>()?,
                password: reader.read_buffer()?,
                read_only: reader.read_u8()? != 0,
            }),
            OpCode::Exists | OpCode::SetData | OpCode::SetACL => {
                Ok(Response::Stat(Stat::read_from(reader)?))
            }
            OpCode::GetData => Ok(Response::GetData {
                bytes: reader.read_buffer()?,
                stat: Stat::read_from(reader)?,
            }),
            OpCode::Delete => Ok(Response::Empty),
            OpCode::GetChildren => Ok(Response::Strings(Vec::<String>::read_from(reader)?)),
            OpCode::Create => Ok(Response::String(reader.read_string()?)),
            OpCode::GetACL => Ok(Response::GetAcl {
                acl: Vec::<Acl>::read_from(reader)?,
                stat: Stat::read_from(reader)?,
            }),
            OpCode::Check => Ok(Response::Empty),
            OpCode::Multi => {
                let mut responses = Vec::new();
                loop {
                    match MultiHeader::read_from(reader)? {
                        MultiHeader::NextErr(e) => {
                            responses.push(Err(e));
                            let _ = reader.read_i32::<BigEndian>()?;
                        }
                        MultiHeader::NextOk(opcode) => {
                            responses.push(Ok(Response::parse(opcode, reader)?));
                        }
                        MultiHeader::Done => break,
                    }
                }
                Ok(Response::Multi(responses))
            }
            _ => panic!("got unexpected response opcode {:?}", opcode),
        }
    }
}
