// Copyright 2021 Damir Jelić
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use ed25519_dalek::SignatureError;
use prost::Message;
use thiserror::Error;

use crate::{cipher::Mac, Curve25519PublicKey};

#[derive(Error, Debug)]
pub enum DecodeError {
    #[error("The message didn't contain a version")]
    MissingVersion,
    #[error("The message was too short, it didn't contain a valid payload")]
    MessageToShort(usize),
    #[error("The message didn't have a valid version, expected {0}, got {1}")]
    InvalidVersion(u8, u8),
    #[error("The message contained a public key with an invalid size, expected {0}, got {1}")]
    InvalidKeyLength(usize, usize),
    #[error("The message contained a MAC with an invalid size, expected {0}, got {1}")]
    InvalidMacLength(usize, usize),
    #[error("The message contained an invalid Signature: {0}")]
    Signature(#[from] SignatureError),
    #[error(transparent)]
    ProtoBufError(#[from] prost::DecodeError),
}

// The integer encoding logic here has been taken from the integer-encoding[1]
// crate and is under the MIT license.
//
// The MIT License (MIT)
//
// Copyright (c) 2016 Google Inc. (lewinb@google.com) -- though not an official
// Google product or in any way related!
// Copyright (c) 2018-2020 Lewin Bormann (lbo@spheniscida.de)
//
// [1]: https://github.com/dermesser/integer-encoding-rs
pub(crate) trait Encode {
    fn encode(self) -> Vec<u8>;
}

/// Most-significant byte, == 0x80
const MSB: u8 = 0b1000_0000;

/// How many bytes an integer uses when being encoded as a VarInt.
#[inline]
fn required_encoded_space_unsigned(mut v: u64) -> usize {
    if v == 0 {
        return 1;
    }

    let mut logcounter = 0;
    while v > 0 {
        logcounter += 1;
        v >>= 7;
    }
    logcounter
}

impl Encode for usize {
    fn encode(self) -> Vec<u8> {
        (self as u64).encode()
    }
}

impl Encode for u32 {
    fn encode(self) -> Vec<u8> {
        (self as u64).encode()
    }
}

impl Encode for u64 {
    #[inline]
    fn encode(self) -> Vec<u8> {
        let mut v = Vec::new();
        v.resize(required_encoded_space_unsigned(self), 0);

        let mut n = self;
        let mut i = 0;

        while n >= 0x80 {
            v[i] = MSB | (n as u8);
            i += 1;
            n >>= 7;
        }

        v[i] = n as u8;

        v
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OlmMessage {
    inner: Vec<u8>,
}

impl From<Vec<u8>> for OlmMessage {
    fn from(bytes: Vec<u8>) -> Self {
        Self { inner: bytes }
    }
}

impl OlmMessage {
    const VERSION: u8 = 3;

    const RATCHET_TAG: &'static [u8; 1] = b"\x0A";
    const INDEX_TAG: &'static [u8; 1] = b"\x10";
    const CIPHER_TAG: &'static [u8; 1] = b"\x22";

    #[cfg(test)]
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_ref()
    }

    pub fn as_payload_bytes(&self) -> &[u8] {
        let end = self.inner.len();
        &self.inner[..end - 8]
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.inner
    }

    pub(crate) fn append_mac(&mut self, mac: Mac) {
        let truncated = mac.truncate();
        self.append_mac_bytes(&truncated)
    }

    fn append_mac_bytes(&mut self, mac: &[u8; Mac::TRUNCATED_LEN]) {
        let end = self.inner.len();
        self.inner[end - Mac::TRUNCATED_LEN..].copy_from_slice(mac);
    }

    pub(crate) fn decode(&self) -> Result<DecodedMessage, DecodeError> {
        let version = *self.inner.get(0).ok_or(DecodeError::MissingVersion)?;

        if version != Self::VERSION {
            Err(DecodeError::InvalidVersion(Self::VERSION, version))
        } else if self.inner.len() < Mac::TRUNCATED_LEN + 2 {
            Err(DecodeError::MessageToShort(self.inner.len()))
        } else {
            let inner =
                InnerMessage::decode(&self.inner[1..self.inner.len() - Mac::TRUNCATED_LEN])?;

            let mut key = [0u8; Curve25519PublicKey::KEY_LENGTH];
            let mut mac = [0u8; Mac::TRUNCATED_LEN];

            let mac_slice = &self.inner[self.inner.len() - Mac::TRUNCATED_LEN..];

            if inner.ratchet_key.len() != Curve25519PublicKey::KEY_LENGTH {
                Err(DecodeError::InvalidKeyLength(
                    Curve25519PublicKey::KEY_LENGTH,
                    inner.ratchet_key.len(),
                ))
            } else if mac_slice.len() != Mac::TRUNCATED_LEN {
                Err(DecodeError::InvalidMacLength(Mac::TRUNCATED_LEN, mac_slice.len()))
            } else {
                key.copy_from_slice(&inner.ratchet_key);
                mac.copy_from_slice(mac_slice);

                let key = Curve25519PublicKey::from(key);
                let chain_index = inner.chain_index;
                let ciphertext = inner.ciphertext;

                let message = DecodedMessage { ratchet_key: key, chain_index, ciphertext, mac };

                Ok(message)
            }
        }
    }

    fn from_parts_untyped(ratchet_key: &[u8], index: u64, ciphertext: Vec<u8>) -> Self {
        // Prost optimizes away the chain index if it's 0, libolm can't decode
        // this, so encode our messages the pedestrian way instead.
        let index = index.encode();
        let ratchet_len = ratchet_key.len().encode();
        let ciphertext_len = ciphertext.len().encode();

        let message = [
            [Self::VERSION].as_ref(),
            Self::RATCHET_TAG.as_ref(),
            &ratchet_len,
            ratchet_key,
            Self::INDEX_TAG.as_ref(),
            &index,
            Self::CIPHER_TAG.as_ref(),
            &ciphertext_len,
            &ciphertext,
            &[0u8; Mac::TRUNCATED_LEN],
        ]
        .concat();

        Self { inner: message }
    }

    pub fn from_parts(ratchet_key: &Curve25519PublicKey, index: u64, ciphertext: Vec<u8>) -> Self {
        Self::from_parts_untyped(ratchet_key.as_bytes(), index, ciphertext)
    }
}

#[derive(Clone, Debug)]
pub struct PreKeyMessage {
    pub(super) inner: Vec<u8>,
}

impl PreKeyMessage {
    const VERSION: u8 = 3;

    pub fn decode(
        self,
    ) -> Result<(Curve25519PublicKey, Curve25519PublicKey, Curve25519PublicKey, Vec<u8>), DecodeError>
    {
        let version = *self.inner.get(0).ok_or(DecodeError::MissingVersion)?;

        if version != Self::VERSION {
            Err(DecodeError::InvalidVersion(Self::VERSION, version))
        } else {
            let inner = InnerPreKeyMessage::decode(&self.inner[1..self.inner.len()])?;

            let mut one_time_key = [0u8; 32];
            let mut base_key = [0u8; 32];
            let mut identity_key = [0u8; 32];

            if inner.one_time_key.len() != Curve25519PublicKey::KEY_LENGTH {
                Err(DecodeError::InvalidKeyLength(
                    Curve25519PublicKey::KEY_LENGTH,
                    inner.one_time_key.len(),
                ))
            } else if inner.identity_key.len() != Curve25519PublicKey::KEY_LENGTH {
                Err(DecodeError::InvalidKeyLength(
                    Curve25519PublicKey::KEY_LENGTH,
                    inner.identity_key.len(),
                ))
            } else if inner.base_key.len() != Curve25519PublicKey::KEY_LENGTH {
                Err(DecodeError::InvalidKeyLength(
                    Curve25519PublicKey::KEY_LENGTH,
                    inner.base_key.len(),
                ))
            } else {
                one_time_key.copy_from_slice(&inner.one_time_key);
                base_key.copy_from_slice(&inner.base_key);
                identity_key.copy_from_slice(&inner.identity_key);

                let one_time_key = Curve25519PublicKey::from(one_time_key);
                let base_key = Curve25519PublicKey::from(base_key);
                let identity_key = Curve25519PublicKey::from(identity_key);

                Ok((one_time_key, base_key, identity_key, inner.message))
            }
        }
    }

    pub fn from_parts(
        one_time_key: &Curve25519PublicKey,
        base_key: &Curve25519PublicKey,
        identity_key: &Curve25519PublicKey,
        message: Vec<u8>,
    ) -> Self {
        Self::from_parts_untyped_prost(
            one_time_key.as_bytes().to_vec(),
            base_key.as_bytes().to_vec(),
            identity_key.as_bytes().to_vec(),
            message,
        )
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.inner
    }

    fn from_parts_untyped_prost(
        one_time_key: Vec<u8>,
        base_key: Vec<u8>,
        identity_key: Vec<u8>,
        message: Vec<u8>,
    ) -> Self {
        let message = InnerPreKeyMessage { one_time_key, base_key, identity_key, message };

        let mut output: Vec<u8> = vec![0u8; message.encoded_len() + 1];
        output[0] = Self::VERSION;

        message
            .encode(&mut output[1..].as_mut())
            .expect("Couldn't encode our message into a protobuf");

        Self { inner: output }
    }
}

impl From<Vec<u8>> for PreKeyMessage {
    fn from(bytes: Vec<u8>) -> Self {
        Self { inner: bytes }
    }
}

pub(crate) struct DecodedMessage {
    pub ratchet_key: Curve25519PublicKey,
    pub chain_index: u64,
    pub ciphertext: Vec<u8>,
    pub mac: [u8; 8],
}

#[derive(Clone, Message, PartialEq)]
struct InnerMessage {
    #[prost(bytes, tag = "1")]
    pub ratchet_key: Vec<u8>,
    #[prost(uint64, tag = "2")]
    pub chain_index: u64,
    #[prost(bytes, tag = "4")]
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Message)]
struct InnerPreKeyMessage {
    #[prost(bytes, tag = "1")]
    pub one_time_key: Vec<u8>,
    #[prost(bytes, tag = "2")]
    pub base_key: Vec<u8>,
    #[prost(bytes, tag = "3")]
    pub identity_key: Vec<u8>,
    #[prost(bytes, tag = "4")]
    pub message: Vec<u8>,
}

#[cfg(test)]
mod test {
    use super::OlmMessage;

    #[test]
    fn encode() {
        let message = b"\x03\n\nratchetkey\x10\x01\"\nciphertext";
        let message_mac = b"\x03\n\nratchetkey\x10\x01\"\nciphertextMACHEREE";

        let ratchet_key = b"ratchetkey";
        let ciphertext = b"ciphertext";

        let mut encoded = OlmMessage::from_parts_untyped(ratchet_key, 1, ciphertext.to_vec());

        assert_eq!(encoded.as_payload_bytes(), message.as_ref());
        encoded.append_mac_bytes(b"MACHEREE");
        assert_eq!(encoded.as_payload_bytes(), message.as_ref());
        assert_eq!(encoded.as_bytes(), message_mac.as_ref());
    }
}
