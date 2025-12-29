//! Simple test client to verify large message handling

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to ws://127.0.0.1:9001");

    let mut stream = TcpStream::connect("127.0.0.1:9001").await?;

    // Send WebSocket handshake
    let handshake = "GET / HTTP/1.1\r\n\
                     Host: 127.0.0.1:9001\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                     Sec-WebSocket-Version: 13\r\n\
                     \r\n";

    stream.write_all(handshake.as_bytes()).await?;
    stream.flush().await?;

    // Read handshake response
    let mut response = vec![0u8; 1024];
    let n = stream.read(&mut response).await?;
    let response_str = String::from_utf8_lossy(&response[..n]);

    if !response_str.contains("101 Switching Protocols") {
        eprintln!("✗ Handshake failed:\n{}", response_str);
        return Err("Handshake failed".into());
    }

    println!("✓ WebSocket handshake successful");

    // Test 1: Small message (100 bytes)
    println!("\n--- Test 1: 100 byte message ---");
    test_message(&mut stream, 100).await?;

    // Test 2: 8KB message
    println!("\n--- Test 2: 8KB message ---");
    test_message(&mut stream, 8192).await?;

    // Test 3: 65535 byte message (the failing case)
    println!("\n--- Test 3: 65535 byte message (critical test) ---");
    test_message(&mut stream, 65535).await?;

    // Test 4: 65536 byte message
    println!("\n--- Test 4: 65536 byte message ---");
    test_message(&mut stream, 65536).await?;

    // Send close frame
    println!("\n--- Closing connection ---");
    let close_frame = vec![0x88, 0x02, 0x03, 0xe8]; // Close with code 1000
    stream.write_all(&close_frame).await?;
    stream.flush().await?;

    // Read close response
    let mut close_response = vec![0u8; 256];
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read(&mut close_response),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => {
            println!("✓ Received close response ({} bytes)", n);
        }
        Ok(Ok(_)) => {
            println!("✓ Connection closed");
        }
        Ok(Err(e)) => {
            eprintln!("✗ Error reading close response: {}", e);
        }
        Err(_) => {
            println!("⚠ Timeout waiting for close response");
        }
    }

    println!("\n=== All tests completed ===");
    Ok(())
}

async fn test_message(
    stream: &mut TcpStream,
    size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Sending {} byte message...", size);

    // Create payload
    let payload: Vec<u8> = vec![b'A'; size];

    // Build WebSocket text frame
    let mut frame = Vec::new();

    // First byte: FIN=1, RSV=0, Opcode=Text(1)
    frame.push(0x81);

    // Second byte: MASK=1 + length
    if size <= 125 {
        frame.push(0x80 | size as u8);
    } else if size <= 65535 {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(size as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(size as u64).to_be_bytes());
    }

    // Mask (4 bytes)
    let mask = [0x12, 0x34, 0x56, 0x78];
    frame.extend_from_slice(&mask);

    // Masked payload
    for (i, byte) in payload.iter().enumerate() {
        frame.push(byte ^ mask[i % 4]);
    }

    // Send frame
    stream.write_all(&frame).await?;
    stream.flush().await?;
    println!("Sent frame ({} bytes total)", frame.len());

    // Read response with timeout
    let mut response = vec![0u8; size + 1024];
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read(&mut response),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 && response[0] & 0x0F != 0x08 => {
            println!("Received {} bytes", n);

            // Parse WebSocket frame header
            if n < 2 {
                eprintln!("✗ Response too short");
                return Err("Response too short".into());
            }

            let fin = response[0] & 0x80 != 0;
            let opcode = response[0] & 0x0F;
            let masked = response[1] & 0x80 != 0;
            let mut len = (response[1] & 0x7F) as usize;
            let mut pos = 2;

            if len == 126 {
                if n < 4 {
                    eprintln!("✗ Response too short for extended length");
                    return Err("Response too short".into());
                }
                len = u16::from_be_bytes([response[2], response[3]]) as usize;
                pos = 4;
            } else if len == 127 {
                if n < 10 {
                    eprintln!("✗ Response too short for 64-bit length");
                    return Err("Response too short".into());
                }
                len = u64::from_be_bytes([
                    response[2],
                    response[3],
                    response[4],
                    response[5],
                    response[6],
                    response[7],
                    response[8],
                    response[9],
                ]) as usize;
                pos = 10;
            }

            if masked {
                eprintln!("✗ Server should not mask frames");
                return Err("Server masked frame".into());
            }

            println!("  FIN={}, Opcode={}, Length={}", fin, opcode, len);

            // Check if we got the full payload
            let payload_start = pos;
            let payload_end = payload_start + len;

            if n < payload_end {
                eprintln!(
                    "✗ Incomplete payload: expected {} bytes, got {} bytes total",
                    payload_end, n
                );
                return Err("Incomplete payload".into());
            }

            if len != size {
                eprintln!("✗ Wrong payload size: expected {}, got {}", size, len);
                return Err("Wrong payload size".into());
            }

            // Verify payload content
            let received_payload = &response[payload_start..payload_end];
            if received_payload.iter().all(|&b| b == b'A') {
                println!("✓ Payload verified: {} bytes echoed correctly", len);
            } else {
                eprintln!("✗ Payload content mismatch");
                return Err("Payload mismatch".into());
            }
        }
        Ok(Ok(n)) if n > 0 => {
            // Received close frame or other control frame
            eprintln!("⚠ Received control frame instead of echo");
            return Err("Received control frame".into());
        }
        Ok(Ok(_)) => {
            eprintln!("✗ Connection closed by server");
            return Err("Connection closed".into());
        }
        Ok(Err(e)) => {
            eprintln!("✗ Read error: {}", e);
            return Err(e.into());
        }
        Err(_) => {
            eprintln!("✗ Timeout waiting for response");
            return Err("Timeout".into());
        }
    }

    Ok(())
}
