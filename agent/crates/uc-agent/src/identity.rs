// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use tracing::{debug, info};

use crate::error::{AgentError, Result};

pub struct LocalIdentity {
    pub key_pair: KeyPair,
    pub csr_pem: String,
}

pub fn fresh_identity(common_name: &str) -> Result<LocalIdentity> {
    let key_pair = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, common_name);
        dn.push(DnType::OrganizationName, "Magic Unicorn UUDA");
        dn
    };
    let csr = params.serialize_request(&key_pair)?;
    Ok(LocalIdentity {
        key_pair,
        csr_pem: csr.pem()?,
    })
}

pub fn write_pem(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    set_secure_perms(path)?;
    debug!(path = %path.display(), "wrote pem");
    Ok(())
}

#[cfg(unix)]
fn set_secure_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    let mode = if path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.contains("key"))
        .unwrap_or(false)
    {
        0o600
    } else {
        0o644
    };
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secure_perms(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn cert_not_after(cert_pem: &str) -> Result<time::OffsetDateTime> {
    let pem = pem_parse(cert_pem)?;
    let (_, cert) = x509_parser::parse_x509_certificate(&pem)
        .map_err(|e| AgentError::Tls(format!("parse x509: {e}")))?;
    let ts = cert.validity().not_after.timestamp();
    time::OffsetDateTime::from_unix_timestamp(ts)
        .map_err(|e| AgentError::Tls(format!("not_after timestamp: {e}")))
}

fn pem_parse(pem: &str) -> Result<Vec<u8>> {
    let mut bytes = pem.as_bytes();
    let mut iter = rustls_pemfile::certs(&mut bytes);
    let first = iter
        .next()
        .ok_or_else(|| AgentError::Tls("no certificate in pem".into()))?
        .map_err(|e| AgentError::Tls(format!("pem decode: {e}")))?;
    Ok(first.to_vec())
}

pub fn warn_if_expiring_soon(cert_pem: &str) {
    match cert_not_after(cert_pem) {
        Ok(not_after) => {
            let now = time::OffsetDateTime::now_utc();
            let remaining = not_after - now;
            if remaining.whole_days() <= 14 {
                tracing::warn!(
                    days_remaining = remaining.whole_days(),
                    not_after = %not_after,
                    "device certificate expires soon, rotation needed"
                );
            } else {
                info!(
                    days_remaining = remaining.whole_days(),
                    "device certificate validity ok"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not parse cert validity");
        }
    }
}
