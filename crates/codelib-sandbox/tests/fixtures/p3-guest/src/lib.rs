//! The WASI 0.3 test guest for `codelib-sandbox` (see `wit/fixture.wit`).
//!
//! Everything WASI goes through the `wasip3` crate, so the clocks, random,
//! stdio, environment and HTTP the tests check are the 0.3 interfaces, not
//! the 0.2 ones the Rust standard library would use.

use std::future::{Future, IntoFuture, poll_fn};
use std::pin::pin;
use std::task::Poll;

use wasip3::http::types::{Fields, Method, Request, Response, Scheme};
use wasip3::sockets::types::{IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, TcpSocket};
use wasip3::{wit_future, wit_stream};
use wit_bindgen::StreamResult;

wit_bindgen::generate!({
    world: "fixture",
    path: "wit",
});

struct Fixture;

impl Guest for Fixture {
    fn add(a: u32, b: u32) -> u32 {
        a.wrapping_add(b)
    }

    async fn echo(s: String) -> String {
        s
    }

    async fn twice_later(ms: u32, x: u32) -> u32 {
        double_later(ms, x).await
    }

    async fn sleep(ms: u64) -> u64 {
        use wasip3::clocks::monotonic_clock;
        let before = monotonic_clock::now();
        monotonic_clock::wait_for(ms * 1_000_000).await;
        monotonic_clock::now().wrapping_sub(before)
    }

    async fn spin() {
        let mut n = 0u64;
        loop {
            n = std::hint::black_box(n.wrapping_add(1));
        }
    }

    async fn print(out: String, err: String) {
        write_all(wasip3::cli::stdout::write_via_stream, out.into_bytes()).await;
        write_all(wasip3::cli::stderr::write_via_stream, err.into_bytes()).await;
    }

    async fn observe() -> Observation {
        let wall = wasip3::clocks::system_clock::now();
        Observation {
            wall_seconds: wall.seconds,
            wall_nanoseconds: wall.nanoseconds,
            monotonic: wasip3::clocks::monotonic_clock::now(),
            random: wasip3::random::random::get_random_bytes(16),
            insecure: wasip3::random::insecure::get_insecure_random_bytes(16),
            insecure_seed: wasip3::random::insecure_seed::get_insecure_seed(),
            env: wasip3::cli::environment::get_environment(),
            args: wasip3::cli::environment::get_arguments(),
        }
    }

    async fn allocate(bytes: u64, fallible: bool) -> bool {
        let Ok(bytes) = usize::try_from(bytes) else {
            return false;
        };
        let mut block = Vec::<u8>::new();
        if fallible {
            if block.try_reserve_exact(bytes).is_err() {
                return false;
            }
        } else {
            block.reserve_exact(bytes);
        }
        // Touch every page, so the memory is really used.
        block.resize(bytes, 1);
        std::hint::black_box(&block);
        true
    }

    async fn probe() -> Sealed {
        let preopens = wasip3::filesystem::preopens::get_directories().len() as u32;
        let tcp_connect = match TcpSocket::create(IpAddressFamily::Ipv4) {
            Ok(socket) => {
                let local = IpSocketAddress::Ipv4(Ipv4SocketAddress {
                    port: 9,
                    address: (127, 0, 0, 1),
                });
                format!("{:?}", socket.connect(local).await)
            }
            Err(error) => format!("create: {error:?}"),
        };
        let name_lookup = format!(
            "{:?}",
            wasip3::sockets::ip_name_lookup::resolve_addresses("localhost".into()).await
        );
        Sealed {
            preopens,
            tcp_connect,
            name_lookup,
        }
    }

    async fn fetch(request: Outgoing) -> Result<Incoming, String> {
        let headers = Fields::new();
        for (name, value) in &request.headers {
            headers
                .append(name, value.as_bytes())
                .map_err(|e| format!("header {name}: {e:?}"))?;
        }
        let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));
        drop(trailers_tx);
        let (body_tx, contents) = if request.body.is_empty() {
            (None, None)
        } else {
            let (tx, rx) = wit_stream::new::<u8>();
            (Some(tx), Some(rx))
        };
        let (outgoing, _sent) = Request::new(headers, contents, trailers_rx, None);
        let method = match request.method.as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            other => Method::Other(other.to_string()),
        };
        let scheme = match request.scheme.as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => Scheme::Other(other.to_string()),
        };
        outgoing.set_method(&method).map_err(|()| "bad method")?;
        outgoing
            .set_scheme(Some(&scheme))
            .map_err(|()| "bad scheme")?;
        outgoing
            .set_authority(Some(&request.authority))
            .map_err(|()| "bad authority")?;
        outgoing
            .set_path_with_query(Some(&request.path))
            .map_err(|()| "bad path")?;

        let body = request.body;
        let write_body = async move {
            if let Some(mut tx) = body_tx {
                let _ = tx.write_all(body).await;
            }
        };
        let response = join(write_body, wasip3::http::client::send(outgoing))
            .await
            .map_err(|e| format!("{e:?}"))?;

        let status = response.get_status_code();
        let (result_tx, result_rx) = wit_future::new(|| Ok(()));
        let (mut stream, _trailers) = Response::consume_body(response, result_rx);
        let mut body = Vec::new();
        loop {
            let (result, chunk) = stream.read(Vec::with_capacity(16 * 1024)).await;
            match result {
                StreamResult::Complete(_) => body.extend_from_slice(&chunk),
                StreamResult::Dropped => break,
                StreamResult::Cancelled => {}
            }
        }
        drop(result_tx);
        Ok(Incoming { status, body })
    }
}

/// Write `bytes` through a `wasi:cli` 0.3 output stream and wait until the
/// host has taken them.
async fn write_all<F>(open: impl FnOnce(wit_bindgen::StreamReader<u8>) -> F, bytes: Vec<u8>)
where
    F: IntoFuture,
{
    let (mut tx, rx) = wit_stream::new::<u8>();
    let done = open(rx);
    let left = tx.write_all(bytes).await;
    debug_assert!(left.is_empty());
    drop(tx);
    let _ = done.await;
}

/// Poll both futures to completion and return the second one's output.
async fn join<A: Future, B: Future>(a: A, b: B) -> B::Output {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut a_done, mut out) = (false, None);
    poll_fn(|cx| {
        if !a_done && a.as_mut().poll(cx).is_ready() {
            a_done = true;
        }
        if out.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(cx)
        {
            out = Some(value);
        }
        if a_done && let Some(value) = out.take() {
            return Poll::Ready(value);
        }
        Poll::Pending
    })
    .await
}

export!(Fixture);
