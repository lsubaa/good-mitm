use super::{HttpContext, MaybeProxyClient, MessageContext, RequestOrResponse, Rewind};
use crate::{
    ca::CertificateAuthority,
    handler::{HttpHandler, MessageHandler, MitmFilter},
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use http::{header, uri::PathAndQuery, HeaderValue};
use hyper::{
    server::conn::Http, service::service_fn, upgrade::Upgraded, Body, Method, Request, Response,
    Uri,
};
use log::*;
use std::{net::SocketAddr, sync::Arc};
use tokio::{io::AsyncReadExt, net::TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{tungstenite, tungstenite::Message};

#[derive(Clone)]
pub(crate) struct Proxy {
    pub ca: Arc<CertificateAuthority>,
    pub client: MaybeProxyClient,
    pub client_addr: SocketAddr,
}

impl Proxy {
    pub(crate) async fn proxy(self, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        match if req.method() == Method::CONNECT {
            self.process_connect(req).await
        } else {
            self.process_request(req).await
        } {
            Ok(resp) => Ok(allow_all_cros(resp)),
            Err(e) => Err(e),
        }
    }

    async fn process_request(self, mut req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        let mut ctx = HttpContext {
            client_addr: self.client_addr,
            uri: None,
            should_modify_response: false,
            rule: vec![],
        };

        if req.uri().path().starts_with("/mitm/cert")
            || req
                .headers()
                .get(http::header::HOST)
                .unwrap()
                .to_str()
                .unwrap_or_default()
                .contains("cert.mitm")
        {
            return Ok(Response::builder()
                .header(
                    http::header::CONTENT_DISPOSITION,
                    "attachment; filename=good-mitm.crt",
                )
                .header(http::header::CONTENT_TYPE, "application/octet-stream")
                .status(http::StatusCode::OK)
                .body(Body::from(self.ca.clone().get_cert()))
                .unwrap());
        }

        req.headers_mut().remove(http::header::HOST);
        req.headers_mut().remove(http::header::ACCEPT_ENCODING);

        let req = match HttpHandler::handle_request(&mut ctx, req).await {
            RequestOrResponse::Request(req) => req,
            RequestOrResponse::Response(res) => return Ok(res),
        };

        let mut res = match self.client {
            MaybeProxyClient::Proxy(client) => client.request(req).await?,
            MaybeProxyClient::Https(client) => client.request(req).await?,
        };

        // Remove `Strict-Transport-Security` to avoid HSTS
        // See: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Strict-Transport-Security
        res.headers_mut().remove(header::STRICT_TRANSPORT_SECURITY);

        Ok(HttpHandler::handle_response(&mut ctx, res).await)
    }

    async fn process_connect(self, req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
        let ctx = HttpContext {
            client_addr: self.client_addr,
            uri: None,
            should_modify_response: false,
            rule: vec![],
        };
        if MitmFilter::filter(&ctx, &req).await {
            tokio::task::spawn(async move {
                let authority = req
                    .uri()
                    .authority()
                    .expect("URI does not contain authority")
                    .clone();

                match hyper::upgrade::on(req).await {
                    Ok(mut upgraded) => {
                        let mut buffer = [0; 4];
                        let bytes_read = upgraded
                            .read(&mut buffer)
                            .await
                            .expect("Failed to read from upgraded connection");

                        let upgraded = Rewind::new_buffered(
                            upgraded,
                            bytes::Bytes::copy_from_slice(buffer[..bytes_read].as_ref()),
                        );

                        let server_config = self.ca.gen_server_config(&authority).await;
                        let stream = TlsAcceptor::from(server_config)
                            .accept(upgraded)
                            .await
                            .expect("Failed to establish TLS connection with client");

                        if let Err(e) = self.serve_https(stream).await {
                            let e_string = e.to_string();
                            if !e_string.starts_with("error shutting down connection") {
                                debug!("res:: {}", e);
                            }
                        }
                    }
                    Err(e) => debug!("upgrade error for {}: {}", authority, e),
                };
            });
        } else {
            tokio::task::spawn(async move {
                let remote_addr = host_addr(req.uri()).unwrap();
                let upgraded = hyper::upgrade::on(req).await.unwrap();
                tunnel(upgraded, remote_addr).await
            });
        }
        Ok(Response::new(Body::empty()))
    }

    async fn serve_https(
        self,
        stream: tokio_rustls::server::TlsStream<Rewind<Upgraded>>,
    ) -> Result<(), hyper::Error> {
        let service = service_fn(|mut req| {
            if req.version() == http::Version::HTTP_10 || req.version() == http::Version::HTTP_11 {
                let authority = req
                    .headers()
                    .get(http::header::HOST)
                    .expect("Host is a required header")
                    .to_str()
                    .expect("Failed to convert host to str");

                let uri = http::uri::Builder::new()
                    .scheme(http::uri::Scheme::HTTPS)
                    .authority(authority)
                    .path_and_query(
                        req.uri()
                            .path_and_query()
                            .unwrap_or(&PathAndQuery::from_static("/"))
                            .to_owned(),
                    )
                    .build()
                    .expect("Failed to build URI");

                let (mut parts, body) = req.into_parts();
                parts.uri = uri;
                req = Request::from_parts(parts, body)
            };

            self.clone().process_request(req)
        });

        Http::new()
            .serve_connection(stream, service)
            .with_upgrades()
            .await
    }
}

fn spawn_message_forwarder(
    mut stream: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
    mut sink: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    client_addr: SocketAddr,
    uri: Uri,
) {
    let ctx = MessageContext {
        client_addr,
        server_uri: uri,
    };

    tokio::spawn(async move {
        while let Some(message) = stream.next().await {
            match message {
                Ok(message) => {
                    let message = match MessageHandler::handle_message(&ctx, message).await {
                        Some(message) => message,
                        None => continue,
                    };

                    match sink.send(message).await {
                        Err(tungstenite::Error::ConnectionClosed) => (),
                        Err(e) => error!("websocket send error: {}", e),
                        _ => (),
                    }
                }
                Err(e) => error!("websocket message error: {}", e),
            }
        }
    });
}

fn allow_all_cros(resp: Response<Body>) -> Response<Body> {
    let mut resp = resp;
    let header = resp.headers_mut();
    let all = HeaderValue::from_str("*").unwrap();
    header.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, all.clone());
    header.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, all.clone());
    header.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, all);
    resp
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

async fn tunnel(mut upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    let mut server = TcpStream::connect(addr).await?;
    tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;
    Ok(())
}
