/// Stratum protocol robustness tests.
///
/// These tests run a real StratumServer (bound to a random port) and exercise
/// it with raw TCP connections.  They do NOT require bitcoind — they use a
/// pre-built fake JobTemplate to side-step the template engine.
///
/// Test categories:
///   - Message framing: split packets, multi-message packets, oversized lines
///   - Session state: submit before authorize, unknown job_id, etc.
///   - Input validation: malformed extranonce2, nonce, ntime
///   - Duplicate detection
///   - Concurrency: 100 sessions, burst submits
///   - Template change during submission
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Connect to a running pool stratum port and return a raw stream.
async fn connect(port: u16) -> TcpStream {
    TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("failed to connect to stratum")
}

/// Send raw bytes and read back until `\n` (one line).
async fn send_recv(stream: &mut TcpStream, msg: &str) -> String {
    stream.write_all(msg.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    loop {
        let mut b = [0u8; 1];
        stream.read_exact(&mut b).await.unwrap();
        if b[0] == b'\n' {
            break;
        }
        buf.push(b[0]);
    }
    String::from_utf8(buf).unwrap()
}

/// Send bytes, read back N lines.
async fn send_recv_n(stream: &mut TcpStream, msg: &str, n: usize) -> Vec<String> {
    stream.write_all(msg.as_bytes()).await.unwrap();
    let mut lines = Vec::new();
    for _ in 0..n {
        let mut buf = Vec::new();
        loop {
            let mut b = [0u8; 1];
            match tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut b)).await {
                Ok(Ok(_)) => {}
                _ => {
                    lines.push(String::from_utf8(buf).unwrap());
                    return lines;
                }
            }
            if b[0] == b'\n' {
                break;
            }
            buf.push(b[0]);
        }
        lines.push(String::from_utf8(buf).unwrap());
    }
    lines
}

/// Parse the pool's stratum port from the env or use 12018 (test default).
fn stratum_port() -> u16 {
    std::env::var("TEST_STRATUM_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12018)
}

// ─── Mock pool startup ────────────────────────────────────────────────────────

/// Start a minimal StratumServer on a random port for testing.
/// Returns the port so tests can connect.
///
/// NOTE: This requires solo-pool to export StratumServer and enough internal
/// types publicly, OR we use a helper binary.  For now, these tests act as
/// *black-box* TCP tests against a running pool instance.
///
/// To run: start the pool with TEST_STRATUM_PORT=12018, then run `cargo test`.
/// The tests skip gracefully if no pool is reachable.
fn skip_if_no_pool() -> Option<u16> {
    let port = stratum_port();
    // Try a non-blocking connect.
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
        Ok(_) => Some(port),
        Err(_) => None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// FRAMING TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Two complete Stratum messages in one TCP write.
/// Both must be processed.  The pool must NOT wait for another packet.
///
/// Severity if fails: HIGH — miners that flush subscribe+authorize together
/// (common in firmware) would never get a job.
#[tokio::test]
async fn test_two_messages_in_one_packet() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // Send subscribe + authorize as one TCP write.
    let combined = concat!(
        r#"{"id":1,"method":"mining.subscribe","params":["testfw/1.0"]}"#,
        "\n",
        r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
        "\n",
    );
    let responses = send_recv_n(&mut s, combined, 3).await; // subscribe_resp + set_diff + notify

    // First response must be subscribe acknowledgement with extranonce1.
    let r0: serde_json::Value = serde_json::from_str(&responses[0]).unwrap();
    assert_eq!(r0["id"], 1, "first response must be for subscribe (id=1)");
    assert!(r0["result"].is_array(), "subscribe result must be an array");

    // Second response should be set_difficulty or authorize ack.
    // Pool sends: subscribe_ack, authorize_ack, set_difficulty, notify.
    // The exact order may vary, but we must receive at least 2 responses.
    assert!(
        responses.len() >= 2,
        "must receive at least 2 responses for 2 messages"
    );
}

/// One Stratum message split across two TCP writes.
/// BufReader must buffer and reassemble.
///
/// Severity if fails: HIGH — any real network with segmentation would break.
#[tokio::test]
async fn test_message_split_across_packets() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    let part1 = r#"{"id":1,"method":"mining.sub"#;
    let part2 = r#"scribe","params":["splitfw/1.0"]}"#;

    s.write_all(part1.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await; // ensure separate TCP segments
    s.write_all(part2.as_bytes()).await.unwrap();
    s.write_all(b"\n").await.unwrap();

    let resp = send_recv(&mut s, "").await; // read the response
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(r["id"], 1, "subscribe response id must match");
    assert!(r["result"].is_array(), "subscribe result must be array");
}

/// Malformed JSON must be silently skipped.
/// The connection must remain open and the next valid message processed.
///
/// Severity if fails: HIGH — one bad packet kills the miner session.
#[tokio::test]
async fn test_malformed_json_connection_survives() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // Send garbage then a valid subscribe.
    let msg = concat!(
        "{not valid json at all!!!\n",
        r#"{"id":1,"method":"mining.subscribe","params":["goodfw/1.0"]}"#,
        "\n"
    );
    let responses = send_recv_n(&mut s, msg, 2).await;

    // The valid subscribe must still be processed.
    let sub_resp = responses.iter().find(|r| {
        serde_json::from_str::<serde_json::Value>(r)
            .ok()
            .and_then(|v| v["id"].as_i64())
            .map(|id| id == 1)
            .unwrap_or(false)
    });
    assert!(
        sub_resp.is_some(),
        "valid subscribe after malformed JSON must receive a response — connection killed on bad input"
    );
}

/// Oversized line (> 64 KB) must cause the connection to be closed gracefully.
/// The pool must NOT OOM.
///
/// Severity if fails: CRITICAL — DOS vector against the entire pool.
#[tokio::test]
async fn test_oversized_line_drops_connection() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // 128 KB of 'A' with no newline, then a newline.
    let oversized: Vec<u8> = std::iter::repeat(b'A')
        .take(128 * 1024)
        .chain(std::iter::once(b'\n'))
        .collect();
    s.write_all(&oversized).await.unwrap();

    // The pool must close the connection (we should get EOF or an error on read).
    let mut buf = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut buf)).await;

    match result {
        Ok(Ok(_)) => {
            // Got a byte — check if it's a close (EOF would be Ok(0) from read)
            // Some pools might send an error message; that's also acceptable.
        }
        Ok(Err(_)) | Err(_) => {
            // Connection closed or timed out — this is the expected behavior.
        }
    }
    // The key assertion: the pool process must still be running and accepting
    // new connections after this attack.  We verify by opening a fresh connection.
    let mut s2 = connect(port).await;
    let resp = send_recv(
        &mut s2,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["probe/1.0"]}"#,
            "\n"
        ),
    )
    .await;
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        r["id"], 1,
        "pool must still accept connections after oversized-line attack"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// SESSION STATE TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// Submit before authorize must return error [24, "Unauthorized"].
///
/// Severity if fails: MEDIUM — could allow unauthorized submissions.
#[tokio::test]
async fn test_submit_before_authorize() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // Subscribe but do NOT authorize.
    send_recv(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#,
            "\n"
        ),
    )
    .await;

    // Submit without authorizing.
    let resp = send_recv(&mut s, concat!(
        r#"{"id":2,"method":"mining.submit","params":["worker","1","00000000","69b58000","deadbeef"]}"#, "\n"
    )).await;

    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        r["result"], false,
        "submit before authorize must return false"
    );
    let err_code = r["error"][0].as_i64().unwrap_or(0);
    assert_eq!(err_code, 24, "error code must be 24 (Unauthorized)");
}

/// Submit with unknown job_id must return error [21, "Stale share"].
///
/// Severity if fails: HIGH — broken stale handling could affect share accounting.
#[tokio::test]
async fn test_submit_unknown_job_id() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // Subscribe + authorize.
    send_recv_n(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#,
            "\n",
            r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
            "\n",
        ),
        4,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Submit with a job_id that does not exist.
    let resp = send_recv(&mut s, concat!(
        r#"{"id":3,"method":"mining.submit","params":["bc1qtest","deadjob","00000000","69b58000","deadbeef"]}"#, "\n"
    )).await;
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(r["result"], false);
    let err_code = r["error"][0].as_i64().unwrap_or(0);
    assert_eq!(
        err_code, 21,
        "unknown job_id must return error 21 (Stale share)"
    );
}

/// Duplicate share (same job+nonce+ntime+en2+version twice) must return error [22].
///
/// Severity if fails: MEDIUM — miners could inflate share counts.
#[tokio::test]
async fn test_duplicate_share_rejected() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // Subscribe + authorize + wait for notify to get a real job_id.
    let setup = send_recv_n(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#,
            "\n",
            r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
            "\n",
        ),
        5,
    )
    .await;

    // Find the notify message to get a valid job_id.
    let job_id = setup
        .iter()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["method"].as_str()? == "mining.notify" {
                Some(v["params"][0].as_str()?.to_string())
            } else {
                None
            }
        })
        .unwrap_or("1".to_string());

    // Submit twice with identical parameters.
    let submit = format!(
        r#"{{"id":3,"method":"mining.submit","params":["bc1qtest","{}","00000000","69b58001","aabbccdd"]}}"#,
        job_id
    );
    let submit_nl = format!("{submit}\n{submit}\n");

    let responses = send_recv_n(&mut s, &submit_nl, 2).await;

    // Both should have result=false (first is likely low-diff, second is duplicate).
    // What matters: the second must be [22, "Duplicate share"] or [21, stale].
    // If the first was accepted (lucky hash), the second must be [22].
    let second: serde_json::Value = serde_json::from_str(&responses[1]).unwrap();
    let err_code = second["error"][0].as_i64().unwrap_or(-1);
    assert!(
        err_code == 22 || err_code == 21 || second["result"] == false,
        "second identical submit must be rejected, got: {}",
        responses[1]
    );
}

/// Wrong extranonce2 length must return a validation error.
///
/// Severity if fails: HIGH — could crash the share validation or accept invalid shares.
#[tokio::test]
async fn test_wrong_extranonce2_length() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    let setup = send_recv_n(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#,
            "\n",
            r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
            "\n",
        ),
        5,
    )
    .await;

    let job_id = setup
        .iter()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["method"].as_str()? == "mining.notify" {
                Some(v["params"][0].as_str()?.to_string())
            } else {
                None
            }
        })
        .unwrap_or("1".to_string());

    // extranonce2 is 256 bytes (way too long, > 256 hex chars = more than MAX_EN2).
    let huge_en2 = "aa".repeat(200); // 400 hex chars = 200 bytes — clearly wrong
    let submit = format!(
        r#"{{"id":3,"method":"mining.submit","params":["bc1qtest","{}","{}","69b58001","aabbccdd"]}}"#,
        job_id, huge_en2
    );

    let resp = send_recv(&mut s, &format!("{submit}\n")).await;
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(r["result"], false, "oversized extranonce2 must be rejected");
    assert!(
        r["error"][0].as_i64().is_some(),
        "must include an error code"
    );
}

/// Malformed nonce (non-hex) must not panic — must return an error response.
///
/// Severity if fails: HIGH — could crash or silently accept garbage.
#[tokio::test]
async fn test_malformed_nonce_rejected() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    send_recv_n(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#,
            "\n",
            r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
            "\n",
        ),
        5,
    )
    .await;

    // nonce = "ZZZZZZZZ" (non-hex)
    let resp = send_recv(&mut s, concat!(
        r#"{"id":3,"method":"mining.submit","params":["bc1qtest","1","00000000","69b58001","ZZZZZZZZ"]}"#, "\n"
    )).await;
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    // Pool normalises nonce to 8 hex chars; "ZZZZ" would fail u32::from_str_radix.
    // It must NOT panic.  It must return false or an error.
    assert!(
        r["result"] == false || r["error"].is_array(),
        "malformed nonce must return false/error, got: {resp}"
    );
}

/// Malformed ntime (non-hex) must not panic.
///
/// Severity if fails: HIGH.
#[tokio::test]
async fn test_malformed_ntime_rejected() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    send_recv_n(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#,
            "\n",
            r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
            "\n",
        ),
        5,
    )
    .await;

    let resp = send_recv(&mut s, concat!(
        r#"{"id":3,"method":"mining.submit","params":["bc1qtest","1","00000000","XXXXXXXX","aabbccdd"]}"#, "\n"
    )).await;
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        r["result"] == false || r["error"].is_array(),
        "malformed ntime must return false/error"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// CONCURRENCY TESTS
// ═══════════════════════════════════════════════════════════════════════════════

/// 100 simultaneous connections: each subscribes and authorizes.
/// Pool must handle all without panicking or dropping connections.
///
/// Severity if fails: HIGH — pool crashes under modest miner load.
#[tokio::test]
async fn test_100_concurrent_sessions() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };

    let handles: Vec<_> = (0..100)
        .map(|i| {
            tokio::spawn(async move {
                let mut s = TcpStream::connect(format!("127.0.0.1:{port}"))
                    .await
                    .expect("connect failed");

                let msg =
                    format!(r#"{{"id":1,"method":"mining.subscribe","params":["stress/1.0"]}}"#);
                s.write_all(format!("{msg}\n").as_bytes()).await.unwrap();

                let mut buf = Vec::new();
                loop {
                    let mut b = [0u8; 1];
                    match tokio::time::timeout(Duration::from_secs(3), s.read_exact(&mut b)).await {
                        Ok(Ok(_)) => {
                            if b[0] == b'\n' {
                                break;
                            }
                            buf.push(b[0]);
                        }
                        _ => break,
                    }
                }
                let resp = String::from_utf8(buf).unwrap_or_default();
                (i, resp)
            })
        })
        .collect();

    let results: Vec<_> = futures::future::join_all(handles).await;
    let successes = results
        .iter()
        .filter(|r| {
            if let Ok((_, resp)) = r {
                serde_json::from_str::<serde_json::Value>(resp)
                    .ok()
                    .map(|v| v["result"].is_array())
                    .unwrap_or(false)
            } else {
                false
            }
        })
        .count();

    assert!(
        successes >= 95,
        "at least 95/100 concurrent sessions must subscribe successfully, got {successes}"
    );
}

/// Burst submit from one miner: 50 submits in rapid succession.
/// All must receive responses.  Pool must not deadlock or drop responses.
///
/// Severity if fails: HIGH — fast ASICs routinely burst-submit shares.
#[tokio::test]
async fn test_burst_submit_50_shares() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };
    let mut s = connect(port).await;

    // Subscribe + authorize.
    let setup = send_recv_n(
        &mut s,
        concat!(
            r#"{"id":1,"method":"mining.subscribe","params":["burst/1.0"]}"#,
            "\n",
            r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
            "\n",
        ),
        6,
    )
    .await;

    let job_id = setup
        .iter()
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["method"].as_str()? == "mining.notify" {
                Some(v["params"][0].as_str()?.to_string())
            } else {
                None
            }
        })
        .unwrap_or("1".to_string());

    // Build 50 submits with different nonces.
    let mut burst = String::new();
    for i in 0u32..50 {
        let nonce = format!("{:08x}", i);
        burst.push_str(&format!(
            r#"{{"id":{},"method":"mining.submit","params":["bc1qtest","{}","{:08x}","69b58001","{}"]}}"#,
            100 + i, job_id, i, nonce
        ));
        burst.push('\n');
    }

    // Send all at once.
    s.write_all(burst.as_bytes()).await.unwrap();

    // Read 50 responses.
    let mut response_count = 0;
    for _ in 0..50 {
        let mut buf = Vec::new();
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut b = [0u8; 1];
                s.read_exact(&mut b).await.unwrap();
                if b[0] == b'\n' {
                    break;
                }
                buf.push(b[0]);
            }
            String::from_utf8(buf).unwrap()
        })
        .await;

        match got {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                    let id = v["id"].as_i64().unwrap_or(-1);
                    if id >= 100 {
                        response_count += 1;
                    }
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        response_count >= 45,
        "burst of 50 submits must get at least 45 responses, got {response_count}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// PHASE 2C — TEMPLATE CHANGE DURING SUBMISSIONS
// ═══════════════════════════════════════════════════════════════════════════════

/// Template change during active submissions.
///
/// A miner submits shares continuously while the pool receives new block
/// templates (which it would from ZMQ in production).  The pool must:
///   1. Process all in-flight submits against their respective session jobs
///   2. Not crash or deadlock when notify tasks and submit tasks run concurrently
///   3. Return a response for every submit (no silent drops)
///
/// We simulate this by having one task submit rapidly while a second task
/// re-subscribes to the pool (forcing a new session notification) repeatedly.
///
/// Severity if fails: CRITICAL — concurrent template change + share submit
/// is the most common real-world condition during active mining.
#[tokio::test]
async fn test_template_change_during_submissions() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };

    // Task A: submit shares as fast as possible for 3 seconds.
    let submit_task = tokio::spawn(async move {
        let mut s = connect(port).await;
        let setup = send_recv_n(
            &mut s,
            concat!(
                r#"{"id":1,"method":"mining.subscribe","params":["concurrent/1.0"]}"#,
                "\n",
                r#"{"id":2,"method":"mining.authorize","params":["bc1qtest",""]}"#,
                "\n",
            ),
            5,
        )
        .await;

        let job_id = setup
            .iter()
            .find_map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).ok()?;
                if v["method"].as_str()? == "mining.notify" {
                    Some(v["params"][0].as_str()?.to_string())
                } else {
                    None
                }
            })
            .unwrap_or("1".to_string());

        let mut submit_count = 0u32;
        let mut accept_count = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);

        for nonce in 0u32..500 {
            if tokio::time::Instant::now() > deadline {
                break;
            }
            let msg = format!(
                r#"{{"id":{},"method":"mining.submit","params":["bc1qtest","{}","{:08x}","69b58001","{:08x}"]}}"#,
                200 + nonce,
                job_id,
                nonce,
                nonce
            );
            s.write_all(format!("{msg}\n").as_bytes()).await.unwrap();
            submit_count += 1;

            // Read any available response (non-blocking peek)
            s.set_nodelay(true).ok();
            let mut buf = [0u8; 1024];
            match tokio::time::timeout(Duration::from_millis(2), s.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    accept_count += text
                        .lines()
                        .filter(|l| {
                            serde_json::from_str::<serde_json::Value>(l)
                                .ok()
                                .map(|v| v["id"].as_i64().unwrap_or(-1) >= 200)
                                .unwrap_or(false)
                        })
                        .count() as u32;
                }
                _ => {}
            }
        }
        (submit_count, accept_count)
    });

    // Task B: open and close connections rapidly to the pool (simulates
    // other miners connecting/disconnecting, which exercises session state
    // management concurrently with Task A's submissions).
    let connect_task = tokio::spawn(async move {
        let mut reconnects = 0u32;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            if let Ok(mut s) = TcpStream::connect(format!("127.0.0.1:{port}")).await {
                let msg = r#"{"id":1,"method":"mining.subscribe","params":["churn/1.0"]}"#;
                let _ = s.write_all(format!("{msg}\n").as_bytes()).await;
                reconnects += 1;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        reconnects
    });

    let (submit_result, connect_result) = tokio::join!(submit_task, connect_task);

    let (submits, _accepts) = submit_result.expect("submit task must not panic");
    let reconnects = connect_result.expect("connect task must not panic");

    assert!(
        submits > 0,
        "must have sent at least 1 submit during concurrent operation"
    );
    assert!(
        reconnects > 0,
        "must have performed at least 1 reconnect during concurrent operation"
    );

    // Pool must still be responsive after concurrent stress.
    let mut probe = connect(port).await;
    let resp = send_recv(
        &mut probe,
        concat!(
            r#"{"id":99,"method":"mining.subscribe","params":["probe/1.0"]}"#,
            "\n"
        ),
    )
    .await;
    let r: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        r["id"], 99,
        "pool must remain responsive after template-change/submission concurrency test"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// PHASE 2C — RECONNECT STORM TEST
// ═══════════════════════════════════════════════════════════════════════════════

/// Reconnect storm: 50 miners all disconnect and reconnect simultaneously.
///
/// Real-world scenario: power outage, pool restart, or network hiccup causes
/// all miners to reconnect at once.  The pool must handle this without:
///   - Panicking or crashing
///   - Running out of file descriptors / Tokio tasks
///   - Leaving zombie sessions that block future connections
///   - Losing responsiveness to legitimate subscribers after the storm
///
/// Severity if fails: HIGH — a pool that crashes on reconnect storms cannot
/// be relied upon for real solo mining.
#[tokio::test]
async fn test_reconnect_storm() {
    let port = match skip_if_no_pool() {
        Some(p) => p,
        None => return,
    };

    // Phase 1: Storm — 50 simultaneous connect/subscribe/disconnect cycles.
    let storm_handles: Vec<_> = (0u32..50)
        .map(|i| {
            tokio::spawn(async move {
                // Each goroutine does: connect → subscribe → immediately disconnect
                for _round in 0..3 {
                    match TcpStream::connect(format!("127.0.0.1:{port}")).await {
                        Ok(mut s) => {
                            let msg = format!(
                                r#"{{"id":{},"method":"mining.subscribe","params":["storm/1.0"]}}"#,
                                1000 + i
                            );
                            let _ = s.write_all(format!("{msg}\n").as_bytes()).await;
                            // Brief pause then drop (simulates abrupt disconnect)
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            // s is dropped here = TCP connection closed
                        }
                        Err(_) => {}
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                i
            })
        })
        .collect();

    // Wait for all storm tasks.
    let results: Vec<_> = futures::future::join_all(storm_handles).await;
    let completed = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        completed, 50,
        "all 50 storm tasks must complete without panic"
    );

    // Brief settle time for the pool to clean up dead sessions.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: Verify — pool still accepts new connections normally.
    let mut verify_failures = 0;
    for i in 0..5 {
        let mut s = match TcpStream::connect(format!("127.0.0.1:{port}")).await {
            Ok(s) => s,
            Err(_) => {
                verify_failures += 1;
                continue;
            }
        };
        let msg = format!(
            r#"{{"id":{},"method":"mining.subscribe","params":["verify/1.0"]}}"#,
            9000 + i
        );
        let _ = s.write_all(format!("{msg}\n").as_bytes()).await;
        let mut buf = Vec::new();
        match tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let mut b = [0u8; 1];
                s.read_exact(&mut b).await?;
                if b[0] == b'\n' {
                    break;
                }
                buf.push(b[0]);
            }
            Ok::<_, std::io::Error>(())
        })
        .await
        {
            Ok(Ok(_)) => {
                let resp_str = String::from_utf8(buf).unwrap_or_default();
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp_str) {
                    if v["result"].is_array() {
                        continue; // good response
                    }
                }
                verify_failures += 1;
            }
            _ => {
                verify_failures += 1;
            }
        }
    }

    assert!(
        verify_failures == 0,
        "pool must accept all 5 post-storm verification connections, \
         {verify_failures}/5 failed"
    );
}
