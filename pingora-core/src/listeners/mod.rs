// Copyright 2024 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The listening endpoints (TCP and TLS) and their configurations.

pub mod inpod;
mod l4;
mod tls;

use crate::protocols::Stream;
use crate::server::ListenFds;

use futures::future::ok;
// use pingora_error::Result;
use pingora_error::{
    ErrorType::{AcceptError, BindError},
    OrErr, Result,
};
use std::{fs::Permissions, sync::Arc};

use inpod::netns::InpodNetns;
use l4::{ListenerEndpoint, Stream as L4Stream};
use tls::Acceptor;

pub use crate::protocols::tls::server::TlsAccept;
pub use l4::{ServerAddress, TcpSocketOptions};
pub use tls::{TlsSettings, ALPN};

struct TransportStackBuilder {
    l4: ServerAddress,
    tls: Option<TlsSettings>,
    netns: Option<InpodNetns>,
}

impl TransportStackBuilder {
    pub fn build(&mut self, upgrade_listeners: Option<ListenFds>) -> TransportStack {
        TransportStack {
            l4: ListenerEndpoint::new(self.l4.clone()),
            tls: self.tls.take().map(|tls| Arc::new(tls.build())),
            upgrade_listeners,
            netns: self.netns.clone(),
        }
    }
}

pub struct TransportStack {
    l4: ListenerEndpoint,
    tls: Option<Arc<Acceptor>>,
    // listeners sent from the old process for graceful upgrade
    upgrade_listeners: Option<ListenFds>,
    netns: Option<InpodNetns>,
}

impl TransportStack {
    pub fn as_str(&self) -> &str {
        self.l4.as_str()
    }

    pub async fn listen(&mut self) -> Result<()> {
        if self.netns.is_none() {
            self.l4.listen(self.upgrade_listeners.take()).await
        } else {
            self.netns.as_ref().and_then(|netns| {
                Some(netns.run(|| async { self.l4.listen(self.upgrade_listeners.take()).await }))
            }).unwrap()
            // let netns = self.netns.as_ref().unwrap();
            // let ret = netns.run(|| async { self.l4.listen(self.upgrade_listeners.take()).await });
            // match ret {
            //     Ok(_ret) => {
            //         return Ok(());
            //     }
            //     Err(e) => Err(e).or_err(BindError, "bind "),
            // }
        }

        // self.l4.listen(self.upgrade_listeners.take()).await
    }

    pub async fn accept(&mut self) -> Result<UninitializedStream> {
        let stream = self.l4.accept().await?;
        Ok(UninitializedStream {
            l4: stream,
            tls: self.tls.clone(),
        })
    }

    pub fn cleanup(&mut self) {
        // placeholder
    }
}

pub struct UninitializedStream {
    l4: L4Stream,
    tls: Option<Arc<Acceptor>>,
}

impl UninitializedStream {
    pub async fn handshake(self) -> Result<Stream> {
        if let Some(tls) = self.tls {
            let tls_stream = tls.tls_handshake(self.l4).await?;
            Ok(Box::new(tls_stream))
        } else {
            Ok(Box::new(self.l4))
        }
    }
}

/// The struct to hold one more multiple listening endpoints
pub struct Listeners {
    stacks: Vec<TransportStackBuilder>,
}

impl Listeners {
    /// Create a new [`Listeners`] with no listening endpoints.
    pub fn new() -> Self {
        Listeners { stacks: vec![] }
    }

    /// Create a new [`Listeners`] with a TCP server endpoint from the given string.
    pub fn tcp(addr: &str) -> Self {
        let mut listeners = Self::new();
        listeners.add_tcp(addr);
        listeners
    }

    /// Create a new [`Listeners`] with a Unix domain socket endpoint from the given string.
    pub fn uds(addr: &str, perm: Option<Permissions>) -> Self {
        let mut listeners = Self::new();
        listeners.add_uds(addr, perm);
        listeners
    }

    /// Create a new [`Listeners`] with a TLS (TCP) endpoint with the given address string,
    /// and path to the certificate/private key pairs.
    /// This endpoint will adopt the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings.
    pub fn tls(addr: &str, cert_path: &str, key_path: &str) -> Result<Self> {
        let mut listeners = Self::new();
        listeners.add_tls(addr, cert_path, key_path)?;
        Ok(listeners)
    }

    /// Add a TCP endpoint to `self`.
    pub fn add_tcp(&mut self, addr: &str) {
        self.add_address(ServerAddress::Tcp(addr.into(), None));
    }

    /// Add a TCP endpoint to `self`, with the given [`TcpSocketOptions`].
    pub fn add_tcp_with_settings(&mut self, addr: &str, sock_opt: TcpSocketOptions) {
        self.add_address(ServerAddress::Tcp(addr.into(), Some(sock_opt)));
    }

    /// Add a TCP endpoint to `self`, with the given [`TcpSocketOptions`] and network namespace.
    pub fn add_ns_tcp_with_settings(
        &mut self,
        netns: InpodNetns,
        addr: &str,
        sock_opt: TcpSocketOptions,
    ) {
        self.add_addres_with_ns(netns, ServerAddress::Tcp(addr.into(), Some(sock_opt)));
    }

    /// Add a Unix domain socket endpoint to `self`.
    pub fn add_uds(&mut self, addr: &str, perm: Option<Permissions>) {
        self.add_address(ServerAddress::Uds(addr.into(), perm));
    }

    /// Add a TLS endpoint to `self` with the [Mozilla Intermediate](https://wiki.mozilla.org/Security/Server_Side_TLS#Intermediate_compatibility_.28recommended.29)
    /// server side TLS settings.
    pub fn add_tls(&mut self, addr: &str, cert_path: &str, key_path: &str) -> Result<()> {
        self.add_tls_with_settings(addr, None, TlsSettings::intermediate(cert_path, key_path)?);
        Ok(())
    }

    /// Add a TLS endpoint to `self` with the given socket and server side TLS settings.
    /// See [`TlsSettings`] and [`TcpSocketOptions`] for more details.
    pub fn add_tls_with_settings(
        &mut self,
        addr: &str,
        sock_opt: Option<TcpSocketOptions>,
        settings: TlsSettings,
    ) {
        self.add_endpoint(ServerAddress::Tcp(addr.into(), sock_opt), Some(settings));
    }

    /// Add the given [`ServerAddress`] to `self`.
    pub fn add_address(&mut self, addr: ServerAddress) {
        self.add_endpoint(addr, None);
    }

    pub fn add_addres_with_ns(&mut self, netns: InpodNetns, addr: ServerAddress) {
        self.add_endpoint_with_ns(addr, None, Some(netns));
    }

    pub fn add_endpoint_with_ns(
        &mut self,
        l4: ServerAddress,
        tls: Option<TlsSettings>,
        netns: Option<InpodNetns>,
    ) {
        self.stacks.push(TransportStackBuilder { l4, tls, netns })
    }

    /// Add the given [`ServerAddress`] to `self` with the given [`TlsSettings`] if provided
    pub fn add_endpoint(&mut self, l4: ServerAddress, tls: Option<TlsSettings>) {
        let netns = None;
        self.stacks.push(TransportStackBuilder { l4, tls, netns })
    }

    pub fn build(&mut self, upgrade_listeners: Option<ListenFds>) -> Vec<TransportStack> {
        self.stacks
            .iter_mut()
            .map(|b| b.build(upgrade_listeners.clone()))
            .collect()
    }

    pub(crate) fn cleanup(&self) {
        // placeholder
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_listen_tcp() {
        let addr1 = "127.0.0.1:7101";
        let addr2 = "127.0.0.1:7102";
        let mut listeners = Listeners::tcp(addr1);
        listeners.add_tcp(addr2);

        let listeners = listeners.build(None);
        assert_eq!(listeners.len(), 2);
        for mut listener in listeners {
            tokio::spawn(async move {
                listener.listen().await.unwrap();
                // just try to accept once
                let stream = listener.accept().await.unwrap();
                stream.handshake().await.unwrap();
            });
        }

        // make sure the above starts before the lines below
        sleep(Duration::from_millis(10)).await;

        TcpStream::connect(addr1).await.unwrap();
        TcpStream::connect(addr2).await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "some_tls")]
    async fn test_listen_tls() {
        use tokio::io::AsyncReadExt;

        let addr = "127.0.0.1:7103";
        let cert_path = format!("{}/tests/keys/server.crt", env!("CARGO_MANIFEST_DIR"));
        let key_path = format!("{}/tests/keys/key.pem", env!("CARGO_MANIFEST_DIR"));
        let mut listeners = Listeners::tls(addr, &cert_path, &key_path).unwrap();
        let mut listener = listeners.build(None).pop().unwrap();

        tokio::spawn(async move {
            listener.listen().await.unwrap();
            // just try to accept once
            let stream = listener.accept().await.unwrap();
            let mut stream = stream.handshake().await.unwrap();
            let mut buf = [0; 1024];
            let _ = stream.read(&mut buf).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\na")
                .await
                .unwrap();
        });
        // make sure the above starts before the lines below
        sleep(Duration::from_millis(10)).await;

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let res = client.get(format!("https://{addr}")).send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
    }
}
