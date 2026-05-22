use crate::{Error, Result};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCommunicationStatus, GattProtocolError, GattReadResult, GattWriteResult,
};

type WinResult<T> = windows::core::Result<T>;

pub(crate) fn check_gatt(operation: &str, result: &impl GattResult) -> Result<()> {
    let status = result.status()?;
    if status == GattCommunicationStatus::Success {
        return Ok(());
    }

    let status_name = match status {
        GattCommunicationStatus::Success => "Success",
        GattCommunicationStatus::Unreachable => "Unreachable",
        GattCommunicationStatus::ProtocolError => "ProtocolError",
        GattCommunicationStatus::AccessDenied => "AccessDenied",
        _ => "Unknown",
    };

    let mut message = format!("WinRT threw error on {}: {}", operation, status_name);

    if status == GattCommunicationStatus::ProtocolError {
        match result.protocol_error().and_then(|error| {
            error
                .map(gatt_protocol_error_name)
                .transpose()
                .map(Option::flatten)
        }) {
            Ok(Some(name)) => message.push_str(&format!(" ({})", name)),
            Ok(None) => message.push_str(" (unknown protocol error)"),
            Err(err) => message.push_str(&format!(" (failed to read protocol error: {:?})", err)),
        }
    }

    Err(Error::Other(message.into()))
}

pub(crate) trait GattResult {
    fn status(&self) -> WinResult<GattCommunicationStatus>;
    fn protocol_error(&self) -> WinResult<Option<u8>> {
        Ok(None)
    }
}

impl GattResult for GattCommunicationStatus {
    fn status(&self) -> WinResult<GattCommunicationStatus> {
        Ok(*self)
    }
}

macro_rules! impl_gatt_result {
    ($ty:ty) => {
        impl GattResult for $ty {
            fn status(&self) -> WinResult<GattCommunicationStatus> {
                self.Status()
            }

            fn protocol_error(&self) -> WinResult<Option<u8>> {
                self.ProtocolError()
                    .and_then(|error| error.Value())
                    .map(Some)
            }
        }
    };
}
impl_gatt_result!(GattReadResult);
impl_gatt_result!(GattWriteResult);

fn gatt_protocol_error_name(protocol_error: u8) -> WinResult<Option<&'static str>> {
    macro_rules! check_protocol_errors {
        ($($name:ident),* $(,)?) => {$(
            if protocol_error == GattProtocolError::$name()? {
                return Ok(Some(stringify!($name)));
            }
        )*};
    }

    check_protocol_errors!(
        InvalidHandle,
        ReadNotPermitted,
        WriteNotPermitted,
        InvalidPdu,
        InsufficientAuthentication,
        RequestNotSupported,
        InvalidOffset,
        InsufficientAuthorization,
        PrepareQueueFull,
        AttributeNotFound,
        AttributeNotLong,
        InsufficientEncryptionKeySize,
        InvalidAttributeValueLength,
        UnlikelyError,
        InsufficientEncryption,
        UnsupportedGroupType,
        InsufficientResources,
    );

    Ok(None)
}
