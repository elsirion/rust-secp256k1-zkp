use core::fmt;
///! This module implements high-level Rust bindings for a Schnorr-based
///! multi-signature scheme called MuSig2 (https://eprint.iacr.org/2020/1261).
///! It is compatible with bip-schnorr.
///!
///! The module also supports adaptor signatures as described in
///! https://github.com/ElementsProject/scriptless-scripts/pull/24
///!
///! The documentation in this include file is for reference and may not be sufficient
///! for users to begin using the library. A full description of the C API usage can be found
///! in [C-musig.md](secp256k1-sys/depend/secp256k1/src/modules/musig/musig.md), and Rust API
///! usage can be found in [Rust-musig.md](USAGE.md).
use {core, std};

use crate::ffi::{self, CPtr};
use secp256k1::Parity;
use crate::ZERO_TWEAK;
use crate::{schnorr, KeyPair, XOnlyPublicKey};
use crate::{Message, PublicKey, Secp256k1, SecretKey, Tweak};
use crate::{Signing, Verification};

///  Data structure containing auxiliary data generated in `pubkey_agg` and
///  required for `session_*_init`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MusigKeyAggCache(ffi::MusigKeyaggCache, XOnlyPublicKey);

impl CPtr for MusigKeyAggCache {
    type Target = ffi::MusigKeyaggCache;

    fn as_c_ptr(&self) -> *const Self::Target {
        self.as_ptr()
    }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target {
        self.as_mut_ptr()
    }
}

impl MusigKeyAggCache {
    /// Create a new [`MusigKeyAggCache`] by supplying a list of PublicKeys used in the session
    ///
    /// Computes a combined public key and the hash of the given public keys.
    ///
    /// Different orders of `pubkeys` result in different `agg_pk`s.
    ///
    /// The pubkeys can be sorted lexicographically before combining with which
    /// ensures the same resulting `agg_pk` for the same multiset of pubkeys.
    /// This is useful to do before aggregating pubkeys, such that the order of pubkeys
    /// does not affect the combined public key.
    ///
    /// # Returns
    ///
    ///  A pair ([`MusigKeyAggCache`], [`XOnlyPublicKey`]) where the first element is the `key_agg_cache`.
    /// This can be used to [`MusigKeyAggCache::nonce_gen`] and [`MusigKeyAggCache::nonce_process`]. The second
    /// element is the resultant Musig aggregated public key.
    ///
    /// #Args:
    ///
    /// * `secp` - Secp256k1 context object initialized for verification
    /// * `pubkeys` - Input array of public keys to combine. The order is important; a
    /// different order will result in a different combined public key
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{MusigKeyAggCache, Secp256k1, SecretKey, KeyPair, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// let _agg_pk = key_agg_cache.agg_pk();
    /// # }
    /// ```
    pub fn new<C: Verification>(secp: &Secp256k1<C>, pubkeys: &[XOnlyPublicKey]) -> Self {
        let cx = *secp.ctx();
        let xonly_ptrs = pubkeys.iter().map(|k| k.as_ptr()).collect::<Vec<_>>();
        let mut key_agg_cache = ffi::MusigKeyaggCache::new();

        unsafe {
            let mut agg_pk = XOnlyPublicKey::from(ffi::XOnlyPublicKey::new());
            if ffi::secp256k1_musig_pubkey_agg(
                cx,
                // FIXME: passing null pointer to ScratchSpace uses less efficient algorithm
                // Need scratch_space_{create,destroy} exposed in public C API to safely handle
                // memory
                core::ptr::null_mut(),
                agg_pk.as_mut_ptr(),
                &mut key_agg_cache,
                xonly_ptrs.as_ptr() as *const *const _,
                xonly_ptrs.len(),
            ) == 0
            {
                // Returns 0 only if the keys are malformed that never happens in safe rust type system.
                unreachable!("Invalid XOnlyPublicKey in input pubkeys")
            } else {
                MusigKeyAggCache(key_agg_cache, agg_pk)
            }
        }
    }

    /// Obtains the aggregate public key for this [`MusigKeyAggCache`]
    pub fn agg_pk(&self) -> XOnlyPublicKey {
        self.1
    }

    /// Apply ordinary "EC" tweaking to a public key in a [`MusigKeyAggCache`] by
    /// adding the generator multiplied with `tweak32` to it. Returns the tweaked [`PublicKey`].
    /// This is useful for deriving child keys from an aggregate public key via BIP32.
    ///
    /// This function is required if you want to _sign_ for a tweaked aggregate key.
    /// On the other hand, if you are only computing a public key, but not intending
    /// to create a signature for it, use [`secp256k1::PublicKey::add_exp_assign`]
    /// instead.
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for verification
    /// * `tweak`: tweak of type [`SecretKey`] with which to tweak the aggregated key
    ///
    /// # Errors:
    ///
    /// If resulting public key would be invalid (only when the tweak is the negation of the corresponding
    /// secret key). For uniformly random 32-byte arrays(for example, in BIP 32 derivation) the chance of
    /// being invalid is negligible (around 1 in 2^128).
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{MusigKeyAggCache, Secp256k1, SecretKey, KeyPair, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let mut key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    ///
    /// let tweak = SecretKey::from_slice(&[2; 32]).unwrap();
    /// let _tweaked_key = key_agg_cache.pubkey_ec_tweak_add(&secp, tweak).unwrap();
    /// # }
    /// ```
    pub fn pubkey_ec_tweak_add<C: Verification>(
        &mut self,
        secp: &Secp256k1<C>,
        tweak: SecretKey,
    ) -> Result<PublicKey, MusigTweakErr> {
        let cx = *secp.ctx();
        unsafe {
            let mut out = PublicKey::from(ffi::PublicKey::new());
            if ffi::secp256k1_musig_pubkey_ec_tweak_add(
                cx,
                out.as_mut_ptr(),
                self.as_mut_ptr(),
                tweak.as_ptr(),
            ) == 0
            {
                Err(MusigTweakErr::InvalidTweak)
            } else {
                Ok(out)
            }
        }
    }

    /// Apply "x-only" tweaking to a public key in a [`MusigKeyAggCache`] by
    /// adding the generator multiplied with `tweak32` to it. Returns the tweaked [`XOnlyPublicKey`].
    /// This is useful in creating taproot outputs.
    ///
    /// This function is required if you want to _sign_ for a tweaked aggregate key.
    /// On the other hand, if you are only computing a public key, but not intending
    /// to create a signature for it, you can just use [`XOnlyPublicKey::tweak_add_assign`]
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for verification
    /// * `tweak`: tweak of type [`SecretKey`] with which to tweak the aggregated key
    ///
    /// # Errors:
    ///
    /// If resulting public key would be invalid (only when the tweak is the negation of the corresponding
    /// secret key). For uniformly random 32-byte arrays(for example, in BIP341 taproot tweaks) the chance of
    /// being invalid is negligible (around 1 in 2^128)
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{MusigKeyAggCache, Secp256k1, SecretKey, KeyPair, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let mut key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    ///
    /// let tweak = SecretKey::from_slice(&[2; 32]).unwrap();
    /// let _x_only_key_tweaked = key_agg_cache.pubkey_xonly_tweak_add(&secp, tweak).unwrap();
    /// # }
    /// ```
    pub fn pubkey_xonly_tweak_add<C: Verification>(
        &mut self,
        secp: &Secp256k1<C>,
        tweak: SecretKey,
    ) -> Result<XOnlyPublicKey, MusigTweakErr> {
        let cx = *secp.ctx();
        unsafe {
            let mut out = XOnlyPublicKey::from(ffi::XOnlyPublicKey::new());
            if ffi::secp256k1_musig_pubkey_xonly_tweak_add(
                cx,
                out.as_mut_ptr(),
                self.as_mut_ptr(),
                tweak.as_ptr(),
            ) == 0
            {
                Err(MusigTweakErr::InvalidTweak)
            } else {
                Ok(out)
            }
        }
    }

    /// Starts a signing session by generating a nonce
    ///
    /// This function outputs a secret nonce that will be required for signing and a
    /// corresponding public nonce that is intended to be sent to other signers.
    ///
    /// MuSig differs from regular Schnorr signing in that implementers _must_ take
    /// special care to not reuse a nonce. If you cannot provide a `sec_key`, `session_id`
    /// UNIFORMLY RANDOM AND KEPT SECRET (even from other signers).
    /// Refer to libsecp256k1-zkp documentation for additional considerations.
    ///
    /// Musig2 nonces can be precomputed without knowing the aggregate public key, the message to sign.
    /// However, for maximal mis-use resistance, this API requires user to have already
    /// have [`SecretKey`], [`Message`] and [`MusigKeyAggCache`]. See the `new_nonce_pair` method
    /// that allows generating [`MusigSecNonce`] and [`MusigPubNonce`] with only the `session_id` field.
    ///
    /// Remember that nonce reuse will immediately leak the secret key!
    ///
    /// # Returns:
    ///
    /// A pair of ([`MusigSecNonce`], [`MusigPubNonce`]) that can be later used signing and aggregation
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `session_id`: Uniform random identifier for this session. This _must_ never be re-used.
    /// If this is not sampled uniformly at random, this can leak the private key
    /// * `sec_key`: [`SecretKey`] that we will use to sign to a create partial signature.
    /// * `msg`: [`Message`] that will be signed later on.
    /// * `extra_rand`: Additional randomness for mis-use resistance
    ///
    /// /// # Errors:
    ///
    /// * `ZeroSession`: if the `session_id` is supplied is all zeros.
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigKeyAggCache, XOnlyPublicKey, Secp256k1, SecretKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    ///
    /// // Provide the current time for mis-use resistance
    /// let extra_rand : Option<[u8; 32]> = None;
    /// let (_sec_nonce, _pub_nonce) = key_agg_cache.nonce_gen(&secp, session_id, sec_key, msg, None)
    ///     .expect("non zero session id");
    /// # }
    /// ```
    pub fn nonce_gen<C: Signing>(
        &self,
        secp: &Secp256k1<C>,
        session_id: [u8; 32],
        sec_key: SecretKey,
        msg: Message,
        extra_rand: Option<[u8; 32]>,
    ) -> Result<(MusigSecNonce, MusigPubNonce), MusigNonceGenError> {
        new_musig_nonce_pair(
            secp,
            session_id,
            Some(&self),
            Some(sec_key),
            Some(msg),
            extra_rand,
        )
    }

    /// Get a const pointer to the inner MusigKeyAggCache
    pub fn as_ptr(&self) -> *const ffi::MusigKeyaggCache {
        &self.0
    }

    /// Get a mut pointer to the inner MusigKeyAggCache
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigKeyaggCache {
        &mut self.0
    }
}

/// Musig tweaking related errors.
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum MusigTweakErr {
    /// Invalid tweak (tweak is the negation of the corresponding secret key).
    InvalidTweak,
}

#[cfg(feature = "std")]
impl std::error::Error for MusigTweakErr {}

impl fmt::Display for MusigTweakErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            MusigTweakErr::InvalidTweak => write!(
                f,
                "Invalid Tweak: This only happens when
                tweak is negation of secret key"
            ),
        }
    }
}

/// Musig tweaking related errors.
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum MusigNonceGenError {
    /// Invalid tweak (tweak is the negation of the corresponding secret key).
    ZeroSession,
}

#[cfg(feature = "std")]
impl std::error::Error for MusigNonceGenError {}

impl fmt::Display for MusigNonceGenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            MusigNonceGenError::ZeroSession => write!(f, "Supplied a zero session id"),
        }
    }
}
/// Starts a signing session by generating a nonce. Use [`MusigKeyAggCache::nonce_gen`] whenever
/// possible. This API provides full flexibility in providing
///
/// This function outputs a secret nonce that will be required for signing and a
/// corresponding public nonce that is intended to be sent to other signers.
///
/// MuSig differs from regular Schnorr signing in that implementers _must_ take
/// special care to not reuse a nonce. If you cannot provide a `sec_key`, `session_id`
/// UNIFORMLY RANDOM AND KEPT SECRET (even from other signers). Refer to libsecp256k1-zkp
/// documentation for additional considerations.
///
/// Musig2 nonces can be precomputed without knowing the aggregate public key, the message to sign.
///
///
/// # Arguments:
///
/// * `secp` : [`Secp256k1`] context object initialized for signing
/// * `session_id`: Uniform random identifier for this session. This _must_ never be re-used.
/// If this is not sampled uniformly at random, this can leak the private key
/// * `sec_key`: Optional [`SecretKey`] that we will use to sign to a create partial signature. Provide this
/// for maximal mis-use resistance.
/// * `msg`: Optional [`Message`] that will be signed later on. Provide this for maximal misuse resistance.
/// * `extra_rand`: Additional randomness for mis-use resistance. Provide this for maximal misuse resistance
///
/// Remember that nonce reuse will immediately leak the secret key!
///
/// # Errors:
///
/// * `ZeroSession`: if the `session_id` is supplied is all zeros.
///
/// Example:
///
/// ```rust
/// # # [cfg(any(test, feature = "rand-std"))] {
/// # use secp256k1_zkp::rand::{thread_rng, RngCore};
/// # use secp256k1_zkp::{Message, KeyPair, MusigKeyAggCache, XOnlyPublicKey, Secp256k1, SecretKey, new_musig_nonce_pair};
/// let secp = Secp256k1::new();
/// // The session id must be sampled at random. Read documentation for more details.
/// let mut session_id = [0; 32];
/// thread_rng().fill_bytes(&mut session_id);
///
/// // Supply extra auxillary randomness to prevent misuse(for example, time of day)
/// let extra_rand : Option<[u8; 32]> = None;
///
/// let (_sec_nonce, _pub_nonce) = new_musig_nonce_pair(&secp, session_id, None, None, None, None)
///     .expect("non zero session id");
/// # }
/// ```
pub fn new_musig_nonce_pair<C: Signing>(
    secp: &Secp256k1<C>,
    session_id: [u8; 32],
    key_agg_cache: Option<&MusigKeyAggCache>,
    sec_key: Option<SecretKey>,
    msg: Option<Message>,
    extra_rand: Option<[u8; 32]>,
) -> Result<(MusigSecNonce, MusigPubNonce), MusigNonceGenError> {
    let cx = *secp.ctx();
    let extra_ptr = extra_rand
        .as_ref()
        .map(|e| e.as_ptr())
        .unwrap_or(core::ptr::null());
    let sk_ptr = sec_key
        .as_ref()
        .map(|e| e.as_ptr())
        .unwrap_or(core::ptr::null());
    let msg_ptr = msg
        .as_ref()
        .map(|ref e| e.as_ptr())
        .unwrap_or(core::ptr::null());
    let cache_ptr = key_agg_cache
        .map(|e| e.as_ptr())
        .unwrap_or(core::ptr::null());
    unsafe {
        let mut sec_nonce = MusigSecNonce(ffi::MusigSecNonce::new());
        let mut pub_nonce = MusigPubNonce(ffi::MusigPubNonce::new());
        if ffi::secp256k1_musig_nonce_gen(
            cx,
            sec_nonce.as_mut_ptr(),
            pub_nonce.as_mut_ptr(),
            (&session_id).as_ref().as_ptr(),
            sk_ptr,
            msg_ptr,
            cache_ptr,
            extra_ptr,
        ) == 0
        {
            // Rust type system guarantees that
            // - input secret key is valid
            // - msg is 32 bytes
            // - Key agg cache is valid
            // - extra input is 32 bytes
            // This can only happen when the session id is all zeros
            Err(MusigNonceGenError::ZeroSession)
        } else {
            Ok((sec_nonce, pub_nonce))
        }
    }
}

/// Opaque data structure that holds a partial MuSig signature.
///
/// Serialized and parsed with [`MusigPartialSignature::serialize`] and
/// [`MusigPartialSignature::from_slice`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MusigPartialSignature(ffi::MusigPartialSignature);

impl CPtr for MusigPartialSignature {
    type Target = ffi::MusigPartialSignature;

    fn as_c_ptr(&self) -> *const Self::Target {
        self.as_ptr()
    }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target {
        self.as_mut_ptr()
    }
}

impl MusigPartialSignature {
    /// Serialize a MuSigPartialSignature or adaptor signature
    ///
    /// # Returns
    ///
    /// 32-byte array when the signature could be serialized
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigSession, XOnlyPublicKey, Secp256k1, SecretKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (_sec_nonce, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None,
    /// );
    ///
    /// let partial_sig = session.partial_sign(
    ///     &secp,
    ///     &mut sec_nonce1,
    ///     &keypair1,
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// let _ser_sig = partial_sig.serialize();
    /// # }
    /// ```
    pub fn serialize(&self) -> [u8; 32] {
        let mut data = [0; 32];
        unsafe {
            if ffi::secp256k1_musig_partial_sig_serialize(
                ffi::secp256k1_context_no_precomp,
                data.as_mut_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Only fails if args are null pointer which is possible in safe rust
                unreachable!("Serialization cannot fail")
            } else {
                data
            }
        }
    }

    /// Deserialize a MusigPartialSignature from bytes.
    ///
    /// # Errors:
    ///
    /// - ArgLenMismatch: If the signature is not 32 bytes
    /// - MalformedArg: If the signature is 32 bytes, but out of curve order
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{
    /// #   Message, MusigAggNonce, MusigPartialSignature, MusigKeyAggCache, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey, KeyPair
    /// # };
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (_sec_nonce, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None,
    /// );
    ///
    /// let partial_sig = session.partial_sign(
    ///     &secp,
    ///     &mut sec_nonce1,
    ///     &keypair1,
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// let ser_sig = partial_sig.serialize();
    /// let _parsed_sig = MusigPartialSignature::from_slice(&ser_sig).unwrap();
    /// # }
    /// ```
    pub fn from_slice(data: &[u8]) -> Result<Self, ParseError> {
        let mut part_sig = MusigPartialSignature(ffi::MusigPartialSignature::new());
        if data.len() != 32 {
            return Err(ParseError::ArgLenMismatch {
                expected: 32,
                got: data.len(),
            });
        }
        unsafe {
            if ffi::secp256k1_musig_partial_sig_parse(
                ffi::secp256k1_context_no_precomp,
                part_sig.as_mut_ptr(),
                data.as_ptr(),
            ) == 0
            {
                Err(ParseError::MalformedArg)
            } else {
                Ok(part_sig)
            }
        }
    }

    /// Get a const pointer to the inner MusigPartialSignature
    pub fn as_ptr(&self) -> *const ffi::MusigPartialSignature {
        &self.0
    }

    /// Get a mut pointer to the inner MusigPartialSignature
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigPartialSignature {
        &mut self.0
    }
}

/// Musig partial signature parsing errors
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum ParseError {
    /// Length mismatch
    ArgLenMismatch {
        /// Expected size.
        expected: usize,
        /// Actual size.
        got: usize,
    },
    /// Parse Argument is malformed. This might occur if the point is on the secp order,
    /// or if the secp scalar is outside of group order
    MalformedArg,
}

#[cfg(feature = "std")]
impl std::error::Error for ParseError {}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match *self {
            ParseError::ArgLenMismatch { expected, got } => {
                write!(f, "Argument must be {} bytes, got {}", expected, got)
            }
            ParseError::MalformedArg => write!(f, "Malformed parse argument"),
        }
    }
}

/// Creates a signature from a pre-signature(not to be confused with [`MusigPartialSignature`])
/// and an adaptor.
///
/// # Arguments:
///
/// * `pre_sig` : [`schnorr::Signature`] to which the adaptor is to be added
/// * `sec_adaptor` : Secret adaptor of [`Tweak`] type to add to pre signature
/// * `nonce_parity`: The [`Parity`] obtained by [`MusigSession::nonce_parity`] for the session
/// used to compute `pre_sig`.
///
/// # Returns:
///
/// The [`schnorr::Signature`] with the adaptor applied.
///
/// Example:
///
/// ```rust
/// # # [cfg(any(test, feature = "rand-std"))] {
/// # use secp256k1_zkp::rand::{thread_rng, RngCore};
/// # use secp256k1_zkp::{adapt, schnorr, Tweak, Message, MusigAggNonce, MusigKeyAggCache, MusigSession, XOnlyPublicKey, Secp256k1, SecretKey, PublicKey, KeyPair};
/// let secp = Secp256k1::new();
/// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
/// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
/// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
/// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
///
/// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
/// let agg_pk = key_agg_cache.agg_pk();
/// // The session id must be sampled at random. Read documentation for more details.
/// let mut session_id = [0; 32];
/// thread_rng().fill_bytes(&mut session_id);
///
/// // Generate the nonce for party with `keypair1`.
/// let sec_key1 = SecretKey::from_keypair(&keypair1);
/// let msg = Message::from_slice(&[3; 32]).unwrap();
/// let mut extra_rand = [0u8; 32];
/// thread_rng().fill_bytes(&mut extra_rand);
/// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
///     .expect("non zero session id");
///
///  // Generate the nonce for party with `keypair2`.
/// let sec_key2 = SecretKey::from_keypair(&keypair2);
/// let mut extra_rand = [0u8; 32];
/// thread_rng().fill_bytes(&mut extra_rand);
/// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
///     .expect("non zero session id");
///
/// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
///
/// // Tweak with a secret adaptor
/// let mut adapt_bytes = [0; 32];
/// thread_rng().fill_bytes(&mut adapt_bytes);
/// let adapt_sec = SecretKey::from_slice(&adapt_bytes).unwrap();
/// let adapt_pub = PublicKey::from_secret_key(&secp, &adapt_sec);
/// let adapt_sec = Tweak::from_slice(adapt_sec.as_ref()).unwrap();
///
/// let session = MusigSession::new(
///     &secp,
///     &key_agg_cache,
///     aggnonce,
///     msg,
///     Some(adapt_pub), // adaptor here
/// );
///
/// let partial_sig1 = session.partial_sign(
///     &secp,
///     &mut sec_nonce1,
///     &keypair1,
///     &key_agg_cache,
/// ).unwrap();
///
/// // Other party creates the other partial signature
/// let partial_sig2 = session.partial_sign(
///     &secp,
///     &mut sec_nonce2,
///     &keypair2,
///     &key_agg_cache,
/// ).unwrap();
///
/// let nonce_parity = session.nonce_parity();
/// let pre_sig = session.partial_sig_agg(&[partial_sig1, partial_sig2]);
///
/// // Note that without the adaptor, the aggregated signature will fail verification
///
/// assert!(secp.verify_schnorr(&pre_sig, &msg, &agg_pk).is_err());
/// // Get the final schnorr signature
/// let schnorr_sig = adapt(pre_sig, adapt_sec, nonce_parity);
/// assert!(secp.verify_schnorr(&schnorr_sig, &msg, &agg_pk).is_ok());
/// # }
/// ```
pub fn adapt(
    pre_sig: schnorr::Signature,
    sec_adaptor: Tweak,
    nonce_parity: Parity,
) -> schnorr::Signature {
    unsafe {
        let mut sig = pre_sig;
        if ffi::secp256k1_musig_adapt(
            ffi::secp256k1_context_no_precomp,
            sig.as_mut_ptr(),
            pre_sig.as_ptr(),
            sec_adaptor.as_ptr(),
            nonce_parity.to_i32(),
        ) == 0
        {
            // Only fails when the arguments are invalid which is not possible in safe rust
            unreachable!("Arguments must be valid and well-typed")
        } else {
            schnorr::Signature::from_slice(sig.as_ref())
                .expect("Adapted signatures from pre-sig must be valid schnorr signatures")
        }
    }
}

/// Extracts a secret adaptor from a MuSig, given all parties' partial
/// signatures. This function will not fail unless given grossly invalid data; if it
/// is merely given signatures that do not verify, the returned value will be
/// nonsense. It is therefore important that all data be verified at earlier steps of
/// any protocol that uses this function.
///
/// # Arguments:
///
/// * `sig`: the [`schnorr::Signature`] with the adaptor applied.
/// * `pre_sig` : Secret adaptor of [`SecretKey`] type to add to pre signature
/// corresponding to `sig`. This is the aggregation of all [`MusigPartialSignature`] without
/// the adaptor
/// * `nonce_parity`: The [`Parity`] obtained by [`MusigSession::nonce_parity`] for the session
/// used to compute `pre_sig64`.
///
/// # Returns:
///
/// The adaptor secret of [`Tweak`]. The [`Tweak`] type is like [`SecretKey`], but also
/// allows for representing the zero value.
///
/// Example:
///
/// ```rust
/// # # [cfg(any(test, feature = "rand-std"))] {
/// # use secp256k1_zkp::rand::{thread_rng, RngCore};
/// # use secp256k1_zkp::{adapt, extract_adaptor};
/// # use secp256k1_zkp::{Message, KeyPair, PublicKey, MusigAggNonce, MusigKeyAggCache, MusigSession, XOnlyPublicKey, Secp256k1, SecretKey, Tweak};
/// let secp = Secp256k1::new();
/// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
/// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
/// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
/// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
///
/// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
/// // The session id must be sampled at random. Read documentation for more details.
/// let mut session_id = [0; 32];
/// thread_rng().fill_bytes(&mut session_id);
///
/// // Generate the nonce for party with `keypair1`.
/// let sec_key1 = SecretKey::from_keypair(&keypair1);
/// let msg = Message::from_slice(&[3; 32]).unwrap();
/// let mut extra_rand = [0u8; 32];
/// thread_rng().fill_bytes(&mut extra_rand);
/// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
///     .expect("non zero session id");
///
///  // Generate the nonce for party with `keypair2`.
/// let sec_key2 = SecretKey::from_keypair(&keypair2);
/// let mut extra_rand = [0u8; 32];
/// thread_rng().fill_bytes(&mut extra_rand);
/// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
///     .expect("non zero session id");
///
/// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
///
/// // Tweak with a secret adaptor
/// let mut adapt_bytes = [0; 32];
/// thread_rng().fill_bytes(&mut adapt_bytes);
/// let adapt_sec = SecretKey::from_slice(&adapt_bytes).unwrap();
/// let adapt_pub = PublicKey::from_secret_key(&secp, &adapt_sec);
/// let adapt_sec = Tweak::from_slice(adapt_sec.as_ref()).unwrap();
///
/// let session = MusigSession::new(
///     &secp,
///     &key_agg_cache,
///     aggnonce,
///     msg,
///     Some(adapt_pub), // adaptor here
/// );
///
/// let partial_sig1 = session.partial_sign(
///     &secp,
///     &mut sec_nonce1,
///     &keypair1,
///     &key_agg_cache,
/// ).unwrap();
///
/// // Other party creates the other partial signature
/// let partial_sig2 = session.partial_sign(
///     &secp,
///     &mut sec_nonce2,
///     &keypair2,
///     &key_agg_cache,
/// ).unwrap();
///
/// let nonce_parity = session.nonce_parity();
/// let pre_sig = session.partial_sig_agg(&[partial_sig1, partial_sig2]);
///
/// let schnorr_sig = adapt(pre_sig, adapt_sec, nonce_parity);
/// let extracted_sec = extract_adaptor(
///     schnorr_sig,
///     pre_sig,
///     nonce_parity,
/// );
/// assert_eq!(extracted_sec, adapt_sec);
/// # }
/// ```
pub fn extract_adaptor(
    sig: schnorr::Signature,
    pre_sig: schnorr::Signature,
    nonce_parity: Parity,
) -> Tweak {
    unsafe {
        let mut secret = ZERO_TWEAK;
        if ffi::secp256k1_musig_extract_adaptor(
            ffi::secp256k1_context_no_precomp,
            secret.as_mut_ptr(),
            sig.as_ptr(),
            pre_sig.as_ptr(),
            nonce_parity.to_i32(),
        ) == 0
        {
            // Only fails when the arguments are invalid which is not possible in safe rust
            unreachable!("Arguments must be valid and well-typed")
        } else {
            secret
        }
    }
}

/// This structure MUST NOT be copied or
/// read or written to it directly. A signer who is online throughout the whole
/// process and can keep this structure in memory can use the provided API
/// functions for a safe standard workflow. See
/// https://blockstream.com/2019/02/18/musig-a-new-multisignature-standard/ for
/// more details about the risks associated with serializing or deserializing
/// this structure. There are no serialization and parsing functions (yet).
///
/// Note this deliberately does not implement `Copy` or `Clone`. After creation, the only
/// use of this nonce is [`MusigSession::partial_sign`] API that takes a mutable reference
/// and overwrites this nonce with zero.
///
/// A signer who is online throughout the whole process and can keep this
/// structure in memory can use the provided API functions for a safe standard
/// workflow. See
/// https://blockstream.com/2019/02/18/musig-a-new-multisignature-standard/ for
/// more details about the risks associated with serializing or deserializing
/// this structure.
///
/// Signers that pre-computes and saves these nonces are not yet supported. Users
/// who want to serialize this must use unsafe rust to do so.
#[derive(Debug, Eq, PartialEq)]
pub struct MusigSecNonce(ffi::MusigSecNonce);

impl CPtr for MusigSecNonce {
    type Target = ffi::MusigSecNonce;

    fn as_c_ptr(&self) -> *const Self::Target {
        self.as_ptr()
    }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target {
        self.as_mut_ptr()
    }
}

impl MusigSecNonce {
    /// Get a const pointer to the inner MusigKeyAggCache
    pub fn as_ptr(&self) -> *const ffi::MusigSecNonce {
        &self.0
    }

    /// Get a mut pointer to the inner MusigKeyAggCache
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigSecNonce {
        &mut self.0
    }
}

/// Opaque data structure that holds a MuSig public nonce.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MusigPubNonce(ffi::MusigPubNonce);

impl CPtr for MusigPubNonce {
    type Target = ffi::MusigPubNonce;

    fn as_c_ptr(&self) -> *const Self::Target {
        self.as_ptr()
    }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target {
        self.as_mut_ptr()
    }
}

impl MusigPubNonce {
    /// Serialize a MusigPubNonce
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigKeyAggCache, MusigPubNonce, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let sec_key = SecretKey::from_slice([1; 32].as_ref()).unwrap();
    /// let keypair = KeyPair::from_secret_key(&secp, sec_key);
    /// let pub_key = XOnlyPublicKey::from_keypair(&keypair);
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key]);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let session_id = [2; 32];
    /// let (mut secnonce, pubnonce) = key_agg_cache.nonce_gen(&secp, session_id, sec_key, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let _pubnonce_ser = pubnonce.serialize();
    /// # }
    /// ```
    pub fn serialize(&self) -> [u8; ffi::MUSIG_PUBNONCE_LEN] {
        let mut data = [0; ffi::MUSIG_PUBNONCE_LEN];
        unsafe {
            if ffi::secp256k1_musig_pubnonce_serialize(
                ffi::secp256k1_context_no_precomp,
                data.as_mut_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Only fails when the arguments are invalid which is not possible in safe rust
                unreachable!("Arguments must be valid and well-typed")
            } else {
                data
            }
        }
    }

    /// Deserialize a MusigPubNonce from a portable byte representation
    ///
    /// # Errors:
    ///
    /// - ArgLenMismatch: If the [`MusigPubNonce`] is not 132 bytes
    /// - MalformedArg: If the [`MusigPubNonce`] is 132 bytes, but out of curve order
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigKeyAggCache, MusigPubNonce, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let sec_key = SecretKey::from_slice([1; 32].as_ref()).unwrap();
    /// let keypair = KeyPair::from_secret_key(&secp, sec_key);
    /// let pub_key = XOnlyPublicKey::from_keypair(&keypair);
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key]);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let session_id = [2; 32];
    /// let (mut secnonce, pubnonce) = key_agg_cache.nonce_gen(&secp, session_id, sec_key, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let pubnonce_ser = pubnonce.serialize();
    /// let parsed_pubnonce = MusigPubNonce::from_slice(&pubnonce_ser).unwrap();
    /// assert_eq!(parsed_pubnonce, pubnonce);
    /// # }
    /// ```
    pub fn from_slice(data: &[u8]) -> Result<Self, ParseError> {
        let mut pubnonce = MusigPubNonce(ffi::MusigPubNonce::new());
        if data.len() != ffi::MUSIG_PUBNONCE_LEN {
            return Err(ParseError::ArgLenMismatch {
                expected: ffi::MUSIG_PUBNONCE_LEN,
                got: data.len(),
            });
        }
        unsafe {
            if ffi::secp256k1_musig_pubnonce_parse(
                ffi::secp256k1_context_no_precomp,
                pubnonce.as_mut_ptr(),
                data.as_ptr(),
            ) == 0
            {
                Err(ParseError::MalformedArg)
            } else {
                Ok(pubnonce)
            }
        }
    }

    /// Get a const pointer to the inner MusigPubNonce
    pub fn as_ptr(&self) -> *const ffi::MusigPubNonce {
        &self.0
    }

    /// Get a mut pointer to the inner MusigPubNonce
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigPubNonce {
        &mut self.0
    }
}

/// Opaque data structure that holds a MuSig aggregated nonce.
///
/// There are no serialization and parsing functions (yet).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MusigAggNonce(ffi::MusigAggNonce);

impl CPtr for MusigAggNonce {
    type Target = ffi::MusigAggNonce;

    fn as_c_ptr(&self) -> *const Self::Target {
        self.as_ptr()
    }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target {
        self.as_mut_ptr()
    }
}

impl MusigAggNonce {
    /// Combine received public nonces into a single aggregated nonce
    ///
    /// This is useful to reduce the communication between signers, because instead
    /// of everyone sending nonces to everyone else, there can be one party
    /// receiving all nonces, combining the nonces with this function and then
    /// sending only the combined nonce back to the signers. The pubnonces argument
    /// of [MusigKeyAggCache::nonce_process] then simply becomes an array whose sole
    /// element is this combined nonce.
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigPubNonce, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (_sec_nonce, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    /// # }
    /// ```
    pub fn new<C: Signing>(secp: &Secp256k1<C>, nonces: &[MusigPubNonce]) -> Self {
        let mut aggnonce = MusigAggNonce(ffi::MusigAggNonce::new());
        let nonce_ptrs = nonces.iter().map(|n| n.as_ptr()).collect::<Vec<_>>();
        unsafe {
            if ffi::secp256k1_musig_nonce_agg(
                *secp.ctx(),
                aggnonce.as_mut_ptr(),
                nonce_ptrs.as_ptr(),
                nonce_ptrs.len(),
            ) == 0
            {
                // This can only crash if the individual nonces are invalid which is not possible is rust.
                // Note that even if aggregate nonce is point at infinity, the musig spec sets it as `G`
                unreachable!("Public key nonces are well-formed and valid in rust typesystem")
            } else {
                aggnonce
            }
        }
    }

    /// Serialize a MusigAggNonce
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigPubNonce, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let sec_key = SecretKey::from_slice([1; 32].as_ref()).unwrap();
    /// let keypair = KeyPair::from_secret_key(&secp, sec_key);
    /// let pub_key = XOnlyPublicKey::from_keypair(&keypair);
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key]);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    ///
    /// let session_id = [2; 32];
    /// let (mut secnonce, pubnonce) = key_agg_cache.nonce_gen(&secp, session_id, sec_key, msg, None)
    ///     .expect("non zero session id");
    /// let aggnonce = MusigAggNonce::new(&secp, &[pubnonce]);
    ///
    /// let _aggnonce_ser = aggnonce.serialize();
    /// # }
    /// ```
    pub fn serialize(&self) -> [u8; ffi::MUSIG_AGGNONCE_LEN] {
        let mut data = [0; ffi::MUSIG_AGGNONCE_LEN];
        unsafe {
            if ffi::secp256k1_musig_aggnonce_serialize(
                ffi::secp256k1_context_no_precomp,
                data.as_mut_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Only fails when the arguments are invalid which is not possible in safe rust
                unreachable!("Arguments must be valid and well-typed")
            } else {
                data
            }
        }
    }

    /// Deserialize a MusigAggNonce from byte slice
    ///
    /// # Errors:
    ///
    /// - ArgLenMismatch: If the slice is not 132 bytes
    /// - MalformedArg: If the byte slice is 132 bytes, but the [`MusigAggNonce`] is invalid
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let sec_key = SecretKey::from_slice([1; 32].as_ref()).unwrap();
    /// let keypair = KeyPair::from_secret_key(&secp, sec_key);
    /// let pub_key = XOnlyPublicKey::from_keypair(&keypair);
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key]);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    ///
    /// let session_id = [2; 32];
    /// let (mut secnonce, pubnonce) = key_agg_cache.nonce_gen(&secp, session_id, sec_key, msg, None)
    ///     .expect("non zero session id");
    /// let aggnonce = MusigAggNonce::new(&secp, &[pubnonce]);
    ///
    /// let aggnonce_ser = aggnonce.serialize();
    /// let parsed_aggnonce = MusigAggNonce::from_slice(&aggnonce_ser).unwrap();
    /// assert_eq!(parsed_aggnonce, aggnonce);
    /// # }
    /// ```
    pub fn from_slice(data: &[u8]) -> Result<Self, ParseError> {
        if data.len() != ffi::MUSIG_AGGNONCE_LEN {
            return Err(ParseError::ArgLenMismatch {
                expected: ffi::MUSIG_AGGNONCE_LEN,
                got: data.len(),
            });
        }
        let mut aggnonce = MusigAggNonce(ffi::MusigAggNonce::new());
        unsafe {
            if ffi::secp256k1_musig_aggnonce_parse(
                ffi::secp256k1_context_no_precomp,
                aggnonce.as_mut_ptr(),
                data.as_ptr(),
            ) == 0
            {
                Err(ParseError::MalformedArg)
            } else {
                Ok(aggnonce)
            }
        }
    }

    /// Get a const pointer to the inner MusigAggNonce
    pub fn as_ptr(&self) -> *const ffi::MusigAggNonce {
        &self.0
    }

    /// Get a mut pointer to the inner MusigAggNonce
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigAggNonce {
        &mut self.0
    }
}

/// Musig session data structure containing the
/// secret and public nonce used in a multi-signature signing session
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MusigSession(ffi::MusigSession);

impl CPtr for MusigSession {
    type Target = ffi::MusigSession;

    fn as_c_ptr(&self) -> *const Self::Target {
        self.as_ptr()
    }

    fn as_mut_c_ptr(&mut self) -> *mut Self::Target {
        self.as_mut_ptr()
    }
}

impl MusigSession {
    /// Takes the public nonces of all signers and computes a session that is
    /// required for signing and verification of partial signatures.
    ///
    /// If the adaptor argument is [`Option::Some`], then the output of
    /// partial signature aggregation will be a pre-signature which is not a valid Schnorr
    /// signature. In order to create a valid signature, the pre-signature and the
    /// secret adaptor must be provided to `musig_adapt`.
    ///
    /// # Returns:
    ///
    /// A [`MusigSession`] that can be later used for signing.
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `key_agg_cache`: [`MusigKeyAggCache`] to be used for this session
    /// * `agg_nonce`: [`MusigAggNonce`], the aggregate nonce
    /// * `msg`: [`Message`] that will be signed later on.
    /// * `adaptor`: The adaptor of type [`PublicKey`] if this is signing session is a part of
    /// an adaptor signature protocol.
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigSession, XOnlyPublicKey, Secp256k1, SecretKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// let agg_pk = key_agg_cache.agg_pk();
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    ///
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None, // adaptor here
    /// );
    /// # }
    /// ```
    pub fn new<C: Signing>(
        secp: &Secp256k1<C>,
        key_agg_cache: &MusigKeyAggCache,
        agg_nonce: MusigAggNonce,
        msg: Message,
        adaptor: Option<PublicKey>,
    ) -> Self {
        let mut session = MusigSession(ffi::MusigSession::new());
        let adaptor_ptr = match adaptor {
            Some(a) => a.as_ptr(),
            None => core::ptr::null(),
        };
        unsafe {
            if ffi::secp256k1_musig_nonce_process(
                *secp.ctx(),
                session.as_mut_ptr(),
                agg_nonce.as_ptr(),
                msg.as_ptr(),
                key_agg_cache.as_ptr(),
                adaptor_ptr,
            ) == 0
            {
                // Only fails on cryptographically unreachable codes or if the args are invalid.
                // None of which can occur in safe rust.
                unreachable!("Impossible to construct invalid arguments in safe rust.
                    Also reaches here if R1 + R2*b == point at infinity, but only occurs with 1/1^128 probability")
            } else {
                session
            }
        }
    }

    /// Produces a partial signature for a given key pair and secret nonce.
    ///
    /// Remember that nonce reuse will immediately leak the secret key!
    ///
    /// # Returns:
    ///
    /// A [`MusigPartialSignature`] that can be later be aggregated into a [`schnorr::Signature`]
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `sec_nonce`: [`MusigSecNonce`] to be used for this session that has never
    /// been used before. For mis-use resistance, this API takes a mutable reference
    /// to `sec_nonce` and sets it to zero even if the partial signing fails.
    /// * `key_pair`: The [`KeyPair`] to sign the message
    /// * `key_agg_cache`: [`MusigKeyAggCache`] containing the aggregate pubkey used in
    /// the creation of this session
    ///
    /// # Errors:
    ///
    /// - If the provided [`MusigSecNonce`] has already been used for signing
    ///
    /// # Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    ///
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None, // adaptor here
    /// );
    ///
    /// let _partial_sig = session.partial_sign(
    ///     &secp,
    ///     &mut sec_nonce1,
    ///     &keypair1,
    ///     &key_agg_cache,
    /// ).unwrap();
    /// # }
    /// ```
    pub fn partial_sign<C: Signing>(
        &self,
        secp: &Secp256k1<C>,
        secnonce: &mut MusigSecNonce,
        keypair: &KeyPair,
        key_agg_cache: &MusigKeyAggCache,
    ) -> Result<MusigPartialSignature, MusigSignError> {
        unsafe {
            let mut partial_sig = MusigPartialSignature(ffi::MusigPartialSignature::new());
            if ffi::secp256k1_musig_partial_sign(
                *secp.ctx(),
                partial_sig.as_mut_ptr(),
                secnonce.as_mut_ptr(),
                keypair.as_ptr(),
                key_agg_cache.as_ptr(),
                self.as_ptr(),
            ) == 0
            {
                // Since the arguments in rust are always session_valid, the only reason
                // this will fail if the nonce was reused.
                Err(MusigSignError::NonceReuse)
            } else {
                Ok(partial_sig)
            }
        }
    }

    /// Checks that an individual partial signature verifies
    ///
    /// This function is essential when using protocols with adaptor signatures.
    /// However, it is not essential for regular MuSig's, in the sense that if any
    /// partial signatures does not verify, the full signature will also not verify, so the
    /// problem will be caught. But this function allows determining the specific party
    /// who produced an invalid signature, so that signing can be restarted without them.
    ///
    /// # Returns:
    ///
    /// true if the partial signature successfully verifies, otherwise returns false
    ///
    /// # Arguments:
    ///
    /// * `secp` : [`Secp256k1`] context object initialized for signing
    /// * `key_agg_cache`: [`MusigKeyAggCache`] containing the aggregate pubkey used in
    /// the creation of this session
    /// * `partial_sig`: [`MusigPartialSignature`] sent by the signer associated with
    /// the given `pub_nonce` and `pubkey`
    /// * `pub_nonce`: The [`MusigPubNonce`] of the signer associated with the `partial_sig`
    /// and `pub_key`
    /// * `pub_key`: The [`XOnlyPublicKey`] of the signer associated with the given
    /// `partial_sig` and `pub_nonce`
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    ///
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None, // adaptor here
    /// );
    ///
    /// let partial_sig1 = session.partial_sign(
    ///     &secp,
    ///     &mut sec_nonce1,
    ///     &keypair1,
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// assert!(session.partial_verify(
    ///     &secp,
    ///     &key_agg_cache,
    ///     partial_sig1,
    ///     pub_nonce1,
    ///     pub_key1,
    /// ));
    /// # }
    /// ```
    pub fn partial_verify<C: Signing>(
        &self,
        secp: &Secp256k1<C>,
        key_agg_cache: &MusigKeyAggCache,
        partial_sig: MusigPartialSignature,
        pub_nonce: MusigPubNonce,
        pub_key: XOnlyPublicKey,
    ) -> bool {
        let cx = *secp.ctx();
        unsafe {
            ffi::secp256k1_musig_partial_sig_verify(
                cx,
                partial_sig.as_ptr(),
                pub_nonce.as_ptr(),
                pub_key.as_ptr(),
                key_agg_cache.as_ptr(),
                self.as_ptr(),
            ) == 1
        }
    }

    /// Aggregate partial signatures for this session into a single [`schnorr::Signature`]
    ///
    /// # Returns:
    ///
    /// A single [`schnorr::Signature`]. Note that this does *NOT* mean that the signature verifies with respect to the
    /// aggregate public key.
    ///
    /// # Arguments:
    ///
    /// * `partial_sigs`: Array of [`MusigPartialSignature`] to be aggregated
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let keypair1 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key1 = XOnlyPublicKey::from_keypair(&keypair1);
    /// let keypair2 = KeyPair::new(&secp, &mut thread_rng());
    /// let pub_key2 = XOnlyPublicKey::from_keypair(&keypair2);
    ///
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key1, pub_key2]);
    /// let agg_pk = key_agg_cache.agg_pk();
    /// // The session id must be sampled at random. Read documentation for more details.
    /// let mut session_id = [0; 32];
    /// thread_rng().fill_bytes(&mut session_id);
    ///
    /// // Generate the nonce for party with `keypair1`.
    /// let sec_key1 = SecretKey::from_keypair(&keypair1);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let (mut sec_nonce1, pub_nonce1) = key_agg_cache.nonce_gen(&secp, session_id, sec_key1, msg, None)
    ///     .expect("non zero session id");
    ///
    ///  // Generate the nonce for party with `keypair2`.
    /// let sec_key2 = SecretKey::from_keypair(&keypair2);
    /// let (mut sec_nonce2, pub_nonce2) = key_agg_cache.nonce_gen(&secp, session_id, sec_key2, msg, None)
    ///     .expect("non zero session id");
    ///
    /// let aggnonce = MusigAggNonce::new(&secp, &[pub_nonce1, pub_nonce2]);
    ///
    ///
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None,
    /// );
    ///
    /// let partial_sig1 = session.partial_sign(
    ///     &secp,
    ///     &mut sec_nonce1,
    ///     &keypair1,
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// // Other party creates the other partial signature
    /// let partial_sig2 = session.partial_sign(
    ///     &secp,
    ///     &mut sec_nonce2,
    ///     &keypair2,
    ///     &key_agg_cache,
    /// ).unwrap();
    ///
    /// let nonce_parity = session.nonce_parity();
    /// let schnorr_sig = session.partial_sig_agg(&[partial_sig1, partial_sig2]);
    ///
    /// // Get the final schnorr signature
    /// assert!(secp.verify_schnorr(&schnorr_sig, &msg, &agg_pk).is_ok())
    /// # }
    /// ```
    pub fn partial_sig_agg(&self, partial_sigs: &[MusigPartialSignature]) -> schnorr::Signature {
        let part_sigs = partial_sigs.iter().map(|s| s.as_ptr()).collect::<Vec<_>>();
        let mut sig = [0u8; 64];
        unsafe {
            if ffi::secp256k1_musig_partial_sig_agg(
                ffi::secp256k1_context_no_precomp,
                sig.as_mut_ptr(),
                self.as_ptr(),
                part_sigs.as_ptr(),
                part_sigs.len(),
            ) == 0
            {
                // All arguments are well-typed partial signatures
                unreachable!("Impossible to construct invalid(not well-typed) partial signatures")
            } else {
                // Resulting signature must be well-typed. Does not mean that will be succeed verification
                schnorr::Signature::from_slice(&sig)
                    .expect("Resulting signature must be well-typed")
            }
        }
    }

    /// Extracts the nonce_parity bit from a session
    ///
    /// This is used for adaptor signatures
    ///
    /// Example:
    ///
    /// ```rust
    /// # # [cfg(any(test, feature = "rand-std"))] {
    /// # use secp256k1_zkp::rand::{thread_rng, RngCore};
    /// # use secp256k1_zkp::{Message, KeyPair, MusigAggNonce, MusigKeyAggCache, MusigSession, Secp256k1, SecretKey, XOnlyPublicKey};
    /// let secp = Secp256k1::new();
    /// let sec_key = SecretKey::from_slice([1; 32].as_ref()).unwrap();
    /// let keypair = KeyPair::from_secret_key(&secp, sec_key);
    /// let pub_key = XOnlyPublicKey::from_keypair(&keypair);
    /// let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key]);
    /// let msg = Message::from_slice(&[3; 32]).unwrap();
    /// let session_id = [1; 32];
    /// let (mut secnonce, pubnonce) = key_agg_cache.nonce_gen(&secp, session_id, sec_key, msg, None)
    ///     .expect("non zero session id");
    /// let aggnonce = MusigAggNonce::new(&secp, &[pubnonce]);
    /// let session = MusigSession::new(
    ///     &secp,
    ///     &key_agg_cache,
    ///     aggnonce,
    ///     msg,
    ///     None,
    /// );
    ///
    /// let _parity = session.nonce_parity();
    /// # }
    /// ```
    pub fn nonce_parity(&self) -> Parity {
        let mut parity = 0i32;
        unsafe {
            if ffi::secp256k1_musig_nonce_parity(
                ffi::secp256k1_context_no_precomp,
                &mut parity,
                self.as_ptr(),
            ) == 0
            {
                unreachable!("Well-typed and valid arguments to the function")
            } else {
                Parity::from_i32(parity).expect("Parity guaranteed to be binary")
            }
        }
    }

    /// Get a const pointer to the inner MusigSession
    pub fn as_ptr(&self) -> *const ffi::MusigSession {
        &self.0
    }

    /// Get a mut pointer to the inner MusigSession
    pub fn as_mut_ptr(&mut self) -> *mut ffi::MusigSession {
        &mut self.0
    }
}

/// Musig tweaking related errors.
#[derive(Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub enum MusigSignError {
    /// Musig nonce re-used.
    /// When creating a partial signature, nonce is cleared and set to all zeros.
    /// This error is caused when we create a partial signature with zero nonce.
    // Note: Because of the current borrowing rules around nonce, this should be impossible.
    // Maybe, we can just unwrap this and not have error at all?
    NonceReuse,
}

#[cfg(feature = "std")]
impl std::error::Error for MusigSignError {}

impl fmt::Display for MusigSignError {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            MusigSignError::NonceReuse => write!(f, "Musig signing nonce re-used"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{thread_rng, RngCore};
    use crate::{KeyPair, XOnlyPublicKey};

    #[test]
    fn test_key_agg_cache() {
        let secp = Secp256k1::new();
        let mut sec_bytes = [0; 32];
        thread_rng().fill_bytes(&mut sec_bytes);
        let sec_key = SecretKey::from_slice(&sec_bytes).unwrap();
        let keypair = KeyPair::from_secret_key(&secp, &sec_key);
        let (pub_key, _parity) = XOnlyPublicKey::from_keypair(&keypair);

        let _key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key, pub_key]);
    }

    #[test]
    fn test_nonce_parsing() {
        let secp = Secp256k1::new();
        let sec_bytes = [1; 32];
        let sec_key = SecretKey::from_slice(&sec_bytes).unwrap();
        let keypair = KeyPair::from_secret_key(&secp, &sec_key);
        let (pub_key, _parity) = XOnlyPublicKey::from_keypair(&keypair);

        let key_agg_cache = MusigKeyAggCache::new(&secp, &[pub_key, pub_key]);
        let msg = Message::from_slice(&[3; 32]).unwrap();
        let session_id = [2; 32];
        let sec_key = SecretKey::from_slice(&[4; 32]).unwrap();
        let (_secnonce, pubnonce) = key_agg_cache
            .nonce_gen(&secp, session_id, sec_key, msg, None)
            .expect("non zero session id");
        let pubnonce_ser = pubnonce.serialize();
        let parsed_pubnonce = MusigPubNonce::from_slice(&pubnonce_ser).unwrap();

        assert_eq!(parsed_pubnonce, pubnonce);
    }
}