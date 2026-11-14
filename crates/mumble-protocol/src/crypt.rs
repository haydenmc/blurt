//! OCB2-AES128 encryption for Mumble's UDP voice channel.
//!
//! A port of Mumble's `CryptStateOCB2` (src/crypto/CryptStateOCB2.cpp),
//! including its countermeasures against the XEX* attack on OCB2 described in
//! section 9 of <https://eprint.iacr.org/2019/311>. Keys and nonces arrive in
//! the TCP `CryptSetup` message.
//!
//! An encrypted packet is `[iv byte][3-byte tag][ciphertext]`: only the low
//! byte of the 16-byte nonce is sent, and the receiver reconstructs the rest,
//! which lets it detect lost, late, and replayed packets.

use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};

const BLOCK: usize = 16;

/// Bytes an encrypted packet adds in front of the plaintext.
pub const HEADER_LEN: usize = 4;

/// How far back (in packets) a late packet is still accepted.
const LATE_WINDOW: i32 = 30;

type Block = [u8; BLOCK];

/// Packet counters, as reported to the server in `Ping` (`good`, `late`, `lost`, `resync`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CryptStats {
    pub good: u32,
    pub late: u32,
    pub lost: u32,
    pub resync: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecryptError {
    #[error("packet too short")]
    TooShort,
    #[error("packet is a replay or too far out of order")]
    Rejected,
    #[error("authentication failed")]
    BadTag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("refusing to encrypt a block that enables the XEX* attack")]
pub struct EncryptError;

#[derive(Clone)]
pub struct CryptState {
    cipher: Aes128,
    raw_key: Block,
    encrypt_iv: Block,
    decrypt_iv: Block,
    /// For each low IV byte, the second IV byte last seen with it: rejects replays.
    decrypt_history: [u8; 256],
    pub stats: CryptStats,
}

impl std::fmt::Debug for CryptState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material.
        f.debug_struct("CryptState")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl CryptState {
    /// `encrypt_iv` is the server's `client_nonce`, `decrypt_iv` its `server_nonce`.
    pub fn new(key: &[u8], encrypt_iv: &[u8], decrypt_iv: &[u8]) -> Option<Self> {
        let raw_key: Block = key.try_into().ok()?;
        Some(Self {
            cipher: Aes128::new(GenericArray::from_slice(&raw_key)),
            raw_key,
            encrypt_iv: encrypt_iv.try_into().ok()?,
            decrypt_iv: decrypt_iv.try_into().ok()?,
            decrypt_history: [0; 256],
            stats: CryptStats::default(),
        })
    }

    pub fn raw_key(&self) -> &[u8; BLOCK] {
        &self.raw_key
    }

    pub fn encrypt_iv(&self) -> &[u8; BLOCK] {
        &self.encrypt_iv
    }

    pub fn decrypt_iv(&self) -> &[u8; BLOCK] {
        &self.decrypt_iv
    }

    /// Applies a nonce resync from the server (a `CryptSetup` with only `server_nonce`).
    pub fn set_decrypt_iv(&mut self, iv: &[u8]) -> bool {
        match iv.try_into() {
            Ok(iv) => {
                self.decrypt_iv = iv;
                self.stats.resync += 1;
                true
            }
            Err(_) => false,
        }
    }

    /// Encrypts `plain` into `out`, which must hold `plain.len() + HEADER_LEN` bytes.
    pub fn encrypt(&mut self, plain: &[u8], out: &mut [u8]) {
        assert_eq!(out.len(), plain.len() + HEADER_LEN, "output buffer size");

        increment(&mut self.encrypt_iv);
        let iv = self.encrypt_iv;
        let (header, body) = out.split_at_mut(HEADER_LEN);
        // Infallible: with `modify_plain_on_attack`, the dangerous block is altered instead.
        let tag = self
            .ocb_encrypt(plain, body, &iv, true)
            .expect("ocb_encrypt with modification never fails");

        header[0] = iv[0];
        header[1..4].copy_from_slice(&tag[..3]);
    }

    /// Decrypts `packet` into `out`, which must hold `packet.len() - HEADER_LEN` bytes.
    pub fn decrypt(&mut self, packet: &[u8], out: &mut [u8]) -> Result<(), DecryptError> {
        if packet.len() < HEADER_LEN {
            return Err(DecryptError::TooShort);
        }
        assert_eq!(out.len(), packet.len() - HEADER_LEN, "output buffer size");

        let save_iv = self.decrypt_iv;
        let iv_byte = packet[0];
        let current = self.decrypt_iv[0];
        let mut restore = false;
        let mut lost: i32 = 0;
        let mut late: i32 = 0;

        if current.wrapping_add(1) == iv_byte {
            // In order, as expected.
            if iv_byte > current {
                self.decrypt_iv[0] = iv_byte;
            } else if iv_byte < current {
                self.decrypt_iv[0] = iv_byte;
                increment(&mut self.decrypt_iv[1..]);
            } else {
                return Err(DecryptError::Rejected);
            }
        } else {
            // Out of order, or a repeat.
            let mut diff = i32::from(iv_byte) - i32::from(current);
            if diff > 128 {
                diff -= 256;
            } else if diff < -128 {
                diff += 256;
            }

            if iv_byte < current && diff > -LATE_WINDOW && diff < 0 {
                // Late, without wraparound.
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = iv_byte;
                restore = true;
            } else if iv_byte > current && diff > -LATE_WINDOW && diff < 0 {
                // Late, from before the low byte wrapped (last was 0x02, here comes 0xff).
                late = 1;
                lost = -1;
                self.decrypt_iv[0] = iv_byte;
                decrement(&mut self.decrypt_iv[1..]);
                restore = true;
            } else if iv_byte > current && diff > 0 {
                // Lost a few packets.
                lost = i32::from(iv_byte) - i32::from(current) - 1;
                self.decrypt_iv[0] = iv_byte;
            } else if iv_byte < current && diff > 0 {
                // Lost a few packets, and the low byte wrapped.
                lost = 256 - i32::from(current) + i32::from(iv_byte) - 1;
                self.decrypt_iv[0] = iv_byte;
                increment(&mut self.decrypt_iv[1..]);
            } else {
                return Err(DecryptError::Rejected);
            }

            if self.decrypt_history[usize::from(self.decrypt_iv[0])] == self.decrypt_iv[1] {
                self.decrypt_iv = save_iv;
                return Err(DecryptError::Rejected);
            }
        }

        let iv = self.decrypt_iv;
        let (tag, ok) = self.ocb_decrypt(&packet[HEADER_LEN..], out, &iv);
        if !ok || tag[..3] != packet[1..4] {
            self.decrypt_iv = save_iv;
            return Err(DecryptError::BadTag);
        }

        self.decrypt_history[usize::from(self.decrypt_iv[0])] = self.decrypt_iv[1];
        if restore {
            self.decrypt_iv = save_iv;
        }

        self.stats.good += 1;
        apply_signed(&mut self.stats.late, late);
        apply_signed(&mut self.stats.lost, lost);
        Ok(())
    }

    fn aes_encrypt(&self, block: &mut Block) {
        self.cipher
            .encrypt_block(GenericArray::from_mut_slice(block));
    }

    fn aes_decrypt(&self, block: &mut Block) {
        self.cipher
            .decrypt_block(GenericArray::from_mut_slice(block));
    }

    /// OCB2 encryption. Returns the full 16-byte tag, or an error when
    /// `modify_plain_on_attack` is false and the input has the shape the XEX*
    /// attack needs. With it true, such input gets one bit flipped instead
    /// (this happens to digital silence, so it can't simply be refused).
    fn ocb_encrypt(
        &self,
        plain: &[u8],
        encrypted: &mut [u8],
        nonce: &Block,
        modify_plain_on_attack: bool,
    ) -> Result<Block, EncryptError> {
        debug_assert_eq!(plain.len(), encrypted.len());
        let mut ok = true;
        let mut delta = *nonce;
        self.aes_encrypt(&mut delta);
        let mut checksum = [0u8; BLOCK];

        let mut len = plain.len();
        let mut offset = 0;
        while len > BLOCK {
            let chunk: &Block = plain[offset..offset + BLOCK].try_into().unwrap();

            // For the attack, the second-to-last block (the last iteration of
            // this loop) must be all zero except for its final byte.
            let mut flip = false;
            if len - BLOCK <= BLOCK && chunk[..BLOCK - 1].iter().all(|&b| b == 0) {
                if modify_plain_on_attack {
                    flip = true;
                } else {
                    ok = false;
                }
            }

            s2(&mut delta);
            let mut tmp = xor(&delta, chunk);
            if flip {
                tmp[0] ^= 1;
            }
            self.aes_encrypt(&mut tmp);
            encrypted[offset..offset + BLOCK].copy_from_slice(&xor(&delta, &tmp));
            checksum = xor(&checksum, chunk);
            if flip {
                checksum[0] ^= 1;
            }

            len -= BLOCK;
            offset += BLOCK;
        }

        s2(&mut delta);
        let mut pad = xor(&length_block(len), &delta);
        self.aes_encrypt(&mut pad);
        let mut tmp = pad;
        tmp[..len].copy_from_slice(&plain[offset..]);
        checksum = xor(&checksum, &tmp);
        let tmp = xor(&pad, &tmp);
        encrypted[offset..].copy_from_slice(&tmp[..len]);

        s3(&mut delta);
        let mut tag = xor(&delta, &checksum);
        self.aes_encrypt(&mut tag);

        if ok { Ok(tag) } else { Err(EncryptError) }
    }

    /// OCB2 decryption. Returns the full 16-byte tag for the caller to compare,
    /// and `false` if the last block has the shape the XEX* attack produces.
    fn ocb_decrypt(&self, encrypted: &[u8], plain: &mut [u8], nonce: &Block) -> (Block, bool) {
        debug_assert_eq!(plain.len(), encrypted.len());
        let mut delta = *nonce;
        self.aes_encrypt(&mut delta);
        let mut checksum = [0u8; BLOCK];

        let mut len = encrypted.len();
        let mut offset = 0;
        while len > BLOCK {
            let chunk: &Block = encrypted[offset..offset + BLOCK].try_into().unwrap();
            s2(&mut delta);
            let mut tmp = xor(&delta, chunk);
            self.aes_decrypt(&mut tmp);
            let out = xor(&delta, &tmp);
            plain[offset..offset + BLOCK].copy_from_slice(&out);
            checksum = xor(&checksum, &out);

            len -= BLOCK;
            offset += BLOCK;
        }

        s2(&mut delta);
        let mut pad = xor(&length_block(len), &delta);
        self.aes_encrypt(&mut pad);
        let mut tmp = [0u8; BLOCK];
        tmp[..len].copy_from_slice(&encrypted[offset..]);
        let tmp = xor(&tmp, &pad);
        checksum = xor(&checksum, &tmp);
        plain[offset..].copy_from_slice(&tmp[..len]);

        // In an attack, the decrypted last block equals `delta ^ len`; `len`
        // only touches the last byte, so compare all the others.
        let attacked = tmp[..BLOCK - 1] == delta[..BLOCK - 1];

        s3(&mut delta);
        let mut tag = xor(&delta, &checksum);
        self.aes_encrypt(&mut tag);

        (tag, !attacked)
    }
}

/// Little-endian increment with carry, as Mumble advances its IVs.
fn increment(bytes: &mut [u8]) {
    for b in bytes {
        *b = b.wrapping_add(1);
        if *b != 0 {
            break;
        }
    }
}

/// Little-endian decrement with borrow.
fn decrement(bytes: &mut [u8]) {
    for b in bytes {
        let was = *b;
        *b = was.wrapping_sub(1);
        if was != 0 {
            break;
        }
    }
}

/// Adds a signed delta to an unsigned counter the way Mumble does: decrements
/// only apply while the counter is strictly larger than them.
fn apply_signed(counter: &mut u32, delta: i32) {
    if delta > 0 {
        *counter = counter.saturating_add(delta.unsigned_abs());
    } else if i64::from(*counter) > i64::from(delta.unsigned_abs()) {
        *counter -= delta.unsigned_abs();
    }
}

fn xor(a: &Block, b: &Block) -> Block {
    std::array::from_fn(|i| a[i] ^ b[i])
}

/// The bit length of the final block, as a big-endian 128-bit integer.
fn length_block(len: usize) -> Block {
    ((len as u128) * 8).to_be_bytes()
}

/// Doubling in GF(2^128): multiply by x.
fn s2(block: &mut Block) {
    let v = u128::from_be_bytes(*block);
    *block = ((v << 1) ^ ((v >> 127) * 0x87)).to_be_bytes();
}

/// Tripling in GF(2^128): `s2(b) ^ b`.
fn s3(block: &mut Block) {
    let orig = *block;
    s2(block);
    *block = xor(block, &orig);
}

#[cfg(test)]
mod tests {
    //! Ported from Mumble's src/tests/TestCrypt/TestCrypt.cpp.
    use super::*;

    const RAW_KEY: Block = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    const NONCE: Block = [
        0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
        0x00,
    ];
    const SECRET: &[u8; 10] = b"abcdefghi\0";

    /// An encrypting and a decrypting state sharing a key, with the encrypt IV forced to 0x55…
    fn pair() -> (CryptState, CryptState) {
        let key: Block = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
        let iv = [0x55; BLOCK];
        let enc = CryptState::new(&key, &iv, &[0; BLOCK]).unwrap();
        let dec = CryptState::new(&key, &[0; BLOCK], &iv).unwrap();
        (enc, dec)
    }

    fn encrypt(cs: &mut CryptState, plain: &[u8]) -> Vec<u8> {
        let mut out = vec![0; plain.len() + HEADER_LEN];
        cs.encrypt(plain, &mut out);
        out
    }

    fn decrypt(cs: &mut CryptState, packet: &[u8]) -> Result<Vec<u8>, DecryptError> {
        let mut out = vec![0; packet.len().saturating_sub(HEADER_LEN)];
        cs.decrypt(packet, &mut out).map(|()| out)
    }

    #[test]
    fn test_vectors() {
        // From draft-krovetz-ocb-00.txt.
        let cs = CryptState::new(&RAW_KEY, &RAW_KEY, &RAW_KEY).unwrap();

        let tag = cs.ocb_encrypt(&[], &mut [], &RAW_KEY, true).unwrap();
        assert_eq!(
            tag,
            [
                0xBF, 0x31, 0x08, 0x13, 0x07, 0x73, 0xAD, 0x5E, 0xC7, 0x0E, 0xC6, 0x9E, 0x78, 0x75,
                0xA7, 0xB0
            ]
        );

        let source: Vec<u8> = (0..40).collect();
        let mut crypt = [0u8; 40];
        let tag = cs.ocb_encrypt(&source, &mut crypt, &RAW_KEY, true).unwrap();
        assert_eq!(
            tag,
            [
                0x9D, 0xB0, 0xCD, 0xF8, 0x80, 0xF7, 0x3E, 0x3E, 0x10, 0xD4, 0xEB, 0x32, 0x17, 0x76,
                0x66, 0x88
            ]
        );
        assert_eq!(
            crypt,
            [
                0xF7, 0x5D, 0x6B, 0xC8, 0xB4, 0xDC, 0x8D, 0x66, 0xB8, 0x36, 0xA2, 0xB0, 0x8B, 0x32,
                0xA6, 0x36, 0x9F, 0x1C, 0xD3, 0xC5, 0x22, 0x8D, 0x79, 0xFD, 0x6C, 0x26, 0x7F, 0x5F,
                0x6A, 0xA7, 0xB2, 0x31, 0xC7, 0xDF, 0xB9, 0xD5, 0x99, 0x51, 0xAE, 0x9C
            ]
        );
    }

    #[test]
    fn auth_crypt_round_trips_every_length() {
        for len in 0..128usize {
            let cs = CryptState::new(&RAW_KEY, &NONCE, &NONCE).unwrap();
            let src: Vec<u8> = (0..len).map(|i| (i + 1) as u8).collect();
            let mut encrypted = vec![0; len];
            let mut decrypted = vec![0; len];

            let enc_tag = cs.ocb_encrypt(&src, &mut encrypted, &NONCE, true).unwrap();
            let (dec_tag, ok) = cs.ocb_decrypt(&encrypted, &mut decrypted, &NONCE);
            assert!(ok, "len {len}");
            assert_eq!(enc_tag, dec_tag, "len {len}");
            assert_eq!(src, decrypted, "len {len}");
        }
    }

    /// The attack from section 4.1 of https://eprint.iacr.org/2019/311 is detected.
    #[test]
    fn xex_star_attack_is_prevented() {
        let cs = CryptState::new(&RAW_KEY, &NONCE, &NONCE).unwrap();

        let mut src = [0u8; 2 * BLOCK];
        // First block: `len(second block)`; second block: arbitrary.
        src[BLOCK - 1] = (BLOCK * 8) as u8;
        src[BLOCK..].fill(42);
        let mut encrypted = [0u8; 2 * BLOCK];
        let mut decrypted = [0u8; 2 * BLOCK];

        let failed_encrypt = cs.ocb_encrypt(&src, &mut encrypted, &NONCE, false).is_err();

        // Perform the attack: forge a one-block message and its tag.
        encrypted[BLOCK - 1] ^= (BLOCK * 8) as u8;
        let forged_tag: Block = std::array::from_fn(|i| src[BLOCK + i] ^ encrypted[BLOCK + i]);
        let (dec_tag, ok) = cs.ocb_decrypt(&encrypted[..BLOCK], &mut decrypted[..BLOCK], &NONCE);

        // The forged tag matches, so the attack is implemented correctly...
        assert_eq!(forged_tag, dec_tag);
        // ...and both sides detect it.
        assert!(failed_encrypt);
        assert!(!ok);

        // ...and with modification on, the critical block is altered instead.
        let enc_tag = cs.ocb_encrypt(&src, &mut encrypted, &NONCE, true).unwrap();
        let (dec_tag, ok) = cs.ocb_decrypt(&encrypted, &mut decrypted, &NONCE);
        assert!(ok);
        assert_eq!(enc_tag, dec_tag);
        assert_eq!(src[0], 0);
        assert_eq!(decrypted[0], 1);
    }

    #[test]
    fn tampering_is_detected() {
        let mut cs = CryptState::new(&RAW_KEY, &NONCE, &NONCE).unwrap();
        let msg = b"It was a funky funky town!\0";
        let mut encrypted = encrypt(&mut cs, msg);

        for i in 0..msg.len() * 8 {
            encrypted[i / 8] ^= 1 << (i % 8);
            assert!(decrypt(&mut cs, &encrypted).is_err(), "bit {i}");
            encrypted[i / 8] ^= 1 << (i % 8);
        }
        assert_eq!(decrypt(&mut cs, &encrypted).unwrap(), msg);
    }

    #[test]
    fn iv_recovery() {
        let (mut enc, mut dec) = pair();

        let crypted = encrypt(&mut enc, SECRET);
        // Can decrypt, correctly...
        assert_eq!(decrypt(&mut dec, &crypted).unwrap(), SECRET);
        // ...but refuses to reuse the same IV.
        assert!(decrypt(&mut dec, &crypted).is_err());

        // Recovers from lost packets.
        let mut crypted = Vec::new();
        for _ in 0..16 {
            crypted = encrypt(&mut enc, SECRET);
        }
        assert!(decrypt(&mut dec, &crypted).is_ok());

        // Wraparound.
        for _ in 0..128 {
            dec.stats.lost = 0;
            for _ in 0..15 {
                crypted = encrypt(&mut enc, SECRET);
            }
            assert!(decrypt(&mut dec, &crypted).is_ok());
            assert_eq!(dec.stats.lost, 14);
        }
        assert_eq!(enc.encrypt_iv(), dec.decrypt_iv());

        // Wrapped too far.
        for _ in 0..257 {
            crypted = encrypt(&mut enc, SECRET);
        }
        assert!(decrypt(&mut dec, &crypted).is_err());

        // Resync.
        let iv = *enc.encrypt_iv();
        dec.set_decrypt_iv(&iv);
        let crypted = encrypt(&mut enc, SECRET);
        assert!(decrypt(&mut dec, &crypted).is_ok());
    }

    #[test]
    fn reverse_recovery() {
        let (mut enc, mut dec) = pair();

        // Out-of-order recovery up to 30 packets back, but not beyond.
        let crypted: Vec<_> = (0..128).map(|_| encrypt(&mut enc, SECRET)).collect();
        let mut i = 0;
        while i < 30 {
            assert!(decrypt(&mut dec, &crypted[127 - i]).is_ok(), "packet {i}");
            i += 1;
        }
        while i < 128 {
            assert!(decrypt(&mut dec, &crypted[127 - i]).is_err(), "packet {i}");
            i += 1;
        }
        for i in 0..30 {
            assert!(decrypt(&mut dec, &crypted[127 - i]).is_err(), "replay {i}");
        }

        // Extensive replay test.
        let crypted: Vec<_> = (0..512).map(|_| encrypt(&mut enc, SECRET)).collect();
        for packet in &crypted {
            assert!(decrypt(&mut dec, packet).is_ok());
        }
        for packet in &crypted {
            assert!(decrypt(&mut dec, packet).is_err());
        }
    }
}
