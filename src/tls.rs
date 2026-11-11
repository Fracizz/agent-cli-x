//! TLS：自签证书 + 客户端指纹固定（首次连接由人工核对指纹）。

use crate::error::{AppError, Result};
use crate::util;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// 指纹不匹配时放进 rustls 错误里的标记，客户端据此给出明确提示。
pub const MISMATCH_TAG: &str = "agent-cli-x-fingerprint-mismatch";

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn fingerprint(der: &[u8]) -> String {
    format!("sha256:{}", util::sha256_hex(der))
}

pub fn generate(home: &Path) -> Result<String> {
    let ck = rcgen::generate_simple_self_signed(vec!["agent-cli-x".to_string()])
        .map_err(|e| AppError::internal(format!("生成证书失败: {e}")))?;
    let cert = ck.cert.der().to_vec();
    let key = ck.key_pair.serialize_der();
    fs::create_dir_all(home)?;
    util::atomic_write(&home.join("cert.der"), &cert)?;
    let key_path = home.join("key.der");
    util::atomic_write(&key_path, &key)?;
    util::restrict_permissions(&key_path);
    Ok(fingerprint(&cert))
}

pub fn local_fingerprint(home: &Path) -> Result<String> {
    let cert = fs::read(home.join("cert.der"))
        .map_err(|e| AppError::config(format!("读取证书失败: {e}。请先执行 `agent-cli-x init`")))?;
    Ok(fingerprint(&cert))
}

pub fn server_config(home: &Path) -> Result<(Arc<ServerConfig>, String)> {
    let cert = fs::read(home.join("cert.der"))
        .map_err(|e| AppError::config(format!("读取证书失败: {e}。请先执行 `agent-cli-x init`")))?;
    let key = fs::read(home.join("key.der"))
        .map_err(|e| AppError::config(format!("读取私钥失败: {e}")))?;
    let fp = fingerprint(&cert);
    let cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::internal(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .map_err(|e| AppError::config(format!("证书/私钥无效: {e}")))?;
    Ok((Arc::new(cfg), fp))
}

/// 校验器：仍完整验证握手签名，只是用“证书指纹固定”代替 CA 链。
/// `pin` 为 None 时只用于发现指纹，调用方不得在这种连接上发送 token。
#[derive(Debug)]
pub struct PinVerifier {
    pin: Option<String>,
    pub seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        *self.seen.lock().unwrap() = Some(fp.clone());
        match &self.pin {
            Some(pin) if !pin.eq_ignore_ascii_case(&fp) => {
                Err(rustls::Error::General(format!("{MISMATCH_TAG}: {fp}")))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn client_config(pin: Option<&str>) -> Result<(Arc<ClientConfig>, Arc<PinVerifier>)> {
    let provider = provider();
    let verifier = Arc::new(PinVerifier {
        pin: pin.map(|s| s.to_string()),
        seen: Mutex::new(None),
        provider: provider.clone(),
    });
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::internal(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    Ok((Arc::new(cfg), verifier))
}
