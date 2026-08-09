//! End-to-end proof that `tls = "none"` really delivers (#57): a real lettre
//! client driven against an in-process plaintext SMTP server. The two TLS modes
//! stay manual (CI has no relay — see `docs/manual-tests/smtp.md`), but the
//! plaintext path is exactly the one that used to be unreachable, so it is the
//! one worth pinning down automatically.

use std::time::Duration;

use freshdock::config::SmtpTls;
use freshdock::notify::smtp::{SmtpNotifier, SmtpParams};
use freshdock::notify::{Notifier, NotifyEvent};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Serve exactly one SMTP conversation and return the message body the client
/// sent between `DATA` and the terminating `.` line. Speaks only the subset
/// lettre drives, and deliberately advertises **no** STARTTLS: a client that
/// negotiated TLS could not complete a session here, which is what makes this a
/// regression test for the plaintext mode.
async fn serve_one(listener: TcpListener) -> String {
    let (socket, _) = listener.accept().await.expect("accept");
    let (read_half, mut write) = socket.into_split();
    let mut reader = BufReader::new(read_half);
    write.write_all(b"220 fake ESMTP\r\n").await.expect("greet");

    let mut data = String::new();
    let mut in_data = false;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.expect("read") == 0 {
            break;
        }
        if in_data {
            if line.trim_end_matches(['\r', '\n']) == "." {
                write.write_all(b"250 OK\r\n").await.expect("accept data");
                // The client has its final 250; nothing else is needed from a
                // pooled connection, so the conversation ends here.
                break;
            }
            data.push_str(&line);
            continue;
        }
        let command = line.trim_end().to_ascii_uppercase();
        let reply: &[u8] = if command.starts_with("EHLO") || command.starts_with("HELO") {
            b"250-fake\r\n250 OK\r\n"
        } else if command.starts_with("DATA") {
            in_data = true;
            b"354 End data with <CR><LF>.<CR><LF>\r\n"
        } else if command.starts_with("QUIT") {
            write.write_all(b"221 Bye\r\n").await.expect("bye");
            break;
        } else {
            // MAIL FROM / RCPT TO / anything else this fake need not model.
            b"250 OK\r\n"
        };
        write.write_all(reply).await.expect("reply");
    }
    data
}

#[tokio::test]
async fn plaintext_transport_delivers_to_a_local_catcher() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(serve_one(listener));

    let notifier = SmtpNotifier::new(SmtpParams {
        name: "email".into(),
        host: addr.ip().to_string(),
        port: addr.port(),
        username: None,
        password: None,
        from: "freshdock@example.com".into(),
        to: vec!["admin@example.com".into()],
        tls: SmtpTls::Plaintext,
    })
    .expect("plaintext notifier builds");

    let msg = NotifyEvent::UpdateSucceeded {
        container: "web".into(),
        image: "nginx:latest".into(),
        new_id: "sha256:abcdef0123456789".into(),
    }
    .render();

    // A TLS-negotiating transport would stall on the handshake against this
    // server; bound the wait so a regression fails the test instead of hanging.
    tokio::time::timeout(Duration::from_secs(10), notifier.send(&msg))
        .await
        .expect("send timed out")
        .expect("plaintext send failed");

    let data = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server timed out")
        .expect("server task");
    assert!(
        data.contains(&format!("Subject: {}", msg.title)),
        "rendered subject reached the wire: {data}"
    );
    assert!(
        data.contains("admin@example.com"),
        "recipient reached the wire: {data}"
    );
}
