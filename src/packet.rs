// Copyright (C) 2018, Cloudflare, Inc.
// Copyright (C) 2018, Alessandro Ghedini
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright
//       notice, this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::cmp;
use std::fmt;
use std::slice;

use ::Result;
use ::Error;

use crypto;
use octets;
use rand;
use ranges;
use recovery;
use stream;

const FORM_BIT: u8 = 0x80;
const KEY_PHASE_BIT: u8 = 0x40;
const DEMUX_BIT: u8 = 0x08;

const TYPE_MASK: u8 = 0x7f;

const MAX_CID_LEN: u8 = 18;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Type {
    Initial,
    Retry,
    Handshake,
    ZeroRTT,
    VersionNegotiation,
    Application,
}

#[derive(Clone)]
pub struct Header {
    pub ty: Type,
    pub version: u32,
    pub flags: u8,
    pub dcid: Vec<u8>,
    pub scid: Vec<u8>,
    pub token: Option<Vec<u8>>,
    pub versions: Option<Vec<u32>>,
}

impl Header {
    pub fn from_slice(buf: &mut [u8], dcil: usize) -> Result<Header> {
        let mut b = octets::Bytes::new(buf);
        Header::from_bytes(&mut b, dcil)
    }

    pub(crate) fn from_bytes(b: &mut octets::Bytes, dcil: usize) -> Result<Header> {
        let first = b.get_u8()?;

        if !Header::is_long(first) {
            // Decode short header.
            let dcid = b.get_bytes(dcil)?;

            return Ok(Header {
                ty: Type::Application,
                flags: first & FORM_BIT,
                version: 0,
                dcid: dcid.to_vec(),
                scid: Vec::new(),
                token: None,
                versions: None,
            });
        }

        // Decode long header.
        let version = b.get_u32()?;

        let ty = if version == 0 {
            Type::VersionNegotiation
        } else {
            match first & TYPE_MASK {
                0x7f => Type::Initial,
                0x7e => Type::Retry,
                0x7d => Type::Handshake,
                0x7c => Type::ZeroRTT,
                _    => return Err(Error::InvalidPacket),
            }
        };

        let (dcil, scil) = match b.get_u8() {
            Ok(v) => {
                let mut dcil = v >> 4;
                let mut scil = v & 0xf;

                if dcil > MAX_CID_LEN || scil > MAX_CID_LEN {
                    return Err(Error::InvalidPacket);
                }

                if dcil > 0 {
                    dcil += 3;
                }

                if scil > 0 {
                    scil += 3;
                }

                (dcil, scil)
            },

            Err(_) => return Err(Error::BufferTooShort),
        };

        let dcid = b.get_bytes(dcil as usize)?.to_vec();
        let scid = b.get_bytes(scil as usize)?.to_vec();

        // End of invariants.

        let mut token: Option<Vec<u8>> = None;
        let mut versions: Option<Vec<u32>> = None;

        match ty {
            Type::Initial => {
                // Only Initial packet have a token.
                token = Some(b.get_bytes_with_varint_length()?.to_vec());
            },

            Type::Retry => {
                panic!("Retry not supported");
            },

            Type::VersionNegotiation => {
                let mut list: Vec<u32> = Vec::new();

                while b.cap() > 0 {
                    let version = b.get_u32()?;
                    list.push(version);
                }

                versions = Some(list);
            },

            _ => (),
        };

        Ok(Header {
            ty,
            flags: first & FORM_BIT,
            version,
            dcid,
            scid,
            token,
            versions,
        })
    }

    pub(crate) fn to_bytes(&self, out: &mut octets::Bytes) -> Result<()> {
        // Encode short header.
        if self.ty == Type::Application {
            let mut first = rand::rand_u8();

            // Unset form bit for short header.
            first &= !FORM_BIT;

            // TODO: support key update
            first &= !KEY_PHASE_BIT;

            // "The third bit (0x20) of octet 0 is set to 1."
            first |= 0x20;

            // "The fourth bit (0x10) of octet 0 is set to 1."
            first |= 0x10;

            // Clear Google QUIC demultiplexing bit
            first &= !DEMUX_BIT;

            out.put_u8(first)?;
            out.put_bytes(&self.dcid)?;

            return Ok(());
        }

        // Encode long header.
        let ty: u8 = match self.ty {
                Type::Initial   => 0x7f,
                Type::Retry     => 0x7e,
                Type::Handshake => 0x7d,
                Type::ZeroRTT   => 0x7c,
                // TODO: unify handling of version negotiation
                _               => return Err(Error::InvalidPacket),
        };

        let first = FORM_BIT | ty;

        out.put_u8(first)?;

        out.put_u32(self.version)?;

        let mut cil: u8 = 0;

        if !self.dcid.is_empty() {
            cil |= ((self.dcid.len() - 3) as u8) << 4;
        }

        if !self.scid.is_empty() {
            cil |= ((self.scid.len() - 3) as u8) & 0xf;
        }

        out.put_u8(cil)?;

        out.put_bytes(&self.dcid)?;
        out.put_bytes(&self.scid)?;

        // Only Initial packet have a token.
        if self.ty == Type::Initial {
            match self.token {
                Some(ref v) => {
                    out.put_bytes(v)?;
                },

                None => {
                    // No token, so lemgth = 0.
                    out.put_varint(0)?;
                }
            }
        }

        Ok(())
    }

    pub fn is_long(b: u8) -> bool {
        b & FORM_BIT != 0
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.ty)?;

        if self.ty != Type::Application {
            write!(f, " vers={:x}", self.version)?;
        }

        write!(f, " dcid=")?;
        for b in &self.dcid {
            write!(f, "{:02x}", b)?;
        }

        if self.ty != Type::Application {
            write!(f, " scid=")?;
            for b in &self.scid {
                write!(f, "{:02x}", b)?;
            }
        }

        if self.ty == Type::VersionNegotiation {
            if let Some(ref versions) = self.versions {
                write!(f, " versions={:x?}", versions)?;
            }
        }

        Ok(())
    }
}

pub fn pkt_num_len(pn: u64) -> Result<usize> {
    let len = if pn < 128 {
        1
    } else if pn < 16384 {
        2
    } else if pn < 1_073_741_824 {
        4
    } else {
        return Err(Error::InvalidPacket);
    };

    Ok(len)
}

pub fn pkt_num_bits(len: usize) -> Result<usize> {
    let bits = match len {
        1 => 7,
        2 => 14,
        4 => 30,
        _ => return Err(Error::InvalidPacket),
    };

    Ok(bits)
}

pub fn decrypt_pkt_num(b: &mut octets::Bytes, aead: &crypto::Open)
                                                    -> Result<(u64,usize)> {
    let max_pn_len = cmp::min(b.cap() - aead.alg().pn_nonce_len(), 4);

    let mut pn_and_sample = b.peek_bytes(max_pn_len + aead.alg().pn_nonce_len())?;

    let (mut ciphertext, sample) = pn_and_sample.split_at(max_pn_len).unwrap();

    let ciphertext = ciphertext.as_mut();

    // Decrypt first byte of pkt num into separate buffer to get length.
    let mut first: u8 = ciphertext[0];

    aead.xor_keystream(sample.as_ref(), slice::from_mut(&mut first))?;

    let len = if first >> 7 == 0 {
        1
    } else {
        // Map most significant 2 bits to actual pkt num length.
        match first >> 6 {
            2 => 2,
            3 => 4,
            _ => return Err(Error::InvalidPacket),
        }
    };

    // Decrypt full pkt num in-place.
    aead.xor_keystream(sample.as_ref(), &mut ciphertext[..len])?;

    let mut plaintext = Vec::with_capacity(len);
    plaintext.extend_from_slice(&ciphertext[..len]);

    // Mask the 2 most significant bits to remove the encoded length.
    if len > 1 {
        plaintext[0] &= 0x3f;
    }

    let mut b = octets::Bytes::new(&mut plaintext);

    // Extract packet number corresponding to the decoded length.
    let out = match len {
        1 => u64::from(b.get_u8()?),
        2 => u64::from(b.get_u16()?),
        4 => u64::from(b.get_u32()?),
        _ => return Err(Error::InvalidPacket),
    };

    Ok((out as u64, len))
}

pub fn decode_pkt_num(largest_pn: u64, truncated_pn: u64, pn_len: usize) -> Result<u64> {
    let pn_nbits     = pkt_num_bits(pn_len)?;
    let expected_pn  = largest_pn + 1;
    let pn_win       = 1 << pn_nbits;
    let pn_hwin      = pn_win / 2;
    let pn_mask      = pn_win - 1;
    let candidate_pn = (expected_pn & !pn_mask) | truncated_pn;

    if candidate_pn + pn_hwin <= expected_pn {
         return Ok(candidate_pn + pn_win);
    }

    if candidate_pn > expected_pn + pn_hwin && candidate_pn > pn_win {
        return Ok(candidate_pn - pn_win);
    }

    Ok(candidate_pn)
}

pub fn encode_pkt_num(pn: u64, b: &mut octets::Bytes) -> Result<()> {
    let len = pkt_num_len(pn)?;

    match len {
        1 => {
            let mut buf = b.put_u8(pn as u8)?;
            buf[0] &= !0x80;
        },

        2 => {
            let mut buf = b.put_u16(pn as u16)?;
            buf[0] &= !0xc0;
            buf[0] |= 0x80;
        },

        4 => {
            let mut buf = b.put_u32(pn as u32)?;
            buf[0] |= 0xc0;
        },

        _ => return Err(Error::InvalidPacket),
    };

    Ok(())
}

pub fn negotiate_version(hdr: &Header, out: &mut [u8]) -> Result<usize> {
    let mut b = octets::Bytes::new(out);

    let first = rand::rand_u8() | FORM_BIT;

    b.put_u8(first)?;
    b.put_u32(0)?;

    // Invert client's scid and dcid.
    let mut cil: u8 = 0;
    if !hdr.scid.is_empty() {
        cil |= ((hdr.scid.len() - 3) as u8) << 4;
    }

    if !hdr.dcid.is_empty() {
        cil |= ((hdr.dcid.len() - 3) as u8) & 0xf;
    }

    b.put_u8(cil)?;
    b.put_bytes(&hdr.scid)?;
    b.put_bytes(&hdr.dcid)?;
    b.put_u32(::VERSION_DRAFT15)?;

    Ok(b.off())
}

pub struct PktNumSpace {
    pub pkt_type: Type,

    pub largest_rx_pkt_num: u64,

    pub next_pkt_num: u64,

    pub recv_pkt_num: ranges::RangeSet,

    pub flight: recovery::InFlight,

    pub do_ack: bool,

    pub crypto_level: crypto::Level,

    pub crypto_open: Option<crypto::Open>,
    pub crypto_seal: Option<crypto::Seal>,

    pub crypto_stream: stream::Stream,
}

impl PktNumSpace {
    pub fn new(ty: Type, crypto_level: crypto::Level) -> PktNumSpace {
        PktNumSpace {
            pkt_type: ty,

            largest_rx_pkt_num: 0,

            next_pkt_num: 0,

            recv_pkt_num: ranges::RangeSet::default(),

            flight: recovery::InFlight::default(),

            do_ack: false,

            crypto_level,

            crypto_open: None,
            crypto_seal: None,

            crypto_stream: stream::Stream::new(std::usize::MAX, std::usize::MAX),
        }
    }

    pub fn clear(&mut self) {
        self.flight = recovery::InFlight::default();
        self.crypto_stream = stream::Stream::default();
    }

    pub fn cipher(&self) -> crypto::Algorithm {
        match self.crypto_open {
            Some(ref v) => v.alg(),
            None => crypto::Algorithm::Null,
        }
    }

    pub fn overhead(&self) -> usize {
        self.crypto_seal.as_ref().unwrap().alg().tag_len()
    }

    pub fn ready(&self) -> bool {
        self.crypto_stream.writable() || !self.flight.lost.is_empty() || self.do_ack
    }
}
