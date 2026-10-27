#![cfg(feature = "tls")]
//! Real TLS + HTTP interoperability against an independent local Node HTTPS server.
use base64::{Engine, engine::general_purpose::STANDARD};
use core::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use embassy_crypto_rand as _;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embedded_io_adapters::tokio_1::FromTokio;
use han_config::diagnostics::Log;
use han_logs::{
    BATCH_BYTES, Endpoint, HEADER_BYTES, Identity, encode_batch, http,
    io_bridge::{Bridge, CHUNK_BYTES},
    tls::{self, TlsClock},
};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, TcpStream},
    time::timeout,
};

static NOW: AtomicI64 = AtomicI64::new(-1);
struct Clock;
impl TlsClock for Clock {
    fn now() -> Option<u64> {
        NOW.load(Ordering::Relaxed).try_into().ok()
    }
}

static CLOCK_CALLS: AtomicUsize = AtomicUsize::new(0);
static AVAILABLE_CALLS: AtomicUsize = AtomicUsize::new(0);
struct DisappearingClock;
impl TlsClock for DisappearingClock {
    fn now() -> Option<u64> {
        (CLOCK_CALLS.fetch_add(1, Ordering::Relaxed) < AVAILABLE_CALLS.load(Ordering::Relaxed))
            .then(Clock::now)
            .flatten()
    }
}

fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "openssl: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Server {
    process: Child,
    port: u16,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
impl Server {
    fn new(dir: &Path, leaf: &str, version: &str) -> Self {
        Self::with_chain(dir, leaf, version, true)
    }

    fn with_chain(dir: &Path, leaf: &str, version: &str, chain: bool) -> Self {
        let mut process = Command::new("node")
            .args([
                "--input-type=commonjs",
                "-e",
                include_str!("server.cjs"),
                leaf,
                version,
                if chain { "full" } else { "omit" },
            ])
            .current_dir(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(process.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line.trim().parse().expect("HTTPS test server port");
        Self { process, port }
    }
}

fn ca(pem: &str) -> Vec<u8> {
    let text: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let mut der = vec![0; text.len()];
    let len = STANDARD.decode_slice(text.as_bytes(), &mut der).unwrap();
    der.truncate(len);
    der
}

async fn upload<C: TlsClock>(
    tls: &mut tls::Client<'_, C>,
    server: &Server,
    host: &str,
    path: &str,
) -> Result<(), tls::Error> {
    // Boot-only log: no HAN telegram or telemetry update precedes delivery.
    let log = Log::<NoopRawMutex>::new();
    log.record(
        0,
        format_args!("Boot; debug log is in RAM and resets on restart"),
    );
    let identity = Identity {
        device: "han-test",
        boot_id: 7,
        firmware: "test",
    };
    let mut body = [0; BATCH_BYTES];
    let len = encode_batch(
        &log.page(0),
        0,
        &identity,
        Some(1_800_000_000_000),
        &mut body,
    )
    .unwrap();
    let url = format!("https://{host}:{}/{path}", server.port);
    let endpoint = Endpoint::parse(&url).unwrap();
    let socket = TcpStream::connect(("127.0.0.1", server.port))
        .await
        .unwrap();
    let mut scratch = [0; HEADER_BYTES];
    let mut bridge = Bridge::<CriticalSectionRawMutex, CHUNK_BYTES>::new();
    let (mut stream, mut socket_owner) = bridge.split();
    let mut socket = FromTokio::new(socket);
    let future = tls.post(
        &mut stream,
        &endpoint,
        "Basic dXNlcjpwYXNz",
        &body[..len],
        &mut scratch,
    );
    assert!(
        core::mem::size_of_val(&future) < 4096,
        "TLS future must not embed record/header buffers"
    );
    timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = future => result,
            _ = socket_owner.serve(&mut socket) => unreachable!(),
        }
    })
    .await
    .expect("TLS request timed out")
}

#[tokio::test]
async fn verified_https_auth_statuses_cancellation_and_certificate_failures() {
    // One test owns the certificate clock for its entire lifetime.
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    openssl(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            // Exercise the RSA-4096 verification used by ISRG Root X1.
            "rsa:4096",
            "-nodes",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
            "-days",
            "10",
            "-subj",
            "/CN=HAN test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ],
    );
    openssl(
        dir,
        &[
            "req",
            "-new",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:secp384r1",
            "-nodes",
            "-keyout",
            "intermediate.key",
            "-out",
            "intermediate.csr",
            "-subj",
            "/CN=HAN intermediate",
        ],
    );
    fs::write(
        dir.join("intermediate.ext"),
        "basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\n",
    )
    .unwrap();
    openssl(
        dir,
        &[
            "x509",
            "-req",
            "-in",
            "intermediate.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-out",
            "intermediate.pem",
            "-days",
            "5",
            "-extfile",
            "intermediate.ext",
        ],
    );
    fs::write(dir.join("leaf.ext"), "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:logs.test\n").unwrap();
    for (leaf, key_args) in [
        ("ec", vec!["ec", "-pkeyopt", "ec_paramgen_curve:prime256v1"]),
        ("rsa", vec!["rsa:2048"]),
    ] {
        let key = format!("{leaf}.key");
        let csr = format!("{leaf}.csr");
        let cert = format!("{leaf}.pem");
        let mut args = vec!["req", "-new", "-newkey"];
        args.extend(key_args);
        args.extend([
            "-nodes",
            "-keyout",
            &key,
            "-out",
            &csr,
            "-subj",
            "/CN=logs.test",
        ]);
        openssl(dir, &args);
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &csr,
                "-CA",
                "intermediate.pem",
                "-CAkey",
                "intermediate.key",
                "-CAcreateserial",
                "-out",
                &cert,
                // Match Let's Encrypt's P-384 / SHA-384 intermediate signatures.
                "-sha384",
                "-days",
                "2",
                "-extfile",
                "leaf.ext",
            ],
        );
    }
    fs::write(dir.join("requests.jsonl"), "").unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    NOW.store(now, Ordering::Relaxed);
    let trusted = ca(&fs::read_to_string(dir.join("ca.pem")).unwrap());
    let isrg_x1 = ca(include_str!("../../../firmware/certs/isrg-root-x1.pem"));
    let isrg_x2 = ca(include_str!("../../../firmware/certs/isrg-root-x2.pem"));
    let roots = [isrg_x1.as_slice(), trusted.as_slice()];
    // Exercise a successful root after an unrelated root, using one certificate buffer.
    let mut tls = tls::Client::<Clock>::new(&roots).unwrap();
    let production_roots = [isrg_x1.as_slice(), isrg_x2.as_slice()];
    let mut production_tls = tls::Client::<Clock>::new(&production_roots).unwrap();
    for leaf in ["ec", "rsa"] {
        let server = Server::new(dir, leaf, "TLSv1.3");
        upload(&mut tls, &server, "logs.test", "insert/jsonline?tenant=1")
            .await
            .unwrap();
    }
    let tls12_only = Server::new(dir, "ec", "TLSv1.2");
    assert!(matches!(
        upload(&mut tls, &tls12_only, "logs.test", "insert/jsonline").await,
        Err(tls::Error::Tls(_))
    ));
    let server = Server::new(dir, "ec", "TLSv1.3");
    let count = || {
        fs::read_to_string(dir.join("requests.jsonl"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(count(), 2);
    let incomplete = Server::with_chain(dir, "ec", "TLSv1.3", false);
    assert!(matches!(
        upload(&mut tls, &incomplete, "logs.test", "insert/jsonline").await,
        Err(tls::Error::Tls(_))
    ));
    assert!(matches!(
        upload(&mut production_tls, &server, "logs.test", "insert/jsonline").await,
        Err(tls::Error::Tls(_))
    ));
    assert!(matches!(
        upload(&mut tls, &server, "wrong.test", "insert/jsonline").await,
        Err(tls::Error::Tls(_))
    ));
    for invalid_time in [-1, now - 365 * 86400, now + 365 * 86400] {
        NOW.store(invalid_time, Ordering::Relaxed);
        assert!(matches!(
            upload(&mut tls, &server, "logs.test", "insert/jsonline").await,
            Err(tls::Error::Tls(_) | tls::Error::Clock)
        ));
    }
    assert_eq!(
        count(),
        2,
        "unverified peers must receive no HTTP request/credentials"
    );
    NOW.store(now, Ordering::Relaxed);
    let single_root = [trusted.as_slice()];
    for available in [1, 2] {
        // Lose the clock either before validation or inside upstream's clock
        // lookup. Neither may activate its optional date-check bypass.
        CLOCK_CALLS.store(0, Ordering::Relaxed);
        AVAILABLE_CALLS.store(available, Ordering::Relaxed);
        let mut losing_time = tls::Client::<DisappearingClock>::new(&single_root).unwrap();
        assert!(matches!(
            upload(&mut losing_time, &server, "logs.test", "insert/jsonline").await,
            Err(tls::Error::Tls(_))
        ));
    }
    assert_eq!(count(), 2, "lost time must never bypass certificate dates");
    let mut first_root = tls::Client::<Clock>::new(&single_root).unwrap();
    upload(&mut first_root, &server, "logs.test", "insert/jsonline")
        .await
        .unwrap();
    for (path, status) in [
        ("reject", 401),
        ("busy", 429),
        ("redirect", 302),
        ("unavailable", 503),
    ] {
        assert!(
            matches!(upload(&mut tls, &server, "logs.test", path).await, Err(tls::Error::Http(http::Error::Status(value))) if value == status)
        );
    }
    assert!(
        upload(&mut tls, &server, "logs.test", "truncate")
            .await
            .is_err()
    );
    // Cancel while waiting for a response, then successfully create another session.
    assert!(
        timeout(
            Duration::from_millis(200),
            upload(&mut tls, &server, "logs.test", "stall")
        )
        .await
        .is_err()
    );
    // Cancel during the TLS handshake too. No session may be reused after cancellation.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (_peer, _) = listener.accept().await.unwrap();
    let endpoint = Endpoint::parse("https://logs.test/insert/jsonline").unwrap();
    assert!(
        timeout(
            Duration::from_millis(100),
            tls.post(
                FromTokio::new(socket),
                &endpoint,
                "Basic dXNlcjpwYXNz",
                b"{}\n",
                &mut [0; HEADER_BYTES]
            )
        )
        .await
        .is_err()
    );
    upload(&mut tls, &server, "logs.test", "insert/jsonline")
        .await
        .unwrap();
    for line in fs::read_to_string(dir.join("requests.jsonl"))
        .unwrap()
        .lines()
    {
        let request: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(request["authorization"], "Basic dXNlcjpwYXNz");
        assert_eq!(request["contentType"], "application/stream+json");
        assert_eq!(request["stream"], "device");
        let body = request["body"].as_str().unwrap();
        assert!(body.ends_with('\n'));
        assert_eq!(body.lines().count(), 1);
        let startup: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            startup["_msg"],
            "Boot; debug log is in RAM and resets on restart"
        );
        assert_eq!(startup["device"], "han-test");
        assert_eq!(startup["sequence"], 1);
        assert_eq!(startup["_time"], 1_800_000_000_000u64);
    }
}
