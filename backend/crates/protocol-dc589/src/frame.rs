//! DC589 二进制分帧：`EE | LEN | CMD | SESSION(6B) | DATA | SUM(XOR)`。
//!
//! LEN 计入 CMD、六字节会话、DATA 与 SUM（固定 8 字节 + DATA）；
//! SUM 为 LEN 到 DATA 的逐字节异或，不含 SOP。

use std::io;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 帧起始标识。
pub const START_BYTE: u8 = 0xEE;
/// LEN 计入的固定 8 字节：CMD、六字节会话与 SUM。
pub const HEADER_SIZE: usize = 8;

/// 帧错误：起始字节、长度或校验不符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 起始字节错误或帧过短。
    Frame,
    /// LEN 与实际长度不符，或载荷超限。
    Length,
    /// XOR 校验失败。
    Sum,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame => write!(f, "无效的 5.8.9 帧"),
            Self::Length => write!(f, "无效的 5.8.9 长度"),
            Self::Sum => write!(f, "无效的 5.8.9 校验和"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<FrameError> for io::Error {
    fn from(e: FrameError) -> Self {
        io::Error::other(e)
    }
}

/// 一条协议帧：命令字、六字节会话与载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// 命令字节（如 0xA0）。
    pub command: u8,
    /// 六字节会话（设备主动帧为全零）。
    pub session: [u8; 6],
    /// 载荷（不含帧头）。
    pub data: Vec<u8>,
}

impl Frame {
    /// 构造帧。
    #[must_use]
    pub fn new(command: u8, session: [u8; 6], data: Vec<u8>) -> Self {
        Self {
            command,
            session,
            data,
        }
    }
}

/// LEN、CMD、SESSION、DATA 的逐字节异或。
#[must_use]
pub fn checksum(payload: &[u8]) -> u8 {
    payload.iter().fold(0u8, |acc, b| acc ^ b)
}

/// 编码帧：起始字节、长度与 XOR 校验；载荷最多 247 字节。
pub fn encode(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    let length = HEADER_SIZE + frame.data.len();
    if length > 255 {
        return Err(FrameError::Length);
    }
    let mut out = Vec::with_capacity(2 + length);
    out.push(START_BYTE);
    out.push(length as u8);
    out.push(frame.command);
    out.extend_from_slice(&frame.session);
    out.extend_from_slice(&frame.data);
    // SUM 覆盖 LEN 到 DATA（不含 SOP 与自身）。
    let sum = checksum(&out[1..]);
    out.push(sum);
    Ok(out)
}

/// 解码整帧：检查起始字节、长度与 XOR 校验，返回独立载荷。
pub fn decode(raw: &[u8]) -> Result<Frame, FrameError> {
    if raw.len() < 2 + HEADER_SIZE || raw[0] != START_BYTE {
        return Err(FrameError::Frame);
    }
    let len = raw[1] as usize;
    if len < HEADER_SIZE || raw.len() != 2 + len {
        return Err(FrameError::Length);
    }
    if checksum(&raw[1..raw.len() - 1]) != raw[raw.len() - 1] {
        return Err(FrameError::Sum);
    }
    let mut session = [0u8; 6];
    session.copy_from_slice(&raw[3..9]);
    Ok(Frame::new(raw[2], session, raw[9..raw.len() - 1].to_vec()))
}

/// 从字节流读取一条完整帧，不依赖网络包边界。
/// 畸形帧直接报错，不为未认证设备扫描恢复分帧。
pub async fn read_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> io::Result<Frame> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head).await?;
    if head[0] != START_BYTE {
        return Err(FrameError::Frame.into());
    }
    let len = head[1] as usize;
    if len < HEADER_SIZE {
        return Err(FrameError::Length.into());
    }
    let mut rest = vec![0u8; len];
    reader.read_exact(&mut rest).await?;
    let mut raw = Vec::with_capacity(2 + len);
    raw.extend_from_slice(&head);
    raw.extend_from_slice(&rest);
    decode(&raw).map_err(io::Error::other)
}

/// 编码并写入一条帧（一次 write_all）。
pub async fn write_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    frame: &Frame,
) -> io::Result<()> {
    let raw = encode(frame).map_err(io::Error::other)?;
    writer.write_all(&raw).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let frame = Frame::new(0xA4, [1, 2, 3, 4, 5, 6], vec![0x53, 0x48, 0x11]);
        let raw = encode(&frame).unwrap();
        assert_eq!(raw[0], START_BYTE);
        assert_eq!(raw[1] as usize, HEADER_SIZE + 3);
        let decoded = decode(&raw).unwrap();
        assert_eq!(decoded, frame);
    }

    #[test]
    fn empty_payload_frame() {
        // 数据为空的帧：LEN = 8（CMD+session+SUM）。
        let frame = Frame::new(0xC7, [0; 6], vec![]);
        let raw = encode(&frame).unwrap();
        assert_eq!(raw.len(), 10);
        assert_eq!(raw[1], 8);
        assert_eq!(decode(&raw).unwrap(), frame);
    }

    #[test]
    fn payload_limit() {
        let frame = Frame::new(0xA4, [0; 6], vec![0; 248]);
        assert_eq!(encode(&frame), Err(FrameError::Length));
    }

    #[test]
    fn rejects_bad_start_and_sum() {
        let mut raw = encode(&Frame::new(0xA5, [0; 6], vec![1])).unwrap();
        raw[0] = 0xDD;
        assert_eq!(decode(&raw), Err(FrameError::Frame));
        let mut raw = encode(&Frame::new(0xA5, [0; 6], vec![1])).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        assert_eq!(decode(&raw), Err(FrameError::Sum));
    }

    #[test]
    fn golden_heartbeat_reply_frame() {
        // 协议文档 A5 示例帧（原文校验合法）。
        let raw: [u8; 10] = [0xEE, 0x09, 0xA5, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];
        // 文档示例的 SUM 在末字节之后——示例排版为 data 后接 SUM；实际字节流见下。
        // 按帧定义重新编码并与文档逐字节对照。
        let frame = Frame::new(0xA5, [0; 6], vec![0x01]);
        let built = encode(&frame).unwrap();
        // 文档：EE 09 A5 00 00 00 00 00 00 01 AD
        let doc: [u8; 11] = [0xEE, 0x09, 0xA5, 0, 0, 0, 0, 0, 0, 0x01, 0xAD];
        assert_eq!(built, doc);
        assert_eq!(decode(&doc).unwrap(), frame);
        let _ = raw;
    }

    #[tokio::test]
    async fn stream_read_spans_packets() {
        let frame = Frame::new(0xB7, [9, 8, 7, 6, 5, 4], vec![1, 2, 3, 4, 5, 6, 7]);
        let raw = encode(&frame).unwrap();
        let mut cursor = std::io::Cursor::new(raw);
        let read = read_frame(&mut cursor).await.unwrap();
        assert_eq!(read, frame);
    }

    #[test]
    fn reject_short_len() {
        // LEN < 8 直接拒绝。
        let raw = [START_BYTE, 5, 0xA5, 0, 0, 0, 0];
        let mut cursor = std::io::Cursor::new(raw.to_vec());
        assert!(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(read_frame(&mut cursor))
                .is_err()
        );
    }
}
