use thiserror::Error;

use crate::runtime_interface::RuntimeCapabilities;

use super::lexical::validate_safe_text;
use super::limits::{
    ProtocolBootstrapResponse, ProtocolLimits, ProtocolNegotiation, ProtocolReject,
    ProtocolVersion, ProtocolWelcome, RuntimeInfo, negotiate_protocol,
};
use super::typed_json::{TypedJsonError, WireV1Codec, decode_protocol_hello_v1};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProtocolBootstrapRouterError {
    #[error("runtime bootstrap identity is empty, unsafe, or exceeds its limit")]
    InvalidRuntimeIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolBootstrapRouter {
    implementation: Box<str>,
    implementation_version: Box<str>,
    capabilities: RuntimeCapabilities,
}

impl ProtocolBootstrapRouter {
    pub fn new(
        implementation: impl AsRef<str>,
        implementation_version: impl AsRef<str>,
        capabilities: RuntimeCapabilities,
    ) -> Result<Self, ProtocolBootstrapRouterError> {
        let implementation = implementation.as_ref();
        let implementation_version = implementation_version.as_ref();
        validate_safe_text(implementation, 128, false)
            .map_err(|_| ProtocolBootstrapRouterError::InvalidRuntimeIdentity)?;
        validate_safe_text(implementation_version, 128, false)
            .map_err(|_| ProtocolBootstrapRouterError::InvalidRuntimeIdentity)?;
        Ok(Self {
            implementation: implementation.into(),
            implementation_version: implementation_version.into(),
            capabilities,
        })
    }

    pub fn route(&self, input: &[u8]) -> Result<ProtocolBootstrapRoute, TypedJsonError> {
        let hello = decode_protocol_hello_v1(input)?;
        let route = match negotiate_protocol(&hello, &self.capabilities) {
            ProtocolNegotiation::Selected {
                version,
                capabilities,
            } => {
                let welcome = ProtocolWelcome::new(
                    version,
                    RuntimeInfo::new(
                        version,
                        self.implementation.clone(),
                        self.implementation_version.clone(),
                    ),
                    capabilities,
                    ProtocolLimits::v1_0(),
                );
                ProtocolBootstrapRoute {
                    response: ProtocolBootstrapResponse::Welcome(welcome),
                    codec: Some(WireV1Codec::v1_0()),
                }
            }
            ProtocolNegotiation::Rejected { reason } => ProtocolBootstrapRoute {
                response: ProtocolBootstrapResponse::Reject(ProtocolReject::new(
                    reason,
                    vec![ProtocolVersion::V1_0],
                )),
                codec: None,
            },
        };
        Ok(route)
    }

    pub const fn capabilities(&self) -> &RuntimeCapabilities {
        &self.capabilities
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolBootstrapRoute {
    response: ProtocolBootstrapResponse,
    codec: Option<WireV1Codec>,
}

impl ProtocolBootstrapRoute {
    pub const fn response(&self) -> &ProtocolBootstrapResponse {
        &self.response
    }

    pub const fn codec(&self) -> Option<&WireV1Codec> {
        self.codec.as_ref()
    }

    pub fn into_parts(self) -> (ProtocolBootstrapResponse, Option<WireV1Codec>) {
        (self.response, self.codec)
    }
}
