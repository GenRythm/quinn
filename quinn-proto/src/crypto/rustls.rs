use std::{
    io,
    ops::{Deref, DerefMut},
    str,
    sync::Arc,
};

use ring::{aead, hkdf, hmac};
pub use rustls::TLSError;
use rustls::{
    self,
    internal::msgs::enums::HashAlgorithm,
    quic::{ClientQuicExt, Secrets, ServerQuicExt},
    Session,
};
use webpki::DNSNameRef;

use super::ring::{hkdf_expand, Crypto};
use crate::{
    crypto, transport_parameters::TransportParameters, CertificateChain, ConnectError,
    ConnectionId, Side, TransportError, TransportErrorCode,
};

/// A rustls TLS session
pub enum TlsSession {
    #[doc(hidden)]
    Client(rustls::ClientSession),
    #[doc(hidden)]
    Server(rustls::ServerSession),
}

impl TlsSession {
    fn side(&self) -> Side {
        match self {
            TlsSession::Client(_) => Side::Client,
            TlsSession::Server(_) => Side::Server,
        }
    }
}

impl crypto::Session for TlsSession {
    type AuthenticationData = AuthenticationData;
    type ClientConfig = Arc<rustls::ClientConfig>;
    type HmacKey = hmac::Key;
    type Keys = Crypto;
    type ServerConfig = Arc<rustls::ServerConfig>;

    fn authentication_data(&self) -> AuthenticationData {
        AuthenticationData {
            peer_certificates: self.get_peer_certificates().map(|v| v.into()),
            protocol: self.get_alpn_protocol().map(|p| p.into()),
            server_name: match self {
                TlsSession::Client(_) => None,
                TlsSession::Server(session) => session.get_sni_hostname().map(|s| s.into()),
            },
        }
    }

    fn early_crypto(&self) -> Option<Self::Keys> {
        let secret = self.get_early_secret()?;
        // If an early secret is known, TLS guarantees it's associated with a resumption
        // ciphersuite,
        let suite = self.get_negotiated_ciphersuite().unwrap();
        Some(Crypto::new(
            self.side(),
            suite.get_aead_alg(),
            secret.clone(),
            secret.clone(),
        ))
    }

    fn early_data_accepted(&self) -> Option<bool> {
        match self {
            TlsSession::Client(session) => Some(session.is_early_data_accepted()),
            _ => None,
        }
    }

    fn is_handshaking(&self) -> bool {
        match self {
            TlsSession::Client(session) => session.is_handshaking(),
            TlsSession::Server(session) => session.is_handshaking(),
        }
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        self.read_hs(buf).map_err(|e| {
            if let Some(alert) = self.get_alert() {
                TransportError {
                    code: TransportErrorCode::crypto(alert.get_u8()),
                    frame: None,
                    reason: e.to_string(),
                }
            } else {
                TransportError::PROTOCOL_VIOLATION(format!("TLS error: {}", e))
            }
        })
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        match self.get_quic_transport_parameters() {
            None => Ok(None),
            Some(buf) => match TransportParameters::read(self.side(), &mut io::Cursor::new(buf)) {
                Ok(params) => Ok(Some(params)),
                Err(e) => Err(e.into()),
            },
        }
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Self::Keys> {
        let secrets = self.write_hs(buf)?;
        let suite = self
            .get_negotiated_ciphersuite()
            .expect("should not get secrets without cipher suite");
        Some(Crypto::new(
            self.side(),
            suite.get_aead_alg(),
            secrets.client,
            secrets.server,
        ))
    }

    fn update_keys(&self, keys: &Self::Keys) -> Self::Keys {
        let (client_secret, server_secret) = match self.side() {
            Side::Client => (&keys.local_secret, &keys.remote_secret),
            Side::Server => (&keys.remote_secret, &keys.local_secret),
        };

        let hash_alg = self
            .get_negotiated_ciphersuite()
            .expect("should not get secrets without cipher suite")
            .hash;
        let secrets = update_secrets(hash_alg, client_secret, server_secret);
        let suite = self.get_negotiated_ciphersuite().unwrap();
        Crypto::new(
            self.side(),
            suite.get_aead_alg(),
            secrets.client,
            secrets.server,
        )
    }

    fn retry_tag(orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        let mut pseudo_packet = Vec::with_capacity(packet.len() + orig_dst_cid.len() + 1);
        pseudo_packet.push(orig_dst_cid.len() as u8);
        pseudo_packet.extend_from_slice(orig_dst_cid);
        pseudo_packet.extend_from_slice(packet);

        let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY).unwrap(),
        );

        let tag = key
            .seal_in_place_separate_tag(nonce, aead::Aad::from(pseudo_packet), &mut [])
            .unwrap();
        let mut result = [0; 16];
        result.copy_from_slice(tag.as_ref());
        result
    }

    fn is_valid_retry(orig_dst_cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        let tag_start = match payload.len().checked_sub(16) {
            Some(x) => x,
            None => return false,
        };

        let mut pseudo_packet =
            Vec::with_capacity(header.len() + payload.len() + orig_dst_cid.len() + 1);
        pseudo_packet.push(orig_dst_cid.len() as u8);
        pseudo_packet.extend_from_slice(orig_dst_cid);
        pseudo_packet.extend_from_slice(header);
        let tag_start = tag_start + pseudo_packet.len();
        pseudo_packet.extend_from_slice(payload);

        let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY).unwrap(),
        );

        let (aad, tag) = pseudo_packet.split_at_mut(tag_start);
        key.open_in_place(nonce, aead::Aad::from(aad), tag).is_ok()
    }
}

impl Deref for TlsSession {
    type Target = dyn rustls::Session;
    fn deref(&self) -> &Self::Target {
        match *self {
            TlsSession::Client(ref session) => session,
            TlsSession::Server(ref session) => session,
        }
    }
}

impl DerefMut for TlsSession {
    fn deref_mut(&mut self) -> &mut (dyn rustls::Session + 'static) {
        match *self {
            TlsSession::Client(ref mut session) => session,
            TlsSession::Server(ref mut session) => session,
        }
    }
}

/// Authentication data for (rustls) TLS session
pub struct AuthenticationData {
    /// The certificate chain used by the peer to authenticate
    ///
    /// For clients, this is the certificate chain of the server. For servers, this is the
    /// certificate chain of the client, if client authentication was completed.
    ///
    /// `None` if this data was requested from the session before this value is available.
    ///
    /// If this is `None`, and `Connection::is_handshaking` returns `false`, the connection
    /// will have already been closed.
    pub peer_certificates: Option<CertificateChain>,
    /// The negotiated application protocol
    pub protocol: Option<Vec<u8>>,
    /// The server name specified by the client
    ///
    /// `None` for outgoing connections.
    pub server_name: Option<String>,
}

impl crypto::ClientConfig<TlsSession> for Arc<rustls::ClientConfig> {
    fn new() -> Self {
        let mut cfg = rustls::ClientConfig::new();
        cfg.versions = vec![rustls::ProtocolVersion::TLSv1_3];
        cfg.enable_early_data = true;
        #[cfg(feature = "native-certs")]
        match rustls_native_certs::load_native_certs() {
            Ok(x) => {
                cfg.root_store = x;
            }
            Err((Some(x), e)) => {
                cfg.root_store = x;
                tracing::warn!("couldn't load some default trust roots: {}", e);
            }
            Err((None, e)) => {
                tracing::warn!("couldn't load any default trust roots: {}", e);
            }
        }
        #[cfg(feature = "certificate-transparency")]
        {
            cfg.ct_logs = Some(&ct_logs::LOGS);
        }
        Arc::new(cfg)
    }

    fn start_session(
        &self,
        server_name: &str,
        params: &TransportParameters,
    ) -> Result<TlsSession, ConnectError> {
        let pki_server_name = DNSNameRef::try_from_ascii_str(server_name)
            .map_err(|_| ConnectError::InvalidDnsName(server_name.into()))?;
        Ok(TlsSession::Client(rustls::ClientSession::new_quic(
            self,
            pki_server_name,
            to_vec(params),
        )))
    }
}

impl crypto::ServerConfig<TlsSession> for Arc<rustls::ServerConfig> {
    fn new() -> Self {
        let mut cfg = rustls::ServerConfig::new(rustls::NoClientAuth::new());
        cfg.versions = vec![rustls::ProtocolVersion::TLSv1_3];
        cfg.max_early_data_size = u32::max_value();
        Arc::new(cfg)
    }

    fn start_session(&self, params: &TransportParameters) -> TlsSession {
        TlsSession::Server(rustls::ServerSession::new_quic(self, to_vec(params)))
    }
}

fn update_secrets(hash_alg: HashAlgorithm, client: &hkdf::Prk, server: &hkdf::Prk) -> Secrets {
    let hkdf_alg = match hash_alg {
        HashAlgorithm::SHA256 => hkdf::HKDF_SHA256,
        HashAlgorithm::SHA384 => hkdf::HKDF_SHA384,
        HashAlgorithm::SHA512 => hkdf::HKDF_SHA512,
        _ => panic!("unknown HKDF algorithm for hash algorithm {:?}", hash_alg),
    };

    Secrets {
        client: hkdf_expand(client, b"quic ku", hkdf_alg),
        server: hkdf_expand(server, b"quic ku", hkdf_alg),
    }
}

fn to_vec(params: &TransportParameters) -> Vec<u8> {
    let mut bytes = Vec::new();
    params.write(&mut bytes);
    bytes
}

const RETRY_INTEGRITY_KEY: [u8; 16] = [
    0x4d, 0x32, 0xec, 0xdb, 0x2a, 0x21, 0x33, 0xc8, 0x41, 0xe4, 0x04, 0x3d, 0xf2, 0x7d, 0x44, 0x30,
];
const RETRY_INTEGRITY_NONCE: [u8; 12] = [
    0x4d, 0x16, 0x11, 0xd0, 0x55, 0x13, 0xa5, 0x52, 0xc5, 0x87, 0xd5, 0x75,
];
