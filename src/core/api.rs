extern crate futures;

use crate::errors::RSocketError;
use crate::payload::Payload;
use futures::{Future, Stream};

pub trait RSocket: Sync + Send {
  fn metadata_push(&self, req: Payload) -> Box<Future<Item = (), Error = RSocketError>>;
  fn request_fnf(&self, req: Payload) -> Box<Future<Item = (), Error = RSocketError>>;
  fn request_response(&self, req: Payload) -> Box<Future<Item = Payload, Error = RSocketError>>;
  fn request_stream(&self, req: Payload) -> Box<Stream<Item = Payload, Error = RSocketError>>;
}
