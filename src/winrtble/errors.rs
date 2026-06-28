// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

use crate::{Error, Result};
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCommunicationStatus, GattProtocolError, GattReadResult, GattWriteResult,
};
use windows::Devices::Enumeration::DevicePairingResultStatus;

type WinResult<T> = windows::core::Result<T>;

/// Inspects a completed GATT operation result and converts it into a btleplug [`Result`].
///
/// Unlike the bare [`GattCommunicationStatus`] returned by the older `*Async` GATT APIs, the
/// `*WithResultAsync` APIs (e.g. `WriteValueWithResultAndOptionAsync`,
/// `ReadValueWithCacheModeAsync`'s [`GattReadResult`]) additionally expose a
/// [`GattProtocolError`] byte when `status == ProtocolError`. That's the only place the ATT-level
/// reason ("insufficient authentication", "insufficient encryption", etc.) is visible - a plain
/// `AccessDenied`/`ProtocolError` status alone doesn't tell us *why*, and on real BLE peripherals
/// "needs pairing" surfaces as `ProtocolError`/`InsufficientAuthentication`, not as
/// `AccessDenied`. See https://github.com/deviceplug/btleplug (Windows pairing) for context.
pub(crate) fn check_gatt(operation: &str, result: &impl GattResult) -> Result<()> {
    let status = result.status()?;
    if status == GattCommunicationStatus::Success {
        return Ok(());
    }

    if status == GattCommunicationStatus::AccessDenied {
        return Err(Error::PermissionDenied);
    }

    if status == GattCommunicationStatus::Unreachable {
        return Err(Error::NotConnected);
    }

    if status == GattCommunicationStatus::ProtocolError {
        if let Ok(Some(protocol_error)) = result.protocol_error() {
            if is_auth_protocol_error(protocol_error).unwrap_or(false) {
                return Err(Error::PermissionDenied);
            }
        }
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

/// Whether a [`GattProtocolError`] byte indicates the link needs to be paired/authenticated/
/// encrypted before the operation can succeed. These are the cases where retrying after a
/// successful `pair()` has a realistic chance of fixing the underlying issue.
fn is_auth_protocol_error(protocol_error: u8) -> WinResult<bool> {
    Ok(
        protocol_error == GattProtocolError::InsufficientAuthentication()?
            || protocol_error == GattProtocolError::InsufficientAuthorization()?
            || protocol_error == GattProtocolError::InsufficientEncryption()?
            || protocol_error == GattProtocolError::InsufficientEncryptionKeySize()?,
    )
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

/// Maps a Windows pairing result status to a btleplug [`Error`]. `Paired` and `AlreadyPaired`
/// are treated as success; everything else (rejection, timeout, hardware failure, etc.) is
/// folded into [`Error::Other`] with the status preserved in the message for debugging, since
/// the long tail of `DevicePairingResultStatus` variants doesn't map cleanly onto btleplug's
/// existing error categories.
pub(crate) fn pairing_status_to_error(status: DevicePairingResultStatus) -> Result<()> {
    if status == DevicePairingResultStatus::Paired
        || status == DevicePairingResultStatus::AlreadyPaired
    {
        Ok(())
    } else {
        Err(Error::Other(
            format!("Pairing failed with status {:?}", status).into(),
        ))
    }
}

/// Maps a Windows pairing result status to a [`crate::api::PairingOutcome`] for reporting on
/// the `pairing_requests()` stream. This is the [`PairingOutcome`](crate::api::PairingOutcome)
/// counterpart to [`pairing_status_to_error`] - same input, but producing a value to broadcast
/// to pairing_requests() listeners rather than an `Err` to return from `pair()`.
pub(crate) fn pairing_status_to_outcome(
    status: DevicePairingResultStatus,
) -> crate::api::PairingOutcome {
    use crate::api::PairingOutcome;
    match status {
        DevicePairingResultStatus::Paired | DevicePairingResultStatus::AlreadyPaired => {
            PairingOutcome::Paired
        }
        DevicePairingResultStatus::AuthenticationTimeout => PairingOutcome::AuthenticationTimeout,
        DevicePairingResultStatus::PairingCanceled => PairingOutcome::Canceled,
        DevicePairingResultStatus::RejectedByHandler => PairingOutcome::Rejected,
        other => PairingOutcome::Failed(format!("{:?}", other)),
    }
}
