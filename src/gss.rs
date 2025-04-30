//! SPNEGO/Kerberos helper for HTTP `Negotiate` authentication.
//!
//! Builds a one-shot SPNEGO initial token for the service `HTTP@<host>`
//! using credentials from the caller's Kerberos credential cache. The
//! token is base64-encoded and intended to be sent in an
//! `Authorization: Negotiate <token>` header per RFC 4559.
//!
//! For the HTTP profile we only need the initial token from the
//! client; multi-round negotiation (e.g. for mutual auth confirmation)
//! is ignored; the server's response either succeeds or fails outright.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use libgssapi::context::{ClientCtx, CtxFlags};
use libgssapi::credential::{Cred, CredUsage};
use libgssapi::name::Name;
use libgssapi::oid::{GSS_MECH_KRB5, GSS_MECH_SPNEGO, GSS_NT_HOSTBASED_SERVICE, OidSet};

/// Build a base64-encoded SPNEGO initial token for `HTTP@<host>`.
pub fn spnego_token(host: &str) -> Result<String> {
    let target = Name::new(
        format!("HTTP@{host}").as_bytes(),
        Some(GSS_NT_HOSTBASED_SERVICE),
    )
    .context("constructing GSSAPI target name")?
    .canonicalize(Some(GSS_MECH_SPNEGO))
    .context("canonicalizing target name")?;

    let mut mechs = OidSet::new();
    mechs
        .add(GSS_MECH_KRB5)
        .context("adding Kerberos OID to mechanism set")?;
    let cred = Cred::acquire(None, None, CredUsage::Initiate, Some(&mechs))
        .context("acquiring GSSAPI credentials (is there a Kerberos ticket?)")?;

    let mut ctx = ClientCtx::new(
        Some(cred),
        target,
        CtxFlags::GSS_C_MUTUAL_FLAG | CtxFlags::GSS_C_SEQUENCE_FLAG,
        Some(GSS_MECH_SPNEGO),
    );
    let token = ctx
        .step(None, None)
        .context("generating SPNEGO initial token")?
        .ok_or_else(|| anyhow!("GSSAPI returned no SPNEGO token"))?;

    Ok(BASE64.encode(&*token))
}
