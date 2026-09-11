// SPDX-License-Identifier: AGPL-3.0-or-later

//! Streaming request body for LXD split image uploads.
//!
//! Streams a rootfs file in bounded chunks (256 KiB) directly from disk,
//! avoiding loading multi-gigabyte rootfs files into process memory.

use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::{Body, Bytes, Frame, SizeHint};
use tokio::fs::File;
use tokio::io::{AsyncRead, ReadBuf};

/// Rootfs chunk size: 256 KiB.
pub const ROOTFS_CHUNK_SIZE: usize = 256 * 1024;

enum State {
    Preamble,
    Rootfs,
    Closing,
    Done,
}

/// Request body for LXD multipart/form-data split image uploads.
///
/// Yields:
/// 1. Preamble frame: multipart boundary, metadata part headers + bytes, and rootfs part headers.
/// 2. Rootfs frames: bounded chunks of up to 256 KiB read from the rootfs file.
/// 3. Closing frame: rootfs part trailing CRLF and the closing multipart boundary.
pub(crate) struct SplitImageBody {
    state: State,
    preamble: Option<Bytes>,
    rootfs_file: File,
    rootfs_remaining: u64,
    closing: Option<Bytes>,
    read_buf: Box<[u8; ROOTFS_CHUNK_SIZE]>,
    total_len: u64,
}

impl SplitImageBody {
    pub(crate) fn new(
        metadata_filename: &str,
        metadata_bytes: &[u8],
        rootfs_filename: &str,
        rootfs_file: File,
        rootfs_len: u64,
        boundary: &str,
    ) -> Self {
        // Build preamble: metadata part + rootfs part headers
        let mut preamble = Vec::with_capacity(metadata_bytes.len() + 512);
        // metadata part
        preamble.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        preamble.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"metadata\"; filename=\"{metadata_filename}\"\r\n"
            )
            .as_bytes(),
        );
        preamble.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        preamble.extend_from_slice(metadata_bytes);
        preamble.extend_from_slice(b"\r\n");

        // rootfs part headers
        preamble.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        preamble.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"rootfs\"; filename=\"{rootfs_filename}\"\r\n"
            )
            .as_bytes(),
        );
        preamble.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");

        // closing boundary preceded by trailing CRLF of the rootfs part
        let mut closing = Vec::with_capacity(128);
        closing.extend_from_slice(b"\r\n");
        closing.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let total_len = (preamble.len() as u64) + rootfs_len + (closing.len() as u64);

        Self {
            state: State::Preamble,
            preamble: Some(Bytes::from(preamble)),
            rootfs_file,
            rootfs_remaining: rootfs_len,
            closing: Some(Bytes::from(closing)),
            read_buf: Box::new([0u8; ROOTFS_CHUNK_SIZE]),
            total_len,
        }
    }
}

impl Body for SplitImageBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        loop {
            match self.state {
                State::Preamble => {
                    let preamble = self
                        .preamble
                        .take()
                        .expect("preamble present in State::Preamble");
                    self.state = State::Rootfs;
                    return Poll::Ready(Some(Ok(Frame::data(preamble))));
                }
                State::Rootfs => {
                    if self.rootfs_remaining == 0 {
                        self.state = State::Closing;
                        continue;
                    }

                    let to_read = (ROOTFS_CHUNK_SIZE as u64).min(self.rootfs_remaining) as usize;
                    let this = self.as_mut().get_mut();
                    let mut read_buf = ReadBuf::new(&mut this.read_buf[..to_read]);

                    match Pin::new(&mut this.rootfs_file).poll_read(cx, &mut read_buf) {
                        Poll::Ready(Ok(())) => {
                            let filled = read_buf.filled();
                            if filled.is_empty() {
                                // Unexpected early EOF on rootfs file
                                return Poll::Ready(Some(Err(std::io::Error::new(
                                    std::io::ErrorKind::UnexpectedEof,
                                    format!(
                                        "unexpected EOF reading rootfs: {} bytes remaining",
                                        this.rootfs_remaining
                                    ),
                                ))));
                            }
                            let chunk_len = filled.len() as u64;
                            this.rootfs_remaining = this.rootfs_remaining.saturating_sub(chunk_len);
                            let bytes = Bytes::copy_from_slice(filled);
                            return Poll::Ready(Some(Ok(Frame::data(bytes))));
                        }
                        Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                        Poll::Pending => return Poll::Pending,
                    }
                }
                State::Closing => {
                    let closing = self
                        .closing
                        .take()
                        .expect("closing present in State::Closing");
                    self.state = State::Done;
                    return Poll::Ready(Some(Ok(Frame::data(closing))));
                }
                State::Done => return Poll::Ready(None),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        matches!(self.state, State::Done)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.total_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_split_image_body_byte_equality_and_chunk_bound() {
        let metadata_filename = "metadata.tar.xz";
        let metadata_bytes = b"fake-metadata-content-12345";
        let rootfs_filename = "rootfs.squashfs";
        let boundary = "testboundary123";

        // Create a rootfs file that spans multiple chunks (700 KiB > 2 * 256 KiB)
        let rootfs_len: usize = 700 * 1024;
        let mut temp = NamedTempFile::new().unwrap();
        let mut expected_rootfs = Vec::with_capacity(rootfs_len);
        for i in 0..rootfs_len {
            expected_rootfs.push((i % 251) as u8);
        }
        temp.write_all(&expected_rootfs).unwrap();
        temp.flush().unwrap();

        // Build expected in-memory multipart body byte-for-byte matching the old logic
        let mut expected_body = Vec::new();
        // metadata part
        expected_body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        expected_body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"metadata\"; filename=\"{metadata_filename}\"\r\n"
            )
            .as_bytes(),
        );
        expected_body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        expected_body.extend_from_slice(metadata_bytes);
        expected_body.extend_from_slice(b"\r\n");

        // rootfs part
        expected_body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        expected_body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"rootfs\"; filename=\"{rootfs_filename}\"\r\n"
            )
            .as_bytes(),
        );
        expected_body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        expected_body.extend_from_slice(&expected_rootfs);
        expected_body.extend_from_slice(b"\r\n");

        // closing boundary
        expected_body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        // Open file and create SplitImageBody
        let file = File::open(temp.path()).await.unwrap();
        let body = SplitImageBody::new(
            metadata_filename,
            metadata_bytes,
            rootfs_filename,
            file,
            rootfs_len as u64,
            boundary,
        );

        // Verify size_hint exact matches expected total length
        assert_eq!(body.size_hint().exact(), Some(expected_body.len() as u64));

        // Stream the body using BodyExt::collect
        let collected = body.collect().await.unwrap();
        let collected_bytes = collected.to_bytes();

        assert_eq!(collected_bytes.len(), expected_body.len());
        assert_eq!(collected_bytes.as_ref(), expected_body.as_slice());

        // Drive manually frame-by-frame to verify no single frame exceeds ROOTFS_CHUNK_SIZE
        // (except potentially the preamble if metadata is huge, but here metadata is small,
        // and all rootfs chunks must be <= ROOTFS_CHUNK_SIZE)
        let file = File::open(temp.path()).await.unwrap();
        let mut body = SplitImageBody::new(
            metadata_filename,
            metadata_bytes,
            rootfs_filename,
            file,
            rootfs_len as u64,
            boundary,
        );

        let mut frame_count = 0;
        let mut rootfs_frame_count = 0;

        while let Some(frame_res) = body.frame().await {
            let frame = frame_res.expect("frame should read without error");
            frame_count += 1;
            if let Some(data) = frame.data_ref() {
                // Frame data length must not exceed ROOTFS_CHUNK_SIZE for rootfs chunks
                assert!(data.len() <= ROOTFS_CHUNK_SIZE);
                if frame_count > 1 && !body.is_end_stream() {
                    rootfs_frame_count += 1;
                }
            }
        }

        // Preamble (1) + rootfs chunks (ceil(700 / 256) = 3) + closing (1) = 5 frames
        assert_eq!(frame_count, 5);
        assert_eq!(rootfs_frame_count, 3);
    }

    #[tokio::test]
    async fn test_split_image_body_empty_rootfs() {
        let metadata_filename = "metadata.tar.xz";
        let metadata_bytes = b"meta";
        let rootfs_filename = "rootfs.squashfs";
        let boundary = "bound";

        let temp = NamedTempFile::new().unwrap();
        let file = File::open(temp.path()).await.unwrap();
        let body = SplitImageBody::new(
            metadata_filename,
            metadata_bytes,
            rootfs_filename,
            file,
            0,
            boundary,
        );

        let collected = body.collect().await.unwrap().to_bytes();
        let expected_preamble = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"; filename=\"{metadata_filename}\"\r\nContent-Type: application/octet-stream\r\n\r\nmeta\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"rootfs\"; filename=\"{rootfs_filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        );
        let expected_closing = format!("\r\n--{boundary}--\r\n");
        let expected = format!("{expected_preamble}{expected_closing}");

        assert_eq!(collected.as_ref(), expected.as_bytes());
    }
}
