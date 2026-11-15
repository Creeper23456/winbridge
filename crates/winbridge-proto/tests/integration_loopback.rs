// SPDX-License-Identifier: GPL-3.0-or-later

//! End-to-end asynchronous protocol loopback integration tests.

use tokio::io::{duplex, split};
use winbridge_proto::async_io::{read_frame_async, write_frame_async};
use winbridge_proto::{ClipboardFormat, Message};

#[tokio::test]
async fn test_async_bidirectional_ping_pong() {
    let (client, server) = duplex(4096);
    let (mut client_read, mut client_write) = split(client);
    let (mut server_read, mut server_write) = split(server);

    let server_task = tokio::spawn(async move {
        let msg = read_frame_async(&mut server_read).await.unwrap();
        if let Message::Ping { seq, timestamp_ms } = msg {
            write_frame_async(
                &mut server_write,
                &Message::Pong { seq, timestamp_ms },
            )
            .await
            .unwrap();
        } else {
            panic!("Expected Ping, got {:?}", msg);
        }
    });

    let client_task = tokio::spawn(async move {
        write_frame_async(
            &mut client_write,
            &Message::Ping {
                seq: 100,
                timestamp_ms: 55555,
            },
        )
        .await
        .unwrap();

        let reply = read_frame_async(&mut client_read).await.unwrap();
        assert_eq!(
            reply,
            Message::Pong {
                seq: 100,
                timestamp_ms: 55555
            }
        );
    });

    server_task.await.unwrap();
    client_task.await.unwrap();
}

#[tokio::test]
async fn test_resolution_and_dpi_handshake() {
    let (client, server) = duplex(4096);
    let (mut client_read, mut client_write) = split(client);
    let (mut server_read, mut server_write) = split(server);

    let server_task = tokio::spawn(async move {
        // Handle SetResolution
        let msg1 = read_frame_async(&mut server_read).await.unwrap();
        assert_eq!(msg1, Message::SetResolution { width: 1920, height: 1080 });
        write_frame_async(
            &mut server_write,
            &Message::ResolutionResult {
                width: 1920,
                height: 1080,
                success: true,
                error: None,
            },
        )
        .await
        .unwrap();

        // Handle SetDpiScale
        let msg2 = read_frame_async(&mut server_read).await.unwrap();
        assert_eq!(msg2, Message::SetDpiScale { scale_percent: 150 });
        write_frame_async(
            &mut server_write,
            &Message::DpiScaleResult {
                scale_percent: 150,
                success: true,
                error: None,
            },
        )
        .await
        .unwrap();
    });

    let client_task = tokio::spawn(async move {
        write_frame_async(
            &mut client_write,
            &Message::SetResolution { width: 1920, height: 1080 },
        )
        .await
        .unwrap();
        let res1 = read_frame_async(&mut client_read).await.unwrap();
        assert_eq!(
            res1,
            Message::ResolutionResult {
                width: 1920,
                height: 1080,
                success: true,
                error: None,
            }
        );

        write_frame_async(
            &mut client_write,
            &Message::SetDpiScale { scale_percent: 150 },
        )
        .await
        .unwrap();
        let res2 = read_frame_async(&mut client_read).await.unwrap();
        assert_eq!(
            res2,
            Message::DpiScaleResult {
                scale_percent: 150,
                success: true,
                error: None,
            }
        );
    });

    server_task.await.unwrap();
    client_task.await.unwrap();
}

#[tokio::test]
async fn test_drag_and_drop_chunking_stream() {
    let (client, server) = duplex(8192);
    let (mut client_read, mut client_write) = split(client);
    let (mut server_read, mut server_write) = split(server);

    let test_payload = vec![0xABu8; 1024 * 64]; // 64 KB

    let payload_for_server = test_payload.clone();
    let server_task = tokio::spawn(async move {
        let mut received = Vec::new();

        loop {
            let msg = read_frame_async(&mut server_read).await.unwrap();
            match msg {
                Message::DropFileStart { transfer_id, filename, total_bytes } => {
                    assert_eq!(transfer_id, 1);
                    assert_eq!(filename, "test.bin");
                    assert_eq!(total_bytes, payload_for_server.len() as u64);
                }
                Message::DropFileChunk { transfer_id, offset, data } => {
                    assert_eq!(transfer_id, 1);
                    assert_eq!(offset, received.len() as u64);
                    received.extend_from_slice(&data);
                }
                Message::DropFileFinish { transfer_id, success, .. } => {
                    assert_eq!(transfer_id, 1);
                    assert!(success);
                    break;
                }
                other => panic!("Unexpected msg: {:?}", other),
            }
        }

        assert_eq!(received, payload_for_server);

        write_frame_async(
            &mut server_write,
            &Message::DropResult {
                success: true,
                message: "Saved 65536 bytes".into(),
            },
        )
        .await
        .unwrap();
    });

    let client_task = tokio::spawn(async move {
        let chunk_size = 16 * 1024;
        write_frame_async(
            &mut client_write,
            &Message::DropFileStart {
                transfer_id: 1,
                filename: "test.bin".into(),
                total_bytes: test_payload.len() as u64,
            },
        )
        .await
        .unwrap();

        for (i, chunk) in test_payload.chunks(chunk_size).enumerate() {
            write_frame_async(
                &mut client_write,
                &Message::DropFileChunk {
                    transfer_id: 1,
                    offset: (i * chunk_size) as u64,
                    data: chunk.to_vec(),
                },
            )
            .await
            .unwrap();
        }

        write_frame_async(
            &mut client_write,
            &Message::DropFileFinish {
                transfer_id: 1,
                success: true,
                error: None,
            },
        )
        .await
        .unwrap();

        let ack = read_frame_async(&mut client_read).await.unwrap();
        assert_eq!(
            ack,
            Message::DropResult {
                success: true,
                message: "Saved 65536 bytes".into(),
            }
        );
    });

    server_task.await.unwrap();
    client_task.await.unwrap();
}

#[tokio::test]
async fn test_clipboard_payload_exchange() {
    let (client, server) = duplex(4096);
    let (mut client_read, mut client_write) = split(client);
    let (mut server_read, mut server_write) = split(server);

    let server_task = tokio::spawn(async move {
        let msg = read_frame_async(&mut server_read).await.unwrap();
        assert_eq!(
            msg,
            Message::ClipboardPayload {
                format: ClipboardFormat::Text,
                data: "Hello from host".as_bytes().to_vec(),
            }
        );

        write_frame_async(
            &mut server_write,
            &Message::ClipboardPayload {
                format: ClipboardFormat::Text,
                data: "Hello from guest".as_bytes().to_vec(),
            },
        )
        .await
        .unwrap();
    });

    let client_task = tokio::spawn(async move {
        write_frame_async(
            &mut client_write,
            &Message::ClipboardPayload {
                format: ClipboardFormat::Text,
                data: "Hello from host".as_bytes().to_vec(),
            },
        )
        .await
        .unwrap();

        let reply = read_frame_async(&mut client_read).await.unwrap();
        assert_eq!(
            reply,
            Message::ClipboardPayload {
                format: ClipboardFormat::Text,
                data: "Hello from guest".as_bytes().to_vec(),
            }
        );
    });

    server_task.await.unwrap();
    client_task.await.unwrap();
}
