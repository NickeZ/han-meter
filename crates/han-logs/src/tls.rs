//! TLS 1.3 with mandatory chain, hostname, and certificate-date verification.
extern crate alloc;

use alloc::vec::Vec;
use core::marker::PhantomData;
use embassy_crypto_rustcrypto as _;
use embedded_io_async::{Read, Write};
pub use embedded_tls::TlsClock;
use embedded_tls::{
    Aes128GcmSha256, Certificate, CertificateEntryRef, CertificateRef, CertificateVerifyRef,
    TlsCipherSuite, TlsConfig, TlsConnection, TlsContext, TlsError, TlsVerifier,
    webpki::CertVerifier,
};

use crate::{Endpoint, http};

// Keep a complete inbound record; small outbound records bound RAM use.
const RX_BYTES: usize = 16640;
const TX_BYTES: usize = 2048;
const CERT_BYTES: usize = 4096;
// Upstream's verifier stores the hostname in a String<64>.
pub const HOST_BYTES: usize = 64;

#[derive(Debug)]
pub enum Error {
    Memory,
    Clock,
    Roots,
    Tls(TlsError),
    Http(http::Error),
}

/// Never let upstream interpret a missing clock as permission to skip dates.
/// The preflight check and verifier also reject an unavailable clock explicitly.
struct CheckedClock<C>(PhantomData<C>);
impl<C: TlsClock> TlsClock for CheckedClock<C> {
    fn now() -> Option<u64> {
        // Beyond every representable X.509 expiry date, including certificates
        // valid at the Unix epoch. Missing time must always fail verification.
        Some(C::now().unwrap_or(u64::MAX))
    }
}

type Inner<'a, C> = CertVerifier<'a, Aes128GcmSha256, CheckedClock<C>, CERT_BYTES>;

struct Verifier<'a, C: TlsClock> {
    roots: &'a [&'a [u8]],
    inner: Inner<'a, C>,
    host: heapless::String<HOST_BYTES>,
    verified: bool,
}

impl<C: TlsClock> TlsVerifier<Aes128GcmSha256> for Verifier<'_, C> {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.verified = false;
        self.host = hostname
            .try_into()
            .map_err(|_| TlsError::InsufficientSpace)?;
        self.inner.set_hostname_verification(hostname)
    }

    fn verify_certificate(
        &mut self,
        transcript: &<Aes128GcmSha256 as TlsCipherSuite>::Hash,
        certificate: CertificateRef<'_>,
    ) -> Result<(), TlsError> {
        self.verified = false;
        if C::now().is_none() {
            return Err(TlsError::InvalidCertificate);
        }
        // Upstream accepts one trust root. Try our small bundle without retaining
        // multiple certificate buffers or accepting a partially verified chain.
        for root in self.roots {
            self.inner = Inner::<C>::new(Certificate::X509(root));
            self.inner.set_hostname_verification(&self.host)?;
            let mut chain = CertificateRef::with_context(&[]);
            for entry in &certificate.entries {
                chain.add(match entry {
                    CertificateEntryRef::X509(der) => CertificateEntryRef::X509(der),
                    CertificateEntryRef::RawPublicKey(_) => {
                        return Err(TlsError::InvalidCertificate);
                    }
                })?;
            }
            if self.inner.verify_certificate(transcript, chain).is_ok() {
                self.verified = true;
                return Ok(());
            }
        }
        Err(TlsError::InvalidCertificate)
    }

    fn verify_signature(&mut self, signature: CertificateVerifyRef<'_>) -> Result<(), TlsError> {
        if !core::mem::take(&mut self.verified) {
            return Err(TlsError::InvalidCertificate);
        }
        self.inner.verify_signature(signature)
    }
}

/// Reusable, fallibly allocated workspaces. Every upload creates a fresh TLS
/// session, including after cancellation; only buffers and trust roots are reused.
pub struct Client<'a, C: TlsClock> {
    rx: Vec<u8>,
    tx: Vec<u8>,
    // Keep the leaf certificate out of nested async futures and task statics.
    verifier: Vec<Verifier<'a, C>>,
}

impl<'a, C: TlsClock> Client<'a, C> {
    pub fn new(roots: &'a [&'a [u8]]) -> Result<Self, Error> {
        let first = roots.first().ok_or(Error::Roots)?;
        let mut rx = Vec::new();
        rx.try_reserve_exact(RX_BYTES).map_err(|_| Error::Memory)?;
        rx.resize(RX_BYTES, 0);
        let mut tx = Vec::new();
        tx.try_reserve_exact(TX_BYTES).map_err(|_| Error::Memory)?;
        tx.resize(TX_BYTES, 0);
        let mut verifier = Vec::new();
        verifier.try_reserve_exact(1).map_err(|_| Error::Memory)?;
        verifier.push(Verifier {
            roots,
            inner: Inner::<C>::new(Certificate::X509(first)),
            host: heapless::String::new(),
            verified: false,
        });
        Ok(Self { rx, tx, verifier })
    }

    pub async fn post<S: Read + Write>(
        &mut self,
        stream: S,
        endpoint: &Endpoint<'_>,
        authorization: &str,
        body: &[u8],
        scratch: &mut [u8; crate::HEADER_BYTES],
    ) -> Result<(), Error> {
        if C::now().is_none() {
            return Err(Error::Clock);
        }
        let config = TlsConfig::new().with_server_name(endpoint.host());
        let mut session =
            TlsConnection::<_, Aes128GcmSha256>::new(stream, &mut self.rx, &mut self.tx);
        session
            .open(TlsContext::new(&config, &mut self.verifier[0]))
            .await
            .map_err(Error::Tls)?;
        http::post(&mut session, endpoint, authorization, body, scratch)
            .await
            .map_err(Error::Http)
    }
}
