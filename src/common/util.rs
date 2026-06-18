// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.

use futures::stream::{Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::broadcast::Receiver;
use tokio_stream::wrappers::BroadcastStream;

/// Adapts a [`tokio::sync::broadcast::Receiver`] into a generic, boxed, pinned [`Stream`].
///
/// Broadcast receivers yield `Result<T, BroadcastStreamRecvError>`, since a receiver that falls
/// behind can miss messages (a "lagged" error). This drops those errors and yields only
/// successfully-received items, which is sufficient for our purposes (notifications, pairing
/// requests, etc.) since we don't currently have a way to act on "you missed some messages"
/// beyond just continuing to listen.
pub fn stream_from_broadcast_receiver<T>(
    receiver: Receiver<T>,
) -> Pin<Box<dyn Stream<Item = T> + Send>>
where
    T: Clone + Send + 'static,
{
    Box::pin(BroadcastStream::new(receiver).filter_map(|x| async move { x.ok() }))
}
