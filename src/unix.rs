use super::{K5Ctx, K5ServerCtx};
use anyhow::{Error, Result};
#[cfg(feature = "krb5_iov")]
use bytes::Buf as _;
use bytes::BytesMut;
#[cfg(feature = "krb5_iov")]
use libgssapi::util::{GssIov, GssIovFake, GssIovType};
use libgssapi::{
    context::{ClientCtx as GssClientCtx, CtxFlags, SecurityContext, ServerCtx as GssServerCtx},
    credential::{Cred, CredUsage},
    error::{Error as GssError, MajorFlags},
    name::Name,
    oid::{OidSet, GSS_MECH_KRB5, GSS_NT_KRB5_PRINCIPAL},
    util::Buf,
};
use std::time::Duration;

#[cfg(feature = "krb5_iov")]
fn wrap_iov(
    ctx: &impl SecurityContext,
    encrypt: bool,
    header: &mut BytesMut,
    data: &mut BytesMut,
    padding: &mut BytesMut,
    trailer: &mut BytesMut,
) -> Result<()> {
    let mut len_iovs = [
        GssIovFake::new(GssIovType::Header),
        GssIov::new(GssIovType::Data, &mut **data).as_fake(),
        GssIovFake::new(GssIovType::Padding),
        GssIovFake::new(GssIovType::Trailer),
    ];
    ctx.wrap_iov_length(encrypt, &mut len_iovs[..])?;
    header.resize(len_iovs[0].len(), 0x0);
    padding.resize(len_iovs[2].len(), 0x0);
    trailer.resize(len_iovs[3].len(), 0x0);
    let mut iovs = [
        GssIov::new(GssIovType::Header, &mut **header),
        GssIov::new(GssIovType::Data, &mut **data),
        GssIov::new(GssIovType::Padding, &mut **padding),
        GssIov::new(GssIovType::Trailer, &mut **trailer),
    ];
    Ok(ctx.wrap_iov(encrypt, &mut iovs)?)
}

#[cfg(not(feature = "krb5_iov"))]
fn wrap_iov(
    ctx: &impl SecurityContext,
    encrypt: bool,
    _header: &mut BytesMut,
    data: &mut BytesMut,
    _padding: &mut BytesMut,
    _trailer: &mut BytesMut,
) -> Result<()> {
    let token = ctx.wrap(encrypt, &**data)?;
    data.clear();
    Ok(data.extend_from_slice(&*token))
}

#[cfg(feature = "krb5_iov")]
fn unwrap_iov(ctx: &impl SecurityContext, len: usize, msg: &mut BytesMut) -> Result<BytesMut> {
    let (hdr_len, data_len) = {
        let mut iov = [
            GssIov::new(GssIovType::Stream, &mut msg[0..len]),
            GssIov::new(GssIovType::Data, &mut []),
        ];
        ctx.unwrap_iov(&mut iov[..])?;
        let hdr_len = iov[0].header_length(&iov[1]).unwrap();
        let data_len = iov[1].len();
        (hdr_len, data_len)
    };
    msg.advance(hdr_len);
    let data = msg.split_to(data_len);
    msg.advance(len - hdr_len - data_len);
    Ok(data) // return the decrypted contents
}

#[cfg(not(feature = "krb5_iov"))]
fn unwrap_iov(ctx: &impl SecurityContext, len: usize, msg: &mut BytesMut) -> Result<BytesMut> {
    let mut msg = msg.split_to(len);
    let decrypted = ctx.unwrap(&*msg)?;
    msg.clear();
    msg.extend_from_slice(&*decrypted);
    Ok(msg)
}

#[derive(Debug, Clone)]
pub struct ClientCtx(GssClientCtx);

impl ClientCtx {
    pub fn new(principal: Option<&str>, target_principal: &str) -> Result<Self> {
        let name = principal
            .map(|n| {
                Name::new(n.as_bytes(), Some(&GSS_NT_KRB5_PRINCIPAL))?
                    .canonicalize(Some(&GSS_MECH_KRB5))
            })
            .transpose()?;
        let target = Name::new(target_principal.as_bytes(), Some(&GSS_NT_KRB5_PRINCIPAL))?
            .canonicalize(Some(&GSS_MECH_KRB5))?;
        let cred = {
            let mut s = OidSet::new()?;
            s.add(&GSS_MECH_KRB5)?;
            Cred::acquire(name.as_ref(), None, CredUsage::Initiate, Some(&s))?
        };
        Ok(ClientCtx(GssClientCtx::new(
            cred,
            target,
            CtxFlags::GSS_C_MUTUAL_FLAG,
            Some(&GSS_MECH_KRB5),
        )))
    }
}

impl K5Ctx for ClientCtx {
    type Buf = Buf;

    fn step(&self, token: Option<&[u8]>) -> Result<Option<Self::Buf>> {
        self.0.step(token).map_err(|e| Error::from(e))
    }

    fn wrap(&self, encrypt: bool, msg: &[u8]) -> Result<Self::Buf> {
        self.0.wrap(encrypt, msg).map_err(|e| Error::from(e))
    }

    fn wrap_iov(
        &self,
        encrypt: bool,
        header: &mut BytesMut,
        data: &mut BytesMut,
        padding: &mut BytesMut,
        trailer: &mut BytesMut,
    ) -> Result<()> {
        wrap_iov(&self.0, encrypt, header, data, padding, trailer)
    }

    fn unwrap(&self, msg: &[u8]) -> Result<Self::Buf> {
        self.0.unwrap(msg).map_err(|e| Error::from(e))
    }

    fn unwrap_iov(&self, len: usize, msg: &mut BytesMut) -> Result<BytesMut> {
        unwrap_iov(&self.0, len, msg)
    }

    fn ttl(&self) -> Result<Duration> {
        self.0.lifetime().map_err(|e| Error::from(e))
    }
}

#[derive(Debug, Clone)]
pub struct ServerCtx(GssServerCtx);

impl ServerCtx {
    pub fn new(principal: Option<&str>) -> Result<ServerCtx> {
        let name = principal
            .map(|principal| -> Result<Name> {
                Ok(
                    Name::new(principal.as_bytes(), Some(&GSS_NT_KRB5_PRINCIPAL))?
                        .canonicalize(Some(&GSS_MECH_KRB5))?,
                )
            })
            .transpose()?;
        let cred = {
            let mut s = OidSet::new()?;
            s.add(&GSS_MECH_KRB5)?;
            Cred::acquire(name.as_ref(), None, CredUsage::Accept, Some(&s))?
        };
        Ok(ServerCtx(GssServerCtx::new(cred)))
    }
}

impl K5Ctx for ServerCtx {
    type Buf = Buf;

    fn step(&self, token: Option<&[u8]>) -> Result<Option<Self::Buf>> {
        match token {
            Some(token) => self.0.step(token),
            None => Err(GssError {
                major: MajorFlags::GSS_S_DEFECTIVE_TOKEN,
                minor: 0,
            }),
        }
        .map_err(|e| Error::from(e))
    }

    fn wrap(&self, encrypt: bool, msg: &[u8]) -> Result<Self::Buf> {
        self.0.wrap(encrypt, msg).map_err(|e| Error::from(e))
    }

    fn wrap_iov(
        &self,
        encrypt: bool,
        header: &mut BytesMut,
        data: &mut BytesMut,
        padding: &mut BytesMut,
        trailer: &mut BytesMut,
    ) -> Result<()> {
        wrap_iov(&self.0, encrypt, header, data, padding, trailer)
    }

    fn unwrap(&self, msg: &[u8]) -> Result<Self::Buf> {
        self.0.unwrap(msg).map_err(|e| Error::from(e))
    }

    fn unwrap_iov(&self, len: usize, msg: &mut BytesMut) -> Result<BytesMut> {
        unwrap_iov(&self.0, len, msg)
    }

    fn ttl(&self) -> Result<Duration> {
        self.0.lifetime().map_err(|e| Error::from(e))
    }
}

impl K5ServerCtx for ServerCtx {
    fn client(&self) -> Result<String> {
        let n = self.0.source_name().map_err(|e| Error::from(e))?;
        Ok(format!("{}", n))
    }
}
