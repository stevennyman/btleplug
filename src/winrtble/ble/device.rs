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

use std::time::Duration;

use crate::{
    Error, Result,
    api::{BDAddr, PairingRequestKind},
    winrtble::errors,
};
use log::{debug, trace, warn};
use tokio::time::timeout;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
        BluetoothLEPreferredConnectionParameters,
        GenericAttributeProfile::{
            GattCharacteristic, GattCommunicationStatus, GattDescriptor, GattDeviceService,
            GattDeviceServicesResult, GattSession,
        },
    },
    Devices::Enumeration::{
        DeviceInformationCustomPairing, DevicePairingKinds, DevicePairingProtectionLevel,
        DevicePairingRequestedEventArgs,
    },
    Foundation::{Deferral, TypedEventHandler},
};

/// Timeout for uncached GATT operations before falling back to cached mode.
/// Some Windows BLE drivers hang indefinitely on uncached requests (see #325).
const GATT_CACHE_TIMEOUT: Duration = Duration::from_secs(5);

pub type ConnectedEventHandler = Box<dyn Fn(bool) + Send>;
pub type MaxPduSizeChangedEventHandler = Box<dyn Fn(u16) + Send>;

/// Called whenever Windows fires a `PairingRequested` event during an in-progress pairing
/// ceremony. The handler must stash the `DevicePairingRequestedEventArgs`/`Deferral` somewhere
/// it can be retrieved later (see `Shared::pending_pairing` in `winrtble/peripheral.rs`) - they
/// can't be reconstructed from a plain enum value, and `Accept`/`AcceptWithPin` must be called
/// on this exact `args` instance before completing the deferral.
pub type PairingRequestedHandler =
    Box<dyn Fn(PairingRequestKind, DevicePairingRequestedEventArgs, Deferral) + Send>;

pub struct BLEDevice {
    device: BluetoothLEDevice,
    gatt_session: GattSession,
    connection_token: i64,
    pdu_change_token: i64,
    services: Vec<GattDeviceService>,
}

impl BLEDevice {
    pub async fn new(
        address: BDAddr,
        connection_status_changed: ConnectedEventHandler,
        max_pdu_size_changed: MaxPduSizeChangedEventHandler,
    ) -> Result<Self> {
        let async_op = BluetoothLEDevice::FromBluetoothAddressAsync(address.into())
            .map_err(|_| Error::DeviceNotFound)?;
        let device = async_op.await.map_err(|_| Error::DeviceNotFound)?;

        let async_op = GattSession::FromDeviceIdAsync(&device.BluetoothDeviceId()?)
            .map_err(|_| Error::DeviceNotFound)?;
        let gatt_session = async_op.await.map_err(|_| Error::DeviceNotFound)?;

        let connection_status_handler =
            TypedEventHandler::<BluetoothLEDevice, _>::new(move |sender, _| {
                if let Some(sender) = sender.as_ref() {
                    let is_connected = sender
                        .ConnectionStatus()
                        .ok()
                        .map_or(false, |v| v == BluetoothConnectionStatus::Connected);
                    connection_status_changed(is_connected);
                    trace!("state {:?}", sender.ConnectionStatus());
                }
                Ok(())
            });
        let connection_token = device
            .ConnectionStatusChanged(&connection_status_handler)
            .map_err(|_| Error::Other("Could not add connection status handler".into()))?;

        max_pdu_size_changed(gatt_session.MaxPduSize().unwrap());
        let max_pdu_size_changed_handler =
            TypedEventHandler::<GattSession, _>::new(move |sender, _| {
                if let Some(sender) = sender.as_ref() {
                    max_pdu_size_changed(sender.MaxPduSize().unwrap());
                }
                Ok(())
            });
        let pdu_change_token = gatt_session
            .MaxPduSizeChanged(&max_pdu_size_changed_handler)
            .map_err(|_| Error::Other("Could not add max pdu size changed handler".into()))?;

        Ok(BLEDevice {
            device,
            gatt_session,
            connection_token,
            pdu_change_token,
            services: vec![],
        })
    }

    async fn get_gatt_services(
        &self,
        cache_mode: BluetoothCacheMode,
    ) -> Result<GattDeviceServicesResult> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let async_op = self
            .device
            .GetGattServicesWithCacheModeAsync(cache_mode)
            .map_err(winrt_error)?;
        let service_result = async_op.await.map_err(winrt_error)?;
        Ok(service_result)
    }

    pub fn name(&self) -> windows::core::Result<windows::core::HSTRING> {
        self.device.Name()
    }

    pub async fn connect(&self) -> Result<()> {
        if self.is_connected().await? {
            return Ok(());
        }

        let service_result = self.get_gatt_services(BluetoothCacheMode::Uncached).await?;
        let status = service_result.Status().map_err(|_| Error::DeviceNotFound)?;
        errors::check_gatt("connect", &status)
    }

    async fn is_connected(&self) -> Result<bool> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let status = self.device.ConnectionStatus().map_err(winrt_error)?;

        Ok(status == BluetoothConnectionStatus::Connected)
    }

    pub async fn get_characteristics(
        service: &GattDeviceService,
    ) -> Result<Vec<GattCharacteristic>> {
        let async_result = match timeout(
            GATT_CACHE_TIMEOUT,
            service
                .GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached)?
                .into_future(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                warn!("Uncached characteristic discovery timed out, falling back to cached mode");
                service
                    .GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Cached)?
                    .await?
            }
        };

        match async_result.Status() {
            Ok(GattCommunicationStatus::Success) => {
                let results = async_result.Characteristics()?;
                debug!("characteristics {:?}", results.Size());
                Ok(results.into_iter().collect())
            }
            Ok(GattCommunicationStatus::ProtocolError) => Err(Error::Other(
                format!(
                    "get_characteristics for {:?} encountered a protocol error",
                    service
                )
                .into(),
            )),
            Ok(status) => {
                debug!("characteristic read failed due to {:?}", status);
                Ok(vec![])
            }
            Err(e) => Err(Error::Other(
                format!("get_characteristics for {:?} failed: {:?}", service, e).into(),
            )),
        }
    }

    pub async fn get_characteristic_descriptors(
        characteristic: &GattCharacteristic,
    ) -> Result<Vec<GattDescriptor>> {
        let async_result = match timeout(
            GATT_CACHE_TIMEOUT,
            characteristic
                .GetDescriptorsWithCacheModeAsync(BluetoothCacheMode::Uncached)?
                .into_future(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                warn!("Uncached descriptor discovery timed out, falling back to cached mode");
                characteristic
                    .GetDescriptorsWithCacheModeAsync(BluetoothCacheMode::Cached)?
                    .await?
            }
        };
        let status = async_result.Status();
        if status == Ok(GattCommunicationStatus::Success) {
            let results = async_result.Descriptors()?;
            debug!("descriptors {:?}", results.Size());
            Ok(results.into_iter().collect())
        } else {
            Err(Error::Other(
                format!(
                    "get_characteristic_descriptors for {:?} failed: {:?}",
                    characteristic, status
                )
                .into(),
            ))
        }
    }

    pub fn get_connection_parameters(&self) -> Result<crate::api::ConnectionParameters> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let params = self.device.GetConnectionParameters().map_err(winrt_error)?;
        // ConnectionInterval is in units of 1.25ms, convert to microseconds
        let interval_us = (params.ConnectionInterval().map_err(winrt_error)? as u32) * 1250;
        let latency = params.ConnectionLatency().map_err(winrt_error)? as u16;
        // LinkTimeout is in units of 10ms, convert to microseconds
        let supervision_timeout_us = (params.LinkTimeout().map_err(winrt_error)? as u32) * 10_000;
        Ok(crate::api::ConnectionParameters {
            interval_us,
            latency,
            supervision_timeout_us,
        })
    }

    pub fn request_connection_parameters(
        &self,
        preset: crate::api::ConnectionParameterPreset,
    ) -> Result<()> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let params = match preset {
            crate::api::ConnectionParameterPreset::Balanced => {
                BluetoothLEPreferredConnectionParameters::Balanced()
            }
            crate::api::ConnectionParameterPreset::ThroughputOptimized => {
                BluetoothLEPreferredConnectionParameters::ThroughputOptimized()
            }
            crate::api::ConnectionParameterPreset::PowerOptimized => {
                BluetoothLEPreferredConnectionParameters::PowerOptimized()
            }
        }
        .map_err(winrt_error)?;
        let result = self
            .device
            .RequestPreferredConnectionParameters(&params)
            .map_err(winrt_error)?;
        let status = result.Status().map_err(winrt_error)?;
        // BluetoothLEPreferredConnectionParametersRequestStatus:
        //   Unspecified = 0, Success = 1, DeviceNotAvailable = 2, AccessDenied = 3
        match status.0 {
            1 => Ok(()),
            2 | 3 => Err(Error::NotSupported(format!(
                "request_connection_parameters not supported (status {:?})",
                status
            ))),
            _ => Err(Error::Other(
                format!(
                    "RequestPreferredConnectionParameters failed with status {:?}",
                    status
                )
                .into(),
            )),
        }
    }

    pub async fn discover_services(&mut self) -> Result<&[GattDeviceService]> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());
        let service_result = self.get_gatt_services(BluetoothCacheMode::Cached).await?;
        let status = service_result.Status().map_err(winrt_error)?;
        if status == GattCommunicationStatus::Success {
            // We need to convert the IVectorView to a Vec, because IVectorView is not Send and so
            // can't be help past the await point below.
            let services: Vec<_> = service_result
                .Services()
                .map_err(winrt_error)?
                .into_iter()
                .collect();
            self.services = services;
            debug!("services {:?}", self.services.len());
        }
        Ok(self.services.as_slice())
    }

    /// Pairs with the device if it isn't already paired. `on_pairing_requested` is invoked
    /// (possibly multiple times, once per ceremony step) whenever Windows needs the application
    /// to confirm a PIN or otherwise acknowledge the pairing request. This call doesn't return
    /// until the ceremony completes one way or another - successfully, rejected, or timed out -
    /// since the corresponding `PairAsync` WinRT call doesn't resolve until then.
    pub async fn start_pairing(&self, on_pairing_requested: PairingRequestedHandler) -> Result<()> {
        let winrt_error = |e| Error::Other(format!("{:?}", e).into());

        let device_information = self.device.DeviceInformation().map_err(winrt_error)?;
        let pairing = device_information.Pairing().map_err(winrt_error)?;

        if pairing.IsPaired().unwrap_or(false) {
            debug!("start_pairing: already paired, nothing to do");
            return Ok(());
        }

        let custom_pairing = pairing.Custom().map_err(winrt_error)?;
        let pairing_requested_token =
            register_pairing_requested_handler(&custom_pairing, on_pairing_requested)
                .map_err(winrt_error)?;

        // Only request the ceremony kinds we actually know how to drive a response for.
        // ProvidePasswordCredential is intentionally excluded - it's effectively unused for BLE
        // peripherals and we don't have a response path for it.
        let kinds = DevicePairingKinds::ConfirmOnly
            | DevicePairingKinds::DisplayPin
            | DevicePairingKinds::ProvidePin
            | DevicePairingKinds::ConfirmPinMatch;

        // `Default` lets Windows skip MITM-protected pairing if it judges the device doesn't
        // need it. Some peripherals advertise GATT attributes that require authentication or
        // encryption (surfaced as `GattProtocolError::InsufficientAuthentication`/
        // `InsufficientEncryption` on read/write) but don't strictly require it during the
        // pairing ceremony itself, so a `Default`-level pairing can complete successfully while
        // leaving those attributes still inaccessible - the retried operation in
        // `Peripheral::ensure_paired` then fails with the exact same error, in a loop.
        // Requesting `EncryptionAndAuthentication` makes Windows negotiate the strongest method
        // the device supports up front, which is what actually unlocks those attributes.
        let pair_result = custom_pairing
            .PairWithProtectionLevelAsync(
                kinds,
                DevicePairingProtectionLevel::EncryptionAndAuthentication,
            )
            .map_err(winrt_error)?
            .await
            .map_err(winrt_error);

        // Best-effort cleanup. A failure here shouldn't mask the actual pairing result, and
        // there's nothing useful we can do about it beyond logging.
        if let Err(err) = custom_pairing.RemovePairingRequested(pairing_requested_token) {
            debug!("start_pairing: remove_pairing_requested {:?}", err);
        }

        let status = pair_result?.Status().map_err(winrt_error)?;
        errors::pairing_status_to_error(status)
    }
}

fn register_pairing_requested_handler(
    custom_pairing: &DeviceInformationCustomPairing,
    on_pairing_requested: PairingRequestedHandler,
) -> windows::core::Result<i64> {
    let pairing_requested_handler = TypedEventHandler::<
        DeviceInformationCustomPairing,
        DevicePairingRequestedEventArgs,
    >::new(move |_, args| {
        if let Some(args) = args.as_ref() {
            let deferral = args.GetDeferral()?;
            let kind = match args.PairingKind() {
                Ok(DevicePairingKinds::DisplayPin) => PairingRequestKind::DisplayPin(
                    args.Pin().map(|pin| pin.to_string()).unwrap_or_default(),
                ),
                Ok(DevicePairingKinds::ConfirmPinMatch) => PairingRequestKind::ConfirmPinMatch(
                    args.Pin().map(|pin| pin.to_string()).unwrap_or_default(),
                ),
                Ok(DevicePairingKinds::ProvidePin) => PairingRequestKind::ProvidePin,
                // ConfirmOnly, and anything we don't explicitly recognize, is treated as
                // a plain yes/no confirmation.
                _ => PairingRequestKind::ConfirmOnly,
            };
            on_pairing_requested(kind, args.clone(), deferral);
        }
        Ok(())
    });

    // TypedEventHandler wraps a raw COM pointer and is not Send. Keep it inside this synchronous
    // helper so it cannot be captured by the async state machine in `start_pairing`.
    custom_pairing.PairingRequested(&pairing_requested_handler)
}

impl Drop for BLEDevice {
    fn drop(&mut self) {
        let result = self
            .gatt_session
            .RemoveMaxPduSizeChanged(self.pdu_change_token);
        if let Err(err) = result {
            debug!("Drop: remove_max_pdu_size_changed {:?}", err);
        }

        let result = self
            .device
            .RemoveConnectionStatusChanged(self.connection_token);
        if let Err(err) = result {
            debug!("Drop:remove_connection_status_changed {:?}", err);
        }

        self.services.iter().for_each(|service| {
            if let Err(err) = service.Close() {
                debug!("Drop:remove_gatt_Service {:?}", err);
            }
        });

        let result = self.device.Close();
        if let Err(err) = result {
            debug!("Drop:close {:?}", err);
        }
    }
}
