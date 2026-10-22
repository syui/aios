// https: rustls (暗号は ring)。ルート証明書は /etc/ssl/certs/ca-certificates.crt (パッケージの ca-certificates)。
// Linux のディストリビューションと同じく、システムの一覧がすべて。組みこみの webpki-roots は、そのファイルがない
// (入れる前や、ほかの OS) か、1 つも読めないときだけ
use crate::http::{self, ReadWrite, Url};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io;
use std::sync::Arc;

const CA_FILE: &str = "/etc/ssl/certs/ca-certificates.crt";

fn config() -> io::Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    if let Ok(certs) = CertificateDer::pem_file_iter(CA_FILE) {
        for c in certs.flatten() {
            let _ = roots.add(c);
        }
    }
    // いっしょに入れると、ca-certificates が外した認証局を組みこみの一覧がまだ信頼する (2026-10 の eMudhra の件:
    // cacert 2026-10-20 は emSign Root CA - C1 と ECC Root CA - C3 を外したが、webpki-roots 1.0.10 にはまだある)
    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

pub fn connect(url: &Url) -> io::Result<Box<dyn ReadWrite>> {
    let name = ServerName::try_from(url.host.to_string()).map_err(io::Error::other)?;
    let conn = ClientConnection::new(config()?, name).map_err(io::Error::other)?;
    let sock = http::tcp(url)?;
    Ok(Box::new(Eof(StreamOwned::new(conn, sock))))
}

/// close_notify を送らずに切るサーバーも多いので、その切断は終わりとして扱う
struct Eof<T>(T);

impl<T: io::Read> io::Read for Eof<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
            r => r,
        }
    }
}

impl<T: io::Write> io::Write for Eof<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
