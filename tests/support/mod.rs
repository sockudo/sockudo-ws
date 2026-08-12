use tokio::io::AsyncReadExt;

pub async fn read_http_request<S>(stream: &mut S) -> Vec<u8>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut request = Vec::new();
    let mut chunk = [0u8; 256];

    loop {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(read > 0, "HTTP request closed before headers completed");
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return request;
        }
    }
}

pub fn extract_header<'a>(request: &'a str, expected_name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(expected_name)
            .then(|| value.trim())
    })
}
