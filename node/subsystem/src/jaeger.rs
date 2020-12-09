// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Jaeger integration.
//!
//! See <https://www.jaegertracing.io/> for an introduction.
//!
//! The easiest way to try Jaeger is:
//!
//! - Start a docker container with the all-in-one docker image (see below).
//! - Open your browser and navigate to <https://localhost:16686> to acces the UI.
//!
//! The all-in-one image can be started with:
//!
//! ```not_rust
//! podman login docker.io
//! podman run -d --name jaeger \
//!  -e COLLECTOR_ZIPKIN_HTTP_PORT=9411 \
//!  -p 5775:5775/udp \
//!  -p 6831:6831/udp \
//!  -p 6832:6832/udp \
//!  -p 5778:5778 \
//!  -p 16686:16686 \
//!  -p 14268:14268 \
//!  -p 14250:14250 \
//!  -p 9411:9411 \
//!  jaegertracing/all-in-one:1.21
//! ```
//!

use polkadot_primitives::v1::{Hash, PoV, CandidateHash};
use parking_lot::Mutex;
use std::sync::Arc;
use std::result;
pub use crate::errors::JaegerError;


lazy_static::lazy_static! {
	static ref INSTANCE: Mutex<Jaeger> = Mutex::new(Jaeger::None);
}

/// Configuration for the jaeger tracing.
#[derive(Clone)]
pub struct JaegerConfig {
	node_name: String,
	agent_addr: std::net::SocketAddr,
}

impl std::default::Default for JaegerConfig {
	fn default() -> Self {
		Self {
			node_name: "unknown_".to_owned(),
			agent_addr: "127.0.0.1:6831".parse().unwrap(),
		}
	}
}

impl JaegerConfig {
	/// Use the builder pattern to construct a configuration.
	pub fn builder() -> JaegerConfigBuilder {
		JaegerConfigBuilder::default()
	}
}


/// Jaeger configuration builder.
#[derive(Default)]
pub struct JaegerConfigBuilder {
	inner: JaegerConfig
}

impl JaegerConfigBuilder {
	/// Set the name for this node.
	pub fn named<S>(mut self, name: S) -> Self where S: AsRef<str> {
		self.inner.node_name = name.as_ref().to_owned();
		self
	}

	/// Set the agent address to send the collected spans to.
	pub fn agent<U>(mut self, addr: U) -> Self where U: Into<std::net::SocketAddr> {
		self.inner.agent_addr = addr.into();
		self
	}

	/// Construct the configuration.
	pub fn build(self) -> JaegerConfig {
		self.inner
	}
}

/// A wrapper type for a span.
///
/// Handles running with and without jaeger.
pub enum JaegerSpan {
	/// Running with jaeger being enabled.
	Enabled(mick_jaeger::Span),
	/// Running with jaeger disabled.
	Disabled,
}

impl JaegerSpan {
	/// Derive a child span from `self`.
	pub fn child(&self, name: impl Into<String>) -> Self {
		match self {
			Self::Enabled(inner) => Self::Enabled(inner.child(name)),
			Self::Disabled => Self::Disabled,
		}
	}
	/// Add an additional tag to the span.
	pub fn add_string_tag(&mut self, tag: &str, value: &str) {
		match self {
			Self::Enabled(ref mut inner) => inner.add_string_tag(tag, value),
			Self::Disabled => {},
		}
	}
}

impl From<Option<mick_jaeger::Span>> for JaegerSpan {
	fn from(src: Option<mick_jaeger::Span>) -> Self {
		if let Some(span) = src {
			Self::Enabled(span)
		} else {
			Self::Disabled
		}
	}
}

impl From<mick_jaeger::Span> for JaegerSpan {
	fn from(src: mick_jaeger::Span) -> Self {
		Self::Enabled(src)
	}
}

/// Shortcut for [`candidate_hash_span`] with the hash of the `Candidate` block.
#[inline(always)]
pub fn candidate_hash_span(candidate_hash: &CandidateHash, span_name: impl Into<String>) -> JaegerSpan {
	hash_span(&candidate_hash.0, span_name)
}

/// Shortcut for [`hash_span`] with the hash of the `PoV`.
#[inline(always)]
pub fn pov_span(pov: &PoV, span_name: impl Into<String>) -> JaegerSpan {
	hash_span(&pov.hash(), span_name)
}

/// Creates a `Span` referring to the given hash. All spans created with [`hash_span`] with the
/// same hash (even from multiple different nodes) will be visible in the same view on Jaeger.
#[inline(always)]
pub fn hash_span(hash: &Hash, span_name: impl Into<String>) -> JaegerSpan {
	INSTANCE.lock().span(hash, span_name).into()
}

/// Stateful convenience wrapper around [`mick_jaeger`].
pub enum Jaeger {
	/// Launched and operational state.
	Launched {
		/// [`mick_jaeger`] provided API to record spans to.
		traces_in: Arc<mick_jaeger::TracesIn>,
	},
	/// Preparation state with the necessary config to launch the collector.
	Prep(JaegerConfig),
	/// Uninitialized, suggests wrong API usage if encountered.
	None,
}

impl Jaeger {
	/// Spawn the jaeger instance.
	pub fn new(cfg: JaegerConfig) -> Self {
		Jaeger::Prep(cfg)
	}

	/// Spawn the background task in order to send the tracing information out via udp
	pub fn launch(self) -> result::Result<(), JaegerError> {
		let cfg = match self {
			Self::Prep(cfg) => Ok(cfg),
			Self::Launched{ .. } => {
				return Err(JaegerError::AlreadyLaunched)
			}
			Self::None => Err(JaegerError::MissingConfiguration),
		}?;

		let jaeger_agent = cfg.agent_addr;

		log::info!("🐹 Collecting jaeger spans for {:?}", &jaeger_agent);

		let (traces_in, mut traces_out) = mick_jaeger::init(mick_jaeger::Config {
			service_name: format!("{}-{}", cfg.node_name, cfg.node_name),
		});


		// Spawn a background task that pulls span information and sends them on the network.
		let _handle = async_std::task::spawn::<_, result::Result<(), JaegerError>>(async move {
			let mut port = 49000_u16;
			let mut udp_socket;

			loop {
				udp_socket = async_std::net::UdpSocket::bind(format!("127.0.0.1:{}", port)).await;
				if udp_socket.is_ok() {
					break;
				}
				port += 1;
				if port == std::primitive::u16::MAX {
					break;
				}
			}
			let udp_socket = udp_socket.map_err(|e| JaegerError::PortAllocationError(e))?;

			loop {
				let buf = traces_out.next().await;
				// UDP sending errors happen only either if the API is misused (in which case
				// panicking is desirable) or in case of missing privilege.
				if let Err(e) = udp_socket.send_to(&buf, jaeger_agent).await
					.map_err(|e| JaegerError::SendError(e))
				{
					log::trace!("Failed to send jaeger span: {:?}", e);
				}
			}
		});


		*INSTANCE.lock() =Self::Launched {
			traces_in,
		};
		Ok(())
	}

	#[inline(always)]
	fn traces_in(&self) -> Option<&Arc<mick_jaeger::TracesIn>> {
		match self {
			Self::Launched {
				traces_in,
				..
			} => Some(&traces_in),
			_ => None,
		}
	}

	fn span(&self, hash: &Hash, span_name: impl Into<String>) -> Option<mick_jaeger::Span> {
		if let Some(traces_in) = self.traces_in() {
			let trace_id = {
				let mut buf = [0u8; 16];
				buf.copy_from_slice(&hash.as_ref()[0..16]);
				std::num::NonZeroU128::new(u128::from_be_bytes(buf))
			}.expect("16 bytes make a u128; qed");
			Some(traces_in.span(trace_id, span_name))
		} else {
			None
		}
	}
}
