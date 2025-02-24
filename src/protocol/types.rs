use super::utils as protocol_utils;
use crate::{
  error::{RedisError, RedisErrorKind},
  modules::inner::RedisClientInner,
  protocol::{cluster, utils::server_to_parts},
  types::*,
  utils,
};
use arcstr::ArcStr;
use bytes_utils::Str;
use rand::Rng;
pub use redis_protocol::{redis_keyslot, resp2::types::NULL, types::CRLF};
use redis_protocol::{resp2::types::Frame as Resp2Frame, resp2_frame_to_resp3, resp3::types::Frame as Resp3Frame};
use std::{
  cmp::Ordering,
  collections::{BTreeMap, BTreeSet, HashMap},
  convert::TryInto,
  fmt::{Display, Formatter},
  hash::{Hash, Hasher},
  net::{SocketAddr, ToSocketAddrs},
  sync::Arc,
};
use tokio::sync::mpsc::UnboundedSender;

pub const REDIS_CLUSTER_SLOTS: u16 = 16384;

#[cfg(any(feature = "enable-rustls", feature = "enable-native-tls"))]
use std::{net::IpAddr, str::FromStr};

#[derive(Debug)]
pub enum ProtocolFrame {
  Resp2(Resp2Frame),
  Resp3(Resp3Frame),
}

impl ProtocolFrame {
  pub fn into_resp3(self) -> Resp3Frame {
    // the `RedisValue::convert` logic already accounts for different encodings of maps and sets, so
    // we can just change everything to RESP3 above the protocol layer
    match self {
      ProtocolFrame::Resp2(frame) => resp2_frame_to_resp3(frame),
      ProtocolFrame::Resp3(frame) => frame,
    }
  }
}

impl From<Resp2Frame> for ProtocolFrame {
  fn from(frame: Resp2Frame) -> Self {
    ProtocolFrame::Resp2(frame)
  }
}

impl From<Resp3Frame> for ProtocolFrame {
  fn from(frame: Resp3Frame) -> Self {
    ProtocolFrame::Resp3(frame)
  }
}

/// State necessary to identify or connect to a server.
#[derive(Debug, Clone)]
pub struct Server {
  /// The hostname or IP address for the server.
  pub host:            ArcStr,
  /// The port for the server.
  pub port:            u16,
  /// The server name used during the TLS handshake.
  #[cfg_attr(docsrs, doc(cfg(any(feature = "enable-rustls", feature = "enable-native-tls"))))]
  pub tls_server_name: Option<ArcStr>,
}

impl Server {
  /// Create a new `Server` from parts with a TLS server name.
  #[cfg(any(feature = "enable-rustls", feature = "enable-native-tls"))]
  #[cfg_attr(docsrs, doc(cfg(any(feature = "enable-rustls", feature = "enable-native-tls"))))]
  pub fn new_with_tls<S: Into<ArcStr>>(host: S, port: u16, tls_server_name: Option<String>) -> Self {
    Server {
      host: host.into(),
      port,
      tls_server_name: tls_server_name.map(|s| s.into()),
    }
  }

  /// Create a new `Server` from parts.
  pub fn new<S: Into<ArcStr>>(host: S, port: u16) -> Self {
    Server {
      host: host.into(),
      port,
      tls_server_name: None,
    }
  }

  #[cfg(any(feature = "enable-rustls", feature = "enable-native-tls"))]
  pub(crate) fn set_tls_server_name(&mut self, policy: &TlsHostMapping, default_host: &str) {
    if *policy == TlsHostMapping::None {
      return;
    }

    let ip = match IpAddr::from_str(&self.host) {
      Ok(ip) => ip,
      Err(_) => return,
    };
    if let Some(tls_server_name) = policy.map(&ip, default_host) {
      self.tls_server_name = Some(ArcStr::from(tls_server_name));
    }
  }

  /// Attempt to parse a `host:port` string.
  pub(crate) fn from_str(s: &str) -> Option<Server> {
    let parts: Vec<&str> = s.trim().split(":").collect();
    if parts.len() == 2 {
      if let Some(port) = parts[1].parse::<u16>().ok() {
        Some(Server {
          host: parts[0].into(),
          port,
          tls_server_name: None,
        })
      } else {
        None
      }
    } else {
      None
    }
  }

  /// Create a new server struct from a `host:port` string and the default host that sent the last command.
  pub(crate) fn from_parts(server: &str, default_host: &str) -> Option<Server> {
    server_to_parts(server).ok().map(|(host, port)| {
      let host = if host.is_empty() {
        ArcStr::from(default_host)
      } else {
        ArcStr::from(host)
      };

      Server {
        host,
        port,
        tls_server_name: None,
      }
    })
  }
}

impl TryFrom<String> for Server {
  type Error = RedisError;

  fn try_from(value: String) -> Result<Self, Self::Error> {
    Server::from_str(&value).ok_or(RedisError::new(RedisErrorKind::Config, "Invalid `host:port` server."))
  }
}

impl TryFrom<&str> for Server {
  type Error = RedisError;

  fn try_from(value: &str) -> Result<Self, Self::Error> {
    Server::from_str(value).ok_or(RedisError::new(RedisErrorKind::Config, "Invalid `host:port` server."))
  }
}

impl From<(String, u16)> for Server {
  fn from((host, port): (String, u16)) -> Self {
    Server {
      host: host.into(),
      port,
      tls_server_name: None,
    }
  }
}

impl From<(&str, u16)> for Server {
  fn from((host, port): (&str, u16)) -> Self {
    Server {
      host: host.into(),
      port,
      tls_server_name: None,
    }
  }
}

impl From<&Server> for Server {
  fn from(value: &Server) -> Self {
    value.clone()
  }
}

impl Display for Server {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}:{}", self.host, self.port)
  }
}

impl PartialEq for Server {
  fn eq(&self, other: &Self) -> bool {
    self.host == other.host && self.port == other.port
  }
}

impl Eq for Server {}

impl Hash for Server {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.host.hash(state);
    self.port.hash(state);
  }
}

impl PartialOrd for Server {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Server {
  fn cmp(&self, other: &Self) -> Ordering {
    let host_ord = self.host.cmp(&other.host);
    if host_ord == Ordering::Equal {
      self.port.cmp(&other.port)
    } else {
      host_ord
    }
  }
}

/// The kind of pubsub message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageKind {
  /// A message from a `subscribe` command.
  Message,
  /// A message from a pattern `psubscribe` command.
  PMessage,
  /// A message from a sharded `ssubscribe` command.
  SMessage,
}

impl MessageKind {
  pub(crate) fn from_str(s: &str) -> Option<MessageKind> {
    Some(match s {
      "message" => MessageKind::Message,
      "pmessage" => MessageKind::PMessage,
      "smessage" => MessageKind::SMessage,
      _ => return None,
    })
  }
}

/// A [publish-subscribe](https://redis.io/docs/manual/pubsub/) message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
  /// The channel on which the message was sent.
  pub channel: Str,
  /// The message contents.
  pub value:   RedisValue,
  /// The type of message subscription.
  pub kind:    MessageKind,
}

pub struct KeyScanInner {
  /// The hash slot for the command.
  pub hash_slot:  Option<u16>,
  /// An optional server override.
  pub server:     Option<Server>,
  /// The index of the cursor in `args`.
  pub cursor_idx: usize,
  /// The arguments sent in each scan command.
  pub args:       Vec<RedisValue>,
  /// The sender half of the results channel.
  pub tx:         UnboundedSender<Result<ScanResult, RedisError>>,
}

impl KeyScanInner {
  /// Update the cursor in place in the arguments.
  pub fn update_cursor(&mut self, cursor: Str) {
    self.args[self.cursor_idx] = cursor.into();
  }

  /// Send an error on the response stream.
  pub fn send_error(&self, error: RedisError) {
    let _ = self.tx.send(Err(error));
  }
}

pub enum ValueScanResult {
  SScan(SScanResult),
  HScan(HScanResult),
  ZScan(ZScanResult),
}

pub struct ValueScanInner {
  /// The index of the cursor argument in `args`.
  pub cursor_idx: usize,
  /// The arguments sent in each scan command.
  pub args:       Vec<RedisValue>,
  /// The sender half of the results channel.
  pub tx:         UnboundedSender<Result<ValueScanResult, RedisError>>,
}

impl ValueScanInner {
  /// Update the cursor in place in the arguments.
  pub fn update_cursor(&mut self, cursor: Str) {
    self.args[self.cursor_idx] = cursor.into();
  }

  /// Send an error on the response stream.
  pub fn send_error(&self, error: RedisError) {
    let _ = self.tx.send(Err(error));
  }

  pub fn transform_hscan_result(mut data: Vec<RedisValue>) -> Result<RedisMap, RedisError> {
    if data.is_empty() {
      return Ok(RedisMap::new());
    }
    if data.len() % 2 != 0 {
      return Err(RedisError::new(
        RedisErrorKind::Protocol,
        "Invalid HSCAN result. Expected array with an even number of elements.",
      ));
    }

    let mut out = HashMap::with_capacity(data.len() / 2);
    while data.len() >= 2 {
      let value = data.pop().unwrap();
      let key: RedisKey = match data.pop().unwrap() {
        RedisValue::String(s) => s.into(),
        RedisValue::Bytes(b) => b.into(),
        _ => {
          return Err(RedisError::new(
            RedisErrorKind::Protocol,
            "Invalid HSCAN result. Expected string.",
          ))
        },
      };

      out.insert(key, value);
    }

    Ok(out.try_into()?)
  }

  pub fn transform_zscan_result(mut data: Vec<RedisValue>) -> Result<Vec<(RedisValue, f64)>, RedisError> {
    if data.is_empty() {
      return Ok(Vec::new());
    }
    if data.len() % 2 != 0 {
      return Err(RedisError::new(
        RedisErrorKind::Protocol,
        "Invalid ZSCAN result. Expected array with an even number of elements.",
      ));
    }

    let mut out = Vec::with_capacity(data.len() / 2);

    for chunk in data.chunks_exact_mut(2) {
      let value = chunk[0].take();
      let score = match chunk[1].take() {
        RedisValue::String(s) => utils::redis_string_to_f64(&s)?,
        RedisValue::Integer(i) => i as f64,
        RedisValue::Double(f) => f,
        _ => {
          return Err(RedisError::new(
            RedisErrorKind::Protocol,
            "Invalid HSCAN result. Expected a string or number score.",
          ))
        },
      };

      out.push((value, score));
    }

    Ok(out)
  }
}

/// A slot range and associated cluster node information from the `CLUSTER SLOTS` command.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SlotRange {
  /// The start of the hash slot range.
  pub start:    u16,
  /// The end of the hash slot range.
  pub end:      u16,
  /// The primary server owner.
  pub primary:  Server,
  /// The internal ID assigned by the server.
  pub id:       ArcStr,
  /// Replica node owners.
  #[cfg(feature = "replicas")]
  #[cfg_attr(docsrs, doc(cfg(feature = "replicas")))]
  pub replicas: Vec<Server>,
}

/// The cached view of the cluster used by the client to route commands to the correct cluster nodes.
#[derive(Debug, Clone)]
pub struct ClusterRouting {
  data: Vec<SlotRange>,
}

impl ClusterRouting {
  /// Create a new empty routing table.
  pub fn new() -> Self {
    ClusterRouting { data: Vec::new() }
  }

  /// Read a set of unique hash slots that each map to a different primary/main node in the cluster.
  pub fn unique_hash_slots(&self) -> Vec<u16> {
    let mut out = BTreeMap::new();

    for slot in self.data.iter() {
      out.insert(&slot.primary, slot.start);
    }

    out.into_iter().map(|(_, v)| v).collect()
  }

  /// Read the set of unique primary nodes in the cluster.
  pub fn unique_primary_nodes(&self) -> Vec<Server> {
    let mut out = BTreeSet::new();

    for slot in self.data.iter() {
      out.insert(slot.primary.clone());
    }

    out.into_iter().collect()
  }

  /// Rebuild the cache in place with the output of a `CLUSTER SLOTS` command.
  pub(crate) fn rebuild(
    &mut self,
    inner: &Arc<RedisClientInner>,
    cluster_slots: RedisValue,
    default_host: &str,
  ) -> Result<(), RedisError> {
    self.data = cluster::parse_cluster_slots(cluster_slots, default_host)?;
    self.data.sort_by(|a, b| a.start.cmp(&b.start));

    cluster::modify_cluster_slot_hostnames(inner, &mut self.data, default_host);
    Ok(())
  }

  /// Calculate the cluster hash slot for the provided key.
  pub fn hash_key(key: &[u8]) -> u16 {
    redis_protocol::redis_keyslot(key)
  }

  /// Find the primary server that owns the provided hash slot.
  pub fn get_server(&self, slot: u16) -> Option<&Server> {
    if self.data.is_empty() {
      return None;
    }

    protocol_utils::binary_search(&self.data, slot).map(|idx| &self.data[idx].primary)
  }

  /// Read the replicas associated with the provided primary node based on the cached CLUSTER SLOTS response.
  #[cfg(feature = "replicas")]
  #[cfg_attr(docsrs, doc(cfg(feature = "replicas")))]
  pub fn replicas(&self, primary: &Server) -> Vec<Server> {
    self
      .data
      .iter()
      .fold(BTreeSet::new(), |mut replicas, slot| {
        if slot.primary == *primary {
          replicas.extend(slot.replicas.clone());
        }

        replicas
      })
      .into_iter()
      .collect()
  }

  /// Read the number of hash slot ranges in the cluster.
  pub fn len(&self) -> usize {
    self.data.len()
  }

  /// Read the hash slot ranges in the cluster.
  pub fn slots(&self) -> &[SlotRange] {
    &self.data
  }

  /// Read a random primary node hash slot range from the cluster cache.
  pub fn random_slot(&self) -> Option<&SlotRange> {
    if self.data.len() > 0 {
      let idx = rand::thread_rng().gen_range(0 .. self.data.len());
      Some(&self.data[idx])
    } else {
      None
    }
  }

  /// Read a random primary node from the cluster cache.
  pub fn random_node(&self) -> Option<&Server> {
    self.random_slot().map(|slot| &slot.primary)
  }
}

/// A trait that can be used to override DNS resolution logic.
///
/// Note: currently this requires [async-trait](https://crates.io/crates/async-trait).
#[async_trait]
#[cfg_attr(docsrs, doc(cfg(feature = "dns")))]
pub trait Resolve: Send + Sync + 'static {
  /// Resolve a hostname.
  async fn resolve(&self, host: String, port: u16) -> Result<SocketAddr, RedisError>;
}

/// Default DNS resolver that uses `to_socket_addrs` under the hood.
#[derive(Clone, Debug)]
pub struct DefaultResolver {
  id: ArcStr,
}

impl DefaultResolver {
  /// Create a new resolver using the system's default DNS resolution.
  pub fn new(id: &ArcStr) -> Self {
    DefaultResolver { id: id.clone() }
  }
}

#[async_trait]
impl Resolve for DefaultResolver {
  async fn resolve(&self, host: String, port: u16) -> Result<SocketAddr, RedisError> {
    let client_id = self.id.clone();

    tokio::task::spawn_blocking(move || {
      let ips: Vec<SocketAddr> = format!("{}:{}", host, port).to_socket_addrs()?.into_iter().collect();

      if ips.is_empty() {
        Err(RedisError::new(
          RedisErrorKind::IO,
          format!("Failed to resolve {}:{}", host, port),
        ))
      } else {
        let possible_addrs = ips.len();
        let addr = ips[0];

        trace!(
          "{}: Using {} among {} possible socket addresses for {}:{}",
          client_id,
          addr.ip(),
          possible_addrs,
          host,
          port
        );
        Ok(addr)
      }
    })
    .await?
  }
}
