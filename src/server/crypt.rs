//-
// Copyright (c) 2016, Jason Lingle
//
// This file is part of Ensync.
//
// Ensync is free software: you can  redistribute it and/or modify it under the
// terms of  the GNU General Public  License as published by  the Free Software
// Foundation, either version  3 of the License, or (at  your option) any later
// version.
//
// Ensync is distributed  in the hope that  it will be useful,  but WITHOUT ANY
// WARRANTY; without  even the implied  warranty of MERCHANTABILITY  or FITNESS
// FOR  A PARTICULAR  PURPOSE.  See the  GNU General  Public  License for  more
// details.
//
// You should have received a copy of the GNU General Public License along with
// Ensync. If not, see <http://www.gnu.org/licenses/>.

//! All the encryption stuff in Ensync.
//!
//! # Choice of encryption (Symmetric vs GPG)
//!
//! Ensync's original design was to use GPG to encrypt files. This mainly has
//! the advantage of being a very known quantity; using asymmetric encryption
//! also allows for interesting possibilities like allowing a semi-trusted
//! client to create files _but not read them back_. Ultimately, this design
//! was dropped in favour of using simple symmetric encryption, for a number of
//! reasons:
//!
//! - Clients always need to be able to re-read the directories they write.
//! Permitting the "write-but-not-read" model would thus require using
//! different keys for directories and files.
//!
//! - Using GPG for encryption would result in a proliferation of the master
//! key(s). There would be no easy way to change what keys had access to the
//! server store.
//!
//! - GPG is pretty slow at handling very large numbers of very small items.
//!
//! # Key derivation
//!
//! For this discussion, we'll consider all forms of seed input to the key
//! derivation system to be the "passphrase". Also look at `KdfList` and
//! `KdfEntry` defined in `serde_types.in.rs`.
//!
//! To protect the user's files, we encrypt them using a passphrase as a key in
//! some way. A trivial approach would be to simply hash the passphrase and use
//! that as the symmetric key. However, this is very vulnerable to brute-force
//! (especially dictionary) attacks and does not permit any form of rekeying.
//!
//! First, we use Scrypt to derive a secondary key. This requires a random
//! salt, as well as parameters that may change in the future; we need to store
//! these somewhere so that later invocations can see what parameters was used
//! to reproduce the key derivation. We thus store in cleartext these
//! parameters for each key.
//!
//! Where to store this? The server protocol already supports a way to store an
//! arbitrary blob with atomic updates -- directories. We thus [ab]use the
//! directory whose id is all zeroes to store this data.
//!
//! We also want to be able to tell whether a passphrase is correct with this
//! data. Even if we only allowed one passphrase and could simply plough ahead
//! with whatever key was derived, we would still want to be able to do this as
//! proceeding with an incorrect key would result in scary "data corrupt"
//! errors if the user mistyped their passphrase. To do this, we also store the
//! SHA-3 of the derived key (*not* the passphrase). Since the derived key
//! effectively has 256 bits of entropy, attempting to reverse this hash is
//! infeasible, and any attacks that reduce that entropy would almost certainly
//! still make it more feasible to break the key derivation function instead.
//!
//! Supporting multiple passphrases is important. There are some use-cases for
//! using multiple in tandem; but more importantly, this support also provides
//! a way to _change_ the passphrase without needing to rebuild the entire data
//! store. To do this, when we initialise the key store, we generate a random
//! master key. Each passphrase as already described produces a secondary key.
//! In the key store, we store the XOR of the master key with each secondary
//! key. In effect, each secondary key is used as a one-time pad to encrypt the
//! master key.
//!
//! To put it all together:
//!
//! - We start off with a passphrase and the key store we fetched.
//!
//! - For each key in the key store:
//!
//! - Apply Scrypt (or whatever algorithms we add in the future) to the
//! passphrase with the stored parameters. Ignore entries not supported.
//!
//! - Hash the derived key. If it does not match the stored hash, move to the
//! next entry.
//!
//! - XOR the derived key with the master key diff and we have the master key.
//!
//! Note that the keys here are actually 256 bits wide, twice the size of an
//! AES key. We thus actually have *two* independent master keys. The first 128
//! bits are used for encryption operations on directories; the second 128 bits
//! are used for encryption operations on objects. The full 256 bits is used as
//! the HMAC secret.
//!
//! # Encrypting objects
//!
//! Since objects are immutable, opaque blobs, their handling is reasonably
//! simple:
//!
//! - Generate a random 128-bit key and IV.
//!
//! - Encrypt those two (two AES blocks) with the object master key in CBC mode
//! with IV 0 (the mode and IV here don't matter since the cleartext is pure
//! entropy) and write that at the beginning of the object. Encrypt the object
//! data in CBC mode using the saved key and IV.
//!
//! Generating a random key for each object ensures that objects with similar
//! prefices are nonetheless different. The random IV may make this stronger,
//! but definitely does not hurt.
//!
//! Objects are padded to the block size with PKCS.
//!
//! # Directory Versions
//!
//! In order to detect reversion attacks, the opaque directory versions are
//! actually encrypted incrementing integers. This is performed as follows:
//!
//! - Encode the version as a little-endian 64-bit integer.
//!
//! - Pad it with 24 zero bytes (to make it the size of a `HashId`).
//!
//! - Encrypt it with AES in CBC mode using the directory key and an IV equal
//! to the two halves of the directory id XORed with each other.
//!
//! Encrypting the integers this way obfuscates how often a directory is
//! usually updated. It does not alone prevent tampering since an attacker
//! could simply choose not to change the version. Because of this, we also
//! store the version in the directory. (This aspect is documented in the
//! directory format docs.)
//!
//! Using an IV based on the directory id means that every directory has a
//! different progression of encrypted version numbers. This prevents an
//! attacker contolling the server from sending a different directory's
//! contents by making an educated guess as to what might have a greater
//! version number than what clients have seen before, which could be used to
//! create an infinite directory tree as part of a padding oracle attack, etc.
//!
//! # Directory Contents
//!
//! The contents of a directory are prefixed with an encrypted key and IV in
//! the same way as objects, except that the directory master key is used.
//!
//! The directory itself is encrypted in CBC mode. Since directory edits
//! require simply appending data to the file, each chunk of data is padded
//! with surrogate 1-byte entries (see the directory format for more details).
//! Appending is done by using the last ciphertext block as the IV.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::result::Result as StdResult;

use chrono::{DateTime, NaiveDateTime, UTC};
use fourleaf;
use keccak;
use rand::{Rng, OsRng};
use rust_crypto::{aes, blockmodes, scrypt};
use rust_crypto::buffer::{BufferResult, ReadBuffer, WriteBuffer,
                          RefReadBuffer, RefWriteBuffer};
use rust_crypto::symmetriccipher::{Decryptor, Encryptor, SymmetricCipherError};

use defs::HashId;
use errors::*;

const SCRYPT_18_8_1: &'static str = "scrypt-18-8-1";
pub const BLKSZ: usize = 16;

/// Stored in cleartext fourleaf as directory `[0u8;32]`.
///
/// This stores the parameters used for the key-derivation function of each
/// passphrase and how to move from a derived key to the master key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdfList {
    pub keys: BTreeMap<String, KdfEntry>,
}

fourleaf_retrofit!(struct KdfList : {} {} {
    |_context, this|
    [1] keys: BTreeMap<String, KdfEntry> = &this.keys,
    { Ok(KdfList { keys: keys }) }
});

/// A single passphrase which may be used to derive the master key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KdfEntry {
    /// The time this (logical) entry was created.
    pub created: DateTime<UTC>,
    /// The time this (logical) entry was last updated.
    pub updated: Option<DateTime<UTC>>,
    /// The time this entry was last used to derive the master key.
    pub used: Option<DateTime<UTC>>,
    /// The algorithm used.
    ///
    /// This includes the parameters used. Note that these are not parsed;
    /// the possible combinations are hardwired.
    ///
    /// The only option right now is "scrypt-18-8-1".
    pub algorithm: String,
    /// The randomly-generated salt.
    pub salt: HashId,
    /// The SHA3 hash of the derived key, to determine whether the key is
    /// correct.
    pub hash: HashId,
    /// The pairwise XOR of the master key with this derived key, allowing
    /// the master key to be derived once this derived key is known.
    pub master_diff: HashId,
}

#[derive(Clone, Copy)]
struct SerDt(DateTime<UTC>);
fourleaf_retrofit!(struct SerDt : {} {} {
    |context, this|
    [1] secs: i64 = this.0.naive_utc().timestamp(),
    [2] nsecs: u32 = this.0.naive_utc().timestamp_subsec_nanos(),
    { NaiveDateTime::from_timestamp_opt(secs, nsecs)
      .ok_or(fourleaf::de::Error::InvalidValueMsg(
          context.to_string(), "invalid timestamp"))
      .map(|ndt| SerDt(DateTime::from_utc(ndt, UTC))) }
});

fourleaf_retrofit!(struct KdfEntry : {} {} {
    |_context, this|
    [1] created: SerDt = SerDt(this.created),
    [2] updated: Option<SerDt> = this.updated.map(SerDt),
    [3] used: Option<SerDt> = this.used.map(SerDt),
    [4] algorithm: String = &this.algorithm,
    [5] salt: HashId = this.salt,
    [6] hash: HashId = this.hash,
    [7] master_diff: HashId = this.master_diff,
    { Ok(KdfEntry { created: created.0,
                    updated: updated.map(|v| v.0),
                    used: used.map(|v| v.0),
                    algorithm: algorithm, salt: salt, hash: hash,
                    master_diff: master_diff }) }
});

thread_local! {
    static RANDOM: RefCell<OsRng> = RefCell::new(
        OsRng::new().expect("Failed to create OsRng"));
}

pub fn rand(buf: &mut [u8]) {
    RANDOM.with(|r| r.borrow_mut().fill_bytes(buf))
}

pub fn rand_hashid() -> HashId {
    let mut h = HashId::default();
    rand(&mut h);
    h
}

#[derive(Clone,Copy,PartialEq,Eq)]
pub struct MasterKey(HashId);

impl MasterKey {
    /// Generates a new, random master key.
    pub fn generate_new() -> Self {
        let mut this = MasterKey(Default::default());
        rand(&mut this.0);
        this
    }

    pub fn dir_key(&self) -> &[u8] {
        &self.0[0..BLKSZ]
    }

    pub fn obj_key(&self) -> &[u8] {
        &self.0[BLKSZ..BLKSZ*2]
    }

    pub fn hmac_secret(&self) -> &[u8] {
        &self.0[..]
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write the SHA3 instead of the actual key so this can't be leaked out
        // accidentally.
        write!(f, "MasterKey(sha3={:?})", sha3(&self.0))
    }
}

fn scrypt_18_8_1(passphrase: &[u8], salt: &[u8]) -> HashId {
    // Scrypt paper recommends n=2**14, r=8, p=1
    // Slides in http://www.tarsnap.com/scrypt/scrypt-slides.pdf suggest
    // n=2**20 for file encryption, but that requires 1GB of memory.
    // As a compromise, we use n=2**18, which needs "only" 256MB.
    //
    // For tests, we implicitly use weaker parameters because scrypt-18-8-1
    // takes forever in debug builds, and it wouldn't make sense to have
    // another algorithm option which exists only for tests to use.
    #[cfg(not(test))] const N: u8  = 18; #[cfg(test)] const N: u8  = 12;
    #[cfg(not(test))] const R: u32 = 8;  #[cfg(test)] const R: u32 = 4;
    let sparms = scrypt::ScryptParams::new(N, R, 1);
    let mut derived: HashId = Default::default();
    scrypt::scrypt(passphrase, &salt, &sparms, &mut derived);

    return derived;
}

fn sha3(data: &[u8]) -> HashId {
    let mut hash = HashId::default();
    let mut kc = keccak::Keccak::new_sha3_256();
    kc.update(&data);
    kc.finalize(&mut hash);
    hash
}

fn hixor(a: &HashId, b: &HashId) -> HashId {
    let mut out = HashId::default();

    for (out, (&a, &b)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *out = a ^ b;
    }

    out
}

/// Creates a new key entry keyed off of the given passphrase which can be used
/// to derive the master key.
///
/// The caller must provide the logic for determining the various date-time
/// fields itself.
pub fn create_key(passphrase: &[u8], master: &MasterKey,
                  created: DateTime<UTC>,
                  updated: Option<DateTime<UTC>>,
                  used: Option<DateTime<UTC>>)
                  -> KdfEntry {
    let mut salt = HashId::default();
    rand(&mut salt);

    let derived = scrypt_18_8_1(passphrase, &salt);
    KdfEntry {
        created: created,
        updated: updated,
        used: used,
        algorithm: SCRYPT_18_8_1.to_owned(),
        salt: salt,
        hash: sha3(&derived),
        master_diff: hixor(&derived, &master.0),
    }
}


/// Attempts to derive the master key from the given single KDF entry.
pub fn try_derive_key_single(passphrase: &[u8], entry: &KdfEntry)
                             -> Option<MasterKey> {
    match entry.algorithm.as_str() {
        SCRYPT_18_8_1 => Some(scrypt_18_8_1(passphrase, &entry.salt)),
        _ => None,
    }.and_then(|derived| {
        if sha3(&derived) == entry.hash {
            Some(MasterKey(hixor(&derived, &entry.master_diff)))
        } else {
            None
        }
    })
}

/// Attempts to derive the master key from the given passphrase and key list.
///
/// If successful, returns the derived master key. Otherwise, returns `None`.
pub fn try_derive_key(passphrase: &[u8], keys: &BTreeMap<String, KdfEntry>)
                      -> Option<MasterKey> {
    keys.iter()
        .filter_map(|(_, k)| try_derive_key_single(passphrase, k))
        .next()
}

// Since rust-crypto uses two traits which are identical except for method
// names, for some reason, which are also incompatible with std::io
trait Cryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>;
}
struct WEncryptor(Box<Encryptor>);
impl Cryptor for WEncryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>
    { self.0.encrypt(input, output, eof) }
}
struct WDecryptor(Box<Decryptor>);
impl Cryptor for WDecryptor {
    fn crypt(&mut self, output: &mut RefWriteBuffer, input: &mut RefReadBuffer,
             eof: bool) -> StdResult<BufferResult, SymmetricCipherError>
    { self.0.decrypt(input, output, eof) }
}

/// Copy `src` into `dst` after passing input bytes through `crypt`.
///
/// If `panic_on_crypt_err` is true and the cryptor itself fails, the process
/// panics. Otherwise, any errors are handled by simply writing to `dst` a
/// number of zero bytes equal to the unconsumed bytes from that pass in `src`.
/// The latter is used on decryption to prevent the software from behaving
/// differently in the presence of invalid padding.
///
/// (The discussion below is not really particular to this function, but it's
/// here to keep it all in one place.)
///
/// Note that while this offers protection against padding oracle attacks,
/// there are still other timing-based attacks that could be performed based on
/// how the data is handled.
///
/// In the case of objects, there are no such attacks, since objects are
/// validated by feeding the whole thing into an HMAC function.
///
/// For directories, we validate each chunk's signature before attempting to
/// parse it. Even if an attacker knew the chunk boundaries (which could be
/// discovered via a sophisticated side-channel attack), a padding-oracle style
/// attack is infeasible since it is necessarily at least as difficult as
/// forging SHA-3 HMAC. (Also note again that directories do not use PKCS
/// padding.)
fn crypt_stream<W : Write, R : Read, C : Cryptor>(
    mut dst: W, mut src: R, mut crypt: &mut C,
    panic_on_crypt_err: bool) -> Result<()>
{
    let mut src_buf = [0u8;4096];
    let mut dst_buf = [0u8;4112]; // Extra space for final padding block
    let mut eof = false;
    while !eof {
        let mut nread = 0;
        while !eof && nread < src_buf.len() {
            let n = try!(src.read(&mut src_buf[nread..]));
            eof |= 0 == n;
            nread += n;
        }

        // Passing src_buf through the cryptor should always result in the
        // entire thing being consumed, as either we have read a multiple of
        // the block size or EOF has been reached.
        let dst_len = {
            let mut dstrbuf = RefWriteBuffer::new(&mut dst_buf);
            let mut srcrbuf = RefReadBuffer::new(&mut src_buf[..nread]);
            match crypt.crypt(&mut dstrbuf, &mut srcrbuf, eof) {
                Ok(_) => {
                    assert!(srcrbuf.is_empty());
                },
                Err(e) => {
                    if panic_on_crypt_err {
                        panic!("Crypt error: {:?}", e);
                    }

                    for d in dstrbuf.take_next(srcrbuf.remaining()) {
                        *d = 0;
                    }
                },
            };
            dstrbuf.position()
        };

        try!(dst.write_all(&dst_buf[..dst_len]));
    }

    Ok(())
}

fn split_key_and_iv(key_and_iv: &[u8;32]) -> ([u8;BLKSZ],[u8;BLKSZ]) {
    let mut key = [0u8;BLKSZ];
    key.copy_from_slice(&key_and_iv[0..BLKSZ]);
    let mut iv = [0u8;BLKSZ];
    iv.copy_from_slice(&key_and_iv[BLKSZ..32]);
    (key, iv)
}

/// Generates and writes the CBC encryption prefix to `dst`.
///
/// `master` is the portion of the master key used to encrypt this prefix.
fn write_cbc_prefix<W : Write>(dst: W, master: &[u8])
                               -> Result<([u8;BLKSZ],[u8;BLKSZ])> {
    let mut key_and_iv = [0u8;32];
    rand(&mut key_and_iv);

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, master, &[0u8;BLKSZ],
        blockmodes::NoPadding));
    try!(crypt_stream(dst, &mut&key_and_iv[..], &mut cryptor, true));

    Ok(split_key_and_iv(&key_and_iv))
}

/// Reads out the data written by `write_cbc_prefix()`.
fn read_cbc_prefix<R : Read>(mut src: R, master: &[u8])
                             -> Result<([u8;BLKSZ],[u8;BLKSZ])> {
    let mut cipher_head = [0u8;32];
    try!(src.read_exact(&mut cipher_head));
    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, master, &[0u8;BLKSZ],
        blockmodes::NoPadding));

    let mut key_and_iv = [0u8;32];
    try!(crypt_stream(&mut&mut key_and_iv[..], &mut&cipher_head[..],
                      &mut cryptor, false));

    Ok(split_key_and_iv(&key_and_iv))
}

/// Encrypts the object data in `src` using the key from `master`, writing the
/// encrypted result to `dst`.
pub fn encrypt_obj<W : Write, R : Read>(mut dst: W, src: R,
                                        master: &MasterKey)
                                        -> Result<()> {
    let (key, iv) = try!(write_cbc_prefix(&mut dst, master.obj_key()));

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::PkcsPadding));
    try!(crypt_stream(dst, src, &mut cryptor, true));
    Ok(())
}

/// Reverses `encrypt_obj()`.
pub fn decrypt_obj<W : Write, R : Read>(dst: W, mut src: R,
                                        master: &MasterKey)
                                        -> Result<()> {
    let (key, iv) = try!(read_cbc_prefix(&mut src, master.obj_key()));

    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::PkcsPadding));
    try!(crypt_stream(dst, src, &mut cryptor, false));
    Ok(())
}

/// Encrypt a whole directory file.
///
/// This generates a new session key and iv. The session key is returned, which
/// can be used with `encrypt_append_dir()` to append more data to the file.
///
/// `src` must produce data which is a multiple of BLKSZ bytes long.
pub fn encrypt_whole_dir<W : Write, R : Read>(mut dst: W, src: R,
                                              master: &MasterKey)
                                              -> Result<[u8;BLKSZ]> {
    let (key, iv) = try!(write_cbc_prefix(&mut dst, master.dir_key()));

    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::NoPadding));
    try!(crypt_stream(dst, src, &mut cryptor, true));
    Ok(key)
}

/// Encrypts data to be appended to a directory.
///
/// `key` is the session key returned by `encrypt_whole_dir()` or
/// `decrypt_whole_dir()`. `iv` is the append-IV returned by `dir_append_iv()`.
pub fn encrypt_append_dir<W : Write, R : Read>(dst: W, src: R,
                                               key: &[u8;BLKSZ], iv: &[u8;BLKSZ])
                                               -> Result<()> {
    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, key, iv, blockmodes::NoPadding));
    try!(crypt_stream(dst, src, &mut cryptor, true));
    Ok(())
}

/// Inverts `encrypt_whole_dir()` and any subsequent calls to
/// `encrypt_append_dir()`.
pub fn decrypt_whole_dir<W : Write, R : Read>(dst: W, mut src: R,
                                              master: &MasterKey)
                                              -> Result<[u8;BLKSZ]> {
    let (key, iv) = try!(read_cbc_prefix(&mut src, master.dir_key()));

    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, &key, &iv, blockmodes::NoPadding));
    try!(crypt_stream(dst, src, &mut cryptor, false));
    Ok(key)
}

/// Given a suffix of the full ciphertext content of a directory `data`, return
/// the IV to pass to `encrypt_append_dir` to append more data to that
/// directory.
pub fn dir_append_iv(data: &[u8]) -> [u8;BLKSZ] {
    let mut iv = [0u8;BLKSZ];
    iv.copy_from_slice(&data[data.len() - BLKSZ..]);
    iv
}

fn dir_ver_iv(dir: &HashId) -> [u8;BLKSZ] {
    let mut iv = [0u8;BLKSZ];
    for ix in 0..8 {
        iv[ix] = dir[ix] ^ dir[ix+BLKSZ];
    }
    iv
}

/// Encrypts the version of the given directory.
pub fn encrypt_dir_ver(dir: &HashId, mut ver: u64, master: &MasterKey)
                       -> HashId {
    let mut cleartext = HashId::default();
    for ix in 0..8 {
        cleartext[ix] = (ver & 0xFF) as u8;
        ver >>= 8;
    }

    let mut res = HashId::default();
    let mut cryptor = WEncryptor(aes::cbc_encryptor(
        aes::KeySize::KeySize128, master.dir_key(),
        &dir_ver_iv(dir),
        blockmodes::NoPadding));
    crypt_stream(&mut res[..], &cleartext[..], &mut cryptor, true)
        .expect("Directory version encryption failed");

    res
}

/// Inverts `encrypt_dir_ver()`.
///
/// If `ciphertext` is invalid, 0 is silently returned instead.
pub fn decrypt_dir_ver(dir: &HashId, ciphertext: &HashId, master: &MasterKey)
                       -> u64 {
    // Initialise to something that we'd reject below
    let mut cleartext = [255u8;32];
    let mut cryptor = WDecryptor(aes::cbc_decryptor(
        aes::KeySize::KeySize128, master.dir_key(),
        &dir_ver_iv(dir),
        blockmodes::NoPadding));
    // Ignore any errors and simply leave `cleartext` initialised to the
    // invalid value.
    let _ = crypt_stream(&mut cleartext[..], &ciphertext[..],
                         &mut cryptor, false);

    // If the padding is invalid, silently return 0 so it gets rejected the
    // same way as simply receding the version.
    for &padding in &cleartext[8..] {
        if 0 != padding {
            return 0;
        }
    }

    let mut ver = 0u64;
    for ix in 0..8 {
        ver |= (cleartext[ix] as u64) << (ix * 8);
    }

    ver
}

#[cfg(test)]
mod test {
    use chrono::UTC;

    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn generate_and_derive_keys() {
        fn ck(passphrase: &[u8], master: &MasterKey) -> KdfEntry {
            create_key(passphrase, master, UTC::now(), None, None)
        }

        let master = MasterKey::generate_new();
        let mut keys = BTreeMap::new();
        keys.insert("a".to_owned(), ck(b"plugh", &master));
        keys.insert("b".to_owned(), ck(b"xyzzy", &master));

        assert_eq!(Some(master), try_derive_key(b"plugh", &keys));
        assert_eq!(Some(master), try_derive_key(b"xyzzy", &keys));
        assert_eq!(None, try_derive_key(b"foo", &keys));
    }
}

// Separate module so only the fast tess can be run when so desired
#[cfg(test)]
mod fast_test {
    use defs::HashId;

    use super::*;

    fn test_crypt_obj(data: &[u8]) {
        let master = MasterKey::generate_new();

        let mut ciphertext = Vec::new();
        encrypt_obj(&mut ciphertext, data, &master).unwrap();

        let mut cleartext = Vec::new();
        decrypt_obj(&mut cleartext, &ciphertext[..], &master).unwrap();

        assert_eq!(data, &cleartext[..]);
    }

    #[test]
    fn crypt_obj_empty() {
        test_crypt_obj(&[]);
    }

    #[test]
    fn crypt_obj_one_block() {
        test_crypt_obj(b"0123456789abcdef");
    }

    #[test]
    fn crypt_obj_partial_single_block() {
        test_crypt_obj(b"hello");
    }

    #[test]
    fn crypt_obj_partial_multi_block() {
        test_crypt_obj(b"This is longer than sixteen bytes.");
    }

    #[test]
    fn crypt_obj_4096() {
        let mut data = [0u8;4096];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_obj_4097() {
        let mut data = [0u8;4097];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_obj_8191() {
        let mut data = [0u8;8191];
        rand(&mut data);
        test_crypt_obj(&data);
    }

    #[test]
    fn crypt_dir_oneshot() {
        let master = MasterKey::generate_new();

        let orig = b"0123456789abcdef0123456789ABCDEF";
        let mut ciphertext = Vec::new();
        let sk1 =
            encrypt_whole_dir(&mut ciphertext, &orig[..], &master).unwrap();

        let mut cleartext = Vec::new();
        let sk2 = decrypt_whole_dir(&mut cleartext, &ciphertext[..],
                                    &master).unwrap();

        assert_eq!(orig, &cleartext[..]);
        assert_eq!(sk1, sk2);
    }

    #[test]
    fn crypt_dir_appended() {
        let master = MasterKey::generate_new();

        let mut ciphertext = Vec::new();
        let sk = encrypt_whole_dir(
            &mut ciphertext, &b"0123456789abcdef"[..], &master).unwrap();
        let iv = dir_append_iv(&ciphertext);
        encrypt_append_dir(
            &mut ciphertext, &b"0123456789ABCDEF"[..], &sk, &iv).unwrap();

        let mut cleartext = Vec::new();
        decrypt_whole_dir(&mut cleartext, &ciphertext[..], &master).unwrap();

        assert_eq!(&b"0123456789abcdef0123456789ABCDEF"[..], &cleartext[..]);
    }

    #[test]
    fn crypt_dir_version() {
        let master = MasterKey::generate_new();

        let mut dir = HashId::default();
        rand(&mut dir);

        assert_eq!(42u64, decrypt_dir_ver(
            &dir, &encrypt_dir_ver(&dir, 42u64, &master), &master));
    }

    #[test]
    fn corrupt_dir_version_decrypted_to_0() {
        let master = MasterKey::generate_new();

        assert_eq!(0u64, decrypt_dir_ver(
            &HashId::default(), &HashId::default(), &master));
    }
}
